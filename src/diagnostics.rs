//! Opt-in, process-local message tracing. No payloads or database writes.
use crate::domain::{Delivery, Publication, topic_matches, valid_topic_filter};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const CAPACITY: usize = 100;
const EVENTS: usize = 128;
const MEMORY: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TraceFilter {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub topic: String,
    pub seconds: u32,
}
impl TraceFilter {
    pub fn valid(&self) -> bool {
        (1..=300).contains(&self.seconds)
            && self.client_id.len() <= 1024
            && self.username.len() <= 128
            && self.topic.len() <= 1024
            && (!self.client_id.is_empty() || !self.username.is_empty() || !self.topic.is_empty())
            && (self.topic.is_empty() || valid_topic_filter(&self.topic))
    }
    fn matches(&self, p: &Publication, client: &[u8], username: &str) -> bool {
        (self.client_id.is_empty() || self.client_id.as_bytes() == client)
            && (self.username.is_empty() || self.username == username)
            && (self.topic.is_empty() || topic_matches(&self.topic, &p.topic))
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceEvent {
    pub elapsed_ms: f64,
    pub at: DateTime<Utc>,
    pub code: &'static str,
    pub delivery_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub client_id: Option<String>,
    pub username: Option<String>,
    pub packet_id: Option<u16>,
    pub detail: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceMessage {
    pub id: Uuid,
    pub received_at: DateTime<Utc>,
    pub client_id: String,
    pub username: String,
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub payload_bytes: usize,
    pub status: &'static str,
    pub queued: usize,
    pub truncated: usize,
    pub metadata_truncated: bool,
    pub events: Vec<TraceEvent>,
}
struct Entry {
    view: TraceMessage,
    started: Instant,
    deliveries: Vec<Uuid>,
    bytes: usize,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceSummary {
    pub id: Uuid,
    pub received_at: DateTime<Utc>,
    pub client_id: String,
    pub username: String,
    pub topic: String,
    pub qos: u8,
    pub status: &'static str,
    pub queued: usize,
    pub truncated: usize,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TracePage {
    pub instance_id: Uuid,
    pub id: Option<Uuid>,
    pub active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub filter: Option<TraceFilter>,
    pub capacity: usize,
    pub event_capacity: usize,
    pub bytes: usize,
    pub evicted: u64,
    pub messages: Vec<TraceSummary>,
}
#[derive(Default)]
struct Buffer {
    id: Option<Uuid>,
    filter: Option<TraceFilter>,
    deadline: Option<Instant>,
    expires_at: Option<DateTime<Utc>>,
    entries: VecDeque<Entry>,
    deliveries: HashMap<Uuid, Uuid>,
    bytes: usize,
    evicted: u64,
}
impl Buffer {
    fn active(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() < d)
    }
    fn trim(&mut self) {
        while self.entries.len() > CAPACITY || self.bytes > MEMORY {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(entry.bytes);
            for id in entry.deliveries {
                self.deliveries.remove(&id);
            }
            self.evicted += 1;
        }
    }
    fn push(&mut self, id: Uuid, mut event: TraceEvent, once: bool) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.view.id == id) else {
            return;
        };
        if once
            && entry.view.events.iter().any(|e| {
                e.code == event.code
                    && e.delivery_id == event.delivery_id
                    && e.session_id == event.session_id
            })
        {
            return;
        }
        if entry.view.events.len() == EVENTS {
            entry.view.truncated += 1;
            return;
        }
        event.elapsed_ms = entry.started.elapsed().as_secs_f64() * 1000.;
        let bytes = 512
            + event.client_id.as_ref().map_or(0, String::len)
            + event.username.as_ref().map_or(0, String::len)
            + event.detail.as_ref().map_or(0, String::len);
        entry.bytes += bytes;
        self.bytes += bytes;
        entry.view.events.push(event);
        self.trim();
    }
}
fn short(s: &str) -> String {
    s.chars().take(256).collect()
}
fn event(code: &'static str) -> TraceEvent {
    TraceEvent {
        elapsed_ms: 0.,
        at: Utc::now(),
        code,
        delivery_id: None,
        session_id: None,
        client_id: None,
        username: None,
        packet_id: None,
        detail: None,
    }
}
#[derive(Clone)]
pub struct Diagnostics {
    instance: Uuid,
    enabled: Arc<AtomicBool>,
    inner: Arc<Mutex<Buffer>>,
}
impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            instance: Uuid::new_v4(),
            enabled: Default::default(),
            inner: Default::default(),
        }
    }
}
impl Diagnostics {
    /// One observer at a time; a new observer starts with a fresh bounded buffer.
    pub fn start(&self, filter: TraceFilter) -> Result<Uuid, &'static str> {
        if !filter.valid() {
            return Err("invalid");
        }
        let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if b.active() {
            return Err("active");
        }
        let id = Uuid::new_v4();
        *b = Buffer {
            id: Some(id),
            deadline: Some(Instant::now() + Duration::from_secs(filter.seconds.into())),
            expires_at: Some(Utc::now() + chrono::Duration::seconds(filter.seconds.into())),
            filter: Some(filter),
            ..Default::default()
        };
        self.enabled.store(true, Ordering::Release);
        Ok(id)
    }
    pub fn stop(&self, id: Uuid) -> bool {
        let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if b.id != Some(id) {
            return false;
        }
        b.deadline = None;
        self.enabled.store(false, Ordering::Release);
        true
    }
    fn active_buffer(&self) -> Option<std::sync::MutexGuard<'_, Buffer>> {
        if !self.enabled.load(Ordering::Acquire) {
            return None;
        }
        let b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !b.active() {
            self.enabled.store(false, Ordering::Release);
            return None;
        }
        Some(b)
    }
    pub fn capture(
        &self,
        id: Uuid,
        p: &Publication,
        client: &[u8],
        username: &str,
    ) -> Option<Capture> {
        self.capture_at(id, p, client, username, Instant::now())
    }
    pub(crate) fn capture_at(
        &self,
        id: Uuid,
        p: &Publication,
        client: &[u8],
        username: &str,
        started: Instant,
    ) -> Option<Capture> {
        let mut b = self.active_buffer()?;
        if !b.filter.as_ref()?.matches(p, client, username) {
            return None;
        }
        // A persistent QoS 2 UUID correlates reception and later PUBREL.
        if !b.entries.iter().any(|e| e.view.id == id) {
            let client = String::from_utf8_lossy(client);
            let metadata_truncated = client.chars().count() > 256 || p.topic.chars().count() > 256;
            let view = TraceMessage {
                id,
                received_at: Utc::now()
                    - chrono::Duration::from_std(started.elapsed()).unwrap_or_default(),
                client_id: short(&client),
                username: short(username),
                topic: short(&p.topic),
                qos: p.qos,
                retain: p.retain,
                payload_bytes: p.payload.len(),
                status: "processing",
                queued: 0,
                truncated: 0,
                metadata_truncated,
                events: vec![],
            };
            let bytes = 1024 + view.client_id.len() + view.username.len() + view.topic.len();
            b.bytes += bytes;
            b.entries.push_back(Entry {
                view,
                started,
                deliveries: vec![],
                bytes,
            });
            b.push(id, event("received"), false);
            if let Some(entry) = b.entries.back_mut()
                && let Some(first) = entry.view.events.first_mut()
            {
                first.at = entry.view.received_at;
                first.elapsed_ms = 0.;
            }

            b.trim();
        }
        Some(Capture {
            tracer: self.clone(),
            observer: b.id?,
            id,
            settled: false,
        })
    }
    pub fn resume(&self, id: Uuid) -> Option<Capture> {
        let b = self.active_buffer()?;
        b.entries.iter().find(|e| e.view.id == id)?;
        Some(Capture {
            tracer: self.clone(),
            observer: b.id?,
            id,
            settled: false,
        })
    }
    pub fn delivery(
        &self,
        id: Uuid,
        code: &'static str,
        client: &str,
        username: &str,
        packet: Option<u16>,
    ) {
        let Some(mut b) = self.active_buffer() else {
            return;
        };
        let Some(message) = b.deliveries.get(&id).copied() else {
            return;
        };
        let mut e = event(code);
        e.delivery_id = Some(id);
        e.client_id = Some(short(client));
        e.username = Some(short(username));
        e.packet_id = packet;
        b.push(
            message,
            e,
            matches!(code, "receive_maximum" | "acl_allowed"),
        );
    }
    pub fn session_event(&self, session: Uuid, code: &'static str, client: &str, username: &str) {
        let Some(mut b) = self.active_buffer() else {
            return;
        };
        let targets: Vec<_> = b
            .entries
            .iter()
            .flat_map(|entry| {
                entry
                    .view
                    .events
                    .iter()
                    .filter(|e| e.code == "queue_prepared" && e.session_id == Some(session))
                    .filter(|queued| {
                        code != "receive_maximum"
                            || !entry.view.events.iter().any(|e| {
                                e.delivery_id == queued.delivery_id
                                    && matches!(
                                        e.code,
                                        "publish_sent"
                                            | "publish_retried"
                                            | "pubrec"
                                            | "pubrel_sent"
                                            | "qos0_written"
                                    )
                            })
                    })
                    .filter(|queued| {
                        !entry.view.events.iter().any(|e| {
                            e.delivery_id == queued.delivery_id
                                && matches!(
                                    e.code,
                                    "puback"
                                        | "pubcomp"
                                        | "qos0_written"
                                        | "expired"
                                        | "packet_too_large"
                                        | "delivery_acl_denied"
                                        | "puback_rejected"
                                        | "pubrec_rejected"
                                        | "pubcomp_rejected"
                                )
                        })
                    })
                    .map(|e| (entry.view.id, e.delivery_id))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (message, delivery) in targets {
            let mut e = event(code);
            e.delivery_id = delivery;
            e.session_id = Some(session);
            e.client_id = Some(short(client));
            e.username = Some(short(username));
            b.push(message, e, true);
        }
    }
    pub(crate) fn observer(&self) -> Option<Uuid> {
        self.active_buffer()?.id
    }
    pub(crate) fn expired(&self, observation: (Option<Uuid>, Vec<Uuid>, bool)) {
        let Some(mut b) = self.active_buffer() else {
            return;
        };
        if b.id != observation.0 {
            return;
        }
        if observation.2 {
            for entry in &mut b.entries {
                entry.view.truncated += 1;
            }
        }
        for id in observation.1 {
            if let Some(message) = b.deliveries.get(&id).copied() {
                let mut e = event("expired");
                e.delivery_id = Some(id);
                b.push(message, e, true);
            }
        }
    }
    pub fn snapshot(&self) -> TracePage {
        let b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        TracePage {
            instance_id: self.instance,
            id: b.id,
            active: b.active(),
            expires_at: b.expires_at,
            filter: b.filter.clone(),
            capacity: CAPACITY,
            event_capacity: EVENTS,
            bytes: b.bytes,
            evicted: b.evicted,
            messages: b
                .entries
                .iter()
                .rev()
                .map(|e| TraceSummary {
                    id: e.view.id,
                    received_at: e.view.received_at,
                    client_id: e.view.client_id.clone(),
                    username: e.view.username.clone(),
                    topic: e.view.topic.clone(),
                    qos: e.view.qos,
                    status: e.view.status,
                    queued: e.view.queued,
                    truncated: e.view.truncated,
                })
                .collect(),
        }
    }
    pub fn message(&self, id: Uuid) -> Option<TraceMessage> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .find(|e| e.view.id == id)
            .map(|e| e.view.clone())
    }
}
pub struct Capture {
    tracer: Diagnostics,
    observer: Uuid,
    pub id: Uuid,
    settled: bool,
}
impl Capture {
    pub fn note(&self, code: &'static str, session: Option<Uuid>, detail: Option<&str>) {
        let Some(mut b) = self.tracer.active_buffer() else {
            return;
        };
        if b.id != Some(self.observer) {
            return;
        }
        let mut e = event(code);
        e.session_id = session;
        e.detail = detail.map(short);
        b.push(self.id, e, false);
    }
    /// Register before COMMIT so a concurrently polled connection can be observed.
    /// This event describes preparation; only `committed` confirms durable queues.
    pub fn prepare(&self, deliveries: &[Delivery]) {
        let Some(mut b) = self.tracer.active_buffer() else {
            return;
        };
        if b.id != Some(self.observer) {
            return;
        }
        let Some(entry) = b.entries.iter_mut().find(|e| e.view.id == self.id) else {
            return;
        };
        entry.view.queued = deliveries.len();
        let mut binding_bytes = 0;
        for d in deliveries.iter().take(EVENTS) {
            if !entry.deliveries.contains(&d.id) {
                entry.deliveries.push(d.id);
                binding_bytes += 128;
            }
        }
        entry.bytes += binding_bytes;
        b.bytes += binding_bytes;
        for d in deliveries.iter().take(EVENTS) {
            b.deliveries.insert(d.id, self.id);
            let mut e = event("queue_prepared");
            e.delivery_id = Some(d.id);
            e.session_id = Some(d.session_id);
            e.detail = Some(format!("QoS {}", d.publication.qos));
            b.push(self.id, e, false);
        }
        if let Some(entry) = b.entries.iter_mut().find(|e| e.view.id == self.id) {
            entry.view.truncated += deliveries.len().saturating_sub(EVENTS);
        }
        b.trim();
    }
    pub fn finish(mut self, status: &'static str) {
        self.settled = true;
        self.note(status, None, None);
        if let Some(mut b) = self.tracer.active_buffer()
            && b.id == Some(self.observer)
            && let Some(entry) = b.entries.iter_mut().find(|e| e.view.id == self.id)
        {
            entry.view.status = status;
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.settled {
            self.note("interrupted", None, None);
            if let Some(mut b) = self.tracer.active_buffer()
                && b.id == Some(self.observer)
                && let Some(entry) = b.entries.iter_mut().find(|e| e.view.id == self.id)
            {
                entry.view.status = "interrupted";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn publication() -> Publication {
        Publication {
            topic: "devices/a/state".into(),
            payload: vec![42; 4096],
            qos: 1,
            retain: false,
            expires_at: None,
            properties: Default::default(),
        }
    }
    fn filter() -> TraceFilter {
        TraceFilter {
            client_id: "esp32-01".into(),
            username: "ivan".into(),
            topic: "devices/+/state".into(),
            seconds: 60,
        }
    }
    #[test]
    fn scoped_filters_and_disabled_capture_keep_payloads_out() {
        let t = Diagnostics::default();
        let p = publication();
        assert!(t.capture(Uuid::new_v4(), &p, b"esp32-01", "ivan").is_none());
        t.start(filter()).unwrap();
        assert!(t.capture(Uuid::new_v4(), &p, b"esp32-01", "alex").is_none());
        assert!(t.capture(Uuid::new_v4(), &p, b"other", "ivan").is_none());
        let id = Uuid::new_v4();
        t.capture(id, &p, b"esp32-01", "ivan")
            .unwrap()
            .finish("committed");
        let json = serde_json::to_value(t.message(id).unwrap()).unwrap();
        assert_eq!(json["payloadBytes"], 4096);
        assert!(json.get("payload").is_none());
        assert_eq!(t.snapshot().messages.len(), 1);
        assert!(
            !TraceFilter {
                topic: "a/#/b".into(),
                ..filter()
            }
            .valid()
        );
        assert!(
            !TraceFilter {
                seconds: 301,
                ..filter()
            }
            .valid()
        );
    }
    #[test]
    fn stop_deadline_and_observer_generation_fence_late_events() {
        let t = Diagnostics::default();
        let p = publication();
        let id = Uuid::new_v4();
        let first = t.start(filter()).unwrap();
        let late = t.capture(id, &p, b"esp32-01", "ivan").unwrap();
        assert!(t.stop(first));
        t.start(filter()).unwrap();
        t.capture(id, &p, b"esp32-01", "ivan")
            .unwrap()
            .finish("committed");
        drop(late);
        assert_eq!(t.message(id).unwrap().status, "committed");
        assert!(!t.stop(first));
        assert!(t.snapshot().active);
        t.inner.lock().unwrap().deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(t.capture(Uuid::new_v4(), &p, b"esp32-01", "ivan").is_none());
        assert!(!t.snapshot().active);
        assert_eq!(t.snapshot().messages.len(), 1);
    }
    #[test]
    fn limits_bound_events_memory_and_delivery_indexes() {
        let t = Diagnostics::default();
        t.start(filter()).unwrap();
        let p = publication();
        for _ in 0..150 {
            let id = Uuid::new_v4();
            let trace = t.capture(id, &p, b"esp32-01", "ivan").unwrap();
            let d = Delivery {
                id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                sequence: 1,
                publication: p.clone(),
                packet_id: None,
                phase: crate::domain::DeliveryPhase::Queued,
                subscription_identifiers: vec![],
            };
            trace.prepare(&[d]);
            for _ in 0..140 {
                trace.note("test", None, Some(&"Ж".repeat(512)));
            }
            trace.finish("committed");
        }
        let page = t.snapshot();
        assert!(page.messages.len() <= CAPACITY);
        assert!(page.bytes <= MEMORY);
        assert!(page.evicted > 0);
        let b = t.inner.lock().unwrap();
        assert!(
            b.entries
                .iter()
                .all(|e| e.view.events.len() == EVENTS && e.view.truncated > 0)
        );
        assert!(
            b.deliveries
                .values()
                .all(|id| b.entries.iter().any(|e| e.view.id == *id))
        );
        assert_eq!(b.bytes, b.entries.iter().map(|e| e.bytes).sum::<usize>());
    }
    #[test]
    fn timing_includes_storage_wait_and_interruption_is_explicit() {
        let t = Diagnostics::default();
        t.start(filter()).unwrap();
        let id = Uuid::new_v4();
        let capture = t
            .capture_at(
                id,
                &publication(),
                b"esp32-01",
                "ivan",
                Instant::now() - Duration::from_millis(50),
            )
            .unwrap();
        capture.note("session_locked", None, None);
        drop(capture);
        let trace = t.message(id).unwrap();
        assert_eq!(trace.events[0].elapsed_ms, 0.);
        assert!(trace.events[1].elapsed_ms >= 50.);
        assert_eq!(trace.status, "interrupted");
        assert!(!trace.events.iter().any(|e| e.code == "committed"));
    }

    #[test]
    fn receive_window_wait_applies_to_unsent_deliveries_and_stops_after_ack() {
        let t = Diagnostics::default();
        t.start(filter()).unwrap();
        let id = Uuid::new_v4();
        let p = publication();
        let d = Delivery {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence: 1,
            publication: p.clone(),
            packet_id: None,
            phase: crate::domain::DeliveryPhase::Queued,
            subscription_identifiers: vec![],
        };
        let capture = t.capture(id, &p, b"esp32-01", "ivan").unwrap();
        capture.prepare(std::slice::from_ref(&d));
        capture.finish("committed");
        t.session_event(d.session_id, "receive_maximum", "reader", "alex");
        t.session_event(d.session_id, "receive_maximum", "reader", "alex");
        assert_eq!(
            t.message(id)
                .unwrap()
                .events
                .iter()
                .filter(|e| e.code == "receive_maximum")
                .count(),
            1
        );
        t.delivery(d.id, "publish_sent", "reader", "alex", Some(1));
        t.session_event(d.session_id, "connection_closed", "reader", "alex");
        assert!(
            t.message(id)
                .unwrap()
                .events
                .iter()
                .any(|e| e.code == "connection_closed")
        );
        t.delivery(d.id, "puback", "reader", "alex", Some(1));
        let count = t.message(id).unwrap().events.len();
        t.session_event(d.session_id, "connection_closed", "reader", "alex");
        assert_eq!(t.message(id).unwrap().events.len(), count);
    }
}
