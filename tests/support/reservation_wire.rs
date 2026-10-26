use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn reserved_acl_and_oversize_rejections_release_window_ids_and_queue_usage() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome: Result<()> = async {
        const CLIENT: &str = "reservation-rejections";
        let mut subscriber = Client::connect_v5_properties(h.address, CLIENT, false, &[0x11,0,0,1,44]).await?;
        subscriber.subscribe("reserve/#", 1, 1).await?;
        subscriber.disconnect().await?;
        timeout(Duration::from_secs(2), async {
            loop {
                let online: bool = sqlx::query_scalar("SELECT online FROM sessions WHERE client_id=$1")
                    .bind(CLIENT.as_bytes()).fetch_one(h.store.pool()).await?;
                if !online { return Ok::<_, anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let mut publisher = h.client(5, "reservation-publisher", true).await?;
        for (index, (topic, payload)) in [
            ("reserve/blocked", b"private".as_slice()),
            ("reserve/large", &[b'x'; 100]),
            ("reserve/value", b"one"),
            ("reserve/value", b"two"),
            ("reserve/value", b"three"),
            ("reserve/value", b"four"),
        ].iter().enumerate() {
            publisher.publish(topic, payload, 1, (index + 1) as u16, false, false).await?;
            ensure!(publisher.read().await?.0 == 0x40);
        }
        publisher.disconnect().await?;
        sqlx::query("UPDATE mqtt_users SET superuser=false WHERE username='wire-test'").execute(h.store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'reserve/#',true,true FROM mqtt_users WHERE username='wire-test'")
            .bind(Uuid::new_v4()).execute(h.store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'reserve/blocked',true,false FROM mqtt_users WHERE username='wire-test'")
            .bind(Uuid::new_v4()).execute(h.store.pool()).await?;
        // Two initial reservations are discarded at the send boundary. The
        // allowed tail must refill that same window and remain ordered.
        let mut subscriber = Client::connect_v5_properties(h.address, CLIENT, false,
            &[0x11,0,0,1,44,0x21,0,2,0x27,0,0,0,64]).await?;
        ensure!(subscriber.session_present);
        let mut ids = Vec::new();
        for expected in [b"one".as_slice(), b"two"] {
            let (header, body) = subscriber.read().await?;
            let (topic, payload, id) = publication(header, &body, 5)?;
            ensure!(header & 8 == 0 && topic == "reserve/value" && payload == expected);
            ensure!(!ids.contains(&id));
            ids.push(id);
        }
        subscriber.assert_quiet().await?;
        let phases: Vec<(String, i64)> = sqlx::query_as("SELECT phase,count(*) FROM deliveries GROUP BY phase ORDER BY phase")
            .fetch_all(h.store.pool()).await?;
        ensure!(phases == [("await_puback".into(),2),("queued".into(),2)]);
        for id in &ids { subscriber.ack(4,*id).await?; }
        for expected in [b"three".as_slice(), b"four"] {
            let (header, body) = subscriber.read().await?;
            let (_, payload, id) = publication(header, &body, 5)?;
            ensure!(header & 8 == 0 && payload == expected && !ids.contains(&id));
            ids.push(id);
            subscriber.ack(4,id).await?;
        }
        subscriber.send(0xc0,&[]).await?;
        ensure!(subscriber.read().await?.0 == 0xd0);
        let empty: bool = sqlx::query_scalar("SELECT queued_count=0 AND queued_bytes=0 AND NOT EXISTS(SELECT 1 FROM deliveries WHERE session_id=s.id) FROM sessions s WHERE client_id=$1")
            .bind(CLIENT.as_bytes()).fetch_one(h.store.pool()).await?;
        ensure!(empty,"discarded reservations or completed deliveries retain quota");
        subscriber.disconnect().await?;
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
