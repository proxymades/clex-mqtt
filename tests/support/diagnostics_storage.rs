use super::*;
use clex_mqtt::diagnostics::TraceFilter;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn diagnostics_correlates_qos2_and_keeps_rollback_distinct_from_commit() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(1, 65536);
        let reader = store
            .open_session(session("reader", "subscriber", true))
            .await?;
        let writer = store
            .open_session(session("writer", "publisher", true))
            .await?;
        store
            .subscribe(
                reader.session_id,
                reader.generation,
                &subscription("trace/#", 2),
            )
            .await?;
        let observer = store
            .diagnostics()
            .start(TraceFilter {
                client_id: "writer".into(),
                username: "publisher".into(),
                topic: "trace/#".into(),
                seconds: 60,
            })
            .unwrap();
        let p = publication("trace/qos2", 2, b"payload-secret-marker", true);
        ensure!(
            store
                .receive_qos2(writer.session_id, writer.generation, 7, &p)
                .await?
        );
        let id = store.diagnostics().snapshot().messages[0].id;
        ensure!(store.diagnostics().message(id).unwrap().status == "await_pubrel");
        ensure!(
            !store
                .receive_qos2(writer.session_id, writer.generation, 7, &p)
                .await?
        );
        ensure!(store.diagnostics().snapshot().messages.len() == 1);
        let released = store
            .release_qos2(writer.session_id, writer.generation, 7)
            .await?;
        ensure!(released.deliveries.len() == 1);
        let trace = store.diagnostics().message(id).unwrap();
        ensure!(trace.status == "committed" && trace.queued == 1);
        ensure!(trace.events.iter().any(|e| e.code == "pubrel_received"));
        ensure!(
            trace
                .events
                .iter()
                .any(|e| e.delivery_id == Some(released.deliveries[0].id))
        );
        ensure!(!serde_json::to_string(&trace)?.contains("payload-secret-marker"));
        // The second retained write and delivery must roll back together at quota.
        let failed = store
            .accept_publication(
                writer.session_id,
                writer.generation,
                &publication("trace/rollback", 1, b"failed", true),
                b"writer",
            )
            .await;
        ensure!(failed.is_err());
        let failed = store.diagnostics().snapshot().messages[0].id;
        let failed = store.diagnostics().message(failed).unwrap();
        ensure!(
            failed.status == "interrupted" && !failed.events.iter().any(|e| e.code == "committed")
        );
        ensure!(failed.events.iter().any(|e| e.code == "quota_exceeded"));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM retained WHERE topic=$1")
            .bind(b"trace/rollback".as_slice())
            .fetch_one(store.pool())
            .await?;
        ensure!(count == 0);
        ensure!(
            store
                .pending_deliveries(reader.session_id, reader.generation, 100)
                .await?
                .len()
                == 1
        );

        let fresh = Store::from_pool(store.pool().clone());
        ensure!(fresh.diagnostics().snapshot().messages.is_empty());
        ensure!(
            fresh
                .pending_deliveries(reader.session_id, reader.generation, 100)
                .await?
                .len()
                == 1
        );
        sqlx::query("UPDATE messages SET expires_at=now()-interval '1 second' WHERE id=$1")
            .bind(id)
            .execute(store.pool())
            .await?;
        store.maintenance().await?;
        ensure!(
            store
                .diagnostics()
                .message(id)
                .unwrap()
                .events
                .iter()
                .any(|e| e.code == "expired")
        );
        ensure!(
            fresh
                .pending_deliveries(reader.session_id, reader.generation, 100)
                .await?
                .is_empty()
        );
        ensure!(store.diagnostics().stop(observer));
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
