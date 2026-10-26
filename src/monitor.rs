//! Bounded, process-local previews of committed MQTT publications.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{Publication, topic_matches};

pub const MESSAGE_CAPACITY: usize = 1000;
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
const PAGE_BYTES: usize = 256 * 1024;
const PAYLOAD_PREVIEW: usize = 4096;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    #[serde(skip)]
    pub(crate) preview_key: Uuid,
    pub id: u64,
    pub received_at: DateTime<Utc>,
    pub client_id: String,
    pub username: String,
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub will: bool,
    pub payload_base64: String,
    pub payload_bytes: usize,
    pub payload_truncated: bool,
}

impl MessageEvent {
    fn preview(publication: &Publication, client_id: &[u8], username: &str, will: bool) -> Self {
        Self {
            preview_key: Uuid::new_v4(),
            id: 0,
            received_at: Utc::now(),
            client_id: String::from_utf8_lossy(client_id).into_owned(),
            username: username.to_owned(),
            topic: publication.topic.clone(),
            qos: publication.qos,
            retain: publication.retain,
            will,
            payload_base64: STANDARD
                .encode(&publication.payload[..publication.payload.len().min(PAYLOAD_PREVIEW)]),
            payload_bytes: publication.payload.len(),
            payload_truncated: publication.payload.len() > PAYLOAD_PREVIEW,
        }
    }

    fn memory_size(&self) -> usize {
        self.client_id.len()
            + self.username.len()
            + self.topic.len()
            + self.payload_base64.len()
            + 512
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageFilter {
    pub after: Option<u64>,
    pub client_id: Option<String>,
    pub username: Option<String>,
    pub topic: Option<String>,
    pub limit: Option<usize>,
}

impl MessageFilter {
    fn matches(&self, message: &MessageEvent) -> bool {
        self.client_id
            .as_ref()
            .is_none_or(|id| id == &message.client_id)
            && self
                .username
                .as_ref()
                .is_none_or(|name| name == &message.username)
            && self
                .topic
                .as_ref()
                .is_none_or(|filter| topic_matches(filter, &message.topic))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub instance_id: Uuid,
    pub cursor: u64,
    pub lost: u64,
    pub has_more: bool,
    pub capacity: usize,
    pub messages: Vec<MessageEvent>,
}

#[derive(Default)]
struct Buffer {
    entries: VecDeque<MessageEvent>,
    bytes: usize,
    latest: u64,
}

#[derive(Clone, Copy, Default)]
pub struct MonitorStats {
    pub total: u64,
    pub entries: usize,
    pub bytes: usize,
}

impl Buffer {
    fn push(&mut self, mut message: MessageEvent) {
        self.latest += 1;
        message.id = self.latest;
        self.bytes += message.memory_size();
        self.entries.push_back(message);
        while self.entries.len() > MESSAGE_CAPACITY || self.bytes > MEMORY_LIMIT {
            if let Some(removed) = self.entries.pop_front() {
                self.bytes -= removed.memory_size();
            }
        }
    }
}

/// Stage bounded previews until the surrounding maintenance transaction commits.
#[derive(Default)]
pub(crate) struct MessageBatch(Buffer);

impl MessageBatch {
    pub(crate) fn record(&mut self, publication: &Publication, client_id: &[u8], username: &str) {
        self.0.push(MessageEvent::preview(
            publication,
            client_id,
            username,
            true,
        ));
    }
}

#[derive(Clone)]
pub struct MessageMonitor {
    instance_id: Uuid,
    inner: Arc<Mutex<Buffer>>,
}

impl Default for MessageMonitor {
    fn default() -> Self {
        Self {
            instance_id: Uuid::new_v4(),
            inner: Default::default(),
        }
    }
}

impl MessageMonitor {
    pub fn stats(&self) -> MonitorStats {
        let buffer = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        MonitorStats {
            total: buffer.latest,
            entries: buffer.entries.len(),
            bytes: buffer.bytes,
        }
    }
    pub(crate) fn record(&self, publication: &Publication, client_id: &[u8], username: &str) {
        let preview = MessageEvent::preview(publication, client_id, username, false);
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(preview);
    }

    pub(crate) fn commit_batch(&self, batch: MessageBatch) {
        let mut buffer = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        buffer.latest += batch.0.latest - batch.0.entries.len() as u64;
        for preview in batch.0.entries {
            buffer.push(preview);
        }
    }

    pub fn read(&self, filter: &MessageFilter) -> MessagePage {
        let buffer = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let limit = filter.limit.unwrap_or(100).clamp(1, 100);
        let mut messages = Vec::new();
        let mut bytes = 0;
        let mut cursor = buffer.latest;
        let mut lost = 0;
        match filter.after.filter(|after| *after <= buffer.latest) {
            None => {
                for message in buffer
                    .entries
                    .iter()
                    .rev()
                    .filter(|entry| filter.matches(entry))
                {
                    let size = message.memory_size();
                    if messages.len() == limit
                        || (!messages.is_empty() && bytes + size > PAGE_BYTES)
                    {
                        break;
                    }
                    bytes += size;
                    messages.push(message.clone());
                }
                messages.reverse();
            }
            Some(after) => {
                lost = buffer
                    .entries
                    .front()
                    .map_or(0, |first| first.id.saturating_sub(after.saturating_add(1)));
                cursor = after;
                for message in buffer.entries.iter().filter(|entry| entry.id > after) {
                    if filter.matches(message) {
                        let size = message.memory_size();
                        if messages.len() == limit
                            || (!messages.is_empty() && bytes + size > PAGE_BYTES)
                        {
                            break;
                        }
                        bytes += size;
                        messages.push(message.clone());
                    }
                    cursor = message.id;
                }
                if buffer.entries.is_empty() {
                    cursor = buffer.latest;
                }
            }
        }
        MessagePage {
            instance_id: self.instance_id,
            cursor,
            lost,
            has_more: cursor < buffer.latest,
            capacity: MESSAGE_CAPACITY,
            messages,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(topic: &str, payload: Vec<u8>) -> Publication {
        Publication {
            topic: topic.into(),
            payload,
            qos: 1,
            retain: false,
            expires_at: None,
            properties: Default::default(),
        }
    }

    #[test]
    fn filters_and_cursor_preserve_each_publication() {
        let monitor = MessageMonitor::default();
        monitor.record(&publication("site/a/value", vec![0, 255]), b"a", "owner-a");
        monitor.record(&publication("site/b/value", vec![2]), b"b", "owner-b");
        monitor.record(&publication("site/a/value", vec![3]), b"a", "owner-a");
        let mut filter = MessageFilter {
            after: Some(0),
            client_id: Some("a".into()),
            username: Some("owner-a".into()),
            topic: Some("site/+/value".into()),
            limit: Some(1),
        };
        let first = monitor.read(&filter);
        assert_eq!(first.messages.len(), 1);
        assert_eq!(first.messages[0].payload_base64, "AP8=");
        assert_eq!(first.cursor, 2);
        assert!(first.has_more);
        filter.after = Some(first.cursor);
        let second = monitor.read(&filter);
        assert_eq!(second.messages[0].id, 3);
        assert!(!second.has_more);
        assert_eq!(monitor.read(&MessageFilter::default()).messages.len(), 3);
    }

    #[test]
    fn buffers_bound_memory_and_report_eviction() {
        let monitor = MessageMonitor::default();
        let sample = publication("telemetry", vec![255; 8192]);
        for _ in 0..MESSAGE_CAPACITY + 5 {
            monitor.record(&sample, b"a", "u");
        }
        let page = monitor.read(&MessageFilter {
            after: Some(0),
            ..Default::default()
        });
        assert_eq!(page.lost, 5);
        assert!(page.messages[0].payload_truncated);
        assert_eq!(page.messages[0].payload_bytes, 8192);
        assert_eq!(
            STANDARD
                .decode(&page.messages[0].payload_base64)
                .unwrap()
                .len(),
            PAYLOAD_PREVIEW
        );
        assert!(monitor.inner.lock().unwrap().bytes <= MEMORY_LIMIT);
        let large_topic = publication("a".repeat(65535).as_str(), vec![]);
        for _ in 0..200 {
            monitor.record(&large_topic, b"a", "u");
        }
        let buffer = monitor.inner.lock().unwrap();
        assert!(buffer.entries.len() < MESSAGE_CAPACITY);
        assert!(buffer.bytes <= MEMORY_LIMIT);
    }

    #[test]
    fn wills_publish_only_when_batch_is_committed_and_reset_has_new_instance() {
        let monitor = MessageMonitor::default();
        let mut batch = MessageBatch::default();
        batch.record(&publication("status", b"offline".to_vec()), b"a", "u");
        assert!(monitor.read(&MessageFilter::default()).messages.is_empty());
        monitor.commit_batch(batch);
        let page = monitor.read(&MessageFilter::default());
        assert!(page.messages[0].will);
        assert_ne!(
            page.instance_id,
            MessageMonitor::default()
                .read(&MessageFilter::default())
                .instance_id
        );
    }
    #[test]
    fn preview_keys_are_stable_random_and_absent_from_admin_serialization() {
        let monitor = MessageMonitor::default();
        let sample = publication("sensor", b"same-payload".to_vec());
        monitor.record(&sample, b"same-client", "same-user");
        monitor.record(&sample, b"same-client", "same-user");
        let page = monitor.read(&MessageFilter::default());
        assert_ne!(page.messages[0].preview_key, page.messages[1].preview_key);
        assert_eq!(page.messages[0].preview_key.get_version_num(), 4);
        assert_eq!(
            monitor.read(&MessageFilter::default()).messages[0].preview_key,
            page.messages[0].preview_key
        );
        let json = serde_json::to_value(&page.messages[0]).unwrap();
        assert!(json.get("previewKey").is_none());
        assert!(json.get("preview_key").is_none());
    }
}
