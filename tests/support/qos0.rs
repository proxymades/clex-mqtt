use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn empty_queue_probe_observes_enqueue_attempt_ownership_and_session_generation() -> Result<()>
{
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let source = store
            .open_session(session("probe-source", "publisher", false))
            .await?;
        let target = store
            .open_session(session("probe-target", "subscriber", false))
            .await?;
        let id = target.session_id;
        let generation = target.generation;
        store
            .subscribe(id, generation, &subscription("probe/#", 0))
            .await?;
        ensure!(!store.has_deliveries(id, generation).await?);
        store
            .accept_publication(
                source.session_id,
                source.generation,
                &publication("probe/value", 0, b"queued", false),
                b"probe-source",
            )
            .await?;
        ensure!(store.has_deliveries(id, generation).await?);
        let batch = store.outgoing_batch(id, generation, &[], false).await?;
        ensure!(batch.started_qos0.len() == 1 && !store.has_deliveries(id, generation).await?);
        queue_usage::assert_consistent(store, true).await?;
        store
            .complete_qos0_batch(id, generation, &batch.started_qos0)
            .await?;
        let replacement = store
            .open_session(session("probe-target", "subscriber", false))
            .await?;
        ensure!(
            store
                .has_deliveries(id, generation)
                .await
                .unwrap_err()
                .is::<clex_mqtt::store::SessionSuperseded>()
        );
        ensure!(!store.has_deliveries(id, replacement.generation).await?);
        store
            .close_session(id, replacement.generation, 300, true)
            .await?;
        ensure!(
            store
                .has_deliveries(id, replacement.generation)
                .await
                .unwrap_err()
                .is::<clex_mqtt::store::SessionSuperseded>()
        );
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn outgoing_selection_claims_qos0_with_a_full_window_and_preserves_pubrel_order() -> Result<()>
{
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let source = store
            .open_session(session("selection-source", "publisher", false))
            .await?;
        let target = store
            .open_session(session("selection-target", "subscriber", false))
            .await?;
        store
            .subscribe(
                target.session_id,
                target.generation,
                &subscription("selection/#", 2),
            )
            .await?;
        for index in 0..70_u8 {
            store
                .accept_publication(
                    source.session_id,
                    source.generation,
                    &publication(
                        "selection/value",
                        if index % 2 == 0 { 0 } else { 2 },
                        &[index],
                        false,
                    ),
                    b"selection-source",
                )
                .await?;
        }
        let id = target.session_id;
        let generation = target.generation;
        let pending = store.pending_deliveries(id, generation, 100).await?;
        store
            .reserve_delivery(id, generation, pending[1].id, 19)
            .await?;
        store.advance_qos2(id, generation, 19).await?;
        let preview = store.delivery_batch(id, generation, &[], false).await?;
        ensure!(preview.started_qos0.is_empty() && preview.deliveries.len() == 36);
        let outgoing = store.outgoing_batch(id, generation, &[], false).await?;
        ensure!(outgoing.started_qos0.len() == 35 && outgoing.inflight == [(pending[1].id, 19)]);
        ensure!(
            outgoing.deliveries.iter().map(|d| d.id).collect::<Vec<_>>()
                == preview.deliveries.iter().map(|d| d.id).collect::<Vec<_>>(),
            "claim changed wire sequence"
        );
        ensure!(
            outgoing
                .deliveries
                .iter()
                .filter(|d| d.publication.qos == 0)
                .all(|d| d.phase == DeliveryPhase::Queued && d.packet_id.is_none()),
            "wire snapshots must retain the initial DUP state"
        );
        let again = store.outgoing_batch(id, generation, &[], false).await?;
        ensure!(
            again.started_qos0.is_empty()
                && again.deliveries.len() == 1
                && again.deliveries[0].phase == DeliveryPhase::AwaitPubComp
        );
        queue_usage::assert_consistent(store, true).await?;
        store
            .complete_qos0_batch(id, generation, &outgoing.started_qos0)
            .await?;
        ensure!(store.pending_deliveries(id, generation, 100).await?.len() == 35);
        ensure!(
            store
                .confirm_deliveries(id, generation, &[19], DeliveryPhase::AwaitPubComp, false)
                .await?
                == [19]
        );
        queue_usage::assert_consistent(store, true).await?;
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn qos0_batches_hold_quota_claim_once_and_complete_only_started_qos0() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(3, 4096);
        let source = store
            .open_session(session("qos0-source", "publisher", false))
            .await?;
        let target = store
            .open_session(session("qos0-target", "subscriber", false))
            .await?;
        let other = store
            .open_session(session("qos0-other", "subscriber", false))
            .await?;
        for recipient in [&target, &other] {
            store
                .subscribe(
                    recipient.session_id,
                    recipient.generation,
                    &subscription("qos0/#", 2),
                )
                .await?;
        }
        for qos in 0..=2 {
            store
                .accept_publication(
                    source.session_id,
                    source.generation,
                    &publication("qos0/значение", qos, &[0, 255, qos], true),
                    b"qos0-source",
                )
                .await?;
        }
        let id = target.session_id;
        let generation = target.generation;
        let pending = store.pending_deliveries(id, generation, 10).await?;
        let foreign = store
            .pending_deliveries(other.session_id, other.generation, 10)
            .await?;
        for invalid in [vec![], vec![pending[0].id; 2], vec![Uuid::nil(); 65]] {
            ensure!(
                store
                    .begin_qos0_batch(id, generation, &invalid)
                    .await
                    .is_err()
            );
            ensure!(
                store
                    .complete_qos0_batch(id, generation, &invalid)
                    .await
                    .is_err()
            );
        }
        let ids = [
            pending[0].id,
            pending[1].id,
            pending[2].id,
            foreign[0].id,
            Uuid::new_v4(),
        ];
        let (left, right) = tokio::join!(
            store.begin_qos0_batch(id, generation, &ids),
            store.begin_qos0_batch(id, generation, &ids)
        );
        let claimed: Vec<_> = left?.into_iter().chain(right?).collect();
        ensure!(
            claimed == [pending[0].id],
            "one durable QoS0 attempt must have one owner"
        );
        ensure!(
            store
                .delivery_batch(id, generation, &[], true)
                .await?
                .deliveries
                .len()
                == 2
        );
        let still_full = store
            .accept_publication(
                source.session_id,
                source.generation,
                &publication("qos0/значение", 1, b"overflow", true),
                b"qos0-source",
            )
            .await
            .unwrap_err();
        ensure!(
            still_full.is::<clex_mqtt::store::QuotaExceeded>(),
            "started QoS0 must keep its queue quota until completion"
        );
        queue_usage::assert_consistent(&store, true).await?;
        store.complete_qos0_batch(id, generation, &ids).await?;
        store.complete_qos0_batch(id, generation, &ids).await?;
        let remaining = store.pending_deliveries(id, generation, 10).await?;
        ensure!(
            remaining.iter().map(|d| d.id).collect::<Vec<_>>() == [pending[1].id, pending[2].id]
        );
        ensure!(
            store
                .pending_deliveries(other.session_id, other.generation, 10)
                .await?
                .len()
                == 3
        );
        queue_usage::assert_consistent(&store, true).await?;
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn qos0_cleanup_on_close_takeover_and_restart_preserves_unstarted_and_qos_handshakes()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        for mode in ["close", "takeover", "restart"] {
            let name = format!("qos0-{mode}");
            let source = store
                .open_session(session(&format!("source-{mode}"), "publisher", false))
                .await?;
            let target = store
                .open_session(session(&name, "subscriber", false))
                .await?;
            let topic = format!("qos0/{mode}");
            store
                .subscribe(
                    target.session_id,
                    target.generation,
                    &subscription(&topic, 2),
                )
                .await?;
            for (index, qos) in [0, 0, 1, 2].into_iter().enumerate() {
                store
                    .accept_publication(
                        source.session_id,
                        source.generation,
                        &publication(&topic, qos, &[index as u8], true),
                        format!("source-{mode}").as_bytes(),
                    )
                    .await?;
            }
            let id = target.session_id;
            let generation = target.generation;
            let pending = store.pending_deliveries(id, generation, 10).await?;
            store
                .reserve_delivery(id, generation, pending[2].id, 77)
                .await?;
            store
                .reserve_delivery(id, generation, pending[3].id, 88)
                .await?;
            ensure!(
                store
                    .begin_qos0_batch(id, generation, &[pending[0].id])
                    .await?
                    == [pending[0].id]
            );
            match mode {
                "close" => store.close_session(id, generation, 3600, false).await?,
                "restart" => store.recover_startup().await?,
                _ => {}
            }
            let reopened = store
                .open_session(session(&name, "subscriber", false))
                .await?;
            ensure!(reopened.session_present);
            let actual = store
                .pending_deliveries(id, reopened.generation, 10)
                .await?;
            ensure!(
                actual.iter().map(|d| d.id).collect::<Vec<_>>()
                    == pending[1..].iter().map(|d| d.id).collect::<Vec<_>>()
            );
            ensure!(actual[0].phase == DeliveryPhase::Queued && actual[0].publication.qos == 0);
            ensure!(
                actual[1].phase == DeliveryPhase::AwaitPubAck && actual[1].packet_id == Some(77)
            );
            ensure!(
                actual[2].phase == DeliveryPhase::AwaitPubRec && actual[2].packet_id == Some(88)
            );
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deliveries WHERE id=$1)")
                    .bind(pending[0].id)
                    .fetch_one(store.pool())
                    .await?;
            ensure!(
                !exists,
                "a started QoS0 attempt survived connection recovery"
            );
            let retained: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM retained WHERE topic=$1)")
                    .bind(topic.as_bytes())
                    .fetch_one(store.pool())
                    .await?;
            ensure!(retained, "QoS0 cleanup changed Retain");
            ensure!(
                store
                    .begin_qos0_batch(id, generation, &[actual[0].id])
                    .await
                    .unwrap_err()
                    .is::<clex_mqtt::store::SessionSuperseded>()
            );
            ensure!(
                store
                    .complete_qos0_batch(id, generation, &[actual[0].id])
                    .await
                    .unwrap_err()
                    .is::<clex_mqtt::store::SessionSuperseded>()
            );
            queue_usage::assert_consistent(store, false).await?;
        }
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
