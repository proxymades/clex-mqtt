use super::*;
use clex_mqtt::domain::{Publication, PublicationProperties, SessionSpec};

async fn registration(h: &Harness, client: &Client, username: &str) -> Result<Value> {
    let options: Value = client
        .get(format!("{}/api/v1/account/options", h.url))
        .send()
        .await?
        .json()
        .await?;
    let result = client
        .post(format!("{}/api/v1/account/register", h.url))
        .header("Origin", &h.url)
        .header(
            "X-CSRF-Token",
            options["csrfToken"].as_str().context("preauth csrf")?,
        )
        .json(&json!({"username":username,"password":"account-test-password"}))
        .send()
        .await?;
    ensure!(
        result.status() == StatusCode::CREATED,
        "account registration failed: {}",
        result.status()
    );
    Ok(result.json().await?)
}
fn mutation(
    client: &Client,
    h: &Harness,
    csrf: &str,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    client
        .request(method, format!("{}{path}", h.url))
        .header("Origin", &h.url)
        .header("X-CSRF-Token", csrf)
}
async fn device(h: &Harness, client: &Client, csrf: &str, label: &str) -> Result<Value> {
    let session: Value = client
        .get(format!("{}/api/v1/account/session", h.url))
        .send()
        .await?
        .json()
        .await?;
    let filter = format!(
        "{}#",
        session["topicPrefix"].as_str().context("account prefix")?
    );
    let response=mutation(client,h,csrf,reqwest::Method::POST,"/api/v1/account/users")
        .json(&json!({"label":label,"password":"group-client-password","publishFilters":[filter],"subscribeFilters":[filter]})).send().await?;
    ensure!(
        response.status() == StatusCode::CREATED,
        "device creation failed: {}",
        response.status()
    );
    Ok(response.json().await?)
}
fn spec(principal: &str, client: &str) -> SessionSpec {
    SessionSpec {
        client_id: client.as_bytes().to_vec(),
        principal: principal.into(),
        clean_start: true,
        expiry_interval: 3600,
        protocol: 5,
        will: None,
        will_delay_interval: 0,
    }
}
fn publication(topic: &str, payload: &[u8], retain: bool) -> Publication {
    Publication {
        topic: topic.into(),
        payload: payload.to_vec(),
        qos: 1,
        retain,
        expires_at: None,
        properties: PublicationProperties::default(),
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn accounts_registration_scopes_messages_and_independent_sessions() -> Result<()> {
    let h = Harness::start().await?;
    let outcome:Result<()>=async {
        let alice=Client::builder().cookie_store(true).build()?;
        let bob=Client::builder().cookie_store(true).build()?;
        let body=json!({"username":"alice","password":"account-test-password"});
        ensure!(alice.post(format!("{}/api/v1/account/register",h.url)).header("Origin",&h.url).json(&body).send().await?.status()==StatusCode::FORBIDDEN);
        let a=registration(&h,&alice,"ALICE").await?;
        let b=registration(&h,&bob,"bob").await?;
        ensure!(a["username"]=="alice" && a["topicPrefix"]=="accounts/alice/" && b["topicPrefix"]=="accounts/bob/");
        let ac=a["csrfToken"].as_str().context("alice csrf")?;
        let bc=b["csrfToken"].as_str().context("bob csrf")?;
        for route in ["overview","users","accounts","audit","diagnostics","retained","messages","groups","bridges"] {
            ensure!(alice.get(format!("{}/api/v1/{route}",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED,"personal session opened administrator route {route}");
        }
        ensure!(alice.post(format!("{}/api/v1/account/users",h.url)).json(&json!({"label":"esp32","password":"group-client-password","publishFilters":["#"],"subscribeFilters":["#"]})).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::POST,"/api/v1/account/users").header("Origin","https://invalid.example").json(&json!({"label":"esp32","password":"group-client-password","publishFilters":["#"],"subscribeFilters":["#"]})).send().await?.status()==StatusCode::FORBIDDEN);
        let ad=device(&h,&alice,ac,"esp32").await?;
        let bd=device(&h,&bob,bc,"esp32").await?;
        ensure!(ad["username"]=="alice.esp32" && bd["username"]=="bob.esp32");
        let immutable=format!("/api/v1/account/users/{}",ad["id"].as_str().context("device id")?);
        for change in [json!({"label":"other"}),json!({"username":"other.esp32"})] {
            ensure!(mutation(&alice,&h,ac,reqwest::Method::PATCH,&immutable).json(&change).send().await?.status()==StatusCode::UNPROCESSABLE_ENTITY);
        }
        let foreign=format!("/api/v1/account/users/{}",bd["id"].as_str().context("bob device id")?);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::PATCH,&foreign).json(&json!({"enabled":false})).send().await?.status()==StatusCode::NOT_FOUND);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::DELETE,&foreign).send().await?.status()==StatusCode::NOT_FOUND);
        let own=format!("/api/v1/account/users/{}",ad["id"].as_str().context("alice device id")?);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::PATCH,&own).json(&json!({"ownerId":b["id"],"superuser":true})).send().await?.status()==StatusCode::UNPROCESSABLE_ENTITY);
        let ap=a["topicPrefix"].as_str().context("alice prefix")?;
        let bp=b["topicPrefix"].as_str().context("bob prefix")?;
        let au=ad["username"].as_str().context("alice mqtt")?;
        let bu=bd["username"].as_str().context("bob mqtt")?;
        ensure!(h.store.authorize(au,&format!("{ap}sensor/value"),true).await?);
        ensure!(!h.store.authorize(au,&format!("{bp}sensor/value"),true).await?);
        ensure!(!h.store.authorize_subscription(au,"#").await?);
        ensure!(h.store.authorize_subscription(au,&format!("$share/workers/{ap}#")).await?);
        let sa=h.store.open_session(spec(au,"esp32-01")).await?;
        let sb=h.store.open_session(spec(bu,"esp32-01")).await?;
        for (session,_user,prefix,bytes) in [(&sa,au,ap,b"alice-only".as_slice()),(&sb,bu,bp,b"bob-only".as_slice())] {
            h.store.accept_publication(session.session_id,session.generation,&publication(&format!("{prefix}state"),bytes,true),b"esp32-01").await?;
        }
        for (client,prefix,other) in [(&alice,ap,"bob-only"),(&bob,bp,"alice-only")] {
            let retained:Value=client.get(format!("{}/api/v1/account/retained",h.url)).send().await?.json().await?;
            ensure!(retained.as_array().context("retained")?.len()==1 && retained[0]["topic"].as_str().context("topic")?.starts_with(prefix));
            let messages:Value=client.get(format!("{}/api/v1/account/messages",h.url)).send().await?.json().await?;
            ensure!(messages["messages"].as_array().context("messages")?.len()==1 && messages.get("cursor").is_none());
            ensure!(messages["messages"][0].get("id").is_none(),"global stream sequence must not leak");
            let event_key=messages["messages"][0]["eventKey"].as_str().context("private event key")?;
            ensure!(event_key.parse::<Uuid>()?.get_version_num()==4);
            let again:Value=client.get(format!("{}/api/v1/account/messages",h.url)).send().await?.json().await?;
            ensure!(again["messages"][0]["eventKey"]==event_key,"private event key changed across snapshots");
            ensure!(!messages.to_string().contains(other));
            let users:Value=client.get(format!("{}/api/v1/account/users",h.url)).send().await?.json().await?;
            ensure!(users.as_array().context("users")?.len()==1 && !users.to_string().contains("password"));
            let sessions:Value=client.get(format!("{}/api/v1/account/clients",h.url)).send().await?.json().await?;
            ensure!(sessions.as_array().context("sessions")?.len()==1);
        }
        ensure!(mutation(&alice,&h,ac,reqwest::Method::POST,"/api/v1/account/retained/delete").json(&json!({"topic":format!("{bp}state")})).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::POST,"/api/v1/account/clients/disconnect").json(&json!({"username":bu,"clientId":"esp32-01"})).send().await?.status()==StatusCode::NOT_FOUND);
        ensure!(mutation(&alice,&h,ac,reqwest::Method::POST,"/api/v1/account/retained/delete").json(&json!({"topic":format!("{ap}state")})).send().await?.status()==StatusCode::OK);
        let changed=mutation(&alice,&h,ac,reqwest::Method::POST,"/api/v1/account/password")
            .json(&json!({"currentPassword":"account-test-password","newPassword":"new-account-test-password"})).send().await?;
        ensure!(changed.status()==StatusCode::NO_CONTENT);
        ensure!(alice.get(format!("{}/api/v1/account/session",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED);
        ensure!(auth::verify_password("group-client-password",&h.store.credential(au).await?.context("mqtt credential")?.password_hash),"account password changed device credentials");
        let opts:Value=alice.get(format!("{}/api/v1/account/options",h.url)).send().await?.json().await?;
        let logged=mutation(&alice,&h,opts["csrfToken"].as_str().context("preauth")?,reqwest::Method::POST,"/api/v1/account/session").json(&json!({"username":"alice","password":"new-account-test-password"})).send().await?;
        ensure!(logged.status()==StatusCode::OK);
        let login:Value=logged.json().await?;
        let lc=login["csrfToken"].as_str().context("login csrf")?;
        ensure!(alice.delete(format!("{}/api/v1/account/session",h.url)).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(mutation(&alice,&h,lc,reqwest::Method::DELETE,"/api/v1/account/session").send().await?.status()==StatusCode::NO_CONTENT);
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn accounts_admin_limits_revocation_and_mqtt_connections() -> Result<()> {
    use tokio::io::AsyncReadExt;
    let h = Harness::start().await?;
    let mut tasks = vec![];
    let outcome:Result<()>=async {
        let person=Client::builder().cookie_store(true).build()?;
        let other=Client::builder().cookie_store(true).build()?;
        let account=registration(&h,&person,"limited").await?;
        let other_account=registration(&h,&other,"other").await?;
        let csrf=account["csrfToken"].as_str().context("csrf")?;
        let first=device(&h,&person,csrf,"one").await?;
        let second=device(&h,&person,csrf,"two").await?;
        let outsider=device(&h,&other,other_account["csrfToken"].as_str().context("other csrf")?,"outside").await?;
        let admin=Client::builder().cookie_store(true).build()?;
        let session:Value=admin.post(format!("{}/api/v1/session",h.url)).json(&json!({"username":"test-admin","password":"test-admin-password"})).send().await?.json().await?;
        let token=session["csrfToken"].as_str().context("admin csrf")?;
        ensure!(admin.get(format!("{}/api/v1/account/session",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED);
        let path=format!("/api/v1/accounts/{}",account["id"].as_str().context("id")?);
        let limits=json!({"maxMqttUsers":2,"maxConnections":1,"maxSessions":2,"maxQueuedMessages":10,"maxQueuedBytes":16384,"maxPublishRate":20});
        ensure!(mutation(&admin,&h,token,reqwest::Method::PATCH,&path).json(&json!({"limits":limits})).send().await?.status()==StatusCode::NO_CONTENT);
        let rejected=mutation(&person,&h,csrf,reqwest::Method::POST,"/api/v1/account/users").json(&json!({"label":"three","password":"group-client-password","publishFilters":[format!("{}#", account["topicPrefix"].as_str().context("prefix")?)],"subscribeFilters":[]})).send().await?;
        ensure!(rejected.status()==StatusCode::CONFLICT);
        let mut live=access_test_mqtt_client(h.broker.clone(),first["username"].as_str().context("mqtt")?,&mut tasks).await?;
        ensure!(access_test_mqtt_client(h.broker.clone(),second["username"].as_str().context("second mqtt")?,&mut tasks).await.is_err(),"account exceeded concurrent connection limit");
        let mut outside=access_test_mqtt_client(h.broker.clone(),outsider["username"].as_str().context("outsider mqtt")?,&mut tasks).await?;
        ensure!(mutation(&admin,&h,token,reqwest::Method::PATCH,&path).json(&json!({"enabled":false})).send().await?.status()==StatusCode::NO_CONTENT);
        let mut byte=[0];ensure!(tokio::time::timeout(Duration::from_secs(3),live.read(&mut byte)).await??==0,"disabled account stayed connected");
        use tokio::io::AsyncWriteExt;
        outside.write_all(&[0xc0,0]).await?;let mut pong=[0;2];tokio::time::timeout(Duration::from_secs(3),outside.read_exact(&mut pong)).await??;ensure!(pong==[0xd0,0],"blocking one owner disconnected another");
        ensure!(person.get(format!("{}/api/v1/account/session",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED);
        ensure!(!h.store.credential(first["username"].as_str().context("mqtt")?).await?.context("credential")?.enabled);
        let accounts:Value=admin.get(format!("{}/api/v1/accounts",h.url)).send().await?.json().await?;
        ensure!(accounts.as_array().context("accounts")?.len()==2 && !accounts.to_string().contains("password"));
        drop(outside);
        Ok(())
    }.await;
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await?;
    }
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn accounts_registration_switch_capacity_and_atomic_device_limit() -> Result<()> {
    let disabled = Harness::start_with_account_options(true, false, false, 100).await?;
    let outcome: Result<()> = async {
        let client = Client::builder().cookie_store(true).build()?;
        let options: Value = client
            .get(format!("{}/api/v1/account/options", disabled.url))
            .send()
            .await?
            .json()
            .await?;
        ensure!(options["registrationEnabled"] == false);
        let result = mutation(
            &client,
            &disabled,
            options["csrfToken"].as_str().context("csrf")?,
            reqwest::Method::POST,
            "/api/v1/account/register",
        )
        .json(&json!({"username":"blocked","password":"account-test-password"}))
        .send()
        .await?;
        ensure!(result.status() == StatusCode::FORBIDDEN);
        Ok(())
    }
    .await;
    let cleanup = disabled.close().await;
    outcome?;
    cleanup?;
    let h = Harness::start_with_account_options(true, false, true, 1).await?;
    let outcome:Result<()>=async {
        let person=Client::builder().cookie_store(true).build()?;
        let account=registration(&h,&person,"only-account").await?;
        let id:Uuid=account["id"].as_str().context("id")?.parse()?;
        sqlx::query("UPDATE accounts SET max_mqtt_users=1 WHERE id=$1").bind(id).execute(&h.pool).await?;
        let csrf=account["csrfToken"].as_str().context("csrf")?;
        let filter=format!("{}#",account["topicPrefix"].as_str().context("prefix")?);
        let create=|label:&str|mutation(&person,&h,csrf,reqwest::Method::POST,"/api/v1/account/users").json(&json!({"label":label,"password":"group-client-password","publishFilters":[filter],"subscribeFilters":[filter]})).send();
        let (left,right)=tokio::join!(create("left"),create("right"));
        let statuses=[left?.status(),right?.status()];
        ensure!(statuses.contains(&StatusCode::CREATED) && statuses.contains(&StatusCode::CONFLICT),"concurrent device creation exceeded account quota");
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM mqtt_users WHERE owner_id=$1").bind(id).fetch_one(&h.pool).await?;
        ensure!(count==1);
        let another=Client::builder().cookie_store(true).build()?;
        let options:Value=another.get(format!("{}/api/v1/account/options",h.url)).send().await?.json().await?;
        let response=mutation(&another,&h,options["csrfToken"].as_str().context("preauth")?,reqwest::Method::POST,"/api/v1/account/register")
            .json(&json!({"username":"excess","password":"account-test-password"})).send().await?;
        ensure!(response.status()==StatusCode::CONFLICT);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM accounts").fetch_one(&h.pool).await?;ensure!(count==1);
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn accounts_login_limits_audit_and_expired_sessions_fail_closed() -> Result<()> {
    let h = Harness::start().await?;
    let outcome:Result<()>=async {
        let person=Client::builder().cookie_store(true).build()?;
        let account=registration(&h,&person,"audit-account").await?;
        let id:Uuid=account["id"].as_str().context("id")?.parse()?;
        sqlx::query("UPDATE account_sessions SET expires_at=now()-interval '1 second' WHERE account_id=$1").bind(id).execute(&h.pool).await?;
        ensure!(person.get(format!("{}/api/v1/account/session",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED);
        let options:Value=person.get(format!("{}/api/v1/account/options",h.url)).send().await?.json().await?;
        let csrf=options["csrfToken"].as_str().context("preauth")?;
        let body=json!({"username":"audit-account","password":"incorrect-password"});
        ensure!(person.post(format!("{}/api/v1/account/session",h.url)).header("X-CSRF-Token",csrf).json(&body).send().await?.status()==StatusCode::FORBIDDEN,"login accepted absent Origin");
        for _ in 0..9 {
            ensure!(mutation(&person,&h,csrf,reqwest::Method::POST,"/api/v1/account/session").json(&body).send().await?.status()==StatusCode::UNAUTHORIZED);
        }
        ensure!(mutation(&person,&h,csrf,reqwest::Method::POST,"/api/v1/account/session").json(&body).send().await?.status()==StatusCode::TOO_MANY_REQUESTS);
        let limits:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='account.login_limited'").fetch_one(&h.pool).await?;
        ensure!(limits==1,"limited account login was not audited");
        let row:Value=sqlx::query_scalar("SELECT details FROM audit_events WHERE action='account.login_failed' ORDER BY id DESC LIMIT 1").fetch_one(&h.pool).await?;
        ensure!(row["ip"]=="127.0.0.1" && row["result"]=="failed");
        let serialized=serde_json::to_string(&row)?;
        ensure!(!serialized.contains("incorrect-password") && !serialized.contains("account-test-password"));
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn accounts_full_filters_match_admin_and_keep_qos0_connected() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::start().await?;
    let mut tasks = Vec::new();
    let outcome: Result<()> = async {
        let person = Client::builder().cookie_store(true).build()?;
        let account = registration(&h, &person, "topic-owner").await?;
        let csrf = account["csrfToken"].as_str().context("csrf")?;
        let prefix = account["topicPrefix"].as_str().context("prefix")?;
        let topic = format!("{prefix}esp32/ask");
        let broad = format!("{prefix}#");
        let created = mutation(&person, &h, csrf, reqwest::Method::POST, "/api/v1/account/users")
            .json(&json!({"label":"home","password":"group-client-password","publishFilters":[topic],"subscribeFilters":[broad]}))
            .send().await?;
        ensure!(created.status() == StatusCode::CREATED);
        let mqtt_user: Value = created.json().await?;
        let username = mqtt_user["username"].as_str().context("mqtt login")?;
        let own = format!("/api/v1/account/users/{}", mqtt_user["id"].as_str().context("mqtt id")?);
        for invalid in ["esp32/ask", "#", "accounts/topic-owner2/esp32/ask", "accounts/other/esp32/ask"] {
            let response = mutation(&person, &h, csrf, reqwest::Method::PATCH, &own)
                .json(&json!({"publishFilters":[invalid]})).send().await?;
            ensure!(response.status() == StatusCode::BAD_REQUEST, "ambiguous or foreign filter accepted");
        }
        for _ in 0..2 {
            let response = mutation(&person, &h, csrf, reqwest::Method::PATCH, &own)
                .json(&json!({"publishFilters":[topic],"subscribeFilters":[broad]})).send().await?;
            ensure!(response.status() == StatusCode::OK);
        }
        let personal: Value = person.get(format!("{}/api/v1/account/users", h.url)).send().await?.json().await?;
        ensure!(personal[0]["publishFilters"] == json!([topic]) && personal[0]["subscribeFilters"] == json!([broad]));
        let admin = Client::builder().cookie_store(true).build()?;
        ensure!(admin.post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"test-admin","password":"test-admin-password"}))
            .send().await?.status() == StatusCode::OK);
        let users: Value = admin.get(format!("{}/api/v1/users", h.url)).send().await?.json().await?;
        let same = users.as_array().context("users")?.iter().find(|u| u["username"] == username).context("owned user")?;
        ensure!(same["publishFilters"] == personal[0]["publishFilters"] && same["subscribeFilters"] == personal[0]["subscribeFilters"]);
        ensure!(h.store.authorize(username, &topic, true).await?);
        ensure!(!h.store.authorize(username, "accounts/other/esp32/ask", true).await?);
        let mut client = access_test_mqtt_client(h.broker.clone(), username, &mut tasks).await?;
        for _ in 0..5 {
            let mut body = Vec::new();
            body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
            body.extend_from_slice(topic.as_bytes());
            body.extend_from_slice(b"value");
            ensure!(body.len() < 128);
            let mut packet = vec![0x31, body.len() as u8];
            packet.extend(body);
            packet.extend_from_slice(&[0xc0, 0]);
            client.write_all(&packet).await?;
            let mut pong = [0; 2];
            tokio::time::timeout(Duration::from_secs(3), client.read_exact(&mut pong)).await??;
            ensure!(pong == [0xd0, 0], "full topic QoS 0 publication closed its connection");
        }
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM retained WHERE topic=$1")
            .bind(topic.as_bytes()).fetch_one(&h.pool).await?;
        ensure!(retained == 1);
        Ok(())
    }.await;
    for task in tasks {
        task.abort();
    }
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
