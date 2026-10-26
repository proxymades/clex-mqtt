use super::*;
use clex_mqtt::domain::{Publication, SessionSpec, Subscription};

const TOKEN: &str = "test_metrics_token_0123456789_abcdefghijklmnopqrstuvwxyz";
fn metric(body: &str, name: &str) -> Result<f64> {
    let prefix = format!("{name} ");
    body.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .context("metric missing")?
        .parse()
        .map_err(Into::into)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn metrics_endpoint_requires_separate_bearer_and_is_disabled_without_token() -> Result<()> {
    let h = Harness::start_with_metrics(false).await?;
    let result = Client::new()
        .get(format!("{}/metrics", h.url))
        .bearer_auth(TOKEN)
        .send()
        .await;
    let cleanup = h.close().await;
    ensure!(result?.status() == StatusCode::NOT_FOUND);
    cleanup?;
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        let client = Client::builder().cookie_store(true).build()?;
        let url = format!("{}/metrics", h.url);
        for response in [
            client.get(&url).send().await?,
            client.get(&url).bearer_auth("wrong").send().await?,
            client.get(format!("{url}?token={TOKEN}")).send().await?,
            client
                .get(&url)
                .header("Authorization", format!("Bearer {TOKEN}"))
                .header("Authorization", format!("Bearer {TOKEN}"))
                .send()
                .await?,
        ] {
            ensure!(response.status() == StatusCode::UNAUTHORIZED);
            ensure!(response.headers()["cache-control"] == "no-store");
            ensure!(response.headers().contains_key("www-authenticate"));
        }
        client
            .post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"test-admin","password":"test-admin-password"}))
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            client.get(&url).send().await?.status() == StatusCode::UNAUTHORIZED,
            "admin cookie must not stand in for metrics token"
        );
        h.pool.close().await;
        ensure!(
            client.get(&url).send().await?.status() == StatusCode::UNAUTHORIZED,
            "authorization must precede all database work"
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
async fn metrics_report_committed_state_histograms_cache_and_database_failure() -> Result<()> {
    let h = Harness::start().await?;
    let outcome:Result<()>=async {
        let principal="private-metrics-fixture-account";
        sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,superuser) VALUES($1,$2,'fixture',true)").bind(Uuid::new_v4()).bind(principal).execute(&h.pool).await?;
        let session=|id:&str|SessionSpec{client_id:id.as_bytes().to_vec(),principal:principal.into(),clean_start:false,expiry_interval:600,protocol:5,will:None,will_delay_interval:0};
        let publisher=h.store.open_session(session("private-publisher")).await?;
        let subscriber=h.store.open_session(session("private-subscriber")).await?;
        h.store.subscribe(subscriber.session_id,subscriber.generation,&Subscription{filter:"private-metrics-topic/#".into(),qos:1,no_local:false,retain_as_published:true,retain_handling:0,subscription_identifier:None}).await?;
        let publication=Publication{topic:"private-metrics-topic/first".into(),payload:b"private-payload-never-export".to_vec(),qos:1,retain:true,expires_at:None,properties:Default::default()};
        h.store.accept_publication(publisher.session_id,publisher.generation,&publication,b"private-publisher").await?;
        let client=Client::new();let url=format!("{}/metrics",h.url);
        let response=client.get(&url).bearer_auth(TOKEN).send().await?;
        ensure!(response.status()==StatusCode::OK);
        ensure!(response.headers()["content-type"]==clex_mqtt::metrics::CONTENT_TYPE && response.headers()["cache-control"]=="no-store");
        let text=response.text().await?;
        ensure!(text.ends_with("# EOF\n") && text.len()<24*1024);
        ensure!(![principal,"private-publisher","private-metrics-topic","private-payload-never-export",TOKEN,"password_hash","postgresql://"].iter().any(|secret|text.contains(secret)));
        ensure!(metric(&text,"clex_mqtt_publications_committed_total")?==1.0 && metric(&text,"clex_mqtt_stored_sessions")?==2.0 && metric(&text,"clex_mqtt_retained_messages")?==1.0);
        ensure!(metric(&text,"clex_mqtt_database_up")?==1.0);
        ensure!(text.contains("clex_mqtt_delivery_queue_messages{state=\"queued\"} 1"));
        let count=metric(&text,"clex_mqtt_storage_commit_seconds_count")?;
        ensure!(count>=4.0 && metric(&text,"clex_mqtt_storage_commits_total")?>=4.0);
        ensure!(metric(&text,"clex_mqtt_storage_commit_seconds_bucket{le=\"+Inf\"}")?==count);
        ensure!(metric(&text,"clex_mqtt_storage_lock_wait_seconds_count")?>=count);
        let timestamp=metric(&text,"clex_mqtt_database_snapshot_timestamp_seconds")?;
        let second=Publication{topic:"private-metrics-topic/second".into(),..publication};
        h.store.accept_publication(publisher.session_id,publisher.generation,&second,b"private-publisher").await?;
        let cached=client.get(&url).bearer_auth(TOKEN).send().await?.text().await?;
        ensure!(metric(&cached,"clex_mqtt_retained_messages")?==1.0 && metric(&cached,"clex_mqtt_publications_committed_total")?==2.0);
        tokio::time::sleep(Duration::from_millis(5100)).await;
        let fresh=client.get(&url).bearer_auth(TOKEN).send().await?.text().await?;
        ensure!(metric(&fresh,"clex_mqtt_retained_messages")?==2.0 && metric(&fresh,"clex_mqtt_database_snapshot_timestamp_seconds")?>timestamp);
        h.pool.close().await;
        tokio::time::sleep(Duration::from_millis(5100)).await;
        let offline=client.get(&url).bearer_auth(TOKEN).send().await?.error_for_status()?.text().await?;
        ensure!(metric(&offline,"clex_mqtt_database_up")?==0.0 && metric(&offline,"clex_mqtt_publications_committed_total")?==2.0 && metric(&offline,"clex_mqtt_retained_messages")?==2.0);
        ensure!(metric(&offline,"clex_mqtt_database_snapshot_timestamp_seconds")?==metric(&fresh,"clex_mqtt_database_snapshot_timestamp_seconds")?);
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn metrics_slow_collection_has_deadline_and_bounded_concurrency() -> Result<()> {
    let h = Harness::start().await?;
    let mut lock = h.pool.begin().await?;
    sqlx::query("LOCK TABLE retained IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await?;
    let outcome: Result<()> = async {
        let client = Client::new();
        let url = format!("{}/metrics", h.url);
        let request = client.get(&url).bearer_auth(TOKEN);
        let started = std::time::Instant::now();
        let first = tokio::spawn(async move { request.send().await });
        tokio::time::sleep(Duration::from_millis(80)).await;
        let busy = client.get(&url).bearer_auth(TOKEN).send().await?;
        ensure!(
            busy.status() == StatusCode::SERVICE_UNAVAILABLE
                && busy.headers()["retry-after"] == "1"
        );
        let text = first.await??.text().await?;
        ensure!(started.elapsed() < Duration::from_secs(3));
        ensure!(
            metric(&text, "clex_mqtt_database_up")? == 0.0
                && metric(&text, "clex_mqtt_database_snapshot_timestamp_seconds")? == 0.0
        );
        ensure!(
            !text.contains("clex_mqtt_stored_sessions "),
            "unavailable first snapshot must not imply empty durable state"
        );
        Ok(())
    }
    .await;
    lock.rollback().await?;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
