//! Independent wire-level integration tests. TEST_DATABASE_URL must name a
//! disposable test database; each test migrates and removes its own schema.
use anyhow::{Context, Result, ensure};
use clex_mqtt::{
    auth::hash_password,
    broker::{Broker, BrokerConfig},
    migrations,
    store::Store,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{env, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[path = "support/bridges.rs"]
mod bridges;
#[path = "support/connection_metrics.rs"]
mod connection_metrics;
#[path = "support/diagnostics_wire.rs"]
mod diagnostics_wire;
#[path = "support/qos0_wire.rs"]
mod qos0_wire;
#[path = "support/reservation_wire.rs"]
mod reservation_wire;
#[path = "support/scram.rs"]
mod scram_auth;

struct Harness {
    broker: Broker,
    store: Store,
    admin: PgPool,
    schema: String,
    address: SocketAddr,
    stop: CancellationToken,
    server: JoinHandle<()>,
}

impl Harness {
    async fn start() -> Result<Option<Self>> {
        Self::start_with(BrokerConfig::default()).await
    }
    async fn start_with(config: BrokerConfig) -> Result<Option<Self>> {
        let url = env::var("TEST_DATABASE_URL")
            .context("MQTT integration tests require isolated TEST_DATABASE_URL")?;
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        let schema = format!("mqtt_wire_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        let path = format!("SET search_path TO {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .after_connect(move |connection, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query(&path).execute(connection).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await?;
        migrations::apply(&pool).await?;
        let password_hash = hash_password("wire-test-password")?;
        sqlx::query("INSERT INTO mqtt_users (id,username,password_hash,enabled,superuser) VALUES ($1,'wire-test',$2,true,true)").bind(Uuid::new_v4()).bind(password_hash).execute(&pool).await?;
        let store = Store::from_pool(pool);
        let broker = Broker::new(
            store.clone(),
            BrokerConfig {
                poll_interval: Duration::from_millis(30),
                ..config
            },
        )
        .await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let task_broker = broker.clone();
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _=task_stop.cancelled()=>break,
                    accepted=listener.accept()=>match accepted {Ok((io,peer))=>{let broker=task_broker.clone();connections.spawn(async move {let _=broker.run_io(io,peer).await;});},Err(_)=>break},
                    _=connections.join_next(),if !connections.is_empty()=>{},
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Some(Self {
            broker,
            store,
            admin,
            schema,
            address,
            stop,
            server,
        }))
    }
    async fn client(&self, version: u8, id: &str, clean: bool) -> Result<Client> {
        Client::connect(self.address, version, id, clean, None).await
    }
    async fn close(self) -> Result<()> {
        self.broker.shutdown().await?;
        self.stop.cancel();
        self.server.await?;
        self.store.pool().close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

struct Client {
    io: TcpStream,
    version: u8,
    session_present: bool,
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn client_ids_are_scoped_by_account_for_all_protocol_versions() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome: Result<()> = async {
        sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,enabled,superuser) VALUES($1,'other-account',$2,true,true)")
            .bind(Uuid::new_v4()).bind(hash_password("other-account-password")?).execute(h.store.pool()).await?;
        for version in [3, 4, 5] {
            let id = format!("esp32-01-v{version}");
            let mut left = h.client(version, &id, false).await?;
            let mut right = Client::connect_authenticated(h.address, version, &id, false, None,
                "other-account", "other-account-password").await?;
            let live = h.broker.clients().await;
            ensure!(live.iter().filter(|client| client.client_id == id).count() == 2,
                "both accounts must retain their active connection");
            if version == 5 {
                ensure!(left.subscribe_options("isolation/value", 5, 1).await? == 1);
                ensure!(right.subscribe_options("isolation/value", 5, 1).await? == 1);
                left.publish_once_qos2("isolation/value", b"cross-account No Local", 2).await?;
                let (header, body) = right.read().await?;
                let (_, payload, pid) = publication(header, &body, version)?;
                ensure!(payload == b"cross-account No Local");
                right.ack(4, pid).await?;
                left.send(0xc0, &[]).await?;
                ensure!(left.read().await?.0 == 0xd0, "No Local must suppress only the publishing account");
            }
            let mut replacement = h.client(version, &id, false).await?;
            ensure!(replacement.session_present);
            if version == 5 { left.assert_disconnect(0x8e).await?; }
            else {
                let mut byte = [0];
                ensure!(timeout(Duration::from_secs(2), left.io.read(&mut byte)).await?? == 0);
            }
            right.send(0xc0, &[]).await?;
            ensure!(right.read().await?.0 == 0xd0, "takeover in one account must preserve the other account");
            ensure!(h.broker.disconnect("wire-test", &id).await);
            if version == 5 { replacement.assert_disconnect(0x8e).await?; }
            else {
                let mut byte = [0];
                ensure!(timeout(Duration::from_secs(2), replacement.io.read(&mut byte)).await?? == 0);
            }
            right.send(0xc0, &[]).await?;
            ensure!(right.read().await?.0 == 0xd0, "targeted admin disconnect must preserve the other account");
            right.disconnect().await?;
        }
        Ok(())
    }.await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn message_stream_tracks_wire_qos_and_will_without_retry_duplicates() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let outcome: Result<()> = async {
        let mut publisher = h.client(5, "monitor-source", true).await?;
        publisher
            .publish("monitor/value", b"zero", 0, 0, false, false)
            .await?;
        publisher.send(0xc0, &[]).await?;
        ensure!(publisher.read().await?.0 == 0xd0);
        publisher
            .publish("monitor/value", b"one", 1, 1, true, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x40);
        publisher
            .publish("monitor/value", &[0, 255, 2], 2, 2, false, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x50);
        publisher
            .publish("monitor/value", &[0, 255, 2], 2, 2, false, true)
            .await?;
        ensure!(publisher.read().await?.0 == 0x50);
        ensure!(h.store.monitor().read(&Default::default()).cursor == 2);
        publisher.ack(6, 2).await?;
        ensure!(publisher.read().await?.0 == 0x70);
        publisher.ack(6, 2).await?;
        ensure!(publisher.read().await?.0 == 0x70);
        let page = h.store.monitor().read(&Default::default());
        ensure!(
            page.messages.len() == 3
                && page
                    .messages
                    .iter()
                    .map(|event| event.qos)
                    .collect::<Vec<_>>()
                    == [0, 1, 2]
        );
        ensure!(page.messages[1].retain && page.messages[2].payload_base64 == "AP8C");
        ensure!(
            page.messages
                .iter()
                .all(|event| event.client_id == "monitor-source" && event.username == "wire-test")
        );
        ensure!(
            h.broker
                .clients()
                .await
                .iter()
                .any(|client| client.client_id == "monitor-source" && client.keep_alive == 60)
        );
        let will = Client::connect(
            h.address,
            4,
            "monitor-will",
            true,
            Some(("monitor/status", b"offline")),
        )
        .await?;
        drop(will);
        timeout(Duration::from_secs(4), async {
            loop {
                if h.store
                    .monitor()
                    .read(&Default::default())
                    .messages
                    .iter()
                    .any(|event| event.will)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let page = h.store.monitor().read(&Default::default());
        ensure!(page.messages.len() == 4);
        let event = page.messages.last().context("Will event")?;
        ensure!(event.will && event.client_id == "monitor-will" && event.topic == "monitor/status");
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
impl Client {
    async fn connect_v5_properties(
        address: SocketAddr,
        id: &str,
        clean: bool,
        properties: &[u8],
    ) -> Result<Self> {
        let io = TcpStream::connect(address).await?;
        let mut client = Self {
            io,
            version: 5,
            session_present: false,
        };
        let mut body = Vec::new();
        string(&mut body, "MQTT");
        body.extend_from_slice(&[5, if clean { 0xc2 } else { 0xc0 }, 0, 60]);
        property_section(&mut body, properties);
        string(&mut body, id);
        string(&mut body, "wire-test");
        string(&mut body, "wire-test-password");
        client.send(0x10, &body).await?;
        let (header, ack) = client.read().await?;
        ensure!(
            header == 0x20 && ack.len() >= 2 && ack[1] == 0,
            "CONNECT rejected: {ack:?}"
        );
        client.session_present = ack[0] & 1 != 0;
        Ok(client)
    }

    async fn connect(
        address: SocketAddr,
        version: u8,
        id: &str,
        clean: bool,
        will: Option<(&str, &[u8])>,
    ) -> Result<Self> {
        Self::connect_authenticated(
            address,
            version,
            id,
            clean,
            will,
            "wire-test",
            "wire-test-password",
        )
        .await
    }

    async fn connect_authenticated(
        address: SocketAddr,
        version: u8,
        id: &str,
        clean: bool,
        will: Option<(&str, &[u8])>,
        username: &str,
        password: &str,
    ) -> Result<Self> {
        let mut io = TcpStream::connect(address).await?;
        let mut body = Vec::new();
        string(&mut body, if version == 3 { "MQIsdp" } else { "MQTT" });
        body.push(version);
        body.push(0xc0 | if clean { 2 } else { 0 } | if will.is_some() { 4 } else { 0 });
        body.extend_from_slice(&60u16.to_be_bytes());
        if version == 5 {
            body.push(5);
            body.push(0x11);
            body.extend_from_slice(&300u32.to_be_bytes());
        }
        string(&mut body, id);
        if let Some((topic, payload)) = will {
            string(&mut body, topic);
            binary(&mut body, payload);
        }
        string(&mut body, username);
        string(&mut body, password);
        io.write_all(&frame(0x10, &body)).await?;
        let mut client = Self {
            io,
            version,
            session_present: false,
        };
        let (header, ack) = client.read().await?;
        ensure!(
            header == 0x20 && ack.len() >= 2 && ack[1] == 0,
            "CONNECT rejected: {ack:?}"
        );
        client.session_present = ack[0] & 1 != 0;
        Ok(client)
    }
    async fn send(&mut self, header: u8, body: &[u8]) -> Result<()> {
        self.io.write_all(&frame(header, body)).await?;
        Ok(())
    }
    async fn publish_v5_properties(
        &mut self,
        topic: &str,
        payload: &[u8],
        pid: u16,
        properties: &[u8],
    ) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, topic);
        body.extend_from_slice(&pid.to_be_bytes());
        property_section(&mut body, properties);
        body.extend_from_slice(payload);
        self.send(0x32, &body).await
    }

    async fn publish_once_qos2(&mut self, topic: &str, payload: &[u8], pid: u16) -> Result<()> {
        self.publish(topic, payload, 2, pid, false, false).await?;
        let (header, body) = self.read().await?;
        ensure!(
            header == 0x50
                && body.starts_with(&pid.to_be_bytes())
                && body.get(2).copied().unwrap_or(0) == 0,
            "expected successful PUBREC"
        );
        self.ack(6, pid).await?;
        let (header, body) = self.read().await?;
        ensure!(
            header == 0x70
                && body.starts_with(&pid.to_be_bytes())
                && body.get(2).copied().unwrap_or(0) == 0,
            "expected successful PUBCOMP"
        );
        Ok(())
    }

    async fn assert_disconnect(&mut self, reason: u8) -> Result<()> {
        let (header, body) = self.read().await?;
        ensure!(
            header == 0xe0 && body == [reason, 0],
            "expected DISCONNECT [{reason:#04x},0], received {header:#04x} {body:?}"
        );
        let mut byte = [0];
        ensure!(
            timeout(Duration::from_secs(2), self.io.read(&mut byte)).await?? == 0,
            "connection remained open after DISCONNECT"
        );
        Ok(())
    }
    async fn read(&mut self) -> Result<(u8, Vec<u8>)> {
        timeout(Duration::from_secs(4), async {
            let header = self.io.read_u8().await?;
            let mut remaining = 0usize;
            let mut multiplier = 1;
            for _ in 0..4 {
                let byte = self.io.read_u8().await?;
                remaining += usize::from(byte & 127) * multiplier;
                if byte & 128 == 0 {
                    let mut body = vec![0; remaining];
                    self.io.read_exact(&mut body).await?;
                    return Ok((header, body));
                }
                multiplier *= 128;
            }
            Err(anyhow::anyhow!("malformed response frame"))
        })
        .await
        .context("MQTT packet timeout")?
    }
    async fn subscribe(&mut self, filter: &str, qos: u8, pid: u16) -> Result<()> {
        let status = self.subscribe_options(filter, qos, pid).await?;
        ensure!(status == qos, "subscription refused {status}");
        Ok(())
    }
    async fn subscribe_options(&mut self, filter: &str, options: u8, pid: u16) -> Result<u8> {
        let mut body = pid.to_be_bytes().to_vec();
        if self.version == 5 {
            body.push(0);
        }
        string(&mut body, filter);
        body.push(options);
        self.send(0x82, &body).await?;
        let (header, ack) = self.read().await?;
        ensure!(header == 0x90, "expected SUBACK");
        Ok(*ack.last().context("missing SUBACK reason")?)
    }
    async fn unsubscribe(&mut self, filter: &str, pid: u16) -> Result<()> {
        let mut body = pid.to_be_bytes().to_vec();
        if self.version == 5 {
            body.push(0);
        }
        string(&mut body, filter);
        self.send(0xa2, &body).await?;
        let (header, ack) = self.read().await?;
        ensure!(header == 0xb0 && ack.starts_with(&pid.to_be_bytes()));
        if self.version == 5 {
            ensure!(ack.last() == Some(&0), "UNSUBSCRIBE failed: {ack:?}");
        }
        Ok(())
    }
    async fn publish(
        &mut self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        pid: u16,
        retain: bool,
        dup: bool,
    ) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, topic);
        if qos > 0 {
            body.extend_from_slice(&pid.to_be_bytes());
        }
        if self.version == 5 {
            body.push(0);
        }
        body.extend_from_slice(payload);
        self.send(
            0x30 | (qos << 1) | u8::from(retain) | (u8::from(dup) << 3),
            &body,
        )
        .await
    }
    async fn ack(&mut self, kind: u8, pid: u16) -> Result<()> {
        self.send(
            (kind << 4) | if kind == 6 { 2 } else { 0 },
            &pid.to_be_bytes(),
        )
        .await
    }
    async fn disconnect(mut self) -> Result<()> {
        self.send(0xe0, &[]).await?;
        self.io.shutdown().await?;
        Ok(())
    }
    async fn assert_quiet(&self) -> Result<()> {
        let mut byte = [0];
        ensure!(
            timeout(Duration::from_millis(120), self.io.peek(&mut byte))
                .await
                .is_err(),
            "unexpected MQTT data"
        );
        Ok(())
    }
}

fn string(out: &mut Vec<u8>, value: &str) {
    binary(out, value.as_bytes());
}
fn binary(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}
fn frame(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![header];
    let mut n = body.len();
    loop {
        let mut byte = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            byte |= 128;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

fn property_section(out: &mut Vec<u8>, properties: &[u8]) {
    // Reuse the independently encoded Remaining Length for a property section.
    let encoded = frame(0, properties);
    out.extend_from_slice(&encoded[1..]);
}
fn publication(header: u8, body: &[u8], version: u8) -> Result<(String, Vec<u8>, u16)> {
    let length =
        u16::from_be_bytes(body.get(..2).context("missing topic length")?.try_into()?) as usize;
    let topic = String::from_utf8(body.get(2..2 + length).context("missing topic")?.to_vec())?;
    let mut at = 2 + length;
    let pid = if header & 6 != 0 {
        let id = u16::from_be_bytes(
            body.get(at..at + 2)
                .context("missing identifier")?
                .try_into()?,
        );
        at += 2;
        id
    } else {
        0
    };
    if version == 5 {
        let mut remaining = 0usize;
        let mut mul = 1;
        loop {
            let byte = *body.get(at).context("missing properties length")?;
            at += 1;
            remaining += usize::from(byte & 127) * mul;
            if byte & 128 == 0 {
                break;
            }
            mul *= 128;
        }
        at += remaining;
    }
    Ok((
        topic,
        body.get(at..).context("missing payload")?.to_vec(),
        pid,
    ))
}

async fn group_publication(left: &mut Client, right: &mut Client) -> Result<(usize, u8, Vec<u8>)> {
    // Peek is cancellation-safe: selecting a member must not consume a partial
    // MQTT frame from the other member.
    let mut left_byte = [0];
    let mut right_byte = [0];
    let selected = timeout(Duration::from_secs(4), async {
        tokio::select! {
            result = left.io.peek(&mut left_byte) => { ensure!(result? > 0, "left member closed"); Ok::<_, anyhow::Error>(0) },
            result = right.io.peek(&mut right_byte) => { ensure!(result? > 0, "right member closed"); Ok::<_, anyhow::Error>(1) },
        }
    })
    .await
    .context("shared delivery timeout")??;
    let (header, body) = if selected == 0 {
        let packet = left.read().await?;
        right.assert_quiet().await?;
        packet
    } else {
        let packet = right.read().await?;
        left.assert_quiet().await?;
        packet
    };
    ensure!(header >> 4 == 3, "expected shared PUBLISH");
    Ok((selected, header, body))
}

async fn complete_delivery(client: &mut Client, header: u8, body: &[u8]) -> Result<()> {
    let (_, _, pid) = publication(header, body, client.version)?;
    match (header >> 1) & 3 {
        1 => client.ack(4, pid).await?,
        2 => {
            client.ack(5, pid).await?;
            let (release, release_body) = client.read().await?;
            ensure!(release == 0x62 && release_body.starts_with(&pid.to_be_bytes()));
            client.ack(7, pid).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn wait_offline(h: &Harness, id: &str) -> Result<()> {
    timeout(Duration::from_secs(2), async {
        loop {
            let online: bool = sqlx::query_scalar("SELECT online FROM sessions WHERE client_id=$1")
                .bind(id.as_bytes())
                .fetch_one(h.store.pool())
                .await?;
            if !online {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("session did not become offline")?
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_groups_round_robin_and_ordinary_subscriptions_receive_separate_copies() -> Result<()>
{
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut left = h.client(5, "shared-left", true).await?;
    let mut right = h.client(5, "shared-right", true).await?;
    let mut other_group = h.client(5, "shared-other-group", true).await?;
    let mut ordinary = h.client(5, "shared-ordinary", true).await?;
    left.subscribe("$share/workers/jobs/+", 1, 1).await?;
    right.subscribe("$share/workers/jobs/+", 1, 1).await?;
    other_group
        .subscribe("$share/observers/jobs/+", 1, 1)
        .await?;
    ordinary.subscribe("jobs/#", 1, 1).await?;
    let mut publisher = h.client(5, "shared-publisher", true).await?;
    let mut counts = [0; 2];
    let mut previous = None;
    for id in 1..=6u16 {
        let payload = [0, id as u8, 128, 255];
        publisher
            .publish("jobs/value", &payload, 1, id, false, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x40);
        let (selected, header, body) = group_publication(&mut left, &mut right).await?;
        let (topic, actual, _) = publication(header, &body, 5)?;
        ensure!(topic == "jobs/value" && actual == payload && header & 6 == 2);
        ensure!(
            previous != Some(selected),
            "shared members did not alternate"
        );
        previous = Some(selected);
        counts[selected] += 1;
        complete_delivery(
            if selected == 0 { &mut left } else { &mut right },
            header,
            &body,
        )
        .await?;
        for client in [&mut other_group, &mut ordinary] {
            let (header, body) = client.read().await?;
            ensure!(publication(header, &body, 5)?.1 == payload);
            complete_delivery(client, header, &body).await?;
            client.assert_quiet().await?;
        }
        left.assert_quiet().await?;
        right.assert_quiet().await?;
    }
    ensure!(counts == [3, 3]);
    publisher.disconnect().await?;
    left.disconnect().await?;
    right.disconnect().await?;
    other_group.disconnect().await?;
    ordinary.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_identity_includes_inner_filter_and_preserves_each_subscription_options()
-> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut member = h.client(5, "shared-overlap", true).await?;
    member.subscribe("$share/work/jobs/#", 1, 1).await?;
    ensure!(
        member
            .subscribe_options("$share/work/jobs/+", 2 | 8, 2)
            .await?
            == 2
    );
    member.subscribe("jobs/value", 1, 3).await?;
    let mut publisher = h.client(5, "shared-overlap-pub", true).await?;
    publisher
        .publish("jobs/value", b"three obligations", 2, 1, true, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x50);
    publisher.ack(6, 1).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    let mut flags = Vec::new();
    let mut deliveries = Vec::new();
    for _ in 0..3 {
        let (header, body) = member.read().await?;
        ensure!(publication(header, &body, 5)?.1 == b"three obligations");
        flags.push(header & 7);
        deliveries.push((header, body));
    }
    for (header, body) in deliveries {
        complete_delivery(&mut member, header, &body).await?;
    }
    flags.sort_unstable();
    ensure!(
        flags == [2, 2, 5],
        "per-subscription QoS/RAP was merged: {flags:?}"
    );
    member.assert_quiet().await?;
    publisher.disconnect().await?;
    member.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_subscriptions_validate_syntax_no_local_and_protocol_version() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut client = h.client(5, "shared-validation", true).await?;
    for (index, filter) in [
        "$share//jobs/#",
        "$share/+/jobs/#",
        "$share/gr#oup/jobs/#",
        "$share/group",
        "$share/group/",
        "$share/group/jobs/#/invalid",
    ]
    .into_iter()
    .enumerate()
    {
        ensure!(
            client
                .subscribe_options(filter, 1, index as u16 + 1)
                .await?
                == 0x8f,
            "invalid shared filter accepted: {filter}"
        );
    }
    let mut sub = 20u16.to_be_bytes().to_vec();
    sub.push(0);
    string(&mut sub, "$share/group/jobs/#");
    sub.push(1 | 4);
    client.send(0x82, &sub).await?;
    client.assert_disconnect(0x82).await?;
    for version in [3, 4] {
        let mut old = h
            .client(version, &format!("shared-v{version}"), true)
            .await?;
        ensure!(old.subscribe_options("$share/group/jobs/#", 1, 1).await? == 0x80);
        old.send(0xc0, &[]).await?;
        ensure!(old.read().await?.0 == 0xd0);
        old.disconnect().await?;
    }
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_subscribe_skips_retained_replay_respects_system_namespace_and_unsubscribe()
-> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut publisher = h.client(5, "shared-retained-pub", true).await?;
    publisher
        .publish("shared/value", b"saved", 1, 1, true, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let mut matching = h.client(5, "shared-filtered", true).await?;
    let mut broad = h.client(5, "shared-broad", true).await?;
    matching.subscribe("$share/readers/shared/+", 1, 1).await?;
    matching.subscribe("$share/readers/$SYS/#", 1, 2).await?;
    broad.subscribe("$share/readers/#", 1, 1).await?;
    matching.assert_quiet().await?;
    broad.assert_quiet().await?;
    publisher
        .publish("$SYS/private", b"system", 1, 2, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = matching.read().await?;
    ensure!(publication(header, &body, 5)?.0 == "$SYS/private");
    complete_delivery(&mut matching, header, &body).await?;
    broad.assert_quiet().await?;
    publisher
        .publish("shared/value", b"live saved", 1, 3, true, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    for client in [&mut matching, &mut broad] {
        let (header, body) = client.read().await?;
        ensure!(header & 1 == 0 && publication(header, &body, 5)?.1 == b"live saved");
        complete_delivery(client, header, &body).await?;
    }
    matching.unsubscribe("$share/readers/shared/+", 3).await?;
    publisher
        .publish("shared/value", b"after unsubscribe", 1, 4, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = broad.read().await?;
    ensure!(publication(header, &body, 5)?.1 == b"after unsubscribe");
    complete_delivery(&mut broad, header, &body).await?;
    matching.assert_quiet().await?;
    broad.assert_quiet().await?;
    publisher.disconnect().await?;
    matching.disconnect().await?;
    broad.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_persistent_members_prefer_online_then_queue_one_offline_delivery() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut left = h.client(5, "shared-durable-left", false).await?;
    let mut right = h.client(5, "shared-durable-right", false).await?;
    left.subscribe("$share/durable/queued/#", 1, 1).await?;
    right.subscribe("$share/durable/queued/#", 1, 1).await?;
    left.disconnect().await?;
    wait_offline(&h, "shared-durable-left").await?;
    let mut publisher = h.client(5, "shared-durable-pub", true).await?;
    publisher
        .publish("queued/value", b"online", 1, 1, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = right.read().await?;
    ensure!(publication(header, &body, 5)?.1 == b"online");
    complete_delivery(&mut right, header, &body).await?;
    right.disconnect().await?;
    wait_offline(&h, "shared-durable-right").await?;
    publisher
        .publish("queued/value", b"offline", 1, 2, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    publisher
        .publish("queued/value", b"offline qos zero", 0, 0, false, false)
        .await?;
    publisher.send(0xc0, &[]).await?;
    ensure!(publisher.read().await?.0 == 0xd0);
    let mut left = h.client(5, "shared-durable-left", false).await?;
    let mut right = h.client(5, "shared-durable-right", false).await?;
    ensure!(left.session_present && right.session_present);
    let (selected, header, body) = group_publication(&mut left, &mut right).await?;
    ensure!(publication(header, &body, 5)?.1 == b"offline");
    complete_delivery(
        if selected == 0 { &mut left } else { &mut right },
        header,
        &body,
    )
    .await?;
    left.assert_quiet().await?;
    right.assert_quiet().await?;
    publisher
        .publish("queued/value", b"resumed", 1, 3, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (next, header, body) = group_publication(&mut left, &mut right).await?;
    ensure!(next != selected && publication(header, &body, 5)?.1 == b"resumed");
    complete_delivery(
        if next == 0 { &mut left } else { &mut right },
        header,
        &body,
    )
    .await?;
    publisher.disconnect().await?;
    left.disconnect().await?;
    right.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_qos2_stays_with_selected_member_across_reconnect_and_pubrel_retry() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut left = h.client(5, "shared-qos2-left", false).await?;
    let mut right = h.client(5, "shared-qos2-right", false).await?;
    left.subscribe("$share/qos2/commands/#", 2, 1).await?;
    right.subscribe("$share/qos2/commands/#", 2, 1).await?;
    let mut publisher = h.client(5, "shared-qos2-pub", false).await?;
    publisher
        .publish("commands/run", b"pinned", 2, 9, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x50);
    left.assert_quiet().await?;
    right.assert_quiet().await?;
    publisher.ack(6, 9).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    let (selected, header, body) = group_publication(&mut left, &mut right).await?;
    let (_, payload, pid) = publication(header, &body, 5)?;
    ensure!(payload == b"pinned" && header & 6 == 4);
    let (mut owner, other, owner_id) = if selected == 0 {
        (left, right, "shared-qos2-left")
    } else {
        (right, left, "shared-qos2-right")
    };
    owner.ack(5, pid).await?;
    let (header, body) = owner.read().await?;
    ensure!(header == 0x62 && body.starts_with(&pid.to_be_bytes()));
    owner.disconnect().await?;
    wait_offline(&h, owner_id).await?;
    other.assert_quiet().await?;
    publisher.ack(6, 9).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    other.assert_quiet().await?;
    let mut owner = h.client(5, owner_id, false).await?;
    ensure!(owner.session_present);
    let (header, body) = owner.read().await?;
    ensure!(header == 0x62 && body.starts_with(&pid.to_be_bytes()));
    owner.ack(7, pid).await?;
    owner.assert_quiet().await?;
    other.assert_quiet().await?;
    publisher.disconnect().await?;
    owner.disconnect().await?;
    other.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_subscribe_authorizes_inner_filter_and_rechecks_revoked_permissions() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let user = Uuid::new_v4();
    let allow = Uuid::new_v4();
    sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,enabled,superuser) VALUES($1,'shared-limited',$2,true,false)")
        .bind(user).bind(hash_password("limited-password")?).execute(h.store.pool()).await?;
    sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) VALUES($1,$2,'jobs/#',true,true),($3,$2,'jobs/private/#',true,false)")
        .bind(allow).bind(user).bind(Uuid::new_v4()).execute(h.store.pool()).await?;
    let mut member = Client::connect_authenticated(
        h.address,
        5,
        "shared-limited-member",
        true,
        None,
        "shared-limited",
        "limited-password",
    )
    .await?;
    ensure!(
        member
            .subscribe_options("$share/workers/jobs/public/+", 1, 1)
            .await?
            == 1,
        "inner topic filter must use the ordinary subscribe ACL"
    );
    for (pid, filter) in [
        (2, "$share/workers/jobs/#"),
        (3, "$share/workers/jobs/private/#"),
        (4, "$share/workers/other/#"),
    ] {
        ensure!(
            member.subscribe_options(filter, 1, pid).await? == 0x87,
            "unauthorized inner filter accepted: {filter}"
        );
    }
    let mut publisher = h.client(5, "shared-acl-publisher", true).await?;
    publisher
        .publish("jobs/public/one", b"allowed", 1, 1, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = member.read().await?;
    ensure!(publication(header, &body, 5)?.1 == b"allowed");
    complete_delivery(&mut member, header, &body).await?;
    sqlx::query("DELETE FROM mqtt_acl_rules WHERE id=$1")
        .bind(allow)
        .execute(h.store.pool())
        .await?;
    publisher
        .publish("jobs/public/one", b"revoked", 1, 2, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    member.assert_quiet().await?;
    ensure!(
        member
            .subscribe_options("$share/workers/jobs/public/+", 1, 5)
            .await?
            == 0x87
    );
    publisher.disconnect().await?;
    member.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_inner_filter_starting_with_share_is_a_literal_topic_after_one_prefix() -> Result<()>
{
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,enabled,superuser) VALUES($1,'shared-literal',$2,true,false)")
        .bind(user).bind(hash_password("literal-password")?).execute(h.store.pool()).await?;
    sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) VALUES($1,$2,'$share/inner/topic',true,true)")
        .bind(Uuid::new_v4()).bind(user).execute(h.store.pool()).await?;
    let mut member = Client::connect_authenticated(
        h.address,
        5,
        "shared-literal-member",
        true,
        None,
        "shared-literal",
        "literal-password",
    )
    .await?;
    member
        .subscribe("$share/outer/$share/inner/topic", 1, 1)
        .await?;
    let mut publisher = h.client(5, "shared-literal-publisher", true).await?;
    publisher
        .publish(
            "$share/inner/topic",
            b"literal dollar topic",
            1,
            1,
            false,
            false,
        )
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = member.read().await?;
    let (topic, payload, _) = publication(header, &body, 5)?;
    ensure!(
        topic == "$share/inner/topic" && payload == b"literal dollar topic",
        "shared prefix must be stripped exactly once and publication ACL must stay literal"
    );
    complete_delivery(&mut member, header, &body).await?;
    publisher
        .publish("topic", b"not the inner topic", 1, 2, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    member.assert_quiet().await?;
    publisher.disconnect().await?;
    member.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_online_full_member_falls_back_and_all_full_returns_negative_puback() -> Result<()> {
    let Some(h) = Harness::start_with(BrokerConfig {
        max_pending: 1,
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    let mut left = h.client(5, "shared-capacity-left", false).await?;
    let mut right = h.client(5, "shared-capacity-right", false).await?;
    left.subscribe("$share/workers/capacity/#", 1, 1).await?;
    right.subscribe("$share/workers/capacity/#", 1, 1).await?;
    let mut publisher = h.client(5, "shared-capacity-publisher", true).await?;
    let mut pending = Vec::new();
    for id in 1..=2u16 {
        publisher
            .publish("capacity/work", &[id as u8], 1, id, false, false)
            .await?;
        let (header, ack) = publisher.read().await?;
        ensure!(header == 0x40 && ack.get(2) == Some(&0));
        let (selected, header, body) = group_publication(&mut left, &mut right).await?;
        ensure!(publication(header, &body, 5)?.1 == [id as u8]);
        pending.push((selected, header, body));
    }
    ensure!(pending[0].0 != pending[1].0);
    let (available, header, body) = pending.pop().unwrap();
    let free_member = if available == 0 {
        &mut left
    } else {
        &mut right
    };
    complete_delivery(free_member, header, &body).await?;
    // The PING response orders acknowledgement processing before publishing
    // through another connection, without relying on a timer.
    free_member.send(0xc0, &[]).await?;
    ensure!(free_member.read().await?.0 == 0xd0);
    publisher
        .publish("capacity/work", b"fallback", 1, 3, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x40 && ack.get(2) == Some(&0));
    let (selected, header, body) = group_publication(&mut left, &mut right).await?;
    ensure!(
        selected == available && publication(header, &body, 5)?.1 == b"fallback",
        "full preferred member must not prevent another online member accepting work"
    );
    pending.push((selected, header, body));
    publisher
        .publish("capacity/work", b"must reject", 1, 4, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x40 && ack.starts_with(&4u16.to_be_bytes()) && ack.get(2) == Some(&0x97));
    publisher.send(0xc0, &[]).await?;
    ensure!(publisher.read().await?.0 == 0xd0);
    left.assert_quiet().await?;
    right.assert_quiet().await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries")
        .fetch_one(h.store.pool())
        .await?;
    ensure!(
        count == 2,
        "negative PUBACK must roll back the rejected obligation"
    );
    for (selected, header, body) in pending {
        complete_delivery(
            if selected == 0 { &mut left } else { &mut right },
            header,
            &body,
        )
        .await?;
    }
    publisher.disconnect().await?;
    left.disconnect().await?;
    right.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn mqtt_versions_retained_and_system_namespace() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut subscriber = h.client(5, "sub", true).await?;
    subscriber.subscribe("devices/#", 1, 1).await?;
    for version in [3, 4, 5] {
        let mut publisher = h.client(version, &format!("p{version}"), true).await?;
        let payload = format!("v{version}");
        publisher
            .publish("devices/one", payload.as_bytes(), 1, 7, version == 5, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x40, "missing PUBACK");
        let (header, body) = subscriber.read().await?;
        let (topic, actual, pid) = publication(header, &body, 5)?;
        ensure!(topic == "devices/one" && actual == payload.as_bytes());
        subscriber.ack(4, pid).await?;
        publisher.disconnect().await?;
    }
    let mut retained = h.client(4, "retained", true).await?;
    retained.subscribe("devices/one", 1, 1).await?;
    let (header, body) = retained.read().await?;
    ensure!(header & 1 == 1, "retained flag missing");
    let (_, payload, pid) = publication(header, &body, 4)?;
    ensure!(payload == b"v5");
    retained.ack(4, pid).await?;
    let mut all = h.client(4, "all", true).await?;
    all.subscribe("#", 0, 1).await?;
    let (header, body) = all.read().await?;
    ensure!(publication(header, &body, 4)?.0 == "devices/one");
    let mut publisher = h.client(4, "system", true).await?;
    publisher
        .publish("$SYS/private", b"system", 0, 0, false, false)
        .await?;
    all.assert_quiet().await?;
    subscriber.disconnect().await?;
    retained.disconnect().await?;
    all.disconnect().await?;
    publisher.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn durable_qos2_resume_from_pubrel_without_duplicate_fanout() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut subscriber = h.client(5, "qos2-sub", false).await?;
    subscriber.subscribe("commands/#", 2, 1).await?;
    let mut publisher = h.client(5, "qos2-pub", false).await?;
    publisher
        .publish("commands/run", b"once", 2, 9, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x50);
    subscriber.assert_quiet().await?;
    publisher
        .publish("commands/run", b"once", 2, 9, false, true)
        .await?;
    ensure!(publisher.read().await?.0 == 0x50);
    publisher.ack(6, 9).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    let (header, body) = subscriber.read().await?;
    let (_, payload, id) = publication(header, &body, 5)?;
    ensure!(header & 6 == 4 && payload == b"once");
    subscriber.ack(5, id).await?;
    ensure!(subscriber.read().await?.0 == 0x62);
    drop(subscriber);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut resumed = h.client(5, "qos2-sub", false).await?;
    ensure!(resumed.session_present);
    let (header, body) = resumed.read().await?;
    ensure!(
        header == 0x62 && u16::from_be_bytes(body[..2].try_into()?) == id,
        "must resume durable PUBREL"
    );
    resumed.ack(7, id).await?;
    publisher.ack(6, 9).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    resumed.assert_quiet().await?;
    publisher.disconnect().await?;
    resumed.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn offline_qos1_reconnect_and_connection_takeover() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut subscriber = h.client(4, "offline", false).await?;
    subscriber.subscribe("sensor/+", 1, 1).await?;
    subscriber.disconnect().await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut publisher = h.client(5, "online", true).await?;
    publisher
        .publish("sensor/a", b"queued", 1, 1, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let mut resumed = h.client(4, "offline", false).await?;
    ensure!(resumed.session_present);
    let (header, body) = resumed.read().await?;
    let (_, payload, pid) = publication(header, &body, 4)?;
    ensure!(payload == b"queued");
    let mut replacement = h.client(4, "offline", false).await?;
    ensure!(replacement.session_present);
    let (header, body) = replacement.read().await?;
    ensure!(header & 8 != 0, "retransmission must carry DUP");
    ensure!(publication(header, &body, 4)?.2 == pid);
    replacement.ack(4, pid).await?;
    let mut byte = [0];
    ensure!(
        timeout(Duration::from_secs(2), resumed.io.read(&mut byte)).await?? == 0,
        "old connection was not fenced"
    );
    publisher.disconnect().await?;
    replacement.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn unexpected_disconnect_publishes_will_but_normal_disconnect_suppresses_it() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut watcher = h.client(4, "watcher", true).await?;
    watcher.subscribe("presence/#", 0, 1).await?;
    let normal = Client::connect(
        h.address,
        4,
        "normal",
        true,
        Some(("presence/normal", b"offline")),
    )
    .await?;
    normal.disconnect().await?;
    watcher.assert_quiet().await?;
    let abrupt = Client::connect(
        h.address,
        4,
        "abrupt",
        true,
        Some(("presence/abrupt", b"offline")),
    )
    .await?;
    drop(abrupt);
    let (header, body) = watcher.read().await?;
    let (topic, payload, _) = publication(header, &body, 4)?;
    ensure!(topic == "presence/abrupt" && payload == b"offline");
    watcher.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn mqtt5_properties_subscription_ids_no_local_and_zero_expiry() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut subscriber = h.client(5, "properties-sub", true).await?;
    let mut sub = 1u16.to_be_bytes().to_vec();
    sub.extend_from_slice(&[2, 0x0b, 23]);
    string(&mut sub, "properties/#");
    sub.push(1 | 4 | 8);
    subscriber.send(0x82, &sub).await?;
    ensure!(subscriber.read().await?.0 == 0x90);
    let mut publisher = h.client(5, "properties-pub", true).await?;
    let mut props = Vec::new();
    props.push(0x03);
    string(&mut props, "application/json");
    props.push(0x08);
    string(&mut props, "reply/to");
    props.push(0x09);
    binary(&mut props, &[0, 255]);
    props.push(0x26);
    string(&mut props, "tag");
    string(&mut props, "one");
    props.push(0x26);
    string(&mut props, "tag");
    string(&mut props, "two");
    let mut body = Vec::new();
    string(&mut body, "properties/value");
    body.extend_from_slice(&7u16.to_be_bytes());
    body.push(props.len() as u8);
    body.extend_from_slice(&props);
    body.extend_from_slice(b"{}");
    publisher.send(0x33, &body).await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = subscriber.read().await?;
    let (topic, payload, pid) = publication(header, &body, 5)?;
    ensure!(topic == "properties/value" && payload == b"{}" && header & 1 == 1);
    let mut at = 2 + topic.len() + 2;
    let props_len = body[at] as usize;
    at += 1;
    let forwarded = &body[at..at + props_len];
    ensure!(
        forwarded.windows(2).any(|pair| pair == [0x0b, 23]),
        "subscription identifier missing"
    );
    for (kind, value) in [(0x03, "application/json"), (0x08, "reply/to")] {
        let mut expected = vec![kind];
        string(&mut expected, value);
        ensure!(
            forwarded
                .windows(expected.len())
                .any(|bytes| bytes == expected),
            "forwarded string property missing"
        );
    }
    ensure!(
        forwarded
            .windows(5)
            .any(|bytes| bytes == [0x09, 0, 2, 0, 255]),
        "binary correlation data changed"
    );
    let mut expected = vec![0x26];
    string(&mut expected, "tag");
    string(&mut expected, "one");
    expected.push(0x26);
    string(&mut expected, "tag");
    string(&mut expected, "two");
    ensure!(
        forwarded
            .windows(expected.len())
            .any(|bytes| bytes == expected),
        "duplicate user properties lost or reordered"
    );
    subscriber.ack(4, pid).await?;
    subscriber
        .publish("properties/self", b"no local echo", 1, 8, false, false)
        .await?;
    ensure!(subscriber.read().await?.0 == 0x40);
    subscriber.assert_quiet().await?;
    let mut expired = Vec::new();
    string(&mut expired, "properties/value");
    expired.extend_from_slice(&9u16.to_be_bytes());
    expired.extend_from_slice(&[5, 0x02, 0, 0, 0, 0]);
    expired.extend_from_slice(b"expired");
    publisher.send(0x33, &expired).await?;
    ensure!(publisher.read().await?.0 == 0x40);
    subscriber.assert_quiet().await?;
    let mut retained = h.client(5, "expiry-check", true).await?;
    retained.subscribe("properties/value", 0, 1).await?;
    retained.assert_quiet().await?;
    publisher.disconnect().await?;
    subscriber.disconnect().await?;
    retained.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn rejects_anonymous_and_wrong_password_without_opening_a_session() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    for authenticated in [false, true] {
        let io = TcpStream::connect(h.address).await?;
        let mut client = Client {
            io,
            version: 5,
            session_present: false,
        };
        let mut body = Vec::new();
        string(&mut body, "MQTT");
        body.extend_from_slice(&[5, if authenticated { 0xc2 } else { 2 }, 0, 60, 0]);
        string(&mut body, "rejected");
        if authenticated {
            string(&mut body, "wire-test");
            string(&mut body, "wrong-password");
        }
        client.send(0x10, &body).await?;
        let (header, ack) = client.read().await?;
        ensure!(header == 0x20 && ack[1] == 0x86);
        let mut byte = [0];
        ensure!(timeout(Duration::from_secs(2), client.io.read(&mut byte)).await?? == 0);
    }
    ensure!(h.broker.clients().await.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(h.store.pool())
        .await?;
    ensure!(count == 0);
    ensure!(
        h.broker.metrics().handshake_failures == 2
            && h.broker.metrics().connections_authenticated == 0
    );
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn inbound_qos2_quota_rejects_without_losing_prior_transaction() -> Result<()> {
    let Some(h) = Harness::start_with(BrokerConfig {
        max_pending: 1,
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    let mut publisher = h.client(5, "bounded-inbox", false).await?;
    publisher
        .publish("quota/incoming", b"first", 2, 1, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x50 && ack[2] == 0);
    publisher
        .publish("quota/incoming", b"second", 2, 2, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(
        header == 0x50 && ack[2] == 0x97,
        "inbox exhaustion must return negative PUBREC"
    );
    publisher
        .publish("quota/incoming", b"first", 2, 1, false, true)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(
        header == 0x50 && ack[2] == 0,
        "DUP must not consume another quota slot"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM incoming_qos2 WHERE NOT released")
        .fetch_one(h.store.pool())
        .await?;
    ensure!(count == 1);
    publisher.ack(6, 1).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    publisher
        .publish("quota/incoming", b"second", 2, 2, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x50 && ack[2] == 0);
    publisher.ack(6, 2).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    publisher.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn outgoing_quota_negative_puback_and_pubrel_recovery_are_transactional() -> Result<()> {
    let Some(h) = Harness::start_with(BrokerConfig {
        max_pending: 1,
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    let mut subscriber = h.client(5, "bounded-offline", false).await?;
    subscriber.subscribe("quota/outgoing", 2, 1).await?;
    subscriber.disconnect().await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut publisher = h.client(5, "quota-publisher", false).await?;
    publisher
        .publish("quota/outgoing", b"first", 1, 11, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x40 && ack[2] == 0);
    publisher
        .publish("quota/outgoing", b"rejected", 1, 12, false, false)
        .await?;
    let (header, ack) = publisher.read().await?;
    ensure!(header == 0x40 && ack[2] == 0x97);
    publisher.send(0xc0, &[]).await?;
    ensure!(publisher.read().await?.0 == 0xd0);
    publisher
        .publish("quota/outgoing", b"qos2-pending", 2, 21, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x50);
    publisher.ack(6, 21).await?;
    let (header, body) = publisher.read().await?;
    ensure!(
        header == 0xe0 && body[0] == 0x97,
        "fanout exhaustion must disconnect and preserve QoS2 receive state"
    );
    drop(publisher);
    let released: bool =
        sqlx::query_scalar("SELECT released FROM incoming_qos2 WHERE packet_id=21")
            .fetch_one(h.store.pool())
            .await?;
    ensure!(!released);
    let mut subscriber = h.client(5, "bounded-offline", false).await?;
    let (header, body) = subscriber.read().await?;
    let (_, payload, pid) = publication(header, &body, 5)?;
    ensure!(payload == b"first");
    subscriber.ack(4, pid).await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut publisher = h.client(5, "quota-publisher", false).await?;
    ensure!(publisher.session_present);
    publisher.ack(6, 21).await?;
    ensure!(publisher.read().await?.0 == 0x70);
    let (header, body) = subscriber.read().await?;
    let (_, payload, pid) = publication(header, &body, 5)?;
    ensure!(payload == b"qos2-pending");
    subscriber.ack(5, pid).await?;
    ensure!(subscriber.read().await?.0 == 0x62);
    subscriber.ack(7, pid).await?;
    subscriber.assert_quiet().await?;
    publisher.disconnect().await?;
    subscriber.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn peer_maximum_packet_size_discards_large_delivery_without_poisoning_connection()
-> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let io = TcpStream::connect(h.address).await?;
    let mut subscriber = Client {
        io,
        version: 5,
        session_present: false,
    };
    let mut connect = Vec::new();
    string(&mut connect, "MQTT");
    connect.extend_from_slice(&[5, 0xc2, 0, 60, 5, 0x27]);
    connect.extend_from_slice(&64u32.to_be_bytes());
    string(&mut connect, "small-peer");
    string(&mut connect, "wire-test");
    string(&mut connect, "wire-test-password");
    subscriber.send(0x10, &connect).await?;
    let (header, ack) = subscriber.read().await?;
    ensure!(header == 0x20 && ack[1] == 0);
    subscriber.subscribe("size", 1, 1).await?;
    let mut publisher = h.client(5, "large-publisher", true).await?;
    publisher
        .publish("size", &[b'x'; 100], 1, 1, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    subscriber.assert_quiet().await?;
    subscriber.send(0xc0, &[]).await?;
    ensure!(
        subscriber.read().await?.0 == 0xd0,
        "oversize delivery must not disconnect the small peer"
    );
    publisher
        .publish("size", b"small", 1, 2, false, false)
        .await?;
    ensure!(publisher.read().await?.0 == 0x40);
    let (header, body) = subscriber.read().await?;
    let (_, payload, id) = publication(header, &body, 5)?;
    ensure!(payload == b"small");
    subscriber.ack(4, id).await?;
    publisher.disconnect().await?;
    subscriber.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn stale_or_disabled_credentials_cannot_open_a_durable_session() -> Result<()> {
    use clex_mqtt::domain::SessionSpec;
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let credential = h
        .store
        .credential("wire-test")
        .await?
        .context("missing fixture user")?;
    let spec = || SessionSpec {
        client_id: b"stale-handshake".to_vec(),
        principal: "wire-test".into(),
        clean_start: true,
        expiry_interval: 300,
        protocol: 5,
        will: None,
        will_delay_interval: 0,
    };
    sqlx::query("UPDATE mqtt_users SET enabled=false WHERE id=$1")
        .bind(credential.id)
        .execute(h.store.pool())
        .await?;
    ensure!(
        h.store
            .open_session_authenticated(spec(), credential.id, &credential.password_hash)
            .await
            .is_err()
    );
    sqlx::query(
        "UPDATE mqtt_users SET enabled=true,password_hash='rotated-credential' WHERE id=$1",
    )
    .bind(credential.id)
    .execute(h.store.pool())
    .await?;
    ensure!(
        h.store
            .open_session_authenticated(spec(), credential.id, &credential.password_hash)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(h.store.pool())
        .await?;
    ensure!(count == 0);
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn credential_rotation_disconnects_even_without_an_admin_registry_snapshot() -> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut client = h.client(5, "rotated-client", true).await?;
    sqlx::query(
        "UPDATE mqtt_users SET password_hash='rotated-credential' WHERE username='wire-test'",
    )
    .execute(h.store.pool())
    .await?;
    let (header, body) = client.read().await?;
    ensure!(header == 0xe0 && body[0] == 0x87);
    let mut byte = [0];
    ensure!(timeout(Duration::from_secs(2), client.io.read(&mut byte)).await?? == 0);
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn receive_maximum_one_keeps_qos2_slot_until_pubcomp_and_accepts_control_packets()
-> Result<()> {
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let mut subscriber =
        Client::connect_v5_properties(h.address, "receive-one", true, &[0x21, 0, 1]).await?;
    subscriber.subscribe("flow/#", 2, 1).await?;
    let mut publisher = h.client(5, "flow-publisher", true).await?;
    for (id, payload) in [
        (10, b"first".as_slice()),
        (11, b"second".as_slice()),
        (12, b"third".as_slice()),
    ] {
        publisher
            .publish_once_qos2("flow/value", payload, id)
            .await?;
    }
    let (header, body) = subscriber.read().await?;
    let (_, payload, first) = publication(header, &body, 5)?;
    ensure!(header & 6 == 4 && payload == b"first");
    ensure!(header & 8 == 0, "first transmission must use DUP=0");
    let phases: Vec<(String, i64)> =
        sqlx::query_as("SELECT phase,count(*) FROM deliveries GROUP BY phase ORDER BY phase")
            .fetch_all(h.store.pool())
            .await?;
    ensure!(
        phases == [("await_pubrec".into(), 1), ("queued".into(), 2)],
        "reservations must fit the recipient window and commit before PUBLISH"
    );
    subscriber.assert_quiet().await?;
    subscriber.ack(5, first).await?;
    let (header, body) = subscriber.read().await?;
    ensure!(header == 0x62 && body.starts_with(&first.to_be_bytes()));
    subscriber.assert_quiet().await?;
    subscriber.send(0xc0, &[]).await?;
    ensure!(subscriber.read().await? == (0xd0, Vec::new()));
    subscriber.assert_quiet().await?;
    subscriber.ack(7, first).await?;
    let (header, body) = subscriber.read().await?;
    let (_, payload, second) = publication(header, &body, 5)?;
    ensure!(header & 6 == 4 && payload == b"second");
    let mut rejected = second.to_be_bytes().to_vec();
    rejected.push(0x80);
    subscriber.send(0x50, &rejected).await?;
    let (header, body) = subscriber.read().await?;
    let (_, payload, third) = publication(header, &body, 5)?;
    ensure!(
        header & 6 == 4 && payload == b"third",
        "negative PUBREC must release the send quota"
    );
    subscriber.ack(5, third).await?;
    ensure!(subscriber.read().await?.0 == 0x62);
    subscriber.ack(7, third).await?;
    publisher.disconnect().await?;
    subscriber.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn topic_alias_mapping_reassigns_resets_and_reports_specific_errors() -> Result<()> {
    let Some(h) = Harness::start_with(BrokerConfig {
        topic_alias_maximum: 2,
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    let mut subscriber = h.client(5, "alias-reader", true).await?;
    subscriber.subscribe("aliases/#", 1, 1).await?;
    let mut publisher = h.client(5, "alias-writer", false).await?;
    for (pid, topic, expected, payload) in [
        (1, "aliases/first", "aliases/first", b"one".as_slice()),
        (2, "", "aliases/first", b"two".as_slice()),
        (3, "aliases/second", "aliases/second", b"three".as_slice()),
        (4, "", "aliases/second", b"four".as_slice()),
    ] {
        publisher
            .publish_v5_properties(topic, payload, pid, &[0x23, 0, 1])
            .await?;
        ensure!(publisher.read().await?.0 == 0x40);
        let (header, body) = subscriber.read().await?;
        let (actual, data, id) = publication(header, &body, 5)?;
        ensure!(actual == expected && data == payload);
        subscriber.ack(4, id).await?;
    }
    publisher.disconnect().await?;
    let mut resumed = h.client(5, "alias-writer", false).await?;
    ensure!(resumed.session_present);
    resumed
        .publish_v5_properties("", b"not mapped", 5, &[0x23, 0, 1])
        .await?;
    resumed.assert_disconnect(0x82).await?;
    for (name, topic, alias, reason) in [
        ("alias-zero", "aliases/invalid", 0u16, 0x94),
        ("alias-range", "aliases/invalid", 3, 0x94),
        ("alias-unknown", "", 2, 0x82),
    ] {
        let mut client = h.client(5, name, true).await?;
        let bytes = alias.to_be_bytes();
        client
            .publish_v5_properties(topic, b"invalid", 1, &[0x23, bytes[0], bytes[1]])
            .await?;
        client.assert_disconnect(reason).await?;
    }
    subscriber.assert_quiet().await?;
    subscriber.disconnect().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn maximum_packet_size_accepts_exact_boundary_and_rejects_oversize_header() -> Result<()> {
    let Some(h) = Harness::start_with(BrokerConfig {
        max_packet_size: 128,
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    let mut client = h.client(5, "size-boundary", true).await?;
    client
        .publish("a", &[b'x'; 120], 1, 1, false, false)
        .await?;
    let (header, ack) = client.read().await?;
    ensure!(
        header == 0x40
            && ack.starts_with(&1u16.to_be_bytes())
            && ack.get(2).copied().unwrap_or(0) < 0x80,
        "128-byte PUBLISH was refused"
    );
    // 2-byte header plus 127 remaining bytes = 129. No body is sent.
    client.io.write_all(&[0x30, 0x7f]).await?;
    client.assert_disconnect(0x95).await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn mqtt5_property_errors_distinguish_singletons_context_and_payload_structure() -> Result<()>
{
    let Some(h) = Harness::start().await? else {
        return Ok(());
    };
    let cases: [(&str, u8, Vec<u8>, u8); 9] = [
        (
            "truncated-byte-property",
            0x30,
            vec![0, 1, b'a', 1, 1, b'x'],
            0x81,
        ),
        (
            "duplicate-content",
            0x32,
            {
                let mut body = vec![0, 1, b'a', 0, 1];
                property_section(&mut body, &[3, 0, 1, b'x', 3, 0, 1, b'y']);
                body
            },
            0x82,
        ),
        (
            "duplicate-expiry",
            0x32,
            {
                let mut body = vec![0, 1, b'a', 0, 1];
                property_section(&mut body, &[2, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
                body
            },
            0x82,
        ),
        (
            "duplicate-alias",
            0x32,
            {
                let mut body = vec![0, 1, b'a', 0, 1];
                property_section(&mut body, &[0x23, 0, 1, 0x23, 0, 1]);
                body
            },
            0x82,
        ),
        (
            "wrong-context",
            0x32,
            {
                let mut body = vec![0, 1, b'a', 0, 1];
                property_section(&mut body, &[0x18, 0, 0, 0, 1]);
                body
            },
            0x81,
        ),
        (
            "client-sub-id",
            0x32,
            {
                let mut body = vec![0, 1, b'a', 0, 1];
                property_section(&mut body, &[0x0b, 1]);
                body
            },
            0x82,
        ),
        (
            "duplicate-ack-reason",
            0x40,
            {
                let mut body = vec![0, 33, 0];
                property_section(&mut body, &[0x1f, 0, 1, b'x', 0x1f, 0, 1, b'y']);
                body
            },
            0x82,
        ),
        (
            "duplicate-sub-id",
            0x82,
            {
                let mut body = vec![0, 1];
                property_section(&mut body, &[0x0b, 1, 0x0b, 2]);
                string(&mut body, "a");
                body.push(1);
                body
            },
            0x82,
        ),
        (
            "duplicate-disconnect",
            0xe0,
            {
                let mut body = vec![0];
                property_section(&mut body, &[0x11, 0, 0, 0, 0, 0x11, 0, 0, 0, 0]);
                body
            },
            0x82,
        ),
    ];
    for (name, header, body, reason) in cases {
        let mut client = h.client(5, name, true).await?;
        client.send(header, &body).await?;
        client.assert_disconnect(reason).await?;
    }
    let mut valid = h.client(5, "repeatable-users", true).await?;
    let mut ack = vec![0, 1, 0];
    property_section(
        &mut ack,
        &[0x26, 0, 1, b'k', 0, 1, b'v', 0x26, 0, 1, b'k', 0, 1, b'w'],
    );
    valid.send(0x40, &ack).await?;
    valid.send(0xc0, &[]).await?;
    ensure!(valid.read().await? == (0xd0, Vec::new()));
    valid.disconnect().await?;
    h.close().await
}
