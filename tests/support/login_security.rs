use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn login_limits_use_trusted_proxy_ip_and_audit_without_secrets() -> Result<()> {
    let h = Harness::start_with_proxy(true, true).await?;
    let outcome: Result<()> =
        async {
            let client = Client::builder().cookie_store(true).build()?;
            let login = |ip: &str, name: &str, password: &str| {
                client
                    .post(format!("{}/api/v1/session", h.url))
                    .header("X-Real-IP", ip)
                    .json(&json!({"username":name,"password":password}))
            };
            let response = login("192.0.2.20", "test-admin", "test-admin-password")
                .send()
                .await?;
            ensure!(response.status() == StatusCode::OK);
            ensure!(
                client
                    .get(format!("{}/api/v1/audit", h.url))
                    .send()
                    .await?
                    .status()
                    == StatusCode::OK
            );
            let wrong = login("192.0.2.10", "test-admin", "wrong-secret-value")
                .send()
                .await?;
            let absent = login("192.0.2.10", "unknown-admin", "wrong-secret-value")
                .send()
                .await?;
            ensure!(
                wrong.status() == StatusCode::UNAUTHORIZED
                    && absent.status() == StatusCode::UNAUTHORIZED
            );
            ensure!(
                wrong.text().await? == absent.text().await?,
                "unknown users must have the same public error"
            );
            for _ in 0..8 {
                ensure!(
                    login("192.0.2.10", "test-admin", "wrong-secret-value")
                        .send()
                        .await?
                        .status()
                        == StatusCode::UNAUTHORIZED
                );
            }
            for _ in 0..5 {
                let response = login("192.0.2.10", "test-admin", "test-admin-password")
                    .send()
                    .await?;
                ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
                ensure!(response.headers().get("retry-after").is_some());
            }
            ensure!(
                login("192.0.2.11", "test-admin", "test-admin-password")
                    .send()
                    .await?
                    .status()
                    == StatusCode::OK,
                "one remote IP must not block another through the same proxy"
            );
            let events: Value = client
                .get(format!("{}/api/v1/audit?ip=192.0.2.10", h.url))
                .send()
                .await?
                .json()
                .await?;
            let events = events.as_array().context("audit array")?;
            ensure!(
                events.len() == 11
                    && events
                        .iter()
                        .filter(|row| row["result"] == "limited")
                        .count()
                        == 1
            );
            ensure!(events.iter().any(
                |row| row["actor"] == "unknown-admin" && row["reason"] == "invalid_credentials"
            ));
            let encoded = serde_json::to_string(events)?;
            ensure!(
                !encoded.contains("wrong-secret-value")
                    && !encoded.contains("test-admin-password")
                    && !encoded.contains("password_hash")
                    && !encoded.contains("csrfToken")
            );
            let all: Value = client
                .get(format!("{}/api/v1/audit?limit=2", h.url))
                .send()
                .await?
                .json()
                .await?;
            let first = all.as_array().context("first page")?;
            ensure!(first.len() == 2);
            let before = first[1]["id"].as_i64().context("cursor")?;
            let next: Value = client
                .get(format!("{}/api/v1/audit?limit=2&before={before}", h.url))
                .send()
                .await?
                .json()
                .await?;
            ensure!(
                next.as_array()
                    .context("next page")?
                    .iter()
                    .all(|row| row["id"].as_i64().unwrap() < before)
            );
            let filtered: Value = client
                .get(format!(
                    "{}/api/v1/audit?search=unknown-admin&result=failed",
                    h.url
                ))
                .send()
                .await?
                .json()
                .await?;
            ensure!(filtered.as_array().context("filtered")?.len() == 1);
            for query in [
                "limit=0",
                "limit=101",
                "before=-1",
                "result=invalid",
                "ip=not-an-ip",
            ] {
                ensure!(
                    client
                        .get(format!("{}/api/v1/audit?{query}", h.url))
                        .send()
                        .await?
                        .status()
                        == StatusCode::BAD_REQUEST
                );
            }
            let duplicate = login("192.0.2.30", "test-admin", "test-admin-password")
                .header("x-real-ip", "192.0.2.31")
                .send()
                .await?;
            ensure!(duplicate.status() == StatusCode::BAD_REQUEST);
            ensure!(
                client
                    .post(format!("{}/api/v1/session", h.url))
                    .json(&json!({"username":"test-admin","password":"test-admin-password"}))
                    .send()
                    .await?
                    .status()
                    == StatusCode::BAD_REQUEST,
                "trusted proxy must supply a real IP"
            );
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
                .fetch_one(&h.pool)
                .await?;
            ensure!(
                count == 2,
                "failed and limited requests must not create sessions"
            );
            Ok(())
        }
        .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn untrusted_forwarded_headers_do_not_bypass_login_limits() -> Result<()> {
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        let client = Client::new();
        for i in 0..11 {
            let response = client
                .post(format!("{}/api/v1/session", h.url))
                .header("X-Real-IP", format!("192.0.2.{}", i + 1))
                .header("X-Forwarded-For", format!("198.51.100.{}", i + 1))
                .json(&json!({"username":"test-admin","password":"not-the-password"}))
                .send()
                .await?;
            ensure!(
                response.status()
                    == if i < 10 {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::TOO_MANY_REQUESTS
                    }
            );
        }
        let ips: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT details->>'ip' FROM audit_events")
                .fetch_all(&h.pool)
                .await?;
        ensure!(ips == vec!["127.0.0.1"]);
        ensure!(
            Client::new()
                .get(format!("{}/api/v1/audit", h.url))
                .send()
                .await?
                .status()
                == StatusCode::UNAUTHORIZED
        );
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn login_history_prunes_old_and_excess_events_preserving_admin_actions() -> Result<()> {
    let h = Harness::start().await?;
    let outcome:Result<()>=async {
        sqlx::query("INSERT INTO audit_events (actor,action,created_at) VALUES ('historic','user.create',now()-interval '40 days'),('old-login','admin.login',now()-interval '40 days')")
            .execute(&h.pool).await?;
        sqlx::query("INSERT INTO audit_events (actor,action) SELECT 'fixture','admin.login_failed' FROM generate_series(1,10005)").execute(&h.pool).await?;
        let client=Client::builder().cookie_store(true).build()?;
        ensure!(client.post(format!("{}/api/v1/session",h.url)).json(&json!({"username":"test-admin","password":"test-admin-password"}))
            .send().await?.status()==StatusCode::OK);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action LIKE 'admin.login%'").fetch_one(&h.pool).await?;
        ensure!(count==10000);
        let names:Vec<String>=sqlx::query_scalar("SELECT actor FROM audit_events WHERE actor IN ('historic','old-login')").fetch_all(&h.pool).await?;
        ensure!(names==vec!["historic"]);
        let older:Value=client.get(format!("{}/api/v1/audit?search=historic",h.url)).send().await?.json().await?;
        ensure!(older.as_array().context("history")?.len()==1,"ordinary audit history remains searchable beyond the newest 100 events");
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn successful_login_rolls_back_session_if_audit_cannot_be_committed() -> Result<()> {
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        sqlx::query("ALTER TABLE audit_events ADD CONSTRAINT fixture_reject_success CHECK (action <> 'admin.login')")
            .execute(&h.pool).await?;
        let client = Client::new();
        let response = client.post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"test-admin","password":"test-admin-password"})).send().await?;
        ensure!(response.status() == StatusCode::INTERNAL_SERVER_ERROR);
        ensure!(response.headers().get("set-cookie").is_none());
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&h.pool).await?;
        ensure!(sessions == 0);
        let invalid = client.post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"invalid\u{0}name","password":"not-the-password"})).send().await?;
        ensure!(invalid.status() == StatusCode::BAD_REQUEST);
        let events: Vec<String> = sqlx::query_scalar("SELECT actor FROM audit_events WHERE action='admin.login_failed'")
            .fetch_all(&h.pool).await?;
        ensure!(events == vec!["invalidname"]);
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
