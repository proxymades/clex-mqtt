//! Real-process lifecycle checks, isolated by database schema and application name.
//! No database server or unrelated process is stopped by this test.
#![cfg(unix)]

use std::{net::SocketAddr, process::Stdio, str::FromStr, time::Duration};

use anyhow::{Context, Result, ensure};
use clex_mqtt::{auth, migrations};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};
use url::Url;
use uuid::Uuid;
#[path = "support/qos0_lifecycle.rs"]
mod qos0_lifecycle;

const INSTANCE_LOCK: i64 = 0x434c45584d515454;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const WIRE_TIMEOUT: Duration = Duration::from_secs(5);
const TOPIC: &str = "lifecycle/durable";
const CLIENT_ID: &str = "lifecycle-persistent-subscriber";
const QOS2_PUBLISHER_ID: &str = "lifecycle-qos2-publisher";
const MQTT_USER: &str = "lifecycle-test-user";
const MQTT_PASSWORD: &str = "lifecycle-test-password";

struct Harness {
    root: PgPool,
    pool: PgPool,
    database_url: Url,
    schema: String,
    application_prefix: String,
    children: Vec<Child>,
    http: reqwest::Client,
}

impl Harness {
    async fn new() -> Result<Self> {
        let database_url = std::env::var("TEST_DATABASE_URL")
            .context("Lifecycle tests require an isolated TEST_DATABASE_URL")?;
        let options = PgConnectOptions::from_str(&database_url)
            .map_err(|_| anyhow::anyhow!("Invalid lifecycle test database configuration"))?;
        ensure!(
            options
                .get_database()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("test"),
            "Lifecycle checks require a dedicated database with 'test' in its name"
        );
        let root = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await?;
        let identifier = Uuid::new_v4().simple().to_string();
        let schema = format!("lifecycle_{identifier}");
        let application_prefix = format!("clex_lifecycle_{identifier}");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&root)
            .await?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(
                options
                    .options([("search_path", schema.as_str())])
                    .application_name(&format!("{application_prefix}_control")),
            )
            .await?;
        if let Err(error) = migrations::apply(&pool).await {
            pool.close().await;
            let _ = sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
                .execute(&root)
                .await;
            return Err(error);
        }
        let hash = auth::hash_password(MQTT_PASSWORD)?;
        sqlx::query("INSERT INTO mqtt_users (id,username,password_hash,enabled,superuser) VALUES($1,$2,$3,true,true)")
            .bind(Uuid::new_v4()).bind(MQTT_USER).bind(hash).execute(&pool).await?;
        let mut database_url = Url::parse(&database_url)
            .map_err(|_| anyhow::anyhow!("Invalid lifecycle test database URL"))?;
        // Appending a SQLx options entry avoids putting credentials in argv or logs.
        database_url
            .query_pairs_mut()
            .append_pair("options[search_path]", &schema);
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(500))
            .build()?;
        Ok(Self {
            root,
            pool,
            database_url,
            schema,
            application_prefix,
            children: vec![],
            http,
        })
    }

    fn application_name(&self, label: &str) -> String {
        format!("{}_{}", self.application_prefix, label)
    }

    fn spawn(&mut self, label: &str, mqtt: SocketAddr, http: SocketAddr) -> Result<usize> {
        self.spawn_with_queue_bytes(label, mqtt, http, 1048576)
    }

    fn spawn_with_queue_bytes(
        &mut self,
        label: &str,
        mqtt: SocketAddr,
        http: SocketAddr,
        queue_bytes: u32,
    ) -> Result<usize> {
        let mut database_url = self.database_url.clone();
        database_url
            .query_pairs_mut()
            .append_pair("application_name", &self.application_name(label));
        let child = Command::new(env!("CARGO_BIN_EXE_clex-mqtt"))
            .arg("serve")
            .env_clear()
            // The application calls dotenvy::dotenv(). Using / prevents discovery
            // of the repository's private .env through an ancestor directory.
            .current_dir("/")
            .env("DATABASE_URL", database_url.as_str())
            .env("CLEX_ENV", "test")
            .env("CLEX_MQTT_ADDR", mqtt.to_string())
            .env("CLEX_HTTP_ADDR", http.to_string())
            .env("CLEX_PUBLIC_URL", format!("http://{http}"))
            .env("CLEX_COOKIE_SECURE", "false")
            .env("CLEX_DB_MAX_CONNECTIONS", "6")
            .env("CLEX_MAX_PACKET_BYTES", "65536")
            .env("CLEX_MAX_CONNECTIONS", "16")
            .env("CLEX_MAX_INFLIGHT", "8")
            .env("CLEX_MAX_PENDING", "512")
            .env("CLEX_MAX_PENDING_BYTES", queue_bytes.to_string())
            .env("CLEX_SHUTDOWN_TIMEOUT_SECS", "5")
            .env("RUST_LOG", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Could not spawn the broker test process")?;
        self.children.push(child);
        Ok(self.children.len() - 1)
    }

    async fn wait_ready(&mut self, index: usize, address: SocketAddr) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            ensure!(
                self.children[index].try_wait()?.is_none(),
                "Broker exited before readiness"
            );
            if self
                .http
                .get(format!("http://{address}/health/ready"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Broker startup exceeded lifecycle test deadline"
            );
            sleep(Duration::from_millis(40)).await;
        }
    }

    async fn healthy(&self, address: SocketAddr) -> Result<()> {
        let response = self
            .http
            .get(format!("http://{address}/health/ready"))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "First broker lost readiness after duplicate startup"
        );
        Ok(())
    }

    async fn offline(&self) -> Result<()> {
        let deadline = Instant::now() + WIRE_TIMEOUT;
        loop {
            let offline: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE client_id=$1 AND NOT online)",
            )
            .bind(CLIENT_ID.as_bytes())
            .fetch_one(&self.pool)
            .await?;
            if offline {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Persistent subscriber did not become offline"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn queue_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM deliveries d JOIN sessions s ON s.id=d.session_id WHERE s.client_id=$1")
            .bind(CLIENT_ID.as_bytes()).fetch_one(&self.pool).await?)
    }

    async fn wait_queue_empty(&self) -> Result<()> {
        let deadline = Instant::now() + WIRE_TIMEOUT;
        loop {
            if self.queue_count().await? == 0 {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Delivery acknowledgement was not persisted"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn kill_child(&mut self, index: usize) -> Result<()> {
        self.children[index].start_kill()?;
        timeout(STARTUP_TIMEOUT, self.children[index].wait())
            .await
            .context("Killed test broker did not exit")??;
        Ok(())
    }

    async fn terminate_only_own_instance_connection(&self, label: &str) -> Result<bool> {
        let application = self.application_name(label);
        // Match the exact 64-bit instance key and this test child's unique name.
        // Never enumerate or terminate other application/database connections.
        let pids:Vec<i32>=sqlx::query_scalar("SELECT DISTINCT a.pid FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid WHERE a.datname=current_database() AND a.application_name=$1 AND a.usename=current_user AND a.pid<>pg_backend_pid() AND l.locktype='advisory' AND l.granted AND l.classid::bigint=$2 AND l.objid::bigint=$3 AND l.objsubid=1")
            .bind(&application).bind(INSTANCE_LOCK>>32).bind(INSTANCE_LOCK&0xffff_ffff)
            .fetch_all(&self.root).await?;
        ensure!(
            pids.len() == 1,
            "Expected exactly one own broker instance-lock connection"
        );
        match sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
            .bind(pids[0])
            .fetch_one(&self.root)
            .await
        {
            Ok(terminated) => {
                ensure!(terminated, "Own instance-lock backend was not terminated");
                Ok(true)
            }
            Err(error)
                if error
                    .as_database_error()
                    .is_some_and(|db| db.code().as_deref() == Some("42501")) =>
            {
                eprintln!(
                    "Instance-lock termination probe skipped: the test role lacks permission"
                );
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn exercise(&mut self) -> Result<()> {
        let (mqtt, http) = free_addresses()?;
        let first = self.spawn("first", mqtt, http)?;
        self.wait_ready(first, http).await?;
        let mut subscriber = Client::connect(mqtt, CLIENT_ID, false).await?;
        ensure!(
            !subscriber.session_present,
            "New subscriber unexpectedly resumed an existing session"
        );
        subscriber.subscribe(TOPIC).await?;

        // Other free listener addresses rule out an address-in-use rejection.
        let (duplicate_mqtt, duplicate_http) = free_addresses()?;
        let duplicate = self.spawn("duplicate", duplicate_mqtt, duplicate_http)?;
        let duplicate_status = timeout(STARTUP_TIMEOUT, self.children[duplicate].wait())
            .await
            .context("Second broker did not reject duplicate ownership")??;
        ensure!(
            !duplicate_status.success(),
            "A second broker acquired the same database"
        );
        self.healthy(http).await?;
        ensure!(
            self.children[first].try_wait()?.is_none(),
            "Duplicate startup stopped the first broker"
        );
        subscriber.send(0xc0, &[]).await?;
        let (header, response) = subscriber.read().await?;
        ensure!(
            header == 0xd0 && response.is_empty(),
            "Duplicate startup disturbed the first broker's live MQTT session"
        );
        subscriber.send(0xe0, &[]).await?;
        drop(subscriber);
        self.offline().await?;
        let mut publisher = Client::connect(mqtt, "lifecycle-publisher", true).await?;
        let expected_payload = b"durable\x00binary\xffpayload";
        for index in 0..128_u16 {
            let payload = [expected_payload.as_slice(), &index.to_be_bytes()].concat();
            publisher.publish(TOPIC, &payload, 42 + index).await?;
            publisher.read_ack(0x40, 42 + index).await?;
        }
        ensure!(
            self.queue_count().await? == 128,
            "Accepted backlog is missing from durable storage"
        );
        self.kill_child(first).await?;
        drop(publisher);

        let restarted = self.spawn("restarted", mqtt, http)?;
        self.wait_ready(restarted, http).await?;
        let mut resumed = Client::connect(mqtt, CLIENT_ID, false).await?;
        ensure!(
            resumed.session_present,
            "Broker restart lost the persistent session"
        );
        // Recover a backlog larger than a payload batch, then break the socket
        // with a full unacknowledged window. The next generation must reuse all
        // reserved Packet Identifiers before progressing through the backlog.
        let mut reserved = Vec::new();
        for index in 0..8_u16 {
            let payload = [expected_payload.as_slice(), &index.to_be_bytes()].concat();
            reserved.push(resumed.read_publication(TOPIC, &payload, 1).await?);
        }
        drop(resumed);
        self.offline().await?;
        let mut resumed = Client::connect(mqtt, CLIENT_ID, false).await?;
        ensure!(
            resumed.session_present,
            "Socket recovery lost the durable session"
        );
        for index in 0..128_u16 {
            let payload = [expected_payload.as_slice(), &index.to_be_bytes()].concat();
            let packet_id = resumed.read_publication(TOPIC, &payload, 1).await?;
            if let Some(previous) = reserved.get(usize::from(index)) {
                ensure!(
                    packet_id == *previous,
                    "Reconnect changed an outstanding Packet Identifier"
                );
            }
            resumed.send(0x40, &packet_id.to_be_bytes()).await?;
        }
        self.wait_queue_empty().await?;
        drop(resumed);

        let restarted = self.qos0_crash_recovery(restarted, mqtt, http).await?;
        let restarted = self.qos2_backlog_recovery(restarted, mqtt, http).await?;
        let qos2_restarted = self.qos2_crash_recovery(restarted, mqtt, http).await?;
        let graceful = if self
            .terminate_only_own_instance_connection("qos2_out")
            .await?
        {
            let status = timeout(STARTUP_TIMEOUT, self.children[qos2_restarted].wait())
                .await
                .context("Broker did not stop after losing its instance-lock connection")??;
            ensure!(
                !status.success(),
                "Instance ownership loss must produce a failed process exit"
            );
            let final_child = self.spawn("graceful", mqtt, http)?;
            self.wait_ready(final_child, http).await?;
            final_child
        } else {
            qos2_restarted
        };
        let pid = self.children[graceful]
            .id()
            .context("Graceful-shutdown child is no longer running")?;
        let signal = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        ensure!(
            signal.success(),
            "Could not send SIGTERM to the owned test broker"
        );
        let status = timeout(Duration::from_secs(12), self.children[graceful].wait())
            .await
            .context("Broker exceeded graceful SIGTERM deadline")??;
        ensure!(
            status.success(),
            "Broker returned a failure after graceful SIGTERM"
        );
        self.forced_shutdown_with_blocked_maintenance(mqtt, http)
            .await?;
        Ok(())
    }

    async fn forced_shutdown_with_blocked_maintenance(
        &mut self,
        mqtt: SocketAddr,
        http: SocketAddr,
    ) -> Result<()> {
        let index = self.spawn("forced_shutdown", mqtt, http)?;
        self.wait_ready(index, http).await?;
        let mut lock = self.pool.begin().await?;
        sqlx::query("LOCK TABLE sessions IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await?;
        let application = self.application_name("forced_shutdown");
        let outcome: Result<()> = async {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.application_name=$1 AND l.relation=to_regclass($2) AND NOT l.granted)")
                    .bind(&application).bind(format!("{}.sessions", self.schema)).fetch_one(&self.pool).await?;
                if blocked { break; }
                ensure!(Instant::now() < deadline, "owned broker maintenance did not reach the test lock");
                sleep(Duration::from_millis(20)).await;
            }
            let pid = self.children[index].id().context("forced-shutdown child exited early")?;
            let signal = Command::new("/bin/kill").arg("-TERM").arg(pid.to_string())
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await?;
            ensure!(signal.success(), "could not signal the owned forced-shutdown child");
            let status = timeout(Duration::from_secs(8), self.children[index].wait()).await
                .context("broker exceeded the configured five-second shutdown deadline")??;
            ensure!(!status.success(), "blocked maintenance must produce a failed deadline exit");
            Ok(())
        }.await;
        lock.rollback().await?;
        outcome?;
        // Wait only for this child's PostgreSQL backends before schema cleanup.
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1",
            )
            .bind(&application)
            .fetch_one(&self.pool)
            .await?;
            if count == 0 {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "exited broker retained a PostgreSQL backend"
            );
            sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn qos2_backlog_recovery(
        &mut self,
        running: usize,
        mqtt: SocketAddr,
        http: SocketAddr,
    ) -> Result<usize> {
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        subscriber.subscribe_qos(TOPIC, 2).await?;
        subscriber.send(0xe0, &[]).await?;
        drop(subscriber);
        self.offline().await?;
        let mut publisher = Client::connect_v5(mqtt, "backlog-qos2-source", true).await?;
        for index in 0..128_u16 {
            publisher
                .publish_qos(TOPIC, &index.to_be_bytes(), index + 1, 2)
                .await?;
            publisher.read_ack(0x50, index + 1).await?;
            publisher.send(0x62, &(index + 1).to_be_bytes()).await?;
            publisher.read_ack(0x70, index + 1).await?;
        }
        ensure!(
            self.queue_count().await? == 128,
            "QoS2 backlog was not committed"
        );
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        let mut ids = Vec::new();
        for index in 0..8_u16 {
            ids.push(
                subscriber
                    .read_publication(TOPIC, &index.to_be_bytes(), 2)
                    .await?,
            );
        }
        subscriber.send_ack_batch(0x50, &ids).await?;
        for id in &ids {
            subscriber.read_ack(0x62, *id).await?;
        }
        let persisted: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries d JOIN sessions s ON s.id=d.session_id WHERE s.client_id=$1 AND d.phase='await_pubcomp'")
            .bind(CLIENT_ID.as_bytes()).fetch_one(&self.pool).await?;
        ensure!(
            persisted == 8,
            "PUBREL batch preceded its durable phase updates"
        );
        self.kill_child(running).await?;
        drop(subscriber);
        drop(publisher);

        let restarted = self.spawn("batch_qos2", mqtt, http)?;
        self.wait_ready(restarted, http).await?;
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        ensure!(
            subscriber.session_present,
            "QoS2 backlog session was lost on restart"
        );
        // The client already delivered these eight payloads before the crash.
        // Resume at PUBREL, preserving IDs and suppressing application replay.
        for id in &ids {
            subscriber.read_ack(0x62, *id).await?;
        }
        subscriber.send_ack_batch(0x70, &ids).await?;
        for start in (8..128_u16).step_by(8) {
            let mut ids = Vec::new();
            for index in start..start + 8 {
                ids.push(
                    subscriber
                        .read_publication(TOPIC, &index.to_be_bytes(), 2)
                        .await?,
                );
            }
            subscriber.send_ack_batch(0x50, &ids).await?;
            for id in &ids {
                subscriber.read_ack(0x62, *id).await?;
            }
            subscriber.send_ack_batch(0x70, &ids).await?;
        }
        self.wait_queue_empty().await?;
        Ok(restarted)
    }

    async fn qos2_crash_recovery(
        &mut self,
        running: usize,
        mqtt: SocketAddr,
        http: SocketAddr,
    ) -> Result<usize> {
        const INCOMING_PACKET_ID: u16 = 9000;
        const SHARED_FILTER: &str = "$share/lifecycle-workers/lifecycle/durable";
        let payload = b"qos2\x00crash\xffrecovery";
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        ensure!(
            subscriber.session_present,
            "MQTT5 reconnect lost the subscriber's existing session"
        );
        subscriber.unsubscribe(TOPIC).await?;
        subscriber.subscribe_qos(SHARED_FILTER, 2).await?;
        let mut publisher = Client::connect_v5(mqtt, QOS2_PUBLISHER_ID, false).await?;
        ensure!(
            !publisher.session_present,
            "New QoS2 publisher unexpectedly resumed a session"
        );
        publisher
            .publish_qos(TOPIC, payload, INCOMING_PACKET_ID, 2)
            .await?;
        publisher.read_ack(0x50, INCOMING_PACKET_ID).await?;
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incoming_qos2 i JOIN sessions s ON s.id=i.session_id WHERE s.client_id=$1 AND i.packet_id=$2 AND NOT i.released",
        )
        .bind(QOS2_PUBLISHER_ID.as_bytes())
        .bind(i32::from(INCOMING_PACKET_ID))
        .fetch_one(&self.pool)
        .await?;
        ensure!(pending == 1, "PUBREC preceded durable incoming QoS2 state");
        ensure!(
            self.queue_count().await? == 0,
            "Incoming QoS2 was routed before PUBREL"
        );
        self.kill_child(running).await?;
        drop(subscriber);
        drop(publisher);

        let incoming_restart = self.spawn("qos2_in", mqtt, http)?;
        self.wait_ready(incoming_restart, http).await?;
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        let mut publisher = Client::connect_v5(mqtt, QOS2_PUBLISHER_ID, false).await?;
        ensure!(
            subscriber.session_present && publisher.session_present,
            "SIGKILL before incoming PUBREL lost a persistent session"
        );
        publisher
            .send(0x62, &INCOMING_PACKET_ID.to_be_bytes())
            .await?;
        publisher.read_ack(0x70, INCOMING_PACKET_ID).await?;
        let outgoing_packet_id = subscriber.read_publication(TOPIC, payload, 2).await?;
        // Repeating the same persisted PUBREL must only repeat PUBCOMP.
        publisher
            .send(0x62, &INCOMING_PACKET_ID.to_be_bytes())
            .await?;
        publisher.read_ack(0x70, INCOMING_PACKET_ID).await?;
        ensure!(
            self.queue_count().await? == 1,
            "Duplicate incoming PUBREL created another fanout delivery"
        );
        subscriber
            .send(0x50, &outgoing_packet_id.to_be_bytes())
            .await?;
        subscriber.read_ack(0x62, outgoing_packet_id).await?;
        let durable_pubrel: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deliveries d JOIN sessions s ON s.id=d.session_id WHERE s.client_id=$1 AND d.packet_id=$2 AND d.phase='await_pubcomp' AND d.shared_filter=$3)",
        )
        .bind(CLIENT_ID.as_bytes())
        .bind(i32::from(outgoing_packet_id))
        .bind(SHARED_FILTER)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            durable_pubrel,
            "PUBREL preceded durable outgoing QoS2 state"
        );
        self.kill_child(incoming_restart).await?;
        drop(subscriber);
        drop(publisher);

        let outgoing_restart = self.spawn("qos2_out", mqtt, http)?;
        self.wait_ready(outgoing_restart, http).await?;
        let mut subscriber = Client::connect_v5(mqtt, CLIENT_ID, false).await?;
        ensure!(
            subscriber.session_present,
            "SIGKILL before outgoing PUBCOMP lost the subscriber session"
        );
        // Recovery must resume at PUBREL with the original packet identifier,
        // without replaying the payload through a fresh PUBLISH exchange.
        subscriber.read_ack(0x62, outgoing_packet_id).await?;
        subscriber
            .send(0x70, &outgoing_packet_id.to_be_bytes())
            .await?;
        self.wait_queue_empty().await?;
        let mut publisher = Client::connect_v5(mqtt, QOS2_PUBLISHER_ID, false).await?;
        ensure!(
            publisher.session_present,
            "Second SIGKILL lost the completed incoming QoS2 session"
        );
        publisher
            .send(0x62, &INCOMING_PACKET_ID.to_be_bytes())
            .await?;
        publisher.read_ack(0x70, INCOMING_PACKET_ID).await?;
        ensure!(
            self.queue_count().await? == 0,
            "Restart lost the incoming QoS2 release marker and repeated fanout"
        );
        Ok(outgoing_restart)
    }

    async fn close(mut self) -> Result<()> {
        for child in &mut self.children {
            if child.try_wait()?.is_none() {
                let _ = child.start_kill();
                let _ = timeout(STARTUP_TIMEOUT, child.wait()).await;
            }
        }
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.root)
            .await?;
        self.root.close().await;
        Ok(())
    }
}

fn free_addresses() -> Result<(SocketAddr, SocketAddr)> {
    let mqtt = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok((mqtt.local_addr()?, http.local_addr()?))
}

struct Client {
    io: TcpStream,
    session_present: bool,
    version: u8,
}

impl Client {
    async fn connect(address: SocketAddr, id: &str, clean: bool) -> Result<Self> {
        Self::connect_version(address, id, clean, 4).await
    }

    async fn connect_v5(address: SocketAddr, id: &str, clean: bool) -> Result<Self> {
        Self::connect_version(address, id, clean, 5).await
    }

    async fn connect_version(
        address: SocketAddr,
        id: &str,
        clean: bool,
        version: u8,
    ) -> Result<Self> {
        Self::connect_version_with_buffer(address, id, clean, version, None).await
    }

    async fn connect_version_with_buffer(
        address: SocketAddr,
        id: &str,
        clean: bool,
        version: u8,
        receive_bytes: Option<u32>,
    ) -> Result<Self> {
        let io = if let Some(bytes) = receive_bytes {
            let socket = tokio::net::TcpSocket::new_v4()?;
            socket.set_recv_buffer_size(bytes)?;
            timeout(WIRE_TIMEOUT, socket.connect(address)).await??
        } else {
            timeout(WIRE_TIMEOUT, TcpStream::connect(address)).await??
        };
        let mut body = Vec::new();
        string(&mut body, "MQTT");
        body.push(version);
        body.push(0xc0 | if clean { 2 } else { 0 });
        body.extend_from_slice(&60_u16.to_be_bytes());
        if version == 5 {
            // Session Expiry Interval remains nonzero across process crashes.
            body.extend_from_slice(&[5, 0x11]);
            body.extend_from_slice(&300_u32.to_be_bytes());
        }
        string(&mut body, id);
        string(&mut body, MQTT_USER);
        string(&mut body, MQTT_PASSWORD);
        let mut client = Self {
            io,
            session_present: false,
            version,
        };
        client.send(0x10, &body).await?;
        let (header, ack) = client.read().await?;
        ensure!(
            header == 0x20 && ack.len() >= 2 && (version == 5 || ack.len() == 2) && ack[1] == 0,
            "Lifecycle MQTT CONNECT was rejected"
        );
        client.session_present = ack[0] & 1 != 0;
        Ok(client)
    }

    async fn send(&mut self, header: u8, body: &[u8]) -> Result<()> {
        let mut frame = vec![header];
        let mut length = body.len();
        loop {
            let mut byte = (length % 128) as u8;
            length /= 128;
            if length > 0 {
                byte |= 128;
            }
            frame.push(byte);
            if length == 0 {
                break;
            }
        }
        frame.extend_from_slice(body);
        timeout(WIRE_TIMEOUT, self.io.write_all(&frame)).await??;
        Ok(())
    }

    async fn read(&mut self) -> Result<(u8, Vec<u8>)> {
        timeout(WIRE_TIMEOUT, async {
            let header = self.io.read_u8().await?;
            let mut remaining = 0_usize;
            let mut multiplier = 1;
            for _ in 0..4 {
                let byte = self.io.read_u8().await?;
                remaining += usize::from(byte & 127) * multiplier;
                ensure!(
                    remaining <= 65536,
                    "Lifecycle MQTT response exceeds test packet limit"
                );
                if byte & 128 == 0 {
                    let mut body = vec![0; remaining];
                    self.io.read_exact(&mut body).await?;
                    return Ok((header, body));
                }
                multiplier *= 128;
            }
            anyhow::bail!("Invalid MQTT response length")
        })
        .await
        .context("Lifecycle MQTT response timeout")?
    }

    async fn subscribe(&mut self, topic: &str) -> Result<()> {
        self.subscribe_qos(topic, 1).await
    }

    async fn subscribe_qos(&mut self, topic: &str, qos: u8) -> Result<()> {
        let mut body = 1_u16.to_be_bytes().to_vec();
        if self.version == 5 {
            body.push(0);
        }
        string(&mut body, topic);
        body.push(qos);
        self.send(0x82, &body).await?;
        let (header, ack) = self.read().await?;
        ensure!(
            header == 0x90 && ack.len() >= 3 && ack[..2] == [0, 1] && ack.last() == Some(&qos),
            "Lifecycle subscription was refused"
        );
        Ok(())
    }

    async fn publish(&mut self, topic: &str, payload: &[u8], packet_id: u16) -> Result<()> {
        self.publish_qos(topic, payload, packet_id, 1).await
    }

    async fn unsubscribe(&mut self, filter: &str) -> Result<()> {
        let mut body = 2_u16.to_be_bytes().to_vec();
        if self.version == 5 {
            body.push(0);
        }
        string(&mut body, filter);
        self.send(0xa2, &body).await?;
        let (header, ack) = self.read().await?;
        let expected = if self.version == 5 {
            vec![0, 2, 0, 0]
        } else {
            vec![0, 2]
        };
        ensure!(
            header == 0xb0 && ack == expected,
            "Lifecycle unsubscribe was refused"
        );
        Ok(())
    }

    async fn publish_qos(
        &mut self,
        topic: &str,
        payload: &[u8],
        packet_id: u16,
        qos: u8,
    ) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, topic);
        body.extend_from_slice(&packet_id.to_be_bytes());
        if self.version == 5 {
            body.push(0);
        }
        body.extend_from_slice(payload);
        self.send(0x30 | (qos << 1), &body).await
    }

    async fn send_ack_batch(&mut self, header: u8, ids: &[u16]) -> Result<()> {
        let mut frames = Vec::with_capacity(ids.len() * 4);
        for id in ids {
            frames.extend_from_slice(&[header, 2]);
            frames.extend_from_slice(&id.to_be_bytes());
        }
        timeout(WIRE_TIMEOUT, self.io.write_all(&frames)).await??;
        Ok(())
    }

    async fn read_ack(&mut self, expected_header: u8, packet_id: u16) -> Result<()> {
        let (header, body) = self.read().await?;
        ensure!(
            header == expected_header && body.len() >= 2,
            "Unexpected packet while resuming the QoS2 handshake"
        );
        ensure!(
            body[..2] == packet_id.to_be_bytes(),
            "QoS2 recovery changed the packet identifier"
        );
        ensure!(
            body.get(2).is_none_or(|reason| *reason < 0x80),
            "QoS2 recovery returned an unsuccessful reason code"
        );
        Ok(())
    }

    async fn read_publication(
        &mut self,
        expected_topic: &str,
        expected_payload: &[u8],
        qos: u8,
    ) -> Result<u16> {
        let (header, body) = self.read().await?;
        ensure!(
            header & 0xf0 == 0x30 && (header >> 1) & 3 == qos,
            "QoS2 recovery did not deliver the expected PUBLISH"
        );
        ensure!(body.len() >= 4, "QoS2 PUBLISH is truncated");
        let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
        ensure!(body.len() >= topic_len + 4, "QoS2 topic is truncated");
        ensure!(
            &body[2..2 + topic_len] == expected_topic.as_bytes(),
            "QoS2 recovery changed the publication topic"
        );
        let packet_id = if qos == 0 {
            0
        } else {
            u16::from_be_bytes([body[2 + topic_len], body[3 + topic_len]])
        };
        ensure!(qos != 0 || header & 8 == 0, "QoS0 PUBLISH must use DUP=0");
        let mut payload_offset = 2 + topic_len + if qos == 0 { 0 } else { 2 };
        if self.version == 5 {
            let mut properties_len = 0_usize;
            let mut multiplier = 1;
            let mut complete = false;
            for _ in 0..4 {
                let byte = *body
                    .get(payload_offset)
                    .context("QoS2 properties are truncated")?;
                payload_offset += 1;
                properties_len += usize::from(byte & 127) * multiplier;
                if byte & 128 == 0 {
                    complete = true;
                    break;
                }
                multiplier *= 128;
            }
            ensure!(complete, "QoS2 properties length is invalid");
            payload_offset += properties_len;
        }
        ensure!(
            (qos == 0 || packet_id != 0) && body.get(payload_offset..) == Some(expected_payload),
            "QoS2 recovery changed the publication payload or packet identifier"
        );
        Ok(packet_id)
    }
}

fn string(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buffer.extend_from_slice(value.as_bytes());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL and the real broker binary"]
async fn binary_singleton_crash_recovery_watchdog_and_graceful_shutdown() -> Result<()> {
    let mut harness = Harness::new().await?;
    let outcome = harness.exercise().await;
    let cleanup = harness.close().await;
    outcome?;
    cleanup
}
