use super::*;
use clex_mqtt::diagnostics::TraceFilter;

async fn synchronized(client: &mut Client) -> Result<()> {
    client.send(0xc0, &[]).await?;
    ensure!(client.read().await?.0 == 0xd0);
    Ok(())
}
fn has(h: &Harness, topic: &str, code: &str) -> bool {
    h.store
        .diagnostics()
        .snapshot()
        .messages
        .iter()
        .find(|m| m.topic == topic)
        .and_then(|m| h.store.diagnostics().message(m.id))
        .is_some_and(|m| m.events.iter().any(|e| e.code == code))
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn diagnostics_observes_real_subscriber_acks_and_reconnect_duplicates() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome: Result<()> = async {
        h.store
            .diagnostics()
            .start(TraceFilter {
                client_id: "writer".into(),
                username: "wire-test".into(),
                topic: "trace/#".into(),
                seconds: 60,
            })
            .unwrap();
        let mut writer = h.client(5, "writer", true).await?;
        for version in [4, 5] {
            let reader_id = format!("reader-{version}");
            let mut reader = h.client(version, &reader_id, false).await?;
            reader.subscribe("trace/#", 2, 1).await?;
            let topic = format!("trace/qos0/{version}");
            writer.publish(&topic, b"zero", 0, 0, false, false).await?;
            let (header, body) = reader.read().await?;
            ensure!(publication(header, &body, version)?.1 == b"zero");
            synchronized(&mut reader).await?;
            ensure!(has(&h, &topic, "qos0_written") && !has(&h, &topic, "puback"));
            let topic = format!("trace/qos1/{version}");
            writer.publish(&topic, b"one", 1, 10, false, false).await?;
            ensure!(writer.read().await?.0 == 0x40);
            let (header, body) = reader.read().await?;
            let (_, payload, pid) = publication(header, &body, version)?;
            ensure!(payload == b"one");
            ensure!(
                has(&h, &topic, "publish_sent") && !has(&h, &topic, "puback"),
                "publisher PUBACK is distinct from subscriber acknowledgement"
            );
            drop(reader);
            let mut reader = h.client(version, &reader_id, false).await?;
            let (header, body) = reader.read().await?;
            ensure!(header & 8 == 8 && publication(header, &body, version)?.2 == pid);
            reader.ack(4, pid).await?;
            synchronized(&mut reader).await?;
            ensure!(has(&h, &topic, "publish_retried") && has(&h, &topic, "puback"));
            let topic = format!("trace/qos2/{version}");
            writer.publish_once_qos2(&topic, b"two", 11).await?;
            let (header, body) = reader.read().await?;
            let (_, _, pid) = publication(header, &body, version)?;
            reader.ack(5, pid).await?;
            ensure!(reader.read().await?.0 == 0x62);
            reader.ack(7, pid).await?;
            synchronized(&mut reader).await?;
            for code in [
                "await_pubrel",
                "pubrel_received",
                "pubrec",
                "pubrel_sent",
                "pubcomp",
            ] {
                ensure!(has(&h, &topic, code), "missing QoS2 trace event {code}");
            }
            reader.disconnect().await?;
        }
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn diagnostics_distinguishes_acl_and_negative_protocol_ack() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome: Result<()> = async {
        h.store
            .diagnostics()
            .start(TraceFilter {
                client_id: "writer".into(),
                username: "wire-test".into(),
                topic: "trace/#".into(),
                seconds: 60,
            })
            .unwrap();
        let mut writer = h.client(5, "writer", true).await?;
        let mut reader = h.client(5, "reader", true).await?;
        reader.subscribe("trace/#", 2, 1).await?;
        for qos in [1, 2] {
            let topic = format!("trace/rejected/{qos}");
            if qos == 2 {
                writer.publish_once_qos2(&topic, b"two", 8).await?;
            } else {
                writer.publish(&topic, b"one", 1, 8, false, false).await?;
                ensure!(writer.read().await?.0 == 0x40);
            }
            let (header, body) = reader.read().await?;
            let (_, _, pid) = publication(header, &body, 5)?;
            let mut body = pid.to_be_bytes().to_vec();
            body.extend_from_slice(&[0x87, 0]);
            reader
                .send(if qos == 1 { 0x40 } else { 0x50 }, &body)
                .await?;
            synchronized(&mut reader).await?;
            ensure!(has(
                &h,
                &topic,
                if qos == 1 {
                    "puback_rejected"
                } else {
                    "pubrec_rejected"
                }
            ));
            ensure!(!has(
                &h,
                &topic,
                if qos == 1 { "puback" } else { "pubcomp" }
            ));
        }
        sqlx::query("UPDATE mqtt_users SET superuser=false WHERE username='wire-test'")
            .execute(h.store.pool())
            .await?;
        writer
            .publish("trace/blocked", b"blocked", 1, 9, false, false)
            .await?;
        let (header, body) = writer.read().await?;
        ensure!(header == 0x40 && body.get(2) == Some(&0x87));
        ensure!(has(&h, "trace/blocked", "publish_acl_denied"));
        reader.assert_quiet().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
