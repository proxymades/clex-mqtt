use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn reservation_batches_are_atomic_bounded_and_recover_with_original_packet_ids() -> Result<()>
{
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let source = store
            .open_session(session("reservation-source", "publisher", false))
            .await?;
        let target = store
            .open_session(session("reservation-target", "subscriber", false))
            .await?;
        let other = store
            .open_session(session("reservation-other", "subscriber", false))
            .await?;
        for recipient in [&target, &other] {
            store
                .subscribe(
                    recipient.session_id,
                    recipient.generation,
                    &subscription("reservations/#", 2),
                )
                .await?;
        }
        for index in 0..66 {
            let qos = if index == 65 { 0 } else { 1 + (index % 2) };
            store
                .accept_publication(
                    source.session_id,
                    source.generation,
                    &publication("reservations/value", qos, &[index], false),
                    b"reservation-source",
                )
                .await?;
        }
        let id = target.session_id;
        let generation = target.generation;
        let queued = store.pending_deliveries(id, generation, 100).await?;
        let foreign = store
            .pending_deliveries(other.session_id, other.generation, 1)
            .await?;
        for invalid in [
            vec![],
            vec![(queued[0].id, 0)],
            vec![(queued[0].id, 1), (queued[0].id, 2)],
            vec![(queued[0].id, 1), (queued[1].id, 1)],
            (0..65).map(|n| (queued[n].id, (n + 1) as u16)).collect(),
            vec![(queued[0].id, 1), (Uuid::new_v4(), 2)],
            vec![(queued[0].id, 1), (foreign[0].id, 2)],
            vec![(queued[0].id, 1), (queued[65].id, 2)],
        ] {
            ensure!(
                store
                    .reserve_deliveries(id, generation, &invalid)
                    .await
                    .is_err()
            );
            ensure!(
                store
                    .delivery_batch(id, generation, &[], true)
                    .await?
                    .inflight
                    .is_empty(),
                "partial reservation escaped rollback"
            );
        }
        store
            .reserve_delivery(id, generation, queued[64].id, 65535)
            .await?;
        // A collision with a previously persisted identifier rolls back every
        // new member, including any row PostgreSQL updated before the conflict.
        ensure!(
            store
                .reserve_deliveries(id, generation, &[(queued[0].id, 1), (queued[1].id, 65535)])
                .await
                .is_err()
        );
        ensure!(
            store
                .delivery_batch(id, generation, &[], true)
                .await?
                .inflight
                .len()
                == 1
        );
        let reservations: Vec<_> = queued
            .iter()
            .take(64)
            .enumerate()
            .map(|(n, delivery)| (delivery.id, (n + 1) as u16))
            .collect();
        store
            .reserve_deliveries(id, generation, &reservations)
            .await?;
        ensure!(
            store
                .reserve_deliveries(id, generation, &reservations)
                .await
                .is_err()
        );
        store.recover_startup().await?;
        let reopened = store
            .open_session(session("reservation-target", "subscriber", false))
            .await?;
        ensure!(reopened.session_present && reopened.generation > generation);
        let recovered = store
            .pending_deliveries(id, reopened.generation, 100)
            .await?;
        for (index, delivery) in recovered.iter().take(64).enumerate() {
            ensure!(delivery.id == queued[index].id && delivery.sequence == queued[index].sequence);
            ensure!(delivery.packet_id == Some((index + 1) as u16));
            ensure!(
                delivery.phase
                    == if index % 2 == 0 {
                        DeliveryPhase::AwaitPubAck
                    } else {
                        DeliveryPhase::AwaitPubRec
                    }
            );
            ensure!(delivery.publication.payload == [index as u8]);
        }
        ensure!(
            recovered[64].packet_id == Some(65535) && recovered[65].phase == DeliveryPhase::Queued
        );
        ensure!(
            store
                .acknowledge(id, reopened.generation, 65535, DeliveryPhase::AwaitPubAck)
                .await?
        );
        // A replacement connection fences a whole batch from an older owner.
        let source = store
            .open_session(session("reservation-source", "publisher", false))
            .await?;
        store
            .accept_publication(
                source.session_id,
                source.generation,
                &publication("reservations/value", 1, b"new", false),
                b"reservation-source",
            )
            .await?;
        let pending = store
            .pending_deliveries(id, reopened.generation, 100)
            .await?;
        let last = pending.last().unwrap();
        ensure!(
            store
                .reserve_deliveries(id, generation, &[(last.id, 65535)])
                .await
                .unwrap_err()
                .is::<clex_mqtt::store::SessionSuperseded>()
        );
        ensure!(
            store
                .pending_deliveries(id, reopened.generation, 100)
                .await?
                .last()
                .unwrap()
                .packet_id
                .is_none()
        );
        queue_usage::assert_consistent(store, false).await?;
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn confirmation_batches_preserve_qos_phases_packet_ids_and_generation_fences() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let publisher = store
            .open_session(session("acks-source", "publisher", false))
            .await?;
        let recipient = store
            .open_session(session("acks-target", "subscriber", false))
            .await?;
        store
            .subscribe(
                recipient.session_id,
                recipient.generation,
                &subscription("acks/#", 2),
            )
            .await?;
        for index in 1..=12 {
            let qos = if index <= 6 { 1 } else { 2 };
            store
                .accept_publication(
                    publisher.session_id,
                    publisher.generation,
                    &publication("acks/value", qos, &[index], false),
                    b"acks-source",
                )
                .await?;
        }
        let pending = store
            .pending_deliveries(recipient.session_id, recipient.generation, 100)
            .await?;
        for (index, delivery) in pending.iter().enumerate() {
            store
                .reserve_delivery(
                    recipient.session_id,
                    recipient.generation,
                    delivery.id,
                    (index + 1) as u16,
                )
                .await?;
        }
        let id = recipient.session_id;
        let generation = recipient.generation;
        ensure!(
            store
                .confirm_deliveries(id, generation, &[1, 7], DeliveryPhase::AwaitPubComp, false)
                .await?
                .is_empty()
        );
        let mut changed = store
            .confirm_deliveries(
                id,
                generation,
                &[1, 2, 7, 8, 99],
                DeliveryPhase::AwaitPubAck,
                false,
            )
            .await?;
        changed.sort_unstable();
        ensure!(changed == [1, 2]);
        let mut changed = store
            .confirm_deliveries(
                id,
                generation,
                &[7, 8, 9, 65535],
                DeliveryPhase::AwaitPubRec,
                true,
            )
            .await?;
        changed.sort_unstable();
        ensure!(changed == [7, 8, 9]);
        // A repeated positive PUBREC repeats PUBREL; a negative one can only
        // delete AwaitPubRec, preserving already committed AwaitPubComp state.
        ensure!(
            store
                .confirm_deliveries(id, generation, &[7, 8, 9], DeliveryPhase::AwaitPubRec, true)
                .await?
                .len()
                == 3
        );
        ensure!(
            store
                .confirm_deliveries(
                    id,
                    generation,
                    &[7, 8, 10],
                    DeliveryPhase::AwaitPubRec,
                    false
                )
                .await?
                == [10]
        );
        ensure!(
            store
                .confirm_deliveries(id, generation, &[7, 7], DeliveryPhase::AwaitPubComp, false)
                .await
                .is_err()
        );
        ensure!(
            store
                .confirm_deliveries(
                    id,
                    generation,
                    &[7, 8, 9],
                    DeliveryPhase::AwaitPubComp,
                    false
                )
                .await?
                .len()
                == 3
        );
        let replacement = store
            .open_session(session("acks-target", "subscriber", false))
            .await?;
        ensure!(
            store
                .confirm_deliveries(id, generation, &[3, 4], DeliveryPhase::AwaitPubAck, false)
                .await
                .unwrap_err()
                .is::<clex_mqtt::store::SessionSuperseded>()
        );
        let remaining = store
            .pending_deliveries(id, replacement.generation, 100)
            .await?;
        ensure!(
            remaining.len() == 6
                && remaining
                    .iter()
                    .all(|delivery| delivery.packet_id.is_some())
        );
        ensure!(
            store
                .confirm_deliveries(
                    id,
                    replacement.generation,
                    &[3, 4, 5, 6],
                    DeliveryPhase::AwaitPubAck,
                    false
                )
                .await?
                .len()
                == 4
        );
        ensure!(
            store
                .confirm_deliveries(
                    id,
                    replacement.generation,
                    &[11, 12],
                    DeliveryPhase::AwaitPubRec,
                    true
                )
                .await?
                .len()
                == 2
        );
        ensure!(
            store
                .confirm_deliveries(
                    id,
                    replacement.generation,
                    &[11, 12],
                    DeliveryPhase::AwaitPubComp,
                    false
                )
                .await?
                .len()
                == 2
        );
        ensure!(
            store
                .pending_deliveries(id, replacement.generation, 100)
                .await?
                .is_empty()
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
async fn delivery_batches_bound_payloads_preserve_all_packet_ids_and_finish_pubrel_at_full_window()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let publisher = store
            .open_session(session("batch-source", "publisher", false))
            .await?;
        let recipient = store
            .open_session(session("batch-target", "subscriber", false))
            .await?;
        store
            .subscribe(
                recipient.session_id,
                recipient.generation,
                &Subscription {
                    filter: "batch/#".into(),
                    qos: 2,
                    no_local: false,
                    retain_as_published: false,
                    retain_handling: 0,
                    subscription_identifier: None,
                },
            )
            .await?;
        for index in 0..160 {
            store
                .accept_publication(
                    publisher.session_id,
                    publisher.generation,
                    &Publication {
                        topic: format!("batch/{index}"),
                        payload: vec![index as u8],
                        qos: 2,
                        retain: false,
                        expires_at: None,
                        properties: Default::default(),
                    },
                    b"batch-source",
                )
                .await?;
        }
        let initial = store
            .pending_deliveries(recipient.session_id, recipient.generation, 1000)
            .await?;
        let mut reserved = Vec::new();
        for (index, delivery) in initial.iter().take(130).enumerate() {
            reserved.push(
                store
                    .reserve_delivery(
                        recipient.session_id,
                        recipient.generation,
                        delivery.id,
                        (index + 1) as u16,
                    )
                    .await?,
            );
        }
        // Resume under a smaller window: every existing Packet Identifier is
        // still reserved, even though only 64 payloads are materialized.
        let batch = store
            .delivery_batch(recipient.session_id, recipient.generation, &[], true)
            .await?;
        ensure!(batch.deliveries.len() == 64 && batch.inflight.len() == 130);
        ensure!(
            batch
                .inflight
                .iter()
                .any(|(id, packet)| *id == reserved[129].id && *packet == 130)
        );
        let sent: Vec<_> = reserved.iter().map(|delivery| delivery.id).collect();
        ensure!(
            store
                .delivery_batch(recipient.session_id, recipient.generation, &sent, false)
                .await?
                .deliveries
                .is_empty()
        );
        // A persisted PUBREL bypasses the full PUBLISH window after reconnect.
        store
            .advance_qos2(recipient.session_id, recipient.generation, 130)
            .await?;
        let mut already_sent = sent.clone();
        already_sent.pop();
        let batch = store
            .delivery_batch(
                recipient.session_id,
                recipient.generation,
                &already_sent,
                false,
            )
            .await?;
        ensure!(
            batch.deliveries.len() == 1
                && batch.deliveries[0].phase == DeliveryPhase::AwaitPubComp
                && batch.deliveries[0].packet_id == Some(130)
        );
        store
            .acknowledge(
                recipient.session_id,
                recipient.generation,
                130,
                DeliveryPhase::AwaitPubComp,
            )
            .await?;
        let batch = store
            .delivery_batch(recipient.session_id, recipient.generation, &sent, true)
            .await?;
        ensure!(batch.inflight.len() == 129 && batch.deliveries.len() == 30);
        ensure!(
            batch
                .deliveries
                .iter()
                .all(|delivery| delivery.packet_id.is_none())
        );
        // Expiry cleans the queued tail while preserving an already started
        // handshake, including its Packet Identifier outside the payload batch.
        sqlx::query("UPDATE messages SET expires_at=now()-interval '1 second' WHERE id IN (SELECT message_id FROM deliveries WHERE id=ANY($1))")
            .bind(vec![reserved[0].id, initial[159].id])
            .execute(store.pool()).await?;
        let batch = store
            .delivery_batch(recipient.session_id, recipient.generation, &sent, true)
            .await?;
        ensure!(batch.deliveries.len() == 29 && batch.inflight.len() == 129);
        ensure!(batch.inflight.iter().any(|(id, _)| *id == reserved[0].id));
        let expired_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deliveries WHERE id=$1)")
            .bind(initial[159].id).fetch_one(store.pool()).await?;
        ensure!(!expired_exists, "expired queued payload must release its quota");
        let replacement = store
            .open_session(session("batch-target", "subscriber", false))
            .await?;
        ensure!(replacement.session_present);
        ensure!(
            store
                .delivery_batch(recipient.session_id, recipient.generation, &[], true)
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
