use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn qos0_batches_recheck_acl_discard_oversize_and_preserve_retain() -> Result<()> {
    use clex_mqtt::domain::{Publication, SessionSpec, Subscription};
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome:Result<()>=async {
        let spec=|name:&str|SessionSpec { client_id:name.as_bytes().to_vec(),principal:"wire-test".into(),clean_start:false,expiry_interval:300,protocol:5,will:None,will_delay_interval:0 };
        let source=h.store.open_session(spec("qos0-source")).await?;
        let target=h.store.open_session(spec("qos0-target")).await?;
        h.store.subscribe(target.session_id,target.generation,&Subscription { filter:"q0/#".into(),qos:0,no_local:false,retain_as_published:false,retain_handling:0,subscription_identifier:None }).await?;
        for (topic,payload) in [("q0/blocked",b"private".as_slice()),("q0/large",&[b'x';100]),("q0/one",b"one"),("q0/two",b"two")] {
            h.store.accept_publication(source.session_id,source.generation,&Publication { topic:topic.into(),payload:payload.to_vec(),qos:0,retain:true,expires_at:None,properties:Default::default() },b"qos0-source").await?;
        }
        sqlx::query("UPDATE mqtt_users SET superuser=false WHERE username='wire-test'").execute(h.store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'q0/#',true,true FROM mqtt_users WHERE username='wire-test'").bind(Uuid::new_v4()).execute(h.store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'q0/blocked',true,false FROM mqtt_users WHERE username='wire-test'").bind(Uuid::new_v4()).execute(h.store.pool()).await?;
        let mut subscriber=Client::connect_v5_properties(h.address,"qos0-target",false,&[0x11,0,0,1,44,0x21,0,1,0x27,0,0,0,64]).await?;
        for expected in [b"one".as_slice(),b"two"] {
            let (header,body)=subscriber.read().await?;
            let (_,payload,id)=publication(header,&body,5)?;
            ensure!(header & 14 == 0 && payload==expected && id==0,"QoS0 batch changed ordering, DUP or authorization");
        }
        subscriber.send(0xc0,&[]).await?;
        ensure!(subscriber.read().await?.0==0xd0);
        let empty:bool=sqlx::query_scalar("SELECT queued_count=0 AND queued_bytes=0 AND NOT EXISTS(SELECT 1 FROM deliveries WHERE session_id=s.id) FROM sessions s WHERE id=$1")
            .bind(target.session_id).fetch_one(h.store.pool()).await?;
        ensure!(empty);
        let retained:i64=sqlx::query_scalar("SELECT count(*) FROM retained").fetch_one(h.store.pool()).await?;
        ensure!(retained==4,"discarding recipient attempts changed Retain");
        subscriber.disconnect().await?;
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
