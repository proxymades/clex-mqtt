use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clex_mqtt::{
    auth,
    broker::{Broker, BrokerConfig},
    config::Config,
    migrations,
    store::Store,
    web::{self, WebState},
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{env, sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
#[path = "support/accounts_api.rs"]
mod accounts_api;
#[path = "support/bridge_api.rs"]
mod bridge_api;
#[path = "support/diagnostics_api.rs"]
mod diagnostics_api;
#[path = "support/login_security.rs"]
mod login_security;
#[path = "support/metrics_api.rs"]
mod metrics_api;

struct Harness {
    pool: PgPool,
    store: Store,
    admin: PgPool,
    schema: String,
    broker: Broker,
    stop: CancellationToken,
    task: JoinHandle<()>,
    url: String,
}

async fn access_test_mqtt_client(
    broker: Broker,
    username: &str,
    tasks: &mut Vec<JoinHandle<Result<()>>>,
) -> Result<tokio::io::DuplexStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut client, server) = tokio::io::duplex(4096);
    tasks.push(tokio::spawn(async move {
        broker
            .run_io(server, "127.0.0.1:12345".parse().unwrap())
            .await
    }));
    let mut body = Vec::new();
    let string = |body: &mut Vec<u8>, value: &str| {
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value.as_bytes());
    };
    string(&mut body, "MQTT");
    body.extend_from_slice(&[4, 0xc2, 0, 60]);
    string(&mut body, "esp32-01");
    string(&mut body, username);
    string(&mut body, "group-client-password");
    ensure!(body.len() < 128);
    let mut packet = vec![0x10, body.len() as u8];
    packet.extend(body);
    client.write_all(&packet).await?;
    let mut ack = [0; 4];
    tokio::time::timeout(Duration::from_secs(3), client.read_exact(&mut ack)).await??;
    ensure!(
        ack == [0x20, 2, 0, 0],
        "MQTT authentication failed in access fixture"
    );
    Ok(client)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn groups_membership_inspector_csrf_and_targeted_revocation() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::start().await?;
    let mut tasks = Vec::new();
    let outcome: Result<()> = async {
        let client=Client::builder().cookie_store(true).build()?;
        ensure!(client.get(format!("{}/api/v1/groups",h.url)).send().await?.status()==StatusCode::UNAUTHORIZED);
        ensure!(client.post(format!("{}/api/v1/access/check",h.url)).json(&json!({"username":"group-member","action":"publish","topic":"workshop/state"})).send().await?.status()==StatusCode::UNAUTHORIZED);
        let login: Value=client.post(format!("{}/api/v1/session",h.url)).json(&json!({"username":"test-admin","password":"test-admin-password"})).send().await?.json().await?;
        let csrf=login["csrfToken"].as_str().context("CSRF token")?;
        let request=|method, path: &str| client.request(method,format!("{}{path}",h.url)).header("Origin",&h.url).header("X-CSRF-Token",csrf);
        let group_body=json!({"name":"Workshop","description":"Telemetry access","publishFilters":["workshop/#"],"subscribeFilters":["workshop/#"]});
        ensure!(client.post(format!("{}/api/v1/groups",h.url)).json(&group_body).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(request(reqwest::Method::POST,"/api/v1/groups").header("Origin","https://invalid.example").json(&group_body).send().await?.status()==StatusCode::FORBIDDEN);
        ensure!(client.post(format!("{}/api/v1/groups",h.url)).header("X-CSRF-Token",csrf).header("Origin","https://invalid.example").json(&group_body).send().await?.status()==StatusCode::FORBIDDEN);
        let created=request(reqwest::Method::POST,"/api/v1/groups").json(&group_body).send().await?;
        ensure!(created.status()==StatusCode::CREATED);
        let group: Value=created.json().await?;
        let id=group["id"].as_str().context("group id")?;
        let group_path=format!("/api/v1/groups/{id}");
        for username in ["group-member","group-outsider"] {
            let group_ids=if username=="group-member" { vec![id] } else { vec![] };
            let response=request(reqwest::Method::POST,"/api/v1/users").json(&json!({"username":username,"password":"group-client-password","publishFilters":[],"subscribeFilters":[],"groupIds":group_ids})).send().await?;
            ensure!(response.status()==StatusCode::CREATED);
        }
        let users: Value=client.get(format!("{}/api/v1/users",h.url)).send().await?.json().await?;
        let member=users.as_array().context("users")?.iter().find(|user| user["username"]=="group-member").context("member")?;
        ensure!(member["groups"][0]["id"]==id && member["publishFilters"].as_array().context("personal rules")?.is_empty());
        let result: Value=request(reqwest::Method::POST,"/api/v1/access/check").json(&json!({"username":"group-member","action":"publish","topic":"workshop/state"})).send().await?.json().await?;
        ensure!(result["decision"]["allowed"]==true && result["decision"]["matchingRules"][0]["groupId"]==id);
        ensure!(!result.to_string().contains("password"));
        ensure!(!h.store.authorize("group-outsider","workshop/state",true).await?);
        let failed=request(reqwest::Method::PATCH,"/api/v1/users/group-member").json(&json!({"publishFilters":["unexpected/#"],"groupIds":[Uuid::new_v4()]})).send().await?;
        ensure!(failed.status()==StatusCode::BAD_REQUEST);
        ensure!(h.store.authorize("group-member","workshop/state",true).await? && !h.store.authorize("group-member","unexpected/value",true).await?,"invalid membership update must roll back all rights");
        let mut member_client=access_test_mqtt_client(h.broker.clone(),"group-member",&mut tasks).await?;
        let mut outsider=access_test_mqtt_client(h.broker.clone(),"group-outsider",&mut tasks).await?;
        ensure!(h.broker.clients().await.len()==2);
        let updated=request(reqwest::Method::PATCH,&group_path).json(&json!({"denyPublishFilters":["workshop/private/#"]})).send().await?;
        ensure!(updated.status()==StatusCode::OK);
        let mut byte=[0]; ensure!(tokio::time::timeout(Duration::from_secs(2),member_client.read(&mut byte)).await??==0,"group mutation must close an affected MQTT connection");
        outsider.write_all(&[0xc0,0]).await?;
        let mut pong=[0;2]; tokio::time::timeout(Duration::from_secs(2),outsider.read_exact(&mut pong)).await??;
        ensure!(pong==[0xd0,0],"another account with the same Client ID must stay connected");
        let denied: Value=request(reqwest::Method::POST,"/api/v1/access/check").json(&json!({"username":"group-member","action":"publish","topic":"workshop/private/value"})).send().await?.json().await?;
        ensure!(denied["decision"]["allowed"]==false && denied["decision"]["reason"]=="explicit_deny");
        let shared: Value=request(reqwest::Method::POST,"/api/v1/access/check").json(&json!({"username":"group-member","action":"subscribe","topic":"$share/workers/workshop/#"})).send().await?.json().await?;
        ensure!(shared["decision"]["allowed"]==true);
        ensure!(request(reqwest::Method::POST,"/api/v1/access/check").json(&json!({"username":"group-member","action":"publish","topic":"workshop/#"})).send().await?.status()==StatusCode::BAD_REQUEST);
        ensure!(request(reqwest::Method::PATCH,&group_path).json(&json!({"enabled":false})).send().await?.status()==StatusCode::OK);
        ensure!(!h.store.authorize("group-member","workshop/state",true).await?);
        ensure!(request(reqwest::Method::DELETE,&group_path).send().await?.status()==StatusCode::NO_CONTENT);
        let remaining: i64=sqlx::query_scalar("SELECT count(*) FROM mqtt_user_groups").fetch_one(&h.pool).await?;
        ensure!(remaining==0);
        let audit: Value=client.get(format!("{}/api/v1/audit",h.url)).send().await?.json().await?;
        ensure!(audit.as_array().context("audit")?.iter().any(|event| event["action"]=="access_group.delete"));
        Ok(())
    }.await;
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await?;
    }
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
impl Harness {
    async fn start() -> Result<Self> {
        Self::start_with_metrics(true).await
    }
    async fn start_with_metrics(metrics_enabled: bool) -> Result<Self> {
        Self::start_with_proxy(metrics_enabled, false).await
    }
    async fn start_with_proxy(metrics_enabled: bool, trusted: bool) -> Result<Self> {
        Self::start_with_account_options(metrics_enabled, trusted, true, 100).await
    }
    async fn start_with_account_options(
        metrics_enabled: bool,
        trusted: bool,
        registration_enabled: bool,
        max_accounts: i64,
    ) -> Result<Self> {
        let url = env::var("TEST_DATABASE_URL")
            .context("TEST_DATABASE_URL must name an isolated test database")?;
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        let schema = format!("api_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        let search_path = format!("SET search_path TO {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .after_connect(move |connection, _| {
                let path = search_path.clone();
                Box::pin(async move {
                    sqlx::query(&path).execute(connection).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await?;
        migrations::apply(&pool).await?;
        sqlx::query(
            "INSERT INTO admin_users (id,username,password_hash) VALUES ($1,'test-admin',$2)",
        )
        .bind(Uuid::new_v4())
        .bind(auth::hash_password("test-admin-password")?)
        .execute(&pool)
        .await?;
        let store = Store::from_pool(pool.clone());
        let broker = Broker::new(store.clone(), BrokerConfig::default()).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let base = format!("http://{address}");
        let config = Config {
            metrics_token: metrics_enabled
                .then(|| {
                    clex_mqtt::metrics::TokenDigest::parse(
                        "test_metrics_token_0123456789_abcdefghijklmnopqrstuvwxyz",
                    )
                })
                .transpose()?,
            bridge_key: Some(
                clex_mqtt::bridge::SecretKey::parse(&STANDARD.encode([17; 32])).unwrap(),
            ),
            database_url: url,
            environment: "test".into(),
            mqtt_addr: "127.0.0.1:0".parse()?,
            http_addr: address,
            public_url: base.parse()?,
            cookie_secure: false,
            registration_enabled,
            max_accounts,
            http_trusted_proxy_ips: if trusted {
                vec!["127.0.0.1".parse()?]
            } else {
                vec![]
            },
            database_connections: 8,
            max_packet_size: 1048576,
            max_connections: 100,
            max_inflight: 16,
            max_pending: 100,
            max_pending_bytes: 1048576,
            tls_addr: None,
            tls_cert: None,
            tls_key: None,
            tls_client_auth: clex_mqtt::tls::ClientAuth::None,
            tls_client_ca: None,
            shutdown_timeout: Duration::from_secs(3),
        };
        let app = web::router(WebState::new(
            pool.clone(),
            broker.clone(),
            Arc::new(config),
        )?);
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await;
        });
        Ok(Self {
            pool,
            store,
            admin,
            schema,
            broker,
            stop,
            task,
            url: base,
        })
    }
    async fn close(self) -> Result<()> {
        self.stop.cancel();
        self.broker.shutdown().await?;
        self.task.await?;
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn message_stream_requires_admin_and_filters_committed_publications() -> Result<()> {
    use clex_mqtt::domain::{Publication, SessionSpec};
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        let client = Client::builder().cookie_store(true).build()?;
        ensure!(client.get(format!("{}/api/v1/messages", h.url)).send().await?.status() == StatusCode::UNAUTHORIZED);
        let login = client.post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"test-admin","password":"test-admin-password"})).send().await?;
        ensure!(login.status() == StatusCode::OK);
        let hash = auth::hash_password("message-stream-fixture")?;
        let mut sources = Vec::new();
        for name in ["source-a", "source-b"] {
            sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,enabled,superuser) VALUES($1,$2,$3,true,true)")
                .bind(Uuid::new_v4()).bind(name).bind(&hash).execute(&h.pool).await?;
            sources.push(h.store.open_session(SessionSpec {
                client_id: b"esp32-01".to_vec(), principal: name.into(), clean_start: true,
                expiry_interval: 300, protocol: 5, will: None, will_delay_interval: 0,
            }).await?);
        }
        for (index, name) in ["source-a", "source-b", "source-a"].into_iter().enumerate() {
            let session = &sources[usize::from(name == "source-b")];
            h.store.accept_publication(session.session_id, session.generation, &Publication {
                topic: format!("fleet/{name}/value"), payload: vec![0, 255, index as u8], qos: 1,
                retain: false, expires_at: None, properties: Default::default(),
            }, b"esp32-01").await?;
        }
        let response = client.get(format!("{}/api/v1/messages?after=0&clientId=esp32-01&username=source-a&topic=fleet%2F%2B%2Fvalue&limit=1", h.url)).send().await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(response.headers().get("cache-control").and_then(|value| value.to_str().ok()) == Some("no-store"));
        let first: Value = response.json().await?;
        ensure!(first["messages"].as_array().context("messages array")?.len() == 1);
        ensure!(first["messages"][0]["clientId"] == "esp32-01" && first["messages"][0]["payloadBase64"] == "AP8A");
        ensure!(first["hasMore"] == true && first["cursor"] == 2);
        let next: Value = client.get(format!("{}/api/v1/messages?after=2&clientId=esp32-01", h.url)).send().await?.json().await?;
        ensure!(next["messages"][0]["id"] == 3 && next["hasMore"] == false);
        let empty: Value = client.get(format!("{}/api/v1/messages?clientId=esp32-01&username=missing-account", h.url)).send().await?.json().await?;
        ensure!(empty["messages"].as_array().context("filtered array")?.is_empty());
        let connections: Value = client.get(format!("{}/api/v1/clients", h.url)).send().await?.json().await?;
        let connections = connections.as_array().context("client array")?;
        ensure!(connections.len() == 2 && connections.iter().all(|item| item["clientId"] == "esp32-01"));
        ensure!(connections[0]["sessionId"] != connections[1]["sessionId"]);
        for query in ["topic=bad%23filter", "limit=0", "limit=101"] {
            ensure!(client.get(format!("{}/api/v1/messages?{query}", h.url)).send().await?.status() == StatusCode::BAD_REQUEST);
        }
        let source = &sources[0];
        let publication = Publication { topic: "fleet/source-a/qos2".into(), payload: b"exactly once".to_vec(), qos: 2, retain: false, expires_at: None, properties: Default::default() };
        ensure!(h.store.receive_qos2(source.session_id, source.generation, 17, &publication).await?);
        ensure!(!h.store.receive_qos2(source.session_id, source.generation, 17, &publication).await?);
        let before = h.broker.monitor().read(&Default::default());
        ensure!(before.cursor == 3, "Unreleased QoS2 appeared in stream");
        h.store.release_qos2(source.session_id, source.generation, 17).await?;
        h.store.release_qos2(source.session_id, source.generation, 17).await?;
        let after = h.broker.monitor().read(&Default::default());
        ensure!(after.cursor == 4 && after.messages.last().context("QoS2 event")?.qos == 2);
        let failed = h.store.accept_publication(source.session_id, source.generation + 1, &publication, b"source-a").await;
        ensure!(failed.is_err() && h.broker.monitor().read(&Default::default()).cursor == 4);
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn broker_shutdown_waits_for_active_database_maintenance() -> Result<()> {
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        let mut lock = h.pool.begin().await?;
        sqlx::query("LOCK TABLE sessions IN ACCESS EXCLUSIVE MODE").execute(&mut *lock).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=to_regclass($1) AND NOT granted)")
                .bind(format!("{}.sessions", h.schema)).fetch_one(&h.admin).await?;
            if blocked { break; }
            ensure!(tokio::time::Instant::now() < deadline, "maintenance did not reach the held session table lock");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ensure!(tokio::time::timeout(Duration::from_millis(100), h.broker.shutdown()).await.is_err(),
            "shutdown returned while maintenance was still blocked in PostgreSQL");
        lock.rollback().await?;
        tokio::time::timeout(Duration::from_secs(3), h.broker.shutdown()).await??;
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn panel_sessions_csrf_acl_and_credential_redaction() -> Result<()> {
    let h = Harness::start().await?;
    let client = Client::builder().cookie_store(true).build()?;
    ensure!(
        client
            .get(format!("{}/api/v1/overview", h.url))
            .send()
            .await?
            .status()
            == StatusCode::UNAUTHORIZED
    );
    let failed = client
        .post(format!("{}/api/v1/session", h.url))
        .json(&json!({"username":"test-admin","password":"incorrect-password"}))
        .send()
        .await?;
    ensure!(failed.status() == StatusCode::UNAUTHORIZED);
    let login = client
        .post(format!("{}/api/v1/session", h.url))
        .json(&json!({"username":"test-admin","password":"test-admin-password"}))
        .send()
        .await?;
    ensure!(login.status() == StatusCode::OK);
    let cookie = login
        .headers()
        .get("set-cookie")
        .context("missing cookie")?
        .to_str()?;
    ensure!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let session: Value = login.json().await?;
    let csrf = session["csrfToken"]
        .as_str()
        .context("missing CSRF token")?;
    let input = json!({"username":"sensor-test","password":"test-device-password","publishFilters":["devices/test/#"],"subscribeFilters":["commands/test/#"]});
    let denied = client
        .post(format!("{}/api/v1/users", h.url))
        .json(&input)
        .send()
        .await?;
    ensure!(
        denied.status() == StatusCode::FORBIDDEN,
        "mutation without CSRF was allowed"
    );
    let cross_origin = client
        .post(format!("{}/api/v1/users", h.url))
        .header("x-csrf-token", csrf)
        .header("origin", "https://untrusted.example")
        .json(&input)
        .send()
        .await?;
    ensure!(
        cross_origin.status() == StatusCode::FORBIDDEN,
        "cross-origin mutation was allowed"
    );
    let created = client
        .post(format!("{}/api/v1/users", h.url))
        .header("x-csrf-token", csrf)
        .header("origin", &h.url)
        .json(&input)
        .send()
        .await?;
    ensure!(
        created.status() == StatusCode::CREATED,
        "user creation failed"
    );
    let users: Value = client
        .get(format!("{}/api/v1/users", h.url))
        .send()
        .await?
        .json()
        .await?;
    let serialized = users.to_string();
    ensure!(
        !serialized.contains("password")
            && !serialized.contains("argon2")
            && !serialized.contains("stored_key")
            && !serialized.contains("server_key")
            && !serialized.contains("scram_verifier"),
        "credential material leaked into API view"
    );
    ensure!(users[0]["publishFilters"][0] == "devices/test/#");
    ensure!(users[0]["scramReady"] == true);
    let before: Value =
        sqlx::query_scalar("SELECT scram_verifier FROM mqtt_users WHERE username='sensor-test'")
            .fetch_one(&h.pool)
            .await?;
    let changed = client
        .patch(format!("{}/api/v1/users/sensor-test", h.url))
        .header("x-csrf-token", csrf)
        .json(&json!({"enabled":false}))
        .send()
        .await?;
    ensure!(changed.status() == StatusCode::OK);
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM mqtt_users WHERE username='sensor-test'")
            .fetch_one(&h.pool)
            .await?;
    ensure!(!enabled);
    let after_rights: Value =
        sqlx::query_scalar("SELECT scram_verifier FROM mqtt_users WHERE username='sensor-test'")
            .fetch_one(&h.pool)
            .await?;
    ensure!(
        before == after_rights,
        "changing access must preserve SCRAM keys"
    );
    let rotated = client
        .patch(format!("{}/api/v1/users/sensor-test", h.url))
        .header("x-csrf-token", csrf)
        .json(&json!({"password":"rotated-api-password"}))
        .send()
        .await?;
    ensure!(rotated.status() == StatusCode::OK);
    let after_password: Value =
        sqlx::query_scalar("SELECT scram_verifier FROM mqtt_users WHERE username='sensor-test'")
            .fetch_one(&h.pool)
            .await?;
    ensure!(
        before != after_password,
        "password rotation must replace SCRAM keys"
    );
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM mqtt_users WHERE username='sensor-test'")
            .fetch_one(&h.pool)
            .await?;
    ensure!(clex_mqtt::auth::verify_password(
        "rotated-api-password",
        &hash
    ));
    let overview: Value = client
        .get(format!("{}/api/v1/overview", h.url))
        .send()
        .await?
        .json()
        .await?;
    ensure!(overview["database"]["healthy"] == true);
    let capabilities = overview["capabilities"]
        .as_array()
        .context("missing capabilities")?;
    ensure!(
        capabilities
            .iter()
            .any(|entry| entry["name"] == "Совместные подписки" && entry["status"] == "available")
    );
    ensure!(
        capabilities
            .iter()
            .any(|entry| entry["name"] == "MQTT 5.0" && entry["status"] == "available")
    );
    let logout = client
        .delete(format!("{}/api/v1/session", h.url))
        .header("x-csrf-token", csrf)
        .send()
        .await?;
    ensure!(logout.status() == StatusCode::NO_CONTENT);
    ensure!(
        client
            .get(format!("{}/api/v1/session", h.url))
            .send()
            .await?
            .status()
            == StatusCode::UNAUTHORIZED
    );
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn static_console_headers_and_readiness_are_available() -> Result<()> {
    let h = Harness::start().await?;
    let client = Client::new();
    let response = client.get(&h.url).send().await?;
    ensure!(
        response.status() == StatusCode::OK,
        "build the frontend before API integration tests"
    );
    ensure!(
        response
            .headers()
            .get("x-content-type-options")
            .and_then(|h| h.to_str().ok())
            == Some("nosniff")
    );
    ensure!(response.headers().contains_key("content-security-policy"));
    ensure!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            == Some("no-cache")
    );
    let html = response.text().await?;
    ensure!(html.contains("CLEX"));
    let icon = client
        .get(format!("{}/clex-mqtt.svg?v=2", h.url))
        .send()
        .await?;
    ensure!(icon.status() == StatusCode::OK);
    ensure!(
        icon.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            == Some("no-cache"),
        "Unversioned public assets must revalidate so updated branding is visible"
    );
    let bundle = html
        .split("src=\"/assets/")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .context("Missing hashed frontend bundle")?;
    let bundle = client
        .get(format!("{}/assets/{bundle}", h.url))
        .send()
        .await?;
    ensure!(bundle.status() == StatusCode::OK);
    ensure!(
        bundle
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            == Some("public, max-age=31536000, immutable")
    );
    ensure!(
        client
            .get(format!("{}/health/ready", h.url))
            .send()
            .await?
            .status()
            == StatusCode::OK
    );
    ensure!(
        client
            .get(format!("{}/api/v1/does-not-exist", h.url))
            .send()
            .await?
            .status()
            == StatusCode::NOT_FOUND
    );
    h.close().await
}
