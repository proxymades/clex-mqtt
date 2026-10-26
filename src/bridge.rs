//! Managed MQTT 5 links. PostgreSQL owns outgoing work; the client owns sockets.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    task::JoinSet,
    time::{Instant, interval, sleep},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    broker::Broker,
    domain::{
        Publication, PublicationProperties, topic_matches, valid_topic_filter, valid_topic_name,
    },
    store::Store,
};

pub const PATH_PROPERTY: &str = "clex-bridge-path";
pub const MAX_HOPS: usize = 8;
pub const MAX_BRIDGES: i64 = 16;

// Deliberately no Debug/Serialize implementations for key material.
#[derive(Clone)]
pub struct SecretKey(Arc<aead::LessSafeKey>);
impl SecretKey {
    pub fn parse(value: &str) -> Result<Self> {
        let bytes = STANDARD
            .decode(value)
            .context("CLEX_BRIDGE_SECRET_KEY must be Base64")?;
        ensure!(
            bytes.len() == 32,
            "CLEX_BRIDGE_SECRET_KEY must contain 32 random bytes"
        );
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &bytes)
            .map_err(|_| anyhow::anyhow!("Invalid bridge encryption key"))?;
        Ok(Self(Arc::new(aead::LessSafeKey::new(key))))
    }
    pub fn seal(&self, id: Uuid, value: &str) -> Result<Vec<u8>> {
        let mut nonce = [0; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("Random source unavailable"))?;
        let mut data = value.as_bytes().to_vec();
        self.0
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(id.as_bytes()),
                &mut data,
            )
            .map_err(|_| anyhow::anyhow!("Bridge encryption failed"))?;
        Ok([nonce.as_slice(), data.as_slice()].concat())
    }
    pub fn open(&self, id: Uuid, sealed: &[u8]) -> Result<String> {
        ensure!(sealed.len() >= 28, "Invalid encrypted bridge credential");
        let nonce: [u8; 12] = sealed[..12].try_into()?;
        let mut data = sealed[12..].to_vec();
        let plain = self
            .0
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(id.as_bytes()),
                &mut data,
            )
            .map_err(|_| anyhow::anyhow!("Bridge credential key mismatch"))?;
        Ok(String::from_utf8(plain.to_vec())?)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Out,
    In,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub direction: Direction,
    pub filter: String,
    pub local_prefix: String,
    pub remote_prefix: String,
}
impl Rule {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_topic_filter(&self.filter) && !self.filter.starts_with("$share/"),
            "Invalid bridge filter"
        );
        for prefix in [&self.local_prefix, &self.remote_prefix] {
            ensure!(
                prefix.is_empty() || (prefix.ends_with('/') && valid_topic_name(prefix)),
                "Prefixes must end with /"
            );
            ensure!(prefix.len() <= 512, "Bridge prefix too long");
        }
        ensure!(self.filter.len() <= 1024, "Bridge filter too long");
        ensure!(
            valid_topic_filter(&format!("{}{}", self.local_prefix, self.filter))
                && valid_topic_filter(&format!("{}{}", self.remote_prefix, self.filter)),
            "Invalid prefixed filter"
        );
        Ok(())
    }
    pub fn map(&self, topic: &str) -> Option<String> {
        let (from, to) = match self.direction {
            Direction::Out => (&self.local_prefix, &self.remote_prefix),
            Direction::In => (&self.remote_prefix, &self.local_prefix),
        };
        let suffix = topic.strip_prefix(from)?;
        // Match the full filter, preserving MQTT's special $ namespace rule.
        if !topic_matches(&format!("{from}{}", self.filter), topic) {
            return None;
        }
        let mapped = format!("{to}{suffix}");
        valid_topic_name(&mapped).then_some(mapped)
    }
}

pub fn can_cross(publication: &Publication, bridge_id: Uuid) -> bool {
    let mut hops = 0;
    for (key, value) in &publication.properties.user_properties {
        if key == PATH_PROPERTY {
            hops += 1;
            if value == &bridge_id.to_string() {
                return false;
            }
        }
    }
    hops < MAX_HOPS
}

struct Link {
    id: Uuid,
    revision: i64,
    remote_revision: i64,
    endpoint: String,
    username: String,
    password: Option<Vec<u8>>,
    ca_pem: Option<String>,
    rules: Vec<Rule>,
}
impl Link {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            revision: row.try_get("revision")?,
            remote_revision: row.try_get("remote_revision")?,
            endpoint: row.try_get("endpoint")?,
            username: row.try_get("username")?,
            password: row.try_get("password")?,
            ca_pem: row.try_get("ca_pem")?,
            rules: serde_json::from_value(row.try_get("rules")?)?,
        })
    }
}

/// A bounded supervisor. Revision changes cancel and join the old socket task
/// before starting its replacement, including disable/delete operations.
pub async fn run(
    store: Store,
    broker: Broker,
    key: Option<SecretKey>,
    max_packet: usize,
    stop: CancellationToken,
) -> Result<()> {
    let worker_lifetime = stop.child_token();
    let _lifetime = worker_lifetime.clone().drop_guard();
    sqlx::query("UPDATE mqtt_bridges SET status=CASE WHEN enabled THEN 'connecting' ELSE 'disabled' END,last_error=NULL").execute(store.pool()).await?;
    let mut workers: HashMap<Uuid, (i64, CancellationToken, tokio::task::JoinHandle<()>)> =
        HashMap::new();
    let mut tick = interval(Duration::from_secs(1));
    let result = async {
        loop {
            tokio::select! { biased; _ = stop.cancelled() => break, _ = tick.tick() => {} }
            let rows = sqlx::query("SELECT * FROM mqtt_bridges WHERE enabled ORDER BY id LIMIT 16")
                .fetch_all(store.pool())
                .await?;
            let links: Vec<_> = rows.iter().map(Link::from_row).collect::<Result<_>>()?;
            let obsolete: Vec<_> = workers
                .iter()
                .filter(|(id, (revision, _, handle))| {
                    handle.is_finished()
                        || !links
                            .iter()
                            .any(|link| link.id == **id && link.revision == *revision)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in obsolete {
                if let Some((_, cancel, handle)) = workers.remove(&id) {
                    cancel.cancel();
                    handle.await?;
                }
            }
            for link in links {
                if workers.contains_key(&link.id) {
                    continue;
                }
                let id = link.id;
                let revision = link.revision;
                let cancel = worker_lifetime.child_token();
                let worker_stop = cancel.clone();
                let worker_store = store.clone();
                let worker_broker = broker.clone();
                let worker_key = key.clone();
                let handle = tokio::spawn(async move {
                    worker(
                        link,
                        worker_store,
                        worker_broker,
                        worker_key,
                        max_packet,
                        worker_stop,
                    )
                    .await;
                });
                workers.insert(id, (revision, cancel, handle));
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut joined = JoinSet::new();
    for (_, (_, cancel, handle)) in workers {
        cancel.cancel();
        joined.spawn(handle);
    }
    while joined.join_next().await.is_some() {}
    sqlx::query(
        "UPDATE mqtt_bridges SET status=CASE WHEN enabled THEN 'stopped' ELSE 'disabled' END",
    )
    .execute(store.pool())
    .await?;
    result
}

async fn state(store: &Store, link: &Link, status: &str, error: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE mqtt_bridges SET status=$3,last_error=$4,connected_at=CASE WHEN $3='connected' THEN now() ELSE connected_at END WHERE id=$1 AND revision=$2 AND enabled")
        .bind(link.id).bind(link.revision).bind(status).bind(error).execute(store.pool()).await?;
    Ok(())
}

async fn worker(
    mut link: Link,
    store: Store,
    broker: Broker,
    key: Option<SecretKey>,
    max_packet: usize,
    stop: CancellationToken,
) {
    let mut delay = 1u64;
    loop {
        if stop.is_cancelled() {
            return;
        }
        let started = Instant::now();
        // Cancellation leaves unacknowledged work for the next connection.
        // Revisions fence imports and acknowledgements always follow commit.
        let attempt = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            result = connect(&link, &store, &broker, key.as_ref(), max_packet) => result,
        };
        if attempt.is_ok() {
            return;
        }
        let message = attempt
            .err()
            .and_then(|e| e.downcast::<LinkError>().ok())
            .map(|e| e.0)
            .unwrap_or(
                "Ошибка связи или хранилища. Повторное подключение выполняется автоматически.",
            );
        let _ = state(&store, &link, "retrying", Some(message)).await;
        if started.elapsed() > Duration::from_secs(30) {
            delay = 1;
        }
        tokio::select! { biased; _ = stop.cancelled() => return, _ = sleep(Duration::from_millis(delay * 1000 + rand::random::<u8>() as u64 * 3)) => {} }
        delay = (delay * 2).min(60);
        if let Ok(Some(row)) = sqlx::query(
            "SELECT remote_revision FROM mqtt_bridges WHERE id=$1 AND revision=$2 AND enabled",
        )
        .bind(link.id)
        .bind(link.revision)
        .fetch_optional(store.pool())
        .await
        {
            link.remote_revision = row.get("remote_revision");
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct LinkError(&'static str);

fn network_error(error: rumqttc::v5::ConnectionError) -> LinkError {
    use rumqttc::v5::{ConnectionError, mqttbytes::v5::ConnectReturnCode};
    LinkError(match error {
        ConnectionError::ConnectionRefused(
            ConnectReturnCode::BadUserNamePassword
            | ConnectReturnCode::NotAuthorized
            | ConnectReturnCode::Banned,
        ) => "Удалённый брокер отклонил вход. Проверьте логин, пароль и доступ учётной записи.",
        ConnectionError::ConnectionRefused(_) => {
            "Удалённый брокер отклонил MQTT 5 подключение. Проверьте возможности сервера и его квоты."
        }
        ConnectionError::Tls(_) => {
            "Ошибка TLS. Проверьте доверенный CA, имя сервера и срок действия сертификата."
        }
        ConnectionError::Io(_) => "Ошибка сети. Проверьте адрес, порт и доступность MQTT-сервера.",
        ConnectionError::Timeout(_) => "Истёк срок ожидания ответа MQTT-сервера.",
        _ => "Ошибка обмена MQTT 5. Проверьте настройки удалённого брокера.",
    })
}

async fn connect(
    link: &Link,
    store: &Store,
    broker: &Broker,
    key: Option<&SecretKey>,
    max_packet: usize,
) -> Result<()> {
    use rumqttc::{
        Outgoing, TlsConfiguration, Transport,
        v5::{
            AsyncClient, Event, MqttOptions,
            mqttbytes::{
                QoS,
                v5::{Filter, Packet, PubAckReason, RetainForwardRule},
            },
        },
    };
    let endpoint = url::Url::parse(&link.endpoint)?;
    let host = endpoint
        .host_str()
        .context("Missing bridge host")?
        .trim_matches(['[', ']']);
    let mut options = MqttOptions::new(
        format!("clex-bridge-{}", link.id),
        host,
        endpoint.port().unwrap_or(if endpoint.scheme() == "mqtts" {
            8883
        } else {
            1883
        }),
    );
    options
        .set_keep_alive(Duration::from_secs(20))
        .set_connection_timeout(10)
        .set_clean_start(link.remote_revision != link.revision)
        .set_session_expiry_interval(Some(86_400))
        .set_manual_acks(true)
        .set_receive_maximum(Some(16))
        .set_max_packet_size(Some(max_packet as u32))
        .set_outgoing_inflight_upper_limit(1);
    if !link.username.is_empty() {
        let password = match &link.password {
            Some(sealed) => key
                .ok_or(LinkError(
                    "Для пароля моста задайте CLEX_BRIDGE_SECRET_KEY.",
                ))?
                .open(link.id, sealed)
                .map_err(|_| LinkError("Проверьте ключ шифрования паролей мостов."))?,
            None => String::new(),
        };
        options.set_credentials(&link.username, password);
    }
    if endpoint.scheme() == "mqtts" {
        // Select ring explicitly; this also works in a process with multiple
        // rustls dependencies and no global crypto provider installed.
        let mut roots = rustls::RootCertStore::empty();
        if let Some(pem) = &link.ca_pem {
            for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
                roots.add(cert?)?;
            }
        } else {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
        }
        ensure!(!roots.is_empty(), "Bridge TLS trust store is empty");
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
            Arc::new(tls),
        )));
    }
    state(store, link, "connecting", None).await?;
    let (client, mut events) = AsyncClient::new(options, 64);
    // poll() includes the entire handshake and socket writes. Keep its future
    // alive across timer ticks, rather than cancelling it in select!.
    let (event_send, mut event_receive) = tokio::sync::mpsc::channel(32);
    let _network = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        loop {
            let event = events.poll().await;
            let failed = event.is_err();
            if event_send.send((Utc::now(), event)).await.is_err() || failed {
                break;
            }
        }
    }));
    let mut connected = false;
    let mut subscribed = false;
    let mut pending: Option<(Uuid, Option<u16>, Instant)> = None;
    let mut tick = interval(Duration::from_millis(100));
    let ready_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        tokio::select! {
            event = event_receive.recv() => {
                let (received_at, event) = event.context("Bridge socket task stopped")?;
                match event.map_err(network_error)? {
                Event::Incoming(Packet::ConnAck(_)) => {
                    connected = true;
                    let filters: Vec<_> = link.rules.iter().filter(|r| r.direction == Direction::In).map(|r| Filter {
                        path: format!("{}{}", r.remote_prefix, r.filter), qos: QoS::AtLeastOnce, nolocal: true, preserve_retain: true, retain_forward_rule: RetainForwardRule::OnNewSubscribe,
                    }).collect();
                    if filters.is_empty() { subscribed = true; mark_ready(store, link).await?; }
                    else { client.subscribe_many(filters).await?; }
                }
                Event::Incoming(Packet::SubAck(ack)) => {
                    use rumqttc::v5::mqttbytes::v5::SubscribeReasonCode;
                    ensure!(ack.return_codes.len() == link.rules.iter().filter(|r| r.direction == Direction::In).count(), "Invalid bridge SUBACK");
                    if ack.return_codes.iter().any(|r| !matches!(r, SubscribeReasonCode::Success(QoS::AtLeastOnce))) {
                        return Err(LinkError("Удалённый брокер отклонил подписку QoS 1. Проверьте права на топики.").into());
                    }
                    subscribed = true; mark_ready(store, link).await?;
                }
                Event::Incoming(Packet::Publish(publish)) => {
                    ensure!(connected && publish.qos != QoS::ExactlyOnce, "Unexpected bridge QoS");
                    let topic = std::str::from_utf8(&publish.topic)?;
                    let props = publish.properties.clone().unwrap_or_default();
                    let expires_at = props.message_expiry_interval.map(|secs| received_at + chrono::Duration::seconds(secs.into()));
                    let mut publication = Publication {
                        topic: topic.to_owned(), payload: publish.payload.to_vec(), qos: if publish.qos == QoS::AtMostOnce {0} else {1}, retain: publish.retain, expires_at,
                        properties: PublicationProperties { payload_format_indicator: props.payload_format_indicator, content_type: props.content_type, response_topic: props.response_topic, correlation_data: props.correlation_data.map(|b| b.to_vec()), user_properties: props.user_properties, message_expiry_interval: props.message_expiry_interval },
                    };
                    if can_cross(&publication, link.id) {
                        publication.properties.user_properties.push((PATH_PROPERTY.to_owned(), link.id.to_string()));
                        // One transaction imports all mappings, then the remote
                        // QoS 1 acknowledgement is allowed into the socket loop.
                        broker.import_bridge(link.id, link.revision, &link.rules, &publication).await?;
                    }
                    client.ack(&publish).await?;
                }
                Event::Outgoing(Outgoing::Publish(pkid)) => { if let Some((_, packet_id, _)) = &mut pending { *packet_id = Some(pkid); } }
                Event::Incoming(Packet::PubAck(ack)) => {
                    if let Some((id, Some(pkid), _)) = pending && pkid == ack.pkid {
                            if !matches!(ack.reason, PubAckReason::Success | PubAckReason::NoMatchingSubscribers) {
                                return Err(LinkError("Удалённый брокер отклонил публикацию. Проверьте права и квоты.").into());
                            }
                            sqlx::query("WITH done AS (DELETE FROM bridge_deliveries WHERE id=$1 AND bridge_id=$2 RETURNING id) UPDATE mqtt_bridges SET sent=sent+(SELECT count(*) FROM done) WHERE id=$2").bind(id).bind(link.id).execute(store.pool()).await?;
                            pending = None;
                    }
                }
                Event::Incoming(Packet::Disconnect(_)) => return Err(LinkError("Удалённый брокер завершил подключение. Выполняется повторная попытка.").into()),
                _ => {}
            }},
            _ = tick.tick() => {
                if !subscribed && Instant::now() > ready_deadline { return Err(LinkError("Истёк срок ожидания подключения или подписки.").into()); }
                if pending.as_ref().is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(30)) { return Err(LinkError("Истёк срок ожидания подтверждения публикации.").into()); }
                if connected && subscribed && pending.is_none()
                    && let Some((id, publication)) = store.next_bridge_delivery(link.id, link.revision).await? {
                        let properties = outgoing_properties(&publication, link.id);
                        client.publish_with_properties(&publication.topic, QoS::AtLeastOnce, publication.retain, publication.payload, properties).await?;
                        pending = Some((id, None, Instant::now()));
                }
            }
        }
    }
}

async fn mark_ready(store: &Store, link: &Link) -> Result<()> {
    sqlx::query(
        "UPDATE mqtt_bridges SET remote_revision=$2 WHERE id=$1 AND revision=$2 AND enabled",
    )
    .bind(link.id)
    .bind(link.revision)
    .execute(store.pool())
    .await?;
    state(store, link, "connected", None).await
}

fn outgoing_properties(
    publication: &Publication,
    id: Uuid,
) -> rumqttc::v5::mqttbytes::v5::PublishProperties {
    let props = &publication.properties;
    let mut user_properties = props.user_properties.clone();
    user_properties.push((PATH_PROPERTY.to_owned(), id.to_string()));
    rumqttc::v5::mqttbytes::v5::PublishProperties {
        payload_format_indicator: props.payload_format_indicator,
        content_type: props.content_type.clone(),
        response_topic: props.response_topic.clone(),
        correlation_data: props.correlation_data.clone().map(Into::into),
        user_properties,
        message_expiry_interval: publication.expires_at.map(|at: DateTime<Utc>| {
            ((at - Utc::now()).num_milliseconds().max(1) as u64)
                .div_ceil(1000)
                .min(u32::MAX as u64) as u32
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_bound_to_key_and_bridge() {
        let key = SecretKey::parse(&STANDARD.encode([41; 32])).unwrap();
        let id = Uuid::new_v4();
        let encrypted = key.seal(id, "fixture-password").unwrap();
        assert_eq!(key.open(id, &encrypted).unwrap(), "fixture-password");
        assert!(key.open(Uuid::new_v4(), &encrypted).is_err());
        assert!(
            SecretKey::parse(&STANDARD.encode([42; 32]))
                .unwrap()
                .open(id, &encrypted)
                .is_err()
        );
        assert_ne!(encrypted, key.seal(id, "fixture-password").unwrap());
    }
    #[test]
    fn prefix_mapping_obeys_mqtt_namespace() {
        let rule = Rule {
            direction: Direction::Out,
            filter: "#".into(),
            local_prefix: "site/".into(),
            remote_prefix: "hub/".into(),
        };
        rule.validate().unwrap();
        assert_eq!(rule.map("site/sensor/1"), Some("hub/sensor/1".into()));
        assert_eq!(rule.map("site/$value"), Some("hub/$value".into()));
        assert!(rule.map("other/sensor").is_none());
        let rule = Rule {
            local_prefix: String::new(),
            ..rule
        };
        assert!(rule.map("$SYS/uptime").is_none());
    }
}
