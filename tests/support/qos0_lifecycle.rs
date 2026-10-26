use super::*;

impl Harness {
    pub(super) async fn qos0_crash_recovery(
        &mut self,
        running: usize,
        mqtt: SocketAddr,
        http: SocketAddr,
    ) -> Result<usize> {
        use clex_mqtt::{
            domain::{SessionSpec, Subscription},
            store::Store,
        };
        const CLIENT: &str = "lifecycle-qos0-slow";
        const DATA: &str = "lifecycle/qos0/data";
        const SENTINEL: &str = "lifecycle/qos0/reliable";
        self.kill_child(running).await?;
        let running = self.spawn_with_queue_bytes("qos0_before", mqtt, http, 16 * 1024 * 1024)?;
        self.wait_ready(running, http).await?;
        let store = Store::from_pool(self.pool.clone());
        // Seed a live persistent subscriber before attaching its deliberately
        // slow socket, so the complete backlog is ready for one bounded claim.
        let target = store
            .open_session(SessionSpec {
                client_id: CLIENT.as_bytes().to_vec(),
                principal: MQTT_USER.into(),
                clean_start: false,
                expiry_interval: 300,
                protocol: 5,
                will: None,
                will_delay_interval: 0,
            })
            .await?;
        for (topic, qos) in [(DATA, 0), (SENTINEL, 1)] {
            store
                .subscribe(
                    target.session_id,
                    target.generation,
                    &Subscription {
                        filter: topic.into(),
                        qos,
                        no_local: false,
                        retain_as_published: false,
                        retain_handling: 0,
                        subscription_identifier: None,
                    },
                )
                .await?;
        }
        let mut publisher = Client::connect_v5(mqtt, "lifecycle-qos0-source", true).await?;
        for index in 0..256_u16 {
            let mut payload = vec![b'q'; 60_000];
            payload[..2].copy_from_slice(&index.to_be_bytes());
            publisher.publish_qos(DATA, &payload, index + 1, 1).await?;
            publisher.read_ack(0x40, index + 1).await?;
        }
        publisher
            .publish_qos(SENTINEL, b"reliable-tail", 300, 1)
            .await?;
        publisher.read_ack(0x40, 300).await?;
        let slow = Client::connect_version_with_buffer(mqtt, CLIENT, false, 5, Some(1024)).await?;
        ensure!(slow.session_present);
        timeout(WIRE_TIMEOUT, async {
            loop {
                let claimed: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM deliveries WHERE session_id=$1 AND phase='sending_qos0'",
                )
                .bind(target.session_id)
                .fetch_one(&self.pool)
                .await?;
                if claimed > 0 {
                    return Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .context("QoS0 writer did not commit a started batch")??;
        self.kill_child(running).await?;
        drop(slow);
        drop(publisher);
        let started: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM deliveries WHERE session_id=$1 AND phase='sending_qos0'",
        )
        .bind(target.session_id)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            started > 0 && started <= 64,
            "crash must interrupt one bounded QoS0 batch"
        );
        let remaining:Vec<Vec<u8>>=sqlx::query_scalar("SELECT substring(m.payload from 1 for 2) FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.session_id=$1 AND d.qos=0 AND d.phase='queued' ORDER BY d.sequence")
            .bind(target.session_id).fetch_all(&self.pool).await?;
        ensure!(
            !remaining.is_empty(),
            "fixture must leave an unstarted QoS0 tail"
        );
        let restarted = self.spawn_with_queue_bytes("qos0_after", mqtt, http, 16 * 1024 * 1024)?;
        self.wait_ready(restarted, http).await?;
        let mut subscriber = Client::connect_v5(mqtt, CLIENT, false).await?;
        ensure!(subscriber.session_present);
        for index in remaining {
            let mut expected = vec![b'q'; 60_000];
            expected[..2].copy_from_slice(&index);
            subscriber.read_publication(DATA, &expected, 0).await?;
        }
        let packet = subscriber
            .read_publication(SENTINEL, b"reliable-tail", 1)
            .await?;
        subscriber.send(0x40, &packet.to_be_bytes()).await?;
        subscriber.send(0xc0, &[]).await?;
        ensure!(
            subscriber.read().await?.0 == 0xd0,
            "QoS0 recovery replayed a started batch"
        );
        let empty: bool =
            sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM deliveries WHERE session_id=$1)")
                .bind(target.session_id)
                .fetch_one(&self.pool)
                .await?;
        ensure!(
            empty,
            "QoS0 recovery left started or completed queue entries"
        );
        subscriber.send(0xe0, &[]).await?;
        Ok(restarted)
    }
}
