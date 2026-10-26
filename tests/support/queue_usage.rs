use super::*;

/// Independent audit against original message bytes, without trusting either
/// the session counters or the per-delivery accounted size.
pub(super) async fn assert_consistent(store: &Store, require_known: bool) -> Result<()> {
    let inconsistent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions s LEFT JOIN (SELECT d.session_id,count(*) AS count,sum(octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text))::bigint AS bytes FROM deliveries d JOIN messages m ON m.id=d.message_id GROUP BY d.session_id) q ON q.session_id=s.id WHERE (s.queued_count IS NOT NULL OR s.queued_bytes IS NOT NULL OR $1) AND (s.queued_count IS DISTINCT FROM coalesce(q.count,0) OR s.queued_bytes IS DISTINCT FROM coalesce(q.bytes,0))) OR EXISTS(SELECT 1 FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.accounted_bytes IS NOT NULL AND d.accounted_bytes<>octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text))")
        .bind(require_known).fetch_one(store.pool()).await?;
    ensure!(
        !inconsistent,
        "queue usage differs from durable message contents"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn usage_tracks_binary_payloads_qos_phases_expiry_discard_and_quota_rollback() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(3, 4096);
        let source = store.open_session(session("usage-source", "publisher", false)).await?;
        let left = store.open_session(session("usage-left", "subscriber", false)).await?;
        let right = store.open_session(session("usage-right", "subscriber", false)).await?;
        for target in [&left, &right] {
            store.subscribe(target.session_id,target.generation,&subscription("usage/#",2)).await?;
        }
        for qos in 0..=2 {
            let mut message = publication("usage/температура",qos,&[0,255,qos],true);
            message.properties.user_properties=vec![("единица".into(),"°C".into()),("единица".into(),"К".into())];
            store.accept_publication(source.session_id,source.generation,&message,b"usage-source").await?;
            assert_consistent(&store,true).await?;
        }
        let rejected = store.accept_publication(source.session_id,source.generation,
            &publication("usage/температура",1,b"overflow",true),b"usage-source").await.unwrap_err();
        ensure!(rejected.is::<clex_mqtt::store::QuotaExceeded>());
        assert_consistent(&store,true).await?;
        let queued = store.pending_deliveries(left.session_id,left.generation,10).await?;
        store.discard_delivery(left.session_id,left.generation,queued[0].id).await?;
        store.reserve_delivery(left.session_id,left.generation,queued[1].id,1).await?;
        store.reserve_delivery(left.session_id,left.generation,queued[2].id,2).await?;
        assert_consistent(&store,true).await?;
        ensure!(!store.acknowledge(left.session_id,left.generation,1,DeliveryPhase::AwaitPubComp).await?);
        ensure!(store.acknowledge(left.session_id,left.generation,1,DeliveryPhase::AwaitPubAck).await?);
        store.advance_qos2(left.session_id,left.generation,2).await?;
        assert_consistent(&store,true).await?;
        store.confirm_deliveries(left.session_id,left.generation,&[2],DeliveryPhase::AwaitPubComp,false).await?;
        let queued = store.pending_deliveries(right.session_id,right.generation,10).await?;
        store.reserve_delivery(right.session_id,right.generation,queued[2].id,3).await?;
        sqlx::query("UPDATE messages SET expires_at=now()-interval '1 second'").execute(store.pool()).await?;
        let remaining = store.delivery_batch(right.session_id,right.generation,&[],true).await?;
        ensure!(remaining.deliveries.len()==1 && remaining.deliveries[0].packet_id==Some(3));
        assert_consistent(&store,true).await?;
        store.maintenance().await?;
        assert_consistent(&store,true).await?;
        store.advance_qos2(right.session_id,right.generation,3).await?;
        store.confirm_deliveries(right.session_id,right.generation,&[3],DeliveryPhase::AwaitPubComp,false).await?;
        let totals: (i64,i64) = sqlx::query_as("SELECT coalesce(sum(queued_count),0)::bigint,coalesce(sum(queued_bytes),0)::bigint FROM sessions").fetch_one(store.pool()).await?;
        ensure!(totals==(0,0));
        assert_consistent(&store,true).await
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn concurrent_publishers_share_one_atomic_quota() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(1, 4096);
        let left = store
            .open_session(session("race-left", "publisher", false))
            .await?;
        let right = store
            .open_session(session("race-right", "publisher", false))
            .await?;
        let target = store
            .open_session(session("race-target", "subscriber", false))
            .await?;
        store
            .subscribe(
                target.session_id,
                target.generation,
                &subscription("race/#", 1),
            )
            .await?;
        let a = publication("race/value", 1, b"left", true);
        let b = publication("race/value", 1, b"right", true);
        let (a, b) = tokio::join!(
            store.accept_publication(left.session_id, left.generation, &a, b"race-left"),
            store.accept_publication(right.session_id, right.generation, &b, b"race-right")
        );
        ensure!(a.is_ok() != b.is_ok());
        ensure!(
            a.err()
                .or_else(|| b.err())
                .unwrap()
                .is::<clex_mqtt::store::QuotaExceeded>()
        );
        assert_consistent(&store, true).await
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn generated_upgrade_and_restart_rebuild_usage_without_changing_legacy_deliveries()
-> Result<()> {
    let db = TestDatabase::create_with_schema(true).await?;
    let outcome: Result<()> = async {
        let store=&db.store;
        let id=Uuid::new_v4();
        sqlx::query("INSERT INTO sessions(id,client_id,principal,protocol,online,expiry_interval,expires_at) VALUES($1,$2,'subscriber',5,false,3600,now()+interval '1 hour')")
            .bind(id).bind(b"legacy-reader".as_slice()).execute(store.pool()).await?;
        sqlx::query("INSERT INTO subscriptions(id,session_id,filter,qos) VALUES($1,$2,'legacy/#',2)")
            .bind(Uuid::new_v4()).bind(id).execute(store.pool()).await?;
        for (phase,packet,qos) in [("queued",None,1),("await_puback",Some(501),1),("await_pubcomp",Some(502),2)] {
            let message=Uuid::new_v4();
            sqlx::query("INSERT INTO messages(id,topic,payload,qos,retain,properties) VALUES($1,$2,$3,$4,false,$5)")
                .bind(message).bind(b"legacy/value".as_slice()).bind([0_u8,255,1].as_slice()).bind(qos)
                .bind(sqlx::types::Json(PublicationProperties::default())).execute(store.pool()).await?;
            sqlx::query("INSERT INTO deliveries(id,session_id,message_id,qos,retain,packet_id,phase) VALUES($1,$2,$3,$4,false,$5,$6)")
                .bind(Uuid::new_v4()).bind(id).bind(message).bind(qos).bind(packet).bind(phase).execute(store.pool()).await?;
        }
        migrations::apply(store.pool()).await?;
        let reader=store.open_session(session("legacy-reader","subscriber",false)).await?;
        ensure!(reader.session_present && reader.session_id==id);
        let deliveries=store.pending_deliveries(id,reader.generation,10).await?;
        ensure!(deliveries.len()==3 && deliveries[0].packet_id.is_none() && deliveries[1].packet_id==Some(501) && deliveries[2].packet_id==Some(502));
        store.acknowledge(id,reader.generation,501,DeliveryPhase::AwaitPubAck).await?;
        store.confirm_deliveries(id,reader.generation,&[502],DeliveryPhase::AwaitPubComp,false).await?;
        let source=store.open_session(session("legacy-source","publisher",false)).await?;
        let bounded=store.clone().with_queue_limits(1,4096);
        let rejected=bounded.accept_publication(source.session_id,source.generation,&publication("legacy/value",1,b"overflow",true),b"legacy-source").await.unwrap_err();
        ensure!(rejected.is::<clex_mqtt::store::QuotaExceeded>());
        let unknown:bool=sqlx::query_scalar("SELECT queued_count IS NULL AND queued_bytes IS NULL FROM sessions WHERE id=$1").bind(id).fetch_one(store.pool()).await?;
        ensure!(unknown,"failed lazy initialization must roll back together with the publication");
        let mut sub=subscription("legacy/#",2); sub.retain_handling=2;
        bounded.subscribe(id,reader.generation,&sub).await?;
        assert_consistent(store,true).await?;
        // Corruption fails closed: the deletion and the counter update roll back.
        sqlx::query("UPDATE sessions SET queued_count=0,queued_bytes=0 WHERE id=$1").bind(id).execute(store.pool()).await?;
        ensure!(store.discard_delivery(id,reader.generation,deliveries[0].id).await.is_err());
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM deliveries WHERE session_id=$1").bind(id).fetch_one(store.pool()).await?;
        ensure!(count==1);
        store.recover_startup().await?;
        let reader=store.open_session(session("legacy-reader","subscriber",false)).await?;
        store.subscribe(id,reader.generation,&sub).await?;
        assert_consistent(store,false).await?;
        store.discard_delivery(id,reader.generation,deliveries[0].id).await?;
        assert_consistent(store,false).await?;
        let usage:(i64,i64)=sqlx::query_as("SELECT queued_count,queued_bytes FROM sessions WHERE id=$1").bind(id).fetch_one(store.pool()).await?;
        ensure!(usage==(0,0));
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
