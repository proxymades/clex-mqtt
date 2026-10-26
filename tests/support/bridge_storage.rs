use super::*;
use clex_mqtt::bridge::{Direction, PATH_PROPERTY, Rule};

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn bridge_queues_share_commit_quotas_expiry_and_loop_fences() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome:Result<()>=async {
        let store=db.store.clone().with_queue_limits(1,4096);
        let source=store.open_session(session("bridge-source","publisher",false)).await?;
        let id=Uuid::new_v4();
        let rules=vec![Rule{direction:Direction::Out,filter:"#".into(),local_prefix:"edge/".into(),remote_prefix:"cloud/".into()}];
        sqlx::query("INSERT INTO mqtt_bridges(id,name,endpoint,rules) VALUES($1,'quota','mqtt://127.0.0.1:1883',$2)").bind(id).bind(serde_json::json!(&rules)).execute(store.pool()).await?;
        let first=Publication{topic:"edge/one".into(),payload:vec![0,255,1],qos:0,retain:false,expires_at:None,properties:Default::default()};
        store.accept_publication(source.session_id,source.generation,&first,b"bridge-source").await?;
        let queued=store.next_bridge_delivery(id,1).await?.unwrap();
        ensure!(queued.1.topic=="cloud/one" && queued.1.payload==first.payload && queued.1.qos==1);
        let second=Publication{topic:"edge/two".into(),qos:1,retain:true,..first.clone()};
        let failed=store.accept_publication(source.session_id,source.generation,&second,b"bridge-source").await.unwrap_err();
        ensure!(failed.is::<clex_mqtt::store::QuotaExceeded>());
        let retained:i64=sqlx::query_scalar("SELECT count(*) FROM retained").fetch_one(store.pool()).await?;ensure!(retained==0,"Retain and bridge obligation must roll back together");
        ensure!(store.monitor().read(&Default::default()).cursor==1);
        // The message garbage collector must keep a bridge's references.
        sqlx::query("UPDATE messages SET created_at=now()-interval '2 hours'").execute(store.pool()).await?;
        store.maintenance().await?;
        ensure!(store.next_bridge_delivery(id,1).await?.is_some());
        let qos2=Publication{qos:2,..second.clone()};
        store.receive_qos2(source.session_id,source.generation,7,&qos2).await?;
        ensure!(store.release_qos2(source.session_id,source.generation,7).await.unwrap_err().is::<clex_mqtt::store::QuotaExceeded>());
        sqlx::query("DELETE FROM bridge_deliveries WHERE bridge_id=$1").bind(id).execute(store.pool()).await?;
        store.release_qos2(source.session_id,source.generation,7).await?;
        store.release_qos2(source.session_id,source.generation,7).await?;
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries").fetch_one(store.pool()).await?;ensure!(count==1,"QoS 2 replay duplicated bridge work");
        sqlx::query("DELETE FROM bridge_deliveries").execute(store.pool()).await?;
        let mut marked=first.clone();marked.properties.user_properties.push((PATH_PROPERTY.into(),id.to_string()));
        store.accept_publication(source.session_id,source.generation,&marked,b"bridge-source").await?;
        ensure!(store.next_bridge_delivery(id,1).await?.is_none());
        let mut exhausted=first.clone();
        for _ in 0..8 {exhausted.properties.user_properties.push((PATH_PROPERTY.into(),Uuid::new_v4().to_string()));}
        store.accept_publication(source.session_id,source.generation,&exhausted,b"bridge-source").await?;
        ensure!(store.next_bridge_delivery(id,1).await?.is_none());
        let expires=Publication{expires_at:Some(chrono::Utc::now()+chrono::Duration::seconds(60)),..first.clone()};
        store.accept_publication(source.session_id,source.generation,&expires,b"bridge-source").await?;
        sqlx::query("UPDATE messages SET expires_at=now()-interval '1 second' WHERE id IN (SELECT message_id FROM bridge_deliveries)").execute(store.pool()).await?;
        store.maintenance().await?;
        ensure!(store.next_bridge_delivery(id,1).await?.is_none());
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries").fetch_one(store.pool()).await?;ensure!(count==0);
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
