use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn bridge_api_protects_secrets_revisions_origins_and_audit() -> Result<()> {
    let h = Harness::start().await?;
    let outcome:Result<()>=async {
        let url=format!("{}/api/v1/bridges",h.url);
        let client=Client::builder().cookie_store(true).build()?;
        ensure!(client.get(&url).send().await?.status()==StatusCode::UNAUTHORIZED);
        let login:Value=client.post(format!("{}/api/v1/session",h.url)).json(&json!({"username":"test-admin","password":"test-admin-password"})).send().await?.json().await?;
        let csrf=login["csrfToken"].as_str().context("csrf")?;
        let body=json!({"name":"Test link","endpoint":"mqtt://127.0.0.1:1883","username":"remote-account","password":"bridge-fixture-password","enabled":true,"rules":[{"direction":"out","filter":"#","localPrefix":"edge/","remotePrefix":"cloud/"}]});
        let send=|method:reqwest::Method,path:&str|client.request(method,path).header("Origin",&h.url).header("X-CSRF-Token",csrf);
        ensure!(client.post(&url).json(&body).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(client.post(&url).header("Origin","https://other.example").header("X-CSRF-Token",csrf).json(&body).send().await?.status()==StatusCode::FORBIDDEN);
        for patch in [json!({"endpoint":"mqtt://remote-account:secret@localhost:1883"}),json!({"endpoint":"https://localhost"}),json!({"endpoint":"mqtt://localhost:0"}),json!({"rules":[{"direction":"out","filter":"#","localPrefix":"missing-slash","remotePrefix":""}]}),json!({"caPem":"invalid PEM fixture","endpoint":"mqtts://localhost"})] {
            let mut invalid=body.clone(); for (key,value) in patch.as_object().context("patch")? {invalid[key]=value.clone();}
            ensure!(send(reqwest::Method::POST,&url).json(&invalid).send().await?.status()==StatusCode::BAD_REQUEST);
        }
        // Seed Retain before creating the bridge through the real admin API.
        sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,superuser) VALUES($1,'retained-source','fixture',true)").bind(Uuid::new_v4()).execute(&h.pool).await?;
        let source=h.store.open_session(clex_mqtt::domain::SessionSpec{client_id:b"seed".to_vec(),principal:"retained-source".into(),clean_start:true,expiry_interval:60,protocol:5,will:None,will_delay_interval:0}).await?;
        h.store.accept_publication(source.session_id,source.generation,&clex_mqtt::domain::Publication{topic:"edge/state".into(),payload:b"ready".to_vec(),qos:1,retain:true,expires_at:None,properties:Default::default()},b"seed").await?;
        let response=send(reqwest::Method::POST,&url).json(&body).send().await?;
        ensure!(response.status()==StatusCode::CREATED);
        let created:Value=response.json().await?;
        let id=Uuid::parse_str(created["id"].as_str().context("id")?)?;
        let encrypted:Vec<u8>=sqlx::query_scalar("SELECT password FROM mqtt_bridges WHERE id=$1").bind(id).fetch_one(&h.pool).await?;
        ensure!(!encrypted.windows(7).any(|part|part==b"fixture"));
        let key=clex_mqtt::bridge::SecretKey::parse(&STANDARD.encode([17;32]))?;
        ensure!(key.open(id,&encrypted)?=="bridge-fixture-password");
        ensure!(send(reqwest::Method::POST,&url).json(&body).send().await?.status()==StatusCode::CONFLICT);
        let view=client.get(&url).send().await?;
        ensure!(view.headers()["cache-control"]=="no-store");
        let text=view.text().await?;
        ensure!(!text.contains("bridge-fixture-password") && !text.contains("ciphertext") && !text.contains("secretKey\""));
        let list:Value=serde_json::from_str(&text)?;
        ensure!(list["secretKeyReady"]==true && list["bridges"][0]["hasPassword"]==true && list["bridges"][0]["queued"]==1);
        let mut update=body.clone(); update.as_object_mut().context("body")?.remove("password"); update["revision"]=json!(1); update["name"]=json!("Renamed link");
        let path=format!("{url}/{id}");
        ensure!(send(reqwest::Method::PATCH,&path).json(&update).send().await?.status()==StatusCode::OK);
        ensure!(send(reqwest::Method::PATCH,&path).json(&update).send().await?.status()==StatusCode::CONFLICT);
        let preserved:Vec<u8>=sqlx::query_scalar("SELECT password FROM mqtt_bridges WHERE id=$1").bind(id).fetch_one(&h.pool).await?;
        ensure!(preserved==encrypted,"empty password update must preserve ciphertext");
        update["revision"]=json!(2); update["endpoint"]=json!("mqtt://another.example:1883");
        ensure!(send(reqwest::Method::PATCH,&path).json(&update).send().await?.status()==StatusCode::BAD_REQUEST,"saved secrets must require explicit reassignment to a new server");
        update["clearPassword"]=json!(true);
        ensure!(send(reqwest::Method::PATCH,&path).json(&update).send().await?.status()==StatusCode::OK);
        ensure!(send(reqwest::Method::DELETE,&path).send().await?.status()==StatusCode::OK);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries WHERE bridge_id=$1").bind(id).fetch_one(&h.pool).await?;ensure!(count==0);
        let audit:Value=client.get(format!("{}/api/v1/audit",h.url)).send().await?.json().await?;
        let audit=serde_json::to_string(&audit)?;
        ensure!(audit.contains("bridge.create") && audit.contains("bridge.update") && audit.contains("bridge.delete") && !audit.contains("bridge-fixture-password"));
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
