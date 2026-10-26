//! MQTT connection tasks own their protocol state. A bounded actor owns the live
//! connection registry; durable routing and QoS state live in PostgreSQL.
mod authentication;
mod delivery_ack;
use delivery_ack::OutgoingAck;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    num::NonZeroU16,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures_util::{FutureExt, SinkExt, StreamExt};
use rmqtt_codec::{MqttPacket, types::QoS, v3, v5};
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Semaphore, mpsc, oneshot},
    time::{Instant, timeout},
};
use tokio_util::{codec::Framed, sync::CancellationToken};
use uuid::Uuid;

use crate::{
    domain::{
        Delivery, DeliveryPhase, Publication, PublicationProperties, SessionSpec, Subscription,
        parse_subscription_filter,
    },
    protocol::{self, BrokerCodec},
    store::{QuotaExceeded, Store},
};

type Wire<T> = Framed<T, BrokerCodec>;

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub max_packet_size: usize,
    pub max_connections: usize,
    pub max_inflight: u16,
    pub max_pending: usize,
    pub max_pending_bytes: usize,
    pub handshake_timeout: Duration,
    pub poll_interval: Duration,
    pub write_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub topic_alias_maximum: u16,
    pub auth_concurrency: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            max_packet_size: 1_048_576,
            max_connections: 1000,
            max_inflight: 32,
            max_pending: 10_000,
            max_pending_bytes: 67_108_864,
            handshake_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_secs(2),
            write_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            topic_alias_maximum: 32,
            auth_concurrency: 8,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientSnapshot {
    pub client_id: String,
    pub username: String,
    pub peer: SocketAddr,
    pub protocol: u8,
    pub keep_alive: u16,
    pub certificate_fingerprint: Option<String>,
    pub session_id: Uuid,
    pub connected_at: DateTime<Utc>,
    pub last_packet_at: DateTime<Utc>,
    pub messages_received: u64,
    pub messages_sent: u64,
}

#[derive(Default, Debug, Serialize)]
pub struct BrokerMetrics {
    pub connections_authenticated: u64,
    pub handshake_failures: u64,
    pub capacity_rejections: u64,
    pub connected_clients: u64,
    pub connections_total: u64,
    pub messages_received: u64,
    pub messages_sent: u64,
    pub publish_accepted: u64,
    pub publish_rejected: u64,
    pub bytes_received: u64,
    pub bytes_sent: u64,
}

#[derive(Default)]
struct Counters {
    connections_authenticated: AtomicU64,
    handshake_failures: AtomicU64,
    capacity_rejections: AtomicU64,
    connected_clients: AtomicU64,
    connections_total: AtomicU64,
    messages_received: AtomicU64,
    messages_sent: AtomicU64,
    publish_accepted: AtomicU64,
    publish_rejected: AtomicU64,
    bytes_received: AtomicU64,
    bytes_sent: AtomicU64,
}

#[derive(Default)]
struct ClientCounters {
    received: AtomicU64,
    sent: AtomicU64,
    last_packet: AtomicI64,
}

struct LiveClient {
    snapshot: ClientSnapshot,
    generation: i64,
    cancel: CancellationToken,
    certificate_revoked: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    counters: Arc<ClientCounters>,
}

enum RegistryCommand {
    Register {
        client: LiveClient,
        reply: oneshot::Sender<bool>,
    },
    Remove {
        username: String,
        client_id: String,
        generation: i64,
    },
    List(oneshot::Sender<Vec<ClientSnapshot>>),
    Disconnect {
        username: String,
        client_id: String,
        reply: oneshot::Sender<bool>,
    },
    Wake(Vec<Uuid>),
    DisconnectCertificate {
        username: String,
        fingerprint: String,
        reply: oneshot::Sender<()>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Broker {
    store: Store,
    config: Arc<BrokerConfig>,
    registry: mpsc::Sender<RegistryCommand>,
    counters: Arc<Counters>,
    connections: Arc<Semaphore>,
    auth: Arc<Semaphore>,
    stop: CancellationToken,
    maintenance_stopped: CancellationToken,
}

struct Connection {
    client_id: String,
    username: String,
    credential_id: Uuid,
    password_hash: String,
    session_id: Uuid,
    generation: i64,
    expiry_interval: u32,
    keep_alive: u16,
    receive_maximum: usize,
    v5: bool,
    cancel: CancellationToken,
    certificate_revoked: Arc<AtomicBool>,
    counters: Arc<ClientCounters>,
    wake: mpsc::Receiver<()>,
    aliases: HashMap<u16, String>,
    sent: HashSet<Uuid>,
    inflight: HashMap<u16, Uuid>,
    flow: HashSet<u16>,
    next_packet_id: u16,
    enhanced_auth: bool,
    reauthentication: Option<authentication::Reauthentication>,
    certificate: Option<crate::domain::CertificateBinding>,
}

impl Connection {
    fn allocate_packet_id(&mut self, reserved: &[(Uuid, u16)]) -> Result<u16> {
        (0..u16::MAX)
            .find_map(|_| {
                let value = self.next_packet_id;
                self.next_packet_id = if value == u16::MAX { 1 } else { value + 1 };
                (!self.inflight.contains_key(&value)
                    && !reserved.iter().any(|(_, packet)| *packet == value))
                .then_some(value)
            })
            .context("all MQTT packet identifiers in use")
    }
}

struct ConnectInfo {
    client_id: String,
    username: Option<String>,
    password: Option<Vec<u8>>,
    clean_start: bool,
    expiry_interval: u32,
    keep_alive: u16,
    receive_maximum: u16,
    maximum_packet_size: Option<u32>,
    will: Option<Publication>,
    will_delay_interval: u32,
    auth_method: Option<String>,
    auth_data: Option<Vec<u8>>,
}

impl Broker {
    pub async fn new(store: Store, config: BrokerConfig) -> Result<Self> {
        ensure!(
            (128..=268_435_455).contains(&config.max_packet_size),
            "invalid MQTT packet limit"
        );
        ensure!(
            config.max_connections > 0
                && config.max_inflight > 0
                && config.max_pending > 0
                && config.auth_concurrency > 0,
            "MQTT limits must be positive"
        );
        ensure!(
            !config.poll_interval.is_zero()
                && !config.write_timeout.is_zero()
                && !config.shutdown_timeout.is_zero()
                && !config.handshake_timeout.is_zero(),
            "MQTT timeouts must be positive"
        );
        let store =
            store.with_queue_limits(config.max_pending as i64, config.max_pending_bytes as i64);
        store.recover_startup().await?;
        let counters = Arc::new(Counters::default());
        let (registry, receiver) = mpsc::channel(256);
        tokio::spawn(registry_loop(receiver, counters.clone()));
        let broker = Self {
            store,
            connections: Arc::new(Semaphore::new(config.max_connections)),
            auth: Arc::new(Semaphore::new(config.auth_concurrency)),
            config: Arc::new(config),
            registry,
            counters,
            stop: CancellationToken::new(),
            maintenance_stopped: CancellationToken::new(),
        };
        let background = broker.clone();
        let completion = broker.maintenance_stopped.clone().drop_guard();
        tokio::spawn(async move {
            let _completion = completion;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    biased;
                    _ = background.stop.cancelled() => break,
                    _ = tick.tick() => match background.store.maintenance().await {
                        Ok(deliveries) => background.wake(deliveries.into_iter().map(|d| d.session_id).collect()).await,
                        Err(error) => tracing::error!(error = %error, "MQTT persistence maintenance failed"),
                    }
                }
            }
        });
        Ok(broker)
    }

    pub async fn clients(&self) -> Vec<ClientSnapshot> {
        let (reply, receiver) = oneshot::channel();
        if self
            .registry
            .send(RegistryCommand::List(reply))
            .await
            .is_err()
        {
            return Vec::new();
        }
        receiver.await.unwrap_or_default()
    }

    pub fn monitor(&self) -> &crate::monitor::MessageMonitor {
        self.store.monitor()
    }

    pub fn diagnostics(&self) -> &crate::diagnostics::Diagnostics {
        self.store.diagnostics()
    }

    pub fn storage_metrics(&self) -> &crate::metrics::StorageMetrics {
        self.store.metrics()
    }

    pub async fn disconnect(&self, username: &str, client_id: &str) -> bool {
        let (reply, receiver) = oneshot::channel();
        if self
            .registry
            .send(RegistryCommand::Disconnect {
                username: username.to_owned(),
                client_id: client_id.to_owned(),
                reply,
            })
            .await
            .is_err()
        {
            return false;
        }
        receiver.await.unwrap_or(false)
    }

    pub fn metrics(&self) -> BrokerMetrics {
        let c = &self.counters;
        BrokerMetrics {
            connections_authenticated: c.connections_authenticated.load(Ordering::Relaxed),
            handshake_failures: c.handshake_failures.load(Ordering::Relaxed),
            capacity_rejections: c.capacity_rejections.load(Ordering::Relaxed),
            connected_clients: c.connected_clients.load(Ordering::Relaxed),
            connections_total: c.connections_total.load(Ordering::Relaxed),
            messages_received: c.messages_received.load(Ordering::Relaxed),
            messages_sent: c.messages_sent.load(Ordering::Relaxed),
            publish_accepted: c.publish_accepted.load(Ordering::Relaxed),
            publish_rejected: c.publish_rejected.load(Ordering::Relaxed),
            bytes_received: c.bytes_received.load(Ordering::Relaxed),
            bytes_sent: c.bytes_sent.load(Ordering::Relaxed),
        }
    }

    pub async fn disconnect_certificate(&self, username: &str, fingerprint: &str) {
        let (reply, receive) = oneshot::channel();
        let _ = self
            .registry
            .send(RegistryCommand::DisconnectCertificate {
                username: username.to_owned(),
                fingerprint: fingerprint.to_owned(),
                reply,
            })
            .await;
        let _ = receive.await;
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.stop.cancel();
        let shutdown = async {
            let (reply, receiver) = oneshot::channel();
            let _ = self.registry.send(RegistryCommand::Shutdown(reply)).await;
            let _ = receiver.await;
            self.maintenance_stopped.cancelled().await;
        };
        timeout(self.config.shutdown_timeout, shutdown)
            .await
            .context("Broker background shutdown exceeded its deadline")?;
        Ok(())
    }

    async fn wake(&self, sessions: Vec<Uuid>) {
        if !sessions.is_empty() {
            let _ = self.registry.send(RegistryCommand::Wake(sessions)).await;
        }
    }

    pub async fn import_bridge(
        &self,
        id: Uuid,
        revision: i64,
        rules: &[crate::bridge::Rule],
        publication: &Publication,
    ) -> Result<()> {
        let deliveries = self
            .store
            .import_bridge(id, revision, rules, publication)
            .await?;
        self.wake(
            deliveries
                .into_iter()
                .map(|delivery| delivery.session_id)
                .collect(),
        )
        .await;
        Ok(())
    }

    pub async fn run_io<T>(&self, io: T, peer: SocketAddr) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.run_io_with_certificate(io, peer, None).await
    }

    pub async fn run_io_with_certificate<T>(
        &self,
        io: T,
        peer: SocketAddr,
        certificate: Option<crate::tls::VerifiedClientCertificate>,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        ensure!(!self.stop.is_cancelled(), "broker is shutting down");
        let _permit = match self.connections.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(error) => {
                self.counters
                    .capacity_rejections
                    .fetch_add(1, Ordering::Relaxed);
                return Err(anyhow::Error::new(error).context("MQTT connection limit reached"));
            }
        };
        self.counters
            .connections_total
            .fetch_add(1, Ordering::Relaxed);
        let mut wire = Framed::new(io, BrokerCodec::new(self.config.max_packet_size));
        let mut connection = match self.handshake(&mut wire, peer, certificate.as_ref()).await {
            Ok(connection) => {
                self.counters
                    .connections_authenticated
                    .fetch_add(1, Ordering::Relaxed);
                connection
            }
            Err(error) => {
                self.counters
                    .handshake_failures
                    .fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };
        let outcome = self.connection_loop(&mut wire, &mut connection).await;
        self.diagnostics().session_event(
            connection.session_id,
            "connection_closed",
            &connection.client_id,
            &connection.username,
        );
        if outcome
            .as_ref()
            .is_err_and(|error| error.is::<crate::store::SessionSuperseded>())
        {
            // A new generation can commit before registry cancellation reaches
            // the old task. Report takeover when a fenced DB read wins.
            self.send_disconnect(&mut wire, v5::DisconnectReasonCode::SessionTakenOver)
                .await;
        }
        let graceful = outcome.as_ref().map(|v| v.0).unwrap_or(false);
        let expiry = outcome
            .as_ref()
            .map(|v| v.1)
            .unwrap_or(connection.expiry_interval);
        // Generation fencing makes cleanup from an old connection harmless after takeover.
        if let Err(error) = self
            .store
            .close_session(
                connection.session_id,
                connection.generation,
                expiry,
                graceful,
            )
            .await
        {
            tracing::error!(error = %error, "MQTT session close failed");
        }
        let _ = self
            .registry
            .send(RegistryCommand::Remove {
                username: connection.username.clone(),
                client_id: connection.client_id.clone(),
                generation: connection.generation,
            })
            .await;
        outcome.map(|_| ())
    }

    async fn handshake<T>(
        &self,
        wire: &mut Wire<T>,
        peer: SocketAddr,
        peer_certificate: Option<&crate::tls::VerifiedClientCertificate>,
    ) -> Result<Connection>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let packet = timeout(self.config.handshake_timeout, wire.next())
            .await
            .context("MQTT CONNECT timed out")?
            .ok_or_else(|| anyhow!("connection closed before CONNECT"))??;
        self.counters
            .bytes_received
            .fetch_add(wire.codec().bytes_received, Ordering::Relaxed);
        self.counters
            .messages_received
            .fetch_add(1, Ordering::Relaxed);
        let v5 = wire.codec().is_v5();
        let mut info = parse_connect(packet)?;
        if wire.codec().last_will_expiry_zero
            && let Some(will) = &mut info.will
        {
            will.properties.message_expiry_interval = Some(0);
        }
        if let Some(maximum) = info.maximum_packet_size {
            wire.codec_mut().set_max_out(maximum as usize);
        }
        let assigned = if info.client_id.is_empty() {
            ensure!(
                wire.codec().version != 3 && (v5 || info.clean_start),
                "empty client identifier is not allowed"
            );
            let generated = format!("clex-{}", Uuid::new_v4().simple());
            info.client_id = generated.clone();
            Some(generated)
        } else {
            None
        };
        ensure!(
            valid_utf8_field(&info.client_id) && info.client_id.len() <= 65535,
            "invalid client identifier"
        );
        let authentication::Authentication {
            credential,
            reply: authentication_reply,
            certificate,
        } = self
            .authenticate_connect(wire, &info, peer_certificate)
            .await?;
        let username = credential.username.as_str();
        if let Some(will) = &info.will {
            ensure!(valid_topic(&will.topic), "invalid Will topic");
            if !self.store.authorize(username, &will.topic, true).await? {
                self.connack(wire, false, 0x87, None).await?;
                return Err(anyhow!("Will topic is not authorized"));
            }
        }
        if let Some(maximum) = info.maximum_packet_size {
            wire.codec_mut().set_max_out(maximum as usize);
        }
        let opened = self
            .store
            .open_session_verified(
                SessionSpec {
                    client_id: info.client_id.as_bytes().to_vec(),
                    principal: username.to_owned(),
                    clean_start: info.clean_start,
                    expiry_interval: info.expiry_interval,
                    protocol: wire.codec().version,
                    will: info.will,
                    will_delay_interval: info.will_delay_interval,
                },
                credential.id,
                &credential.password_hash,
                certificate.as_ref(),
            )
            .await?;
        let (wake, receive_wake) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let certificate_revoked = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(ClientCounters::default());
        counters
            .last_packet
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        let snapshot = ClientSnapshot {
            client_id: info.client_id.clone(),
            username: username.to_owned(),
            peer,
            protocol: wire.codec().version,
            keep_alive: info.keep_alive,
            certificate_fingerprint: certificate
                .as_ref()
                .map(|certificate| crate::tls::fingerprint_text(&certificate.fingerprint)),
            session_id: opened.session_id,
            connected_at: Utc::now(),
            last_packet_at: Utc::now(),
            messages_received: 0,
            messages_sent: 0,
        };
        let (reply, registered) = oneshot::channel();
        self.registry
            .send(RegistryCommand::Register {
                client: LiveClient {
                    snapshot,
                    generation: opened.generation,
                    cancel: cancel.clone(),
                    certificate_revoked: certificate_revoked.clone(),
                    wake,
                    counters: counters.clone(),
                },
                reply,
            })
            .await?;
        ensure!(
            registered.await?,
            "connection replaced by a newer connection"
        );
        let connection = Connection {
            client_id: info.client_id,
            username: username.to_owned(),
            credential_id: credential.id,
            password_hash: credential.password_hash,
            session_id: opened.session_id,
            generation: opened.generation,
            expiry_interval: info.expiry_interval,
            keep_alive: info.keep_alive,
            receive_maximum: usize::from(info.receive_maximum.min(self.config.max_inflight)),
            v5,
            cancel,
            certificate_revoked,
            counters,
            wake: receive_wake,
            aliases: HashMap::new(),
            sent: HashSet::new(),
            inflight: HashMap::new(),
            flow: HashSet::new(),
            next_packet_id: 1,
            enhanced_auth: authentication_reply.is_some(),
            reauthentication: None,
            certificate,
        };
        // Registration precedes this final read: an admin revocation either
        // observes this connection in its disconnect snapshot or is seen here.
        let authentication_current = self.credentials_current(&connection).await;
        let suppress_will = !matches!(&authentication_current, Ok(true));
        let connack = match authentication_current {
            Ok(true) if !connection.cancel.is_cancelled() => {
                self.connack_with_auth(
                    wire,
                    opened.session_present,
                    0,
                    assigned,
                    authentication_reply,
                )
                .await
            }
            Ok(_) => {
                let _ = self.connack(wire, false, 0x87, None).await;
                Err(anyhow!("MQTT credentials changed during handshake"))
            }
            Err(error) => Err(error),
        };
        if let Err(error) = connack {
            let _ = self
                .store
                .close_session(
                    connection.session_id,
                    connection.generation,
                    connection.expiry_interval,
                    suppress_will,
                )
                .await;
            let _ = self
                .registry
                .send(RegistryCommand::Remove {
                    username: connection.username,
                    client_id: connection.client_id,
                    generation: connection.generation,
                })
                .await;
            return Err(error);
        }
        Ok(connection)
    }

    async fn credentials_current(&self, connection: &Connection) -> Result<bool> {
        let Some(credential) = self.store.credential(&connection.username).await? else {
            return Ok(false);
        };
        if !credential.enabled
            || credential.id != connection.credential_id
            || credential.password_hash != connection.password_hash
            || credential
                .bound_client_id
                .as_ref()
                .is_some_and(|id| id != connection.client_id.as_bytes())
            || credential.require_client_certificate && connection.certificate.is_none()
        {
            return Ok(false);
        }
        if let Some(certificate) = &connection.certificate {
            return self
                .store
                .certificate_current(credential.id, certificate)
                .await;
        }
        Ok(true)
    }

    async fn connack<T>(
        &self,
        wire: &mut Wire<T>,
        session_present: bool,
        reason: u8,
        assigned: Option<String>,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.connack_with_auth(wire, session_present, reason, assigned, None)
            .await
    }

    async fn connack_with_auth<T>(
        &self,
        wire: &mut Wire<T>,
        session_present: bool,
        reason: u8,
        assigned: Option<String>,
        authentication_reply: Option<String>,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let packet = if wire.codec().is_v5() {
            MqttPacket::V5(v5::Packet::ConnectAck(Box::new(v5::ConnectAck {
                session_present,
                reason_code: reason
                    .try_into()
                    .unwrap_or(v5::ConnectAckReason::UnspecifiedError),
                receive_max: NonZeroU16::new(self.config.max_inflight).unwrap(),
                max_packet_size: Some(self.config.max_packet_size as u32),
                assigned_client_id: assigned.map(Into::into),
                topic_alias_max: self.config.topic_alias_maximum,
                shared_subscription_available: true,
                auth_method: authentication_reply
                    .as_ref()
                    .map(|_| crate::scram::METHOD.into()),
                auth_data: authentication_reply.map(|data| data.into_bytes().into()),
                ..Default::default()
            })))
        } else {
            let code = match reason {
                0 => v3::ConnectAckReason::ConnectionAccepted,
                0x86 => v3::ConnectAckReason::BadUserNameOrPassword,
                0x87 => v3::ConnectAckReason::NotAuthorized,
                _ => v3::ConnectAckReason::ServiceUnavailable,
            };
            MqttPacket::V3(v3::Packet::ConnectAck(v3::ConnectAck {
                session_present,
                return_code: code,
            }))
        };
        self.send(wire, packet, None).await
    }

    async fn send<T>(
        &self,
        wire: &mut Wire<T>,
        packet: MqttPacket,
        connection: Option<&Connection>,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let bytes_before = wire.codec().bytes_sent;
        timeout(self.config.write_timeout, wire.send(packet))
            .await
            .context("MQTT peer write timeout")??;
        self.counters
            .bytes_sent
            .fetch_add(wire.codec().bytes_sent - bytes_before, Ordering::Relaxed);
        self.counters.messages_sent.fetch_add(1, Ordering::Relaxed);
        if let Some(connection) = connection {
            connection.counters.sent.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn send_disconnect<T>(&self, wire: &mut Wire<T>, reason: v5::DisconnectReasonCode)
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if wire.codec().is_v5() {
            let _ = self
                .send(
                    wire,
                    MqttPacket::V5(v5::Packet::Disconnect(v5::Disconnect::new(reason))),
                    None,
                )
                .await;
        }
    }

    async fn connection_loop<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
    ) -> Result<(bool, u32)>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut tick = tokio::time::interval(self.config.poll_interval);
        let mut last_packet = Instant::now();
        let mut pending_packet = None;
        loop {
            let more_deliveries = self.flush_deliveries(wire, connection).await?;
            let authentication_deadline = connection
                .reauthentication
                .as_ref()
                .map(|auth| auth.deadline);
            let bytes_before = wire.codec().bytes_received;
            let keep_alive = if connection.keep_alive == 0 {
                Duration::from_secs(86400 * 365)
            } else {
                Duration::from_millis(u64::from(connection.keep_alive) * 1500)
            };
            tokio::select! {
                _ = self.stop.cancelled() => { self.send_disconnect(wire,v5::DisconnectReasonCode::ServerShuttingDown).await; return Ok((false,connection.expiry_interval)); },
                _ = connection.cancel.cancelled() => {
                    let reason = if connection.certificate_revoked.load(Ordering::Acquire) { v5::DisconnectReasonCode::NotAuthorized } else { v5::DisconnectReasonCode::SessionTakenOver };
                    self.send_disconnect(wire,reason).await; return Ok((false,connection.expiry_interval));
                },
                _ = tokio::time::sleep_until(authentication_deadline.unwrap_or_else(|| Instant::now() + self.config.handshake_timeout)), if authentication_deadline.is_some() => {
                    self.send_disconnect(wire, v5::DisconnectReasonCode::NotAuthorized).await;
                    return Err(anyhow!("MQTT reauthentication timed out"));
                },
                _ = tokio::time::sleep_until(last_packet + keep_alive), if connection.keep_alive > 0 => {
                    self.send_disconnect(wire, v5::DisconnectReasonCode::KeepAliveTimeout).await;
                    return Err(anyhow!("MQTT keepalive timeout"));
                },
                _ = tick.tick() => {
                    let enabled = self.credentials_current(connection).await?;
                    if !enabled { self.send_disconnect(wire,v5::DisconnectReasonCode::NotAuthorized).await; return Ok((false,connection.expiry_interval)); }
                },
                _ = connection.wake.recv() => {},
                _ = std::future::ready(()), if more_deliveries => {},
                incoming = async {
                    match pending_packet.take() {
                        Some(packet) => Some(packet),
                        None => wire.next().await,
                    }
                } => {
                    let Some(incoming) = incoming else { return Ok((false,connection.expiry_interval)); };
                    let packet = match incoming { Ok(packet) => packet, Err(error) => { self.send_disconnect(wire,protocol::disconnect_reason(&error)).await; return Err(error.into()); } };
                    let mut peer_closed = false;
                    let confirmation = if let Some((kind, packet_id)) = OutgoingAck::decode(&packet) {
                        let mut ids = vec![packet_id];
                        // Only coalesce ready confirmations of the same phase.
                        // Stop at duplicates or any other packet to retain wire
                        // order, keeping at most one extra payload in memory.
                        while ids.len() < 32 {
                            match wire.next().now_or_never() {
                                Some(Some(Ok(next))) => {
                                    match OutgoingAck::decode(&next) {
                                        Some((next_kind, id)) if next_kind == kind && !ids.contains(&id) => ids.push(id),
                                        _ => { pending_packet = Some(Ok(next)); break; }
                                    }
                                },
                                Some(Some(Err(error))) => { pending_packet = Some(Err(error)); break; },
                                Some(None) => { peer_closed = true; break; },
                                None => break,
                            }
                        }
                        Some((kind, ids))
                    } else { None };
                    let received = confirmation.as_ref().map_or(1, |(_, ids)| ids.len() as u64);
                    self.counters.bytes_received.fetch_add(wire.codec().bytes_received-bytes_before,Ordering::Relaxed);
                    last_packet = Instant::now();
                    self.counters.messages_received.fetch_add(received, Ordering::Relaxed);
                    connection.counters.received.fetch_add(received, Ordering::Relaxed);
                    connection.counters.last_packet.store(Utc::now().timestamp(), Ordering::Relaxed);
                    if let Some((kind, ids)) = confirmation {
                        self.confirm_delivery_batch(wire, connection, kind, &ids).await?;
                    } else if let Some(close) = self.handle_packet(wire, connection, packet).await? {
                        return Ok(close);
                    }
                    if peer_closed { return Ok((false,connection.expiry_interval)); }

                },
            }
        }
    }

    async fn handle_packet<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
        packet: MqttPacket,
    ) -> Result<Option<(bool, u32)>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        match packet {
            MqttPacket::V5(v5::Packet::Auth(auth)) => {
                self.reauthenticate(wire, connection, auth).await?;
            }
            MqttPacket::V3(v3::Packet::Publish(p)) | MqttPacket::V5(v5::Packet::Publish(p)) => {
                self.accept_publish(wire, connection, *p).await?
            }
            MqttPacket::V3(v3::Packet::Subscribe {
                packet_id,
                topic_filters,
            }) => {
                let mut status = Vec::new();
                ensure!(!topic_filters.is_empty(), "empty SUBSCRIBE");
                for (filter, qos) in topic_filters {
                    let accepted = self
                        .subscribe(
                            connection,
                            Subscription {
                                filter: filter.to_string(),
                                qos: qos.into(),
                                no_local: false,
                                retain_as_published: false,
                                retain_handling: 0,
                                subscription_identifier: None,
                            },
                        )
                        .await?;
                    status.push(if accepted == 0 {
                        v3::SubscribeReturnCode::Success(qos)
                    } else {
                        v3::SubscribeReturnCode::Failure
                    });
                }
                self.send(
                    wire,
                    MqttPacket::V3(v3::Packet::SubscribeAck { packet_id, status }),
                    Some(connection),
                )
                .await?;
            }
            MqttPacket::V5(v5::Packet::Subscribe(sub)) => {
                ensure!(!sub.topic_filters.is_empty(), "empty SUBSCRIBE");
                let mut status = Vec::new();
                for (filter, options) in sub.topic_filters {
                    let reason = self
                        .subscribe(
                            connection,
                            Subscription {
                                filter: filter.to_string(),
                                qos: options.qos.into(),
                                no_local: options.no_local,
                                retain_as_published: options.retain_as_published,
                                retain_handling: options.retain_handling.into(),
                                subscription_identifier: sub.id.map(|n| n.get()),
                            },
                        )
                        .await?;
                    status.push(if reason == 0 {
                        u8::from(options.qos).try_into().unwrap()
                    } else {
                        reason
                            .try_into()
                            .unwrap_or(v5::SubscribeAckReason::UnspecifiedError)
                    });
                }
                self.send(
                    wire,
                    MqttPacket::V5(v5::Packet::SubscribeAck(v5::SubscribeAck {
                        packet_id: sub.packet_id,
                        properties: Vec::new(),
                        reason_string: None,
                        status,
                    })),
                    Some(connection),
                )
                .await?;
            }
            MqttPacket::V3(v3::Packet::Unsubscribe {
                packet_id,
                topic_filters,
            }) => {
                ensure!(!topic_filters.is_empty(), "empty UNSUBSCRIBE");
                for filter in topic_filters {
                    ensure!(valid_filter(&filter), "invalid topic filter");
                    self.store
                        .unsubscribe(connection.session_id, connection.generation, &filter)
                        .await?;
                }
                self.send(
                    wire,
                    MqttPacket::V3(v3::Packet::UnsubscribeAck { packet_id }),
                    Some(connection),
                )
                .await?;
            }
            MqttPacket::V5(v5::Packet::Unsubscribe(unsub)) => {
                ensure!(!unsub.topic_filters.is_empty(), "empty UNSUBSCRIBE");
                let mut status = Vec::new();
                for filter in unsub.topic_filters {
                    if !valid_filter(&filter) {
                        status.push(v5::UnsubscribeAckReason::TopicFilterInvalid);
                        continue;
                    }
                    let existed = self
                        .store
                        .unsubscribe(connection.session_id, connection.generation, &filter)
                        .await?;
                    status.push(if existed {
                        v5::UnsubscribeAckReason::Success
                    } else {
                        v5::UnsubscribeAckReason::NoSubscriptionExisted
                    });
                }
                self.send(
                    wire,
                    MqttPacket::V5(v5::Packet::UnsubscribeAck(v5::UnsubscribeAck {
                        packet_id: unsub.packet_id,
                        properties: Vec::new(),
                        reason_string: None,
                        status,
                    })),
                    Some(connection),
                )
                .await?;
            }
            MqttPacket::V3(v3::Packet::PublishAck { packet_id }) => {
                self.acknowledge(connection, packet_id.get(), DeliveryPhase::AwaitPubAck)
                    .await?
            }
            MqttPacket::V5(v5::Packet::PublishAck(ack)) => {
                self.acknowledge(connection, ack.packet_id.get(), DeliveryPhase::AwaitPubAck)
                    .await?
            }
            MqttPacket::V3(v3::Packet::PublishReceived { packet_id }) => {
                self.receive_pubrec(wire, connection, packet_id, false)
                    .await?
            }
            MqttPacket::V5(v5::Packet::PublishReceived(ack)) => {
                self.receive_pubrec(
                    wire,
                    connection,
                    ack.packet_id,
                    u8::from(ack.reason_code) >= 128,
                )
                .await?
            }
            MqttPacket::V3(v3::Packet::PublishRelease { packet_id }) => {
                self.receive_pubrel(wire, connection, packet_id).await?
            }
            MqttPacket::V5(v5::Packet::PublishRelease(ack)) => {
                self.receive_pubrel(wire, connection, ack.packet_id).await?
            }
            MqttPacket::V3(v3::Packet::PublishComplete { packet_id }) => {
                self.acknowledge(connection, packet_id.get(), DeliveryPhase::AwaitPubComp)
                    .await?
            }
            MqttPacket::V5(v5::Packet::PublishComplete(ack)) => {
                self.acknowledge(connection, ack.packet_id.get(), DeliveryPhase::AwaitPubComp)
                    .await?
            }
            MqttPacket::V3(v3::Packet::PingRequest) => {
                self.send(
                    wire,
                    MqttPacket::V3(v3::Packet::PingResponse),
                    Some(connection),
                )
                .await?
            }
            MqttPacket::V5(v5::Packet::PingRequest) => {
                self.send(
                    wire,
                    MqttPacket::V5(v5::Packet::PingResponse),
                    Some(connection),
                )
                .await?
            }
            MqttPacket::V3(v3::Packet::Disconnect) => {
                return Ok(Some((true, connection.expiry_interval)));
            }
            MqttPacket::V5(v5::Packet::Disconnect(disconnect)) => {
                let expiry = disconnect
                    .session_expiry_interval_secs
                    .unwrap_or(connection.expiry_interval);
                ensure!(
                    connection.expiry_interval != 0 || expiry == 0,
                    "cannot increase a zero session expiry at disconnect"
                );
                return Ok(Some((
                    disconnect.reason_code != v5::DisconnectReasonCode::DisconnectWithWillMessage,
                    expiry,
                )));
            }
            _ => {
                self.send_disconnect(wire, v5::DisconnectReasonCode::ProtocolError)
                    .await;
                return Err(anyhow!(
                    "packet is not permitted from an established MQTT client"
                ));
            }
        }
        Ok(None)
    }

    async fn subscribe(&self, connection: &Connection, subscription: Subscription) -> Result<u8> {
        let filter = &subscription.filter;
        if filter.starts_with("$share/") && !connection.v5 {
            return Ok(0x9e);
        }
        if !valid_filter(filter) {
            return Ok(0x8f);
        }
        if !self
            .store
            .authorize_subscription(&connection.username, filter)
            .await?
        {
            return Ok(0x87);
        }
        if let Err(error) = self
            .store
            .subscribe(connection.session_id, connection.generation, &subscription)
            .await
        {
            if error.is::<QuotaExceeded>() {
                return Ok(0x97);
            }
            return Err(error);
        }
        Ok(0)
    }

    async fn accept_publish<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
        mut publish: rmqtt_codec::types::Publish,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if let Some(properties) = &publish.properties {
            ensure!(
                properties.subscription_ids.is_empty(),
                "clients cannot publish subscription identifiers"
            );
            if let Some(alias) = properties.topic_alias {
                if alias.get() > self.config.topic_alias_maximum {
                    self.send_disconnect(wire, v5::DisconnectReasonCode::TopicAliasInvalid)
                        .await;
                    return Err(anyhow!("topic alias exceeds the negotiated maximum"));
                }
                if publish.topic.is_empty() {
                    let Some(topic) = connection.aliases.get(&alias.get()) else {
                        self.send_disconnect(wire, v5::DisconnectReasonCode::ProtocolError)
                            .await;
                        return Err(anyhow!("topic alias has no mapping on this connection"));
                    };
                    publish.topic = topic.clone().into();
                } else {
                    connection
                        .aliases
                        .insert(alias.get(), publish.topic.to_string());
                }
            }
        }
        ensure!(valid_topic(&publish.topic), "invalid publish topic");
        let mut publication = protocol::publication(&publish);
        if wire.codec().last_expiry_zero {
            publication.properties.message_expiry_interval = Some(0);
            publication.expires_at = Some(Utc::now());
        }
        if publication.properties.payload_format_indicator == Some(1) {
            ensure!(
                std::str::from_utf8(&publication.payload).is_ok(),
                "payload format is not UTF-8"
            );
        }
        if let Some(response) = &publication.properties.response_topic {
            ensure!(valid_topic(response), "invalid response topic");
        }
        if !self
            .store
            .authorize(&connection.username, &publication.topic, true)
            .await?
        {
            if let Some(t) = self.diagnostics().capture(
                Uuid::new_v4(),
                &publication,
                connection.client_id.as_bytes(),
                &connection.username,
            ) {
                t.finish("publish_acl_denied");
            }
            self.counters
                .publish_rejected
                .fetch_add(1, Ordering::Relaxed);
            if connection.v5 && publish.qos != QoS::AtMostOnce {
                self.send(
                    wire,
                    protocol::pub_ack(
                        true,
                        if publish.qos == QoS::ExactlyOnce {
                            5
                        } else {
                            4
                        },
                        publish.packet_id.context("missing packet identifier")?,
                        0x87,
                    ),
                    Some(connection),
                )
                .await?;
                return Ok(());
            }
            self.send_disconnect(wire, v5::DisconnectReasonCode::NotAuthorized)
                .await;
            return Err(anyhow!("publish not authorized"));
        }
        if publish.qos == QoS::ExactlyOnce {
            let id = publish.packet_id.context("missing packet identifier")?;
            if let Err(error) = self
                .store
                .receive_qos2(
                    connection.session_id,
                    connection.generation,
                    id.get(),
                    &publication,
                )
                .await
            {
                if self
                    .reject_quota(wire, connection, &error, Some((5, id)))
                    .await?
                {
                    return Ok(());
                }
                return Err(error);
            }
            self.send(
                wire,
                protocol::pub_ack(connection.v5, 5, id, 0),
                Some(connection),
            )
            .await?;
        } else {
            let deliveries = match self
                .store
                .accept_publication(
                    connection.session_id,
                    connection.generation,
                    &publication,
                    connection.client_id.as_bytes(),
                )
                .await
            {
                Ok(deliveries) => deliveries,
                Err(error) => {
                    if self
                        .reject_quota(
                            wire,
                            connection,
                            &error,
                            publish.packet_id.map(|id| (4, id)),
                        )
                        .await?
                    {
                        return Ok(());
                    }
                    return Err(error);
                }
            };
            self.wake(deliveries.into_iter().map(|d| d.session_id).collect())
                .await;
            if publish.qos == QoS::AtLeastOnce {
                self.send(
                    wire,
                    protocol::pub_ack(
                        connection.v5,
                        4,
                        publish.packet_id.context("missing packet identifier")?,
                        0,
                    ),
                    Some(connection),
                )
                .await?;
            }
        }
        self.counters
            .publish_accepted
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn reject_quota<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &Connection,
        error: &anyhow::Error,
        ack: Option<(u8, NonZeroU16)>,
    ) -> Result<bool>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if !error.is::<QuotaExceeded>() {
            return Ok(false);
        }
        self.counters
            .publish_rejected
            .fetch_add(1, Ordering::Relaxed);
        if connection.v5
            && let Some((kind, id)) = ack
        {
            self.send(
                wire,
                protocol::pub_ack(true, kind, id, 0x97),
                Some(connection),
            )
            .await?;
            return Ok(true);
        }
        self.send_disconnect(wire, v5::DisconnectReasonCode::QuotaExceeded)
            .await;
        Err(anyhow!("MQTT session quota exceeded"))
    }

    async fn receive_pubrel<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &Connection,
        packet_id: NonZeroU16,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let release = match self
            .store
            .release_qos2(
                connection.session_id,
                connection.generation,
                packet_id.get(),
            )
            .await
        {
            Ok(release) => release,
            Err(error) => {
                if error.is::<QuotaExceeded>() {
                    self.counters
                        .publish_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    self.send_disconnect(wire, v5::DisconnectReasonCode::QuotaExceeded)
                        .await;
                }
                return Err(error);
            }
        };
        self.wake(
            release
                .deliveries
                .into_iter()
                .map(|d| d.session_id)
                .collect(),
        )
        .await;
        self.send(
            wire,
            protocol::pub_ack(
                connection.v5,
                7,
                packet_id,
                if release.found { 0 } else { 0x92 },
            ),
            Some(connection),
        )
        .await
    }

    async fn acknowledge(
        &self,
        connection: &mut Connection,
        packet_id: u16,
        phase: DeliveryPhase,
    ) -> Result<()> {
        if self
            .store
            .acknowledge(
                connection.session_id,
                connection.generation,
                packet_id,
                phase,
            )
            .await?
        {
            connection.flow.remove(&packet_id);
            if let Some(delivery) = connection.inflight.remove(&packet_id) {
                connection.sent.remove(&delivery);
            }
        }
        Ok(())
    }

    async fn receive_pubrec<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
        packet_id: NonZeroU16,
        rejected: bool,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if rejected {
            return self
                .acknowledge(connection, packet_id.get(), DeliveryPhase::AwaitPubRec)
                .await;
        }
        let found = self
            .store
            .advance_qos2(
                connection.session_id,
                connection.generation,
                packet_id.get(),
            )
            .await?;
        // MQTT 5 §4.9: successful PUBREC keeps the QoS2 send-quota slot until
        // PUBCOMP. A negative PUBREC is handled by acknowledge() above.
        self.send(
            wire,
            protocol::pub_ack(connection.v5, 6, packet_id, if found { 0 } else { 0x92 }),
            Some(connection),
        )
        .await
    }

    async fn flush_deliveries<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
    ) -> Result<bool>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if connection.cancel.is_cancelled() || self.stop.is_cancelled() {
            return Ok(false);
        }
        if !self.credentials_current(connection).await? {
            self.send_disconnect(wire, v5::DisconnectReasonCode::NotAuthorized)
                .await;
            return Err(anyhow!("MQTT credentials revoked or rotated"));
        }
        if connection.inflight.is_empty()
            && connection.sent.is_empty()
            && !self
                .store
                .has_deliveries(connection.session_id, connection.generation)
                .await?
        {
            return Ok(false);
        }
        if connection.flow.len() >= connection.receive_maximum {
            self.diagnostics().session_event(
                connection.session_id,
                "receive_maximum",
                &connection.client_id,
                &connection.username,
            );
        }
        let batch = self
            .store
            .outgoing_batch(
                connection.session_id,
                connection.generation,
                &connection.sent.iter().copied().collect::<Vec<_>>(),
                connection.flow.len() < connection.receive_maximum,
            )
            .await?;
        let valid: HashSet<Uuid> = batch.inflight.iter().map(|(id, _)| *id).collect();
        connection.inflight.retain(|_, id| valid.contains(id));
        connection.sent.retain(|id| valid.contains(id));
        for (id, packet_id) in batch.inflight {
            connection.inflight.insert(packet_id, id);
        }
        connection
            .flow
            .retain(|id| connection.inflight.contains_key(id));
        let full_batch = batch.deliveries.len() == 64;
        // Account for retries as well as new PUBLISH packets when planning the
        // free window. PUBREL continues an existing handshake outside it.
        let mut slots = connection
            .receive_maximum
            .saturating_sub(connection.flow.len());
        let mut reservations = Vec::new();
        for delivery in &batch.deliveries {
            if connection.sent.contains(&delivery.id)
                || delivery.publication.qos == 0
                || delivery.phase == DeliveryPhase::AwaitPubComp
                || (delivery.phase == DeliveryPhase::Queued
                    && delivery
                        .publication
                        .expires_at
                        .is_some_and(|at| at <= Utc::now()))
                || slots == 0
            {
                continue;
            }
            slots -= 1;
            if delivery.packet_id.is_none() {
                let packet_id = connection.allocate_packet_id(&reservations)?;
                reservations.push((delivery.id, packet_id));
            }
        }
        if !reservations.is_empty() {
            self.store
                .reserve_deliveries(connection.session_id, connection.generation, &reservations)
                .await?;
            for (id, packet_id) in &reservations {
                connection.inflight.insert(*packet_id, *id);
            }
        }
        let reservations: HashMap<_, _> = reservations.into_iter().collect();
        if connection.cancel.is_cancelled() || self.stop.is_cancelled() {
            return Ok(false);
        }
        let qos0: HashSet<_> = batch.started_qos0.into_iter().collect();
        for mut delivery in batch.deliveries {
            if connection.cancel.is_cancelled() || self.stop.is_cancelled() {
                return Ok(false);
            }
            if connection.sent.contains(&delivery.id) {
                continue;
            }
            let duplicate = delivery.phase != DeliveryPhase::Queued;
            if delivery.publication.qos == 0 {
                if !qos0.contains(&delivery.id) {
                    continue;
                }
                delivery.phase = DeliveryPhase::SendingQos0;
            }
            if let Some(packet_id) = reservations.get(&delivery.id) {
                delivery.packet_id = Some(*packet_id);
                delivery.phase = if delivery.publication.qos == 1 {
                    DeliveryPhase::AwaitPubAck
                } else {
                    DeliveryPhase::AwaitPubRec
                };
            }
            if !duplicate
                && delivery
                    .publication
                    .expires_at
                    .is_some_and(|at| at <= Utc::now())
            {
                self.diagnostics().delivery(
                    delivery.id,
                    "expired",
                    &connection.client_id,
                    &connection.username,
                    delivery.packet_id,
                );
                self.diagnostics().delivery(
                    delivery.id,
                    "delivery_acl_denied",
                    &connection.client_id,
                    &connection.username,
                    delivery.packet_id,
                );
                self.discard_outgoing(connection, &delivery).await?;
                continue;
            }
            if delivery.phase != DeliveryPhase::AwaitPubComp {
                self.diagnostics().delivery(
                    delivery.id,
                    "acl_allowed",
                    &connection.client_id,
                    &connection.username,
                    delivery.packet_id,
                );
            }
            if delivery.publication.qos > 0
                && delivery.phase != DeliveryPhase::AwaitPubComp
                && connection.flow.len() >= connection.receive_maximum
            {
                self.diagnostics().delivery(
                    delivery.id,
                    "receive_maximum",
                    &connection.client_id,
                    &connection.username,
                    delivery.packet_id,
                );
                continue;
            }
            // Check ACL at the actual send boundary, after a flow slot exists.
            if delivery.phase != DeliveryPhase::AwaitPubComp
                && !self
                    .store
                    .authorize(&connection.username, &delivery.publication.topic, false)
                    .await?
            {
                self.discard_outgoing(connection, &delivery).await?;
                continue;
            }
            if delivery.publication.qos > 0 && delivery.packet_id.is_none() {
                // Expiry or ACL rejection can free a slot after planning.
                let id = connection.allocate_packet_id(&[])?;
                delivery = self
                    .store
                    .reserve_delivery(
                        connection.session_id,
                        connection.generation,
                        delivery.id,
                        id,
                    )
                    .await?;
                connection.inflight.insert(id, delivery.id);
            }
            if connection.cancel.is_cancelled() || self.stop.is_cancelled() {
                return Ok(false);
            }
            let packet = if delivery.phase == DeliveryPhase::AwaitPubComp {
                protocol::pub_ack(
                    connection.v5,
                    6,
                    NonZeroU16::new(
                        delivery
                            .packet_id
                            .context("persisted PUBREL has no packet identifier")?,
                    )
                    .unwrap(),
                    0,
                )
            } else {
                protocol::delivery_packet(&delivery, connection.v5, duplicate)
            };
            if let Err(error) = self.send(wire, packet, Some(connection)).await {
                if error
                    .downcast_ref::<std::io::Error>()
                    .and_then(|error| error.get_ref())
                    .is_some_and(|error| error.is::<protocol::PeerPacketTooLarge>())
                {
                    self.diagnostics().delivery(
                        delivery.id,
                        "packet_too_large",
                        &connection.client_id,
                        &connection.username,
                        delivery.packet_id,
                    );
                    self.discard_outgoing(connection, &delivery).await?;
                    tracing::warn!(session_id=%connection.session_id,delivery_id=%delivery.id,"MQTT delivery exceeds peer packet limit and was discarded");
                    continue;
                }
                self.diagnostics().delivery(
                    delivery.id,
                    "write_failed",
                    &connection.client_id,
                    &connection.username,
                    delivery.packet_id,
                );
                return Err(error);
            }
            self.diagnostics().delivery(
                delivery.id,
                if delivery.phase == DeliveryPhase::AwaitPubComp {
                    "pubrel_sent"
                } else if delivery.publication.qos == 0 {
                    "qos0_written"
                } else if duplicate {
                    "publish_retried"
                } else {
                    "publish_sent"
                },
                &connection.client_id,
                &connection.username,
                delivery.packet_id,
            );
            if delivery.phase != DeliveryPhase::AwaitPubComp
                && let Some(id) = delivery.packet_id
            {
                connection.flow.insert(id);
            }
            if delivery.publication.qos > 0 {
                connection.sent.insert(delivery.id);
            }
        }
        if !qos0.is_empty() {
            self.store
                .complete_qos0_batch(
                    connection.session_id,
                    connection.generation,
                    &qos0.into_iter().collect::<Vec<_>>(),
                )
                .await?;
        }
        Ok(full_batch)
    }

    async fn discard_outgoing(
        &self,
        connection: &mut Connection,
        delivery: &Delivery,
    ) -> Result<()> {
        if delivery.phase != DeliveryPhase::SendingQos0 {
            self.store
                .discard_delivery(connection.session_id, connection.generation, delivery.id)
                .await?;
        }
        if let Some(id) = delivery.packet_id {
            connection.inflight.remove(&id);
            connection.flow.remove(&id);
        }
        Ok(())
    }
}

async fn registry_loop(mut commands: mpsc::Receiver<RegistryCommand>, counters: Arc<Counters>) {
    let mut clients: HashMap<(String, String), LiveClient> = HashMap::new();
    let mut sessions: HashMap<Uuid, (String, String)> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            RegistryCommand::Register { client, reply } => {
                let id = (
                    client.snapshot.username.clone(),
                    client.snapshot.client_id.clone(),
                );
                if clients
                    .get(&id)
                    .is_some_and(|old| old.generation > client.generation)
                {
                    let _ = reply.send(false);
                    continue;
                }
                if let Some(old) = clients.remove(&id) {
                    sessions.remove(&old.snapshot.session_id);
                    old.cancel.cancel();
                }
                sessions.insert(client.snapshot.session_id, id.clone());
                clients.insert(id, client);
                counters
                    .connected_clients
                    .store(clients.len() as u64, Ordering::Relaxed);
                let _ = reply.send(true);
            }
            RegistryCommand::Remove {
                username,
                client_id,
                generation,
            } => {
                let key = (username, client_id);
                if clients
                    .get(&key)
                    .is_some_and(|c| c.generation == generation)
                    && let Some(client) = clients.remove(&key)
                {
                    sessions.remove(&client.snapshot.session_id);
                }
                counters
                    .connected_clients
                    .store(clients.len() as u64, Ordering::Relaxed);
            }
            RegistryCommand::List(reply) => {
                let mut snapshots: Vec<_> = clients
                    .values()
                    .map(|client| {
                        let mut s = client.snapshot.clone();
                        s.messages_received = client.counters.received.load(Ordering::Relaxed);
                        s.messages_sent = client.counters.sent.load(Ordering::Relaxed);
                        s.last_packet_at = DateTime::from_timestamp(
                            client.counters.last_packet.load(Ordering::Relaxed),
                            0,
                        )
                        .unwrap_or(s.connected_at);
                        s
                    })
                    .collect();
                snapshots
                    .sort_by(|a, b| (&a.username, &a.client_id).cmp(&(&b.username, &b.client_id)));
                let _ = reply.send(snapshots);
            }
            RegistryCommand::Disconnect {
                username,
                client_id,
                reply,
            } => {
                let found = clients.get(&(username, client_id)).is_some_and(|client| {
                    client.cancel.cancel();
                    true
                });
                let _ = reply.send(found);
            }
            RegistryCommand::Wake(ids) => {
                for id in ids {
                    if let Some(client) = sessions.get(&id).and_then(|key| clients.get(key)) {
                        let _ = client.wake.try_send(());
                    }
                }
            }
            RegistryCommand::DisconnectCertificate {
                username,
                fingerprint,
                reply,
            } => {
                for client in clients.values() {
                    if client.snapshot.username == username
                        && client.snapshot.certificate_fingerprint.as_deref()
                            == Some(fingerprint.as_str())
                    {
                        client.certificate_revoked.store(true, Ordering::Release);
                        client.cancel.cancel();
                    }
                }
                let _ = reply.send(());
            }
            RegistryCommand::Shutdown(reply) => {
                for client in clients.values() {
                    client.cancel.cancel();
                }
                let _ = reply.send(());
            }
        }
    }
    for client in clients.values() {
        client.cancel.cancel();
    }
}

fn valid_utf8_field(value: &str) -> bool {
    !value
        .chars()
        .any(|c| c == '\0' || matches!(c as u32, 0xfdd0..=0xfdef) || (c as u32 & 0xffff) >= 0xfffe)
}

pub fn valid_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic.len() <= 65535
        && valid_utf8_field(topic)
        && !topic.contains(['+', '#'])
}

pub fn valid_filter(filter: &str) -> bool {
    valid_utf8_field(filter) && parse_subscription_filter(filter).is_some()
}

fn parse_connect(packet: MqttPacket) -> Result<ConnectInfo> {
    Ok(match packet {
        MqttPacket::V3(v3::Packet::Connect(c)) => ConnectInfo {
            client_id: c.client_id.to_string(),
            username: c.username.map(|s| s.to_string()),
            password: c.password.map(|b| b.to_vec()),
            clean_start: c.clean_session,
            expiry_interval: if c.clean_session { 0 } else { u32::MAX },
            keep_alive: c.keep_alive,
            receive_maximum: u16::MAX,
            maximum_packet_size: None,
            will: c.last_will.map(|w| Publication {
                topic: w.topic.to_string(),
                payload: w.message.to_vec(),
                qos: w.qos.into(),
                retain: w.retain,
                expires_at: None,
                properties: PublicationProperties::default(),
            }),
            will_delay_interval: 0,
            auth_method: None,
            auth_data: None,
        },
        MqttPacket::V5(v5::Packet::Connect(c)) => {
            let delay = c
                .last_will
                .as_ref()
                .and_then(|w| w.will_delay_interval_sec)
                .unwrap_or(0);
            ConnectInfo {
                client_id: c.client_id.to_string(),
                username: c.username.map(|s| s.to_string()),
                password: c.password.map(|b| b.to_vec()),
                clean_start: c.clean_start,
                expiry_interval: c.session_expiry_interval_secs,
                keep_alive: c.keep_alive,
                receive_maximum: c.receive_max.map(NonZeroU16::get).unwrap_or(u16::MAX),
                maximum_packet_size: c.max_packet_size.map(|v| v.get()),
                will: c.last_will.map(|w| Publication {
                    topic: w.topic.to_string(),
                    payload: w.message.to_vec(),
                    qos: w.qos.into(),
                    retain: w.retain,
                    expires_at: None,
                    properties: PublicationProperties {
                        payload_format_indicator: w.is_utf8_payload.map(u8::from),
                        content_type: w.content_type.map(|s| s.to_string()),
                        response_topic: w.response_topic.map(|s| s.to_string()),
                        correlation_data: w.correlation_data.map(|b| b.to_vec()),
                        user_properties: w
                            .user_properties
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                        message_expiry_interval: w.message_expiry_interval.map(|v| v.get()),
                    },
                }),
                will_delay_interval: delay,
                auth_method: c.auth_method.map(|method| method.to_string()),
                auth_data: c.auth_data.map(|data| data.to_vec()),
            }
        }
        _ => return Err(anyhow!("first packet must be CONNECT")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topic_and_filter_validation() {
        for filter in [
            "#",
            "+",
            "a/+/#",
            "/",
            "a//b",
            "$SYS/#",
            "$share/workers/a/+",
            "$share/workers//finance",
        ] {
            assert!(valid_filter(filter), "{filter}");
        }
        for filter in [
            "",
            "a/#/b",
            "a+",
            "a/+x",
            "a\0",
            "$share//a",
            "$share/g/",
            "$share/g+/a",
        ] {
            assert!(!valid_filter(filter), "{filter}");
        }
        assert!(valid_topic("a//b"));
        assert!(!valid_topic("a/+"));
        assert!(!valid_topic("x\0"));
    }
}
