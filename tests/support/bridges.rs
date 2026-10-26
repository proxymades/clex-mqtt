use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use clex_mqtt::{
    bridge::{self, Direction, Rule, SecretKey},
    domain::Publication,
};
use serde_json::json;

#[allow(dead_code)]
#[path = "pki.rs"]
mod pki;

fn key() -> SecretKey {
    SecretKey::parse(&STANDARD.encode([38; 32])).unwrap()
}
async fn insert_bridge(
    h: &Harness,
    id: Uuid,
    endpoint: String,
    rules: &[Rule],
    ca: Option<String>,
) -> Result<()> {
    sqlx::query("INSERT INTO mqtt_bridges(id,name,endpoint,username,password,ca_pem,rules) VALUES($1,'wire bridge',$2,'wire-test',$3,$4,$5)")
        .bind(id).bind(endpoint).bind(key().seal(id,"wire-test-password")?).bind(ca).bind(json!(rules)).execute(h.store.pool()).await?;
    Ok(())
}
async fn status(h: &Harness, id: Uuid, expected: &str) -> Result<()> {
    let result = timeout(Duration::from_secs(12), async {
        loop {
            let state: String = sqlx::query_scalar("SELECT status FROM mqtt_bridges WHERE id=$1")
                .bind(id)
                .fetch_one(h.store.pool())
                .await?;
            if state == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    if result.is_err() {
        let row = sqlx::query("SELECT status,last_error FROM mqtt_bridges WHERE id=$1")
            .bind(id)
            .fetch_one(h.store.pool())
            .await?;
        anyhow::bail!(
            "Bridge expected {expected}; actual {}, detail {:?}",
            sqlx::Row::get::<String, _>(&row, "status"),
            sqlx::Row::get::<Option<String>, _>(&row, "last_error")
        );
    }
    result??;
    Ok(())
}
async fn queue_empty(h: &Harness, id: Uuid) -> Result<()> {
    timeout(Duration::from_secs(8), async {
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries WHERE bridge_id=$1")
                    .bind(id)
                    .fetch_one(h.store.pool())
                    .await?;
            if count == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Bridge queue deadline")??;
    Ok(())
}
async fn receive(client: &mut Client, topic: &str, payload: &[u8], retain: bool) -> Result<()> {
    let (header, body) = client.read().await?;
    let (actual_topic, actual_payload, packet_id) = publication(header, &body, client.version)?;
    ensure!(
        actual_topic == topic && actual_payload == payload,
        "Bridge mapped a different publication: {actual_topic}"
    );
    ensure!(header & 1 == u8::from(retain), "Bridge lost Retain");
    if header & 6 != 0 {
        client.ack(4, packet_id).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn tcp_bridges_map_binary_retain_resume_queues_and_prevent_loops() -> Result<()> {
    let local = Harness::start().await?.context("local broker")?;
    let remote = Harness::start().await?.context("remote broker")?;
    let stop = CancellationToken::new();
    let mut task = None;
    let outcome: Result<()> = async {
        let id = Uuid::new_v4();
        let mut local_source = local.client(5,"local-source",true).await?;
        let mut remote_source = remote.client(5,"remote-source",true).await?;
        let mut local_sub = local.client(5,"local-sub",true).await?;
        ensure!(local_sub.subscribe_options("device/#",9,1).await? == 1);
        let mut remote_sub = remote.client(5,"remote-sub",true).await?;
        ensure!(remote_sub.subscribe_options("cloud/#",9,1).await? == 1);
        local_source.publish("edge/initial",b"initial",1,1,true,false).await?;
        ensure!(local_source.read().await?.0 == 0x40);
        remote_source.publish("commands/boot",b"boot",1,1,true,false).await?;
        ensure!(remote_source.read().await?.0 == 0x40);
        let rules = vec![
            Rule {direction:Direction::Out,filter:"#".into(),local_prefix:"edge/".into(),remote_prefix:"cloud/".into()},
            Rule {direction:Direction::In,filter:"#".into(),local_prefix:"device/".into(),remote_prefix:"commands/".into()},
            Rule {direction:Direction::In,filter:"#".into(),local_prefix:"edge/".into(),remote_prefix:"cloud/".into()},
        ];
        insert_bridge(&local,id,format!("mqtt://{}",remote.address),&rules,None).await?;
        let mut tx = local.store.pool().begin().await?;
        local.store.seed_bridge_retained(&mut tx,id,&rules).await?; tx.commit().await?;
        task = Some(tokio::spawn(bridge::run(local.store.clone(),local.broker.clone(),Some(key()),1024*1024,stop.clone())));
        status(&local,id,"connected").await?;
        receive(&mut local_sub,"device/boot",b"boot",true).await?;
        receive(&mut remote_sub,"cloud/initial",b"initial",true).await?;
        local_source.publish_once_qos2("edge/binary",&[0,255,2,128],2).await?;
        receive(&mut remote_sub,"cloud/binary",&[0,255,2,128],false).await?;
        queue_empty(&local,id).await?;
        let received: i64 = sqlx::query_scalar("SELECT received FROM mqtt_bridges WHERE id=$1").bind(id).fetch_one(local.store.pool()).await?;
        ensure!(received == 1,"own exports must not be imported");
        let mut properties=vec![0x01,1,0x02,0,0,0,60,0x03];
        string(&mut properties,"application/json");
        properties.push(0x08);string(&mut properties,"reply/unchanged");
        properties.extend_from_slice(&[0x09,0,3,0,255,2]);
        for value in ["first","second"] {properties.push(0x26);string(&mut properties,"tag");string(&mut properties,value);}
        local_source.publish_v5_properties("edge/properties",br#"{"value":1}"#,5,&properties).await?;
        ensure!(local_source.read().await?.0==0x40);
        receive(&mut remote_sub,"cloud/properties",br#"{"value":1}"#,false).await?;
        remote_source.publish_v5_properties("commands/properties",br#"{"value":2}"#,5,&properties).await?;
        ensure!(remote_source.read().await?.0==0x40);
        receive(&mut local_sub,"device/properties",br#"{"value":2}"#,false).await?;
        for (h,topic) in [(&local,"device/properties"),(&remote,"cloud/properties")] {
            let stored:serde_json::Value=sqlx::query_scalar("SELECT properties FROM messages WHERE topic=$1").bind(topic.as_bytes()).fetch_one(h.store.pool()).await?;
            ensure!(stored["payload_format_indicator"]==1 && stored["content_type"]=="application/json" && stored["response_topic"]=="reply/unchanged");
            ensure!(stored["correlation_data"]==json!([0,255,2]));
            ensure!(stored["user_properties"][0]==json!(["tag","first"]) && stored["user_properties"][1]==json!(["tag","second"]));
            let remaining:i64=sqlx::query_scalar("SELECT extract(epoch FROM expires_at-now())::bigint FROM messages WHERE topic=$1").bind(topic.as_bytes()).fetch_one(h.store.pool()).await?;
            ensure!((40..=60).contains(&remaining),"bridge must preserve remaining expiry");
        }
        // Incoming external publications traverse a mapping once. The bridge
        // marker then fences the matching reverse export rule.
        remote_source.publish("cloud/external",b"remote",1,2,false,false).await?;
        ensure!(remote_source.read().await?.0 == 0x40);
        receive(&mut remote_sub,"cloud/external",b"remote",false).await?;
        timeout(Duration::from_secs(3),async {
            loop {
                let count:i64=sqlx::query_scalar("SELECT count(*) FROM messages WHERE topic=$1").bind(b"edge/external".as_slice()).fetch_one(local.store.pool()).await?;
                if count == 1 {break;} tokio::time::sleep(Duration::from_millis(20)).await;
            } Ok::<_,anyhow::Error>(())
        }).await??;
        remote_sub.send(0xc0,&[]).await?;
        ensure!(remote_sub.read().await?.0 == 0xd0,"loop emitted an extra publication");

        // Simulate losing the local network worker. Publications still commit
        // to its SQL queue, including QoS 0 sources; MQTT 5 remote subscriptions
        // also retain their offline QoS 1 work.
        let running=task.take().context("bridge worker")?; running.abort(); let _=running.await;
        timeout(Duration::from_secs(3),async {
            loop {
                if remote.broker.clients().await.iter().all(|c|c.client_id!=format!("clex-bridge-{id}")) {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await?;
        local_source.publish("edge/offline",b"queued",1,3,false,false).await?;
        ensure!(local_source.read().await?.0 == 0x40);
        remote_source.publish("commands/offline",b"remote queue",1,3,false,false).await?;
        ensure!(remote_source.read().await?.0 == 0x40);
        let queued:i64=sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries WHERE bridge_id=$1").bind(id).fetch_one(local.store.pool()).await?;
        ensure!(queued == 1,"outgoing work must survive loss of network worker");
        task = Some(tokio::spawn(bridge::run(local.store.clone(),local.broker.clone(),Some(key()),1024*1024,stop.clone())));
        status(&local,id,"connected").await?;
        receive(&mut remote_sub,"cloud/offline",b"queued",false).await?;
        receive(&mut local_sub,"device/offline",b"remote queue",false).await?;
        queue_empty(&local,id).await?;
        // Disable revision fences imports and leaves the existing queue for
        // explicit resume; deletion cascades only this link's queued work.
        sqlx::query("UPDATE mqtt_bridges SET enabled=false,revision=revision+1,status='disabled' WHERE id=$1").bind(id).execute(local.store.pool()).await?;
        ensure!(local.store.import_bridge(id,1,&rules,&Publication {topic:"commands/late".into(),payload:vec![1],qos:1,retain:false,expires_at:None,properties:Default::default()}).await.is_err());
        local_source.publish("edge/paused",b"paused",1,4,false,false).await?;
        ensure!(local_source.read().await?.0 == 0x40);
        queue_empty(&local,id).await?;
        Ok(())
    }.await;
    stop.cancel();
    if let Some(task) = task {
        timeout(Duration::from_secs(5), task).await???;
    }
    let cleanup_local = local.close().await;
    let cleanup_remote = remote.close().await;
    outcome?;
    cleanup_local?;
    cleanup_remote
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn tls_bridge_verifies_ca_and_resumes_after_trust_update() -> Result<()> {
    let pki = pki::Pki::new()?;
    let local = Harness::start().await?.context("local broker")?;
    let remote = Harness::start().await?.context("remote broker")?;
    let stop = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let acceptor = clex_mqtt::tls::acceptor(
        &pki.path("server.pem"),
        &pki.path("server.key"),
        clex_mqtt::tls::ClientAuth::None,
        None,
    )?;
    let server_stop = stop.clone();
    let broker = remote.broker.clone();
    let server = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _=server_stop.cancelled()=>break,
                accepted=listener.accept()=> {let(io,peer)=accepted?;let acceptor=acceptor.clone();let broker=broker.clone();tasks.spawn(async move {if let Ok(io)=acceptor.accept(io).await {let _=broker.run_io(io,peer).await;}});},
                _=tasks.join_next(),if !tasks.is_empty()=>{},
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok::<_, anyhow::Error>(())
    });
    let mut task = None;
    let outcome: Result<()> = async {
        let id = Uuid::new_v4();
        let rules = vec![Rule {
            direction: Direction::Out,
            filter: "#".into(),
            local_prefix: "edge/".into(),
            remote_prefix: "cloud/".into(),
        }];
        insert_bridge(
            &local,
            id,
            format!("mqtts://{address}"),
            &rules,
            Some(pki.pem("other-ca")?),
        )
        .await?;
        task = Some(tokio::spawn(bridge::run(
            local.store.clone(),
            local.broker.clone(),
            Some(key()),
            1024 * 1024,
            stop.clone(),
        )));
        status(&local, id, "retrying").await?;
        let mut source = local.client(5, "tls-source", true).await?;
        source
            .publish("edge/trusted", b"verified TLS", 1, 1, false, false)
            .await?;
        ensure!(source.read().await?.0 == 0x40);
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM bridge_deliveries WHERE bridge_id=$1")
                .bind(id)
                .fetch_one(local.store.pool())
                .await?;
        ensure!(count == 1, "failed TLS must preserve queued work");
        let mut recipient = remote.client(5, "tls-recipient", true).await?;
        ensure!(recipient.subscribe_options("cloud/#", 1, 1).await? == 1);
        sqlx::query("UPDATE mqtt_bridges SET ca_pem=$2,revision=revision+1 WHERE id=$1")
            .bind(id)
            .bind(pki.pem("ca")?)
            .execute(local.store.pool())
            .await?;
        status(&local, id, "connected").await?;
        receive(&mut recipient, "cloud/trusted", b"verified TLS", false).await?;
        queue_empty(&local, id).await?;
        Ok(())
    }
    .await;
    stop.cancel();
    if let Some(task) = task {
        timeout(Duration::from_secs(5), task).await???;
    }
    server.await??;
    let a = local.close().await;
    let b = remote.close().await;
    outcome?;
    a?;
    b
}
