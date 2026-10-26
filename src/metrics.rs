//! Bounded operational metrics. Label values are fixed product dimensions.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use chrono::Utc;
use prometheus_client::{
    encoding::text::encode,
    metrics::{
        counter::{ConstCounter, Counter},
        family::Family,
        gauge::ConstGauge,
        histogram::Histogram,
    },
    registry::Registry,
};
use sqlx::{PgPool, Row};
use subtle::ConstantTimeEq;

use crate::{auth, broker::BrokerMetrics, monitor::MonitorStats};

pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";
pub const CACHE_SECONDS: u64 = 5;

// Neither the token nor its digest belongs in a public configuration view.
#[derive(Clone)]
pub struct TokenDigest(Vec<u8>);
impl TokenDigest {
    pub fn parse(token: &str) -> Result<Self> {
        ensure!(
            (32..=256).contains(&token.len())
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "CLEX_METRICS_TOKEN must contain 32-256 URL-safe random characters"
        );
        Ok(Self(auth::token_hash(token)))
    }
    pub fn accepts(&self, header: &str) -> bool {
        if header.len() > 512 {
            return false;
        }
        let Some((scheme, token)) = header.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Bearer") && bool::from(self.0.ct_eq(&auth::token_hash(token)))
    }
}

#[derive(Clone)]
pub struct StorageMetrics {
    pub lock_wait: Histogram,
    pub commit_time: Histogram,
    pub commits: Counter,
    pub commit_errors: Counter,
    pub unknown_commits: Counter,
}
impl Default for StorageMetrics {
    fn default() -> Self {
        let buckets = [
            0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
        ];
        Self {
            lock_wait: Histogram::new(buckets),
            commit_time: Histogram::new(buckets),
            commits: Counter::default(),
            commit_errors: Counter::default(),
            unknown_commits: Counter::default(),
        }
    }
}

#[derive(Clone, Default)]
pub struct DatabaseSnapshot {
    pub timestamp: f64,
    pub sessions: i64,
    pub subscriptions: i64,
    pub retained: i64,
    pub queues: [i64; 4],
    pub queue_bytes: i64,
    pub incoming_qos2: i64,
    pub bridges: i64,
    pub bridges_enabled: i64,
    pub bridges_connected: i64,
    pub bridge_queue: i64,
    pub bridge_queue_bytes: i64,
}
impl DatabaseSnapshot {
    async fn collect(pool: &PgPool) -> Result<Self> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout='1500ms'")
            .execute(&mut *tx)
            .await?;
        // One statement uses one MVCC snapshot. No broker advisory lock, user
        // labels, payload previews, or unbounded result sets are involved.
        let row = sqlx::query("SELECT (SELECT count(*) FROM sessions) AS sessions,(SELECT count(*) FROM subscriptions) AS subscriptions,(SELECT count(*) FROM retained) AS retained,(SELECT count(*) FROM deliveries WHERE phase='queued') AS queued,(SELECT count(*) FROM deliveries WHERE phase='await_puback') AS puback,(SELECT count(*) FROM deliveries WHERE phase='await_pubrec') AS pubrec,(SELECT count(*) FROM deliveries WHERE phase='await_pubcomp') AS pubcomp,(SELECT coalesce(sum(octet_length(m.payload)+octet_length(m.topic)+octet_length(m.properties::text)),0)::bigint FROM deliveries d JOIN messages m ON m.id=d.message_id) AS queue_bytes,(SELECT count(*) FROM incoming_qos2 WHERE NOT released) AS incoming_qos2,(SELECT count(*) FROM mqtt_bridges) AS bridges,(SELECT count(*) FROM mqtt_bridges WHERE enabled) AS bridges_enabled,(SELECT count(*) FROM mqtt_bridges WHERE enabled AND status='connected') AS bridges_connected,(SELECT count(*) FROM bridge_deliveries) AS bridge_queue,(SELECT coalesce(sum(octet_length(m.payload)+octet_length(d.topic)+octet_length(m.properties::text)),0)::bigint FROM bridge_deliveries d JOIN messages m ON m.id=d.message_id) AS bridge_queue_bytes")
            .fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(Self {
            timestamp: Utc::now().timestamp_millis() as f64 / 1000.0,
            sessions: row.get("sessions"),
            subscriptions: row.get("subscriptions"),
            retained: row.get("retained"),
            queues: [
                row.get("queued"),
                row.get("puback"),
                row.get("pubrec"),
                row.get("pubcomp"),
            ],
            queue_bytes: row.get("queue_bytes"),
            incoming_qos2: row.get("incoming_qos2"),
            bridges: row.get("bridges"),
            bridges_enabled: row.get("bridges_enabled"),
            bridges_connected: row.get("bridges_connected"),
            bridge_queue: row.get("bridge_queue"),
            bridge_queue_bytes: row.get("bridge_queue_bytes"),
        })
    }
}

#[derive(Clone, Default)]
pub struct DatabaseView {
    pub snapshot: Option<DatabaseSnapshot>,
    pub healthy: bool,
    attempted: Option<Instant>,
}
#[derive(Default)]
pub struct Exporter {
    view: Mutex<DatabaseView>,
    collecting: tokio::sync::Mutex<()>,
}
impl Exporter {
    pub async fn database(&self, pool: &PgPool) -> Option<DatabaseView> {
        let current = self.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if current
            .attempted
            .is_some_and(|at| at.elapsed() < Duration::from_secs(CACHE_SECONDS))
        {
            return Some(current);
        }
        let Ok(_guard) = self.collecting.try_lock() else {
            // A cached snapshot can be returned immediately; the first scrape
            // gets a bounded busy response rather than an unbounded wait queue.
            return current.attempted.map(|_| current);
        };
        // Another worker may have completed between the initial cache read
        // and acquiring the collection guard.
        let latest = self.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if latest
            .attempted
            .is_some_and(|at| at.elapsed() < Duration::from_secs(CACHE_SECONDS))
        {
            return Some(latest);
        }
        let collected =
            tokio::time::timeout(Duration::from_secs(2), DatabaseSnapshot::collect(pool)).await;
        let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        view.attempted = Some(Instant::now());
        view.healthy = matches!(&collected, Ok(Ok(_)));
        if let Ok(Ok(snapshot)) = collected {
            view.snapshot = Some(snapshot);
        }
        Some(view.clone())
    }
}

pub fn render(
    broker: &BrokerMetrics,
    storage: &StorageMetrics,
    monitor: MonitorStats,
    db: &DatabaseView,
    pool: &PgPool,
    uptime: Duration,
) -> Result<String> {
    let mut registry = Registry::with_prefix("clex_mqtt");
    registry.register(
        "build",
        "Broker build version",
        prometheus_client::metrics::info::Info::new(vec![("version", env!("CARGO_PKG_VERSION"))]),
    );
    registry.register(
        "uptime_seconds",
        "Process uptime in seconds",
        ConstGauge::new(uptime.as_secs_f64()),
    );
    registry.register(
        "connections",
        "Currently authenticated MQTT connections",
        ConstGauge::new(broker.connected_clients as i64),
    );
    for (name, help, value) in [
        (
            "connection_attempts",
            "MQTT socket tasks admitted to handshake",
            broker.connections_total,
        ),
        (
            "connections_authenticated",
            "Successful MQTT handshakes including takeovers",
            broker.connections_authenticated,
        ),
        (
            "handshake_failures",
            "Failed or timed out MQTT handshakes",
            broker.handshake_failures,
        ),
        (
            "connection_capacity_rejections",
            "Socket tasks rejected by the connection limit",
            broker.capacity_rejections,
        ),
        (
            "packets_received",
            "Decoded MQTT packets received including control packets",
            broker.messages_received,
        ),
        (
            "packets_sent",
            "MQTT packets successfully written including control packets",
            broker.messages_sent,
        ),
        (
            "publish_packets_accepted",
            "Accepted PUBLISH packets including QoS retransmissions",
            broker.publish_accepted,
        ),
        (
            "publish_packets_rejected",
            "Publication stages rejected by ACL or quota",
            broker.publish_rejected,
        ),
        (
            "wire_bytes_received",
            "Decoded MQTT wire bytes received",
            broker.bytes_received,
        ),
        (
            "wire_bytes_sent",
            "MQTT wire bytes successfully written",
            broker.bytes_sent,
        ),
        (
            "publications_committed",
            "Committed publication events including bridge imports and Wills",
            monitor.total,
        ),
    ] {
        registry.register(name, help, ConstCounter::new(value));
    }
    registry.register(
        "storage_lock_wait_seconds",
        "Wait to start a state transaction including pool and advisory lock",
        storage.lock_wait.clone(),
    );
    registry.register(
        "storage_commit_seconds",
        "Duration of completed state COMMIT attempts",
        storage.commit_time.clone(),
    );
    registry.register(
        "storage_commits",
        "Confirmed state transaction commits",
        storage.commits.clone(),
    );
    registry.register(
        "storage_commit_errors",
        "State commit attempts returning an error including unknown outcomes",
        storage.commit_errors.clone(),
    );
    registry.register(
        "storage_commit_unknown",
        "State commits whose result requires reconciliation",
        storage.unknown_commits.clone(),
    );
    registry.register(
        "database_up",
        "Result of the latest database metrics collection",
        ConstGauge::new(i64::from(db.healthy)),
    );
    registry.register(
        "database_snapshot_timestamp_seconds",
        "Unix time of the last successful database metrics snapshot",
        ConstGauge::new(db.snapshot.as_ref().map_or(0.0, |s| s.timestamp)),
    );
    registry.register(
        "database_pool_connections",
        "Connections currently managed by the application pool",
        ConstGauge::new(i64::from(pool.size())),
    );
    registry.register(
        "database_pool_idle_connections",
        "Idle connections in the application pool",
        ConstGauge::new(pool.num_idle() as i64),
    );
    registry.register(
        "monitor_events",
        "Events currently in the bounded publication preview",
        ConstGauge::new(monitor.entries as i64),
    );
    registry.register(
        "monitor_accounted_bytes",
        "Accounted preview memory including metadata",
        ConstGauge::new(monitor.bytes as i64),
    );
    if let Some(snapshot) = &db.snapshot {
        for (name, help, value) in [
            ("stored_sessions", "Stored MQTT sessions", snapshot.sessions),
            (
                "subscriptions",
                "Stored MQTT subscriptions",
                snapshot.subscriptions,
            ),
            (
                "retained_messages",
                "Stored Retain topics",
                snapshot.retained,
            ),
            (
                "delivery_queue_bytes",
                "Accounted payload topic and property bytes in all delivery phases",
                snapshot.queue_bytes,
            ),
            (
                "incoming_qos2",
                "Stored incoming QoS 2 publications awaiting PUBREL",
                snapshot.incoming_qos2,
            ),
            ("bridges", "Configured MQTT bridges", snapshot.bridges),
            (
                "bridges_enabled",
                "Enabled MQTT bridges",
                snapshot.bridges_enabled,
            ),
            (
                "bridges_connected",
                "Enabled MQTT bridges last reported as connected",
                snapshot.bridges_connected,
            ),
            (
                "bridge_queue_messages",
                "Stored bridge outgoing obligations",
                snapshot.bridge_queue,
            ),
            (
                "bridge_queue_bytes",
                "Accounted payload topic and property bytes in bridge queues",
                snapshot.bridge_queue_bytes,
            ),
        ] {
            registry.register(name, help, ConstGauge::new(value));
        }
        let queues: Family<Vec<(&str, &str)>, prometheus_client::metrics::gauge::Gauge> =
            Family::default();
        for (state, count) in ["queued", "await_puback", "await_pubrec", "await_pubcomp"]
            .into_iter()
            .zip(snapshot.queues)
        {
            queues.get_or_create(&vec![("state", state)]).set(count);
        }
        registry.register(
            "delivery_queue_messages",
            "Stored MQTT deliveries by protocol phase",
            queues,
        );
    }
    let mut output = String::with_capacity(12 * 1024);
    encode(&mut output, &registry)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bearer_tokens_require_exact_secret_and_support_http_scheme_case() {
        let token = "test_metrics_token_0123456789_abcdefghijklmnopqrstuvwxyz";
        let digest = TokenDigest::parse(token).unwrap();
        assert!(digest.accepts(&format!("Bearer {token}")));
        assert!(digest.accepts(&format!("bearer {token}")));
        for value in [
            token.to_owned(),
            format!("Basic {token}"),
            format!("Bearer {token} "),
            format!("Bearer {token}wrong"),
        ] {
            assert!(!digest.accepts(&value));
        }
        assert!(TokenDigest::parse("short").is_err());
        assert!(TokenDigest::parse(&" ".repeat(32)).is_err());
    }
}
