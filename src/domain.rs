use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationProperties {
    pub payload_format_indicator: Option<u8>,
    pub content_type: Option<String>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    #[serde(default)]
    pub user_properties: Vec<(String, String)>,
    pub message_expiry_interval: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Publication {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub properties: PublicationProperties,
}

impl Publication {
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|expiry| expiry <= Utc::now())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(valid_topic_name(&self.topic), "invalid MQTT topic name");
        anyhow::ensure!(self.qos <= 2, "invalid MQTT QoS");
        if let Some(topic) = &self.properties.response_topic {
            anyhow::ensure!(valid_topic_name(topic), "invalid response topic");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscription {
    pub filter: String,
    pub qos: u8,
    pub no_local: bool,
    pub retain_as_published: bool,
    pub retain_handling: u8,
    pub subscription_identifier: Option<u32>,
}

impl Subscription {
    pub fn validate(&self) -> anyhow::Result<()> {
        let filter = parse_subscription_filter(&self.filter)
            .ok_or_else(|| anyhow::anyhow!("invalid MQTT topic filter"))?;
        anyhow::ensure!(
            filter.share_name.is_none() || !self.no_local,
            "shared subscriptions require No Local to be zero"
        );
        anyhow::ensure!(
            self.qos <= 2 && self.retain_handling <= 2,
            "invalid subscription options"
        );
        anyhow::ensure!(
            self.subscription_identifier
                .is_none_or(|value| (1..=268_435_455).contains(&value)),
            "invalid subscription identifier"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPhase {
    Queued,
    SendingQos0,
    AwaitPubAck,
    AwaitPubRec,
    AwaitPubComp,
}

impl DeliveryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::SendingQos0 => "sending_qos0",
            Self::AwaitPubAck => "await_puback",
            Self::AwaitPubRec => "await_pubrec",
            Self::AwaitPubComp => "await_pubcomp",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "sending_qos0" => Ok(Self::SendingQos0),
            "await_puback" => Ok(Self::AwaitPubAck),
            "await_pubrec" => Ok(Self::AwaitPubRec),
            "await_pubcomp" => Ok(Self::AwaitPubComp),
            _ => anyhow::bail!("invalid stored delivery phase"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub id: Uuid,
    pub session_id: Uuid,
    pub sequence: i64,
    pub publication: Publication,
    pub packet_id: Option<u16>,
    pub phase: DeliveryPhase,
    #[serde(default)]
    pub subscription_identifiers: Vec<u32>,
}

#[derive(Debug)]
pub struct DeliveryBatch {
    pub inflight: Vec<(Uuid, u16)>,
    pub deliveries: Vec<Delivery>,
    pub started_qos0: Vec<Uuid>,
}

#[derive(Clone, Debug)]
pub struct SessionSpec {
    pub client_id: Vec<u8>,
    pub principal: String,
    pub clean_start: bool,
    pub expiry_interval: u32,
    pub protocol: u8,
    pub will: Option<Publication>,
    pub will_delay_interval: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenedSession {
    pub session_id: Uuid,
    pub generation: i64,
    pub session_present: bool,
}

// Intentionally not serializable: credentials must not become API response views.
#[derive(Clone, Debug)]
pub struct Credential {
    pub owner_id: Option<Uuid>,
    pub owner_username: Option<String>,
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub scram_verifier: Option<crate::scram::Verifier>,
    pub require_client_certificate: bool,
    pub enabled: bool,
    pub superuser: bool,
    pub bound_client_id: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct CertificateBinding {
    pub id: Uuid,
    pub fingerprint: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Qos2Release {
    pub found: bool,
    pub deliveries: Vec<Delivery>,
}

pub fn valid_topic_name(topic: &str) -> bool {
    !topic.is_empty() && topic.len() <= u16::MAX as usize && !topic.contains(['\0', '+', '#'])
}

pub fn valid_topic_filter(filter: &str) -> bool {
    if filter.is_empty() || filter.len() > u16::MAX as usize || filter.contains('\0') {
        return false;
    }
    let levels: Vec<_> = filter.split('/').collect();
    levels.iter().enumerate().all(|(index, level)| {
        (!level.contains('+') || *level == "+")
            && (!level.contains('#') || (*level == "#" && index + 1 == levels.len()))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionFilter<'a> {
    pub share_name: Option<&'a str>,
    pub topic_filter: &'a str,
}

/// Parse a subscription's optional shared prefix once. The inner filter uses
/// ordinary MQTT matching, including the special rules for the $ namespace.
pub fn parse_subscription_filter(filter: &str) -> Option<SubscriptionFilter<'_>> {
    if filter.is_empty() || filter.len() > u16::MAX as usize || filter.contains('\0') {
        return None;
    }
    let (share_name, topic_filter) = match filter.strip_prefix("$share/") {
        Some(shared) => {
            let (name, inner) = shared.split_once('/')?;
            if name.is_empty() || name.contains(['+', '#']) {
                return None;
            }
            (Some(name), inner)
        }
        None => (None, filter),
    };
    valid_topic_filter(topic_filter).then_some(SubscriptionFilter {
        share_name,
        topic_filter,
    })
}

pub fn topic_matches(filter: &str, topic: &str) -> bool {
    if !valid_topic_filter(filter) || !valid_topic_name(topic) {
        return false;
    }
    if topic.starts_with('$') && matches!(filter.split('/').next(), Some("#" | "+")) {
        return false;
    }
    let mut topics = topic.split('/');
    for level in filter.split('/') {
        if level == "#" {
            return true;
        }
        match topics.next() {
            Some(actual) if level == "+" || level == actual => {}
            _ => return false,
        }
    }
    topics.next().is_none()
}

/// Whether every topic accepted by `requested` is within `allowed`.
pub fn filter_covers(allowed: &str, requested: &str) -> bool {
    if !valid_topic_filter(allowed) || !valid_topic_filter(requested) {
        return false;
    }
    let a: Vec<_> = allowed.split('/').collect();
    let r: Vec<_> = requested.split('/').collect();
    if r[0].starts_with('$') && matches!(a[0], "#" | "+") {
        return false;
    }
    let mut index = 0;
    loop {
        match (a.get(index), r.get(index)) {
            (Some(&"#"), _) => return true,
            (None, None) => return true,
            (Some(&"+"), Some(&requested)) if requested != "#" => {}
            (Some(&left), Some(&right)) if left == right && right != "#" => {}
            _ => return false,
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_matching_preserves_empty_levels_and_system_namespace() {
        assert!(topic_matches("a/#", "a"));
        assert!(topic_matches("a/+/c", "a//c"));
        assert!(!topic_matches("a/+", "a"));
        assert!(!topic_matches("#", "$SYS/status"));
        assert!(topic_matches("$SYS/#", "$SYS/status"));
        assert!(!topic_matches("a/#/b", "a/x/b"));
    }

    #[test]
    fn filter_authorization_checks_entire_requested_namespace() {
        assert!(filter_covers("devices/u/#", "devices/u/+/state"));
        assert!(filter_covers("devices/u/#", "devices/u"));
        assert!(!filter_covers("devices/u/+", "devices/u/#"));
        assert!(!filter_covers("devices/u/#", "devices/+/state"));
        assert!(!filter_covers("#", "$SYS/#"));
        assert!(filter_covers("a/+", "a/+"));
    }

    #[test]
    fn shared_subscription_parser_preserves_identity_and_inner_namespace() {
        let parsed = parse_subscription_filter("$share/workers/devices/+/#").unwrap();
        assert_eq!(parsed.share_name, Some("workers"));
        assert_eq!(parsed.topic_filter, "devices/+/#");
        assert_eq!(
            parse_subscription_filter("devices/#").unwrap().share_name,
            None
        );
        assert!(!topic_matches(
            parse_subscription_filter("$share/workers/#")
                .unwrap()
                .topic_filter,
            "$SYS/status"
        ));
        assert!(topic_matches(
            parse_subscription_filter("$share/workers/$SYS/#")
                .unwrap()
                .topic_filter,
            "$SYS/status"
        ));
        assert_eq!(
            parse_subscription_filter("$share/outer/$share/inner/topic")
                .unwrap()
                .topic_filter,
            "$share/inner/topic"
        );
        for filter in [
            "$share/",
            "$share//a",
            "$share/+/a",
            "$share/a",
            "$share/a/",
            "$share/a#/x",
            "$share/a/b/#/c",
            "$share/a/b\0",
        ] {
            assert!(parse_subscription_filter(filter).is_none(), "{filter:?}");
        }
        let oversized = format!("$share/g/{}", "a".repeat(65535));
        assert!(parse_subscription_filter(&oversized).is_none());
    }

    #[test]
    fn shared_subscription_rejects_no_local() {
        let mut subscription = Subscription {
            filter: "$share/workers/commands/#".into(),
            qos: 2,
            no_local: false,
            retain_as_published: true,
            retain_handling: 0,
            subscription_identifier: Some(7),
        };
        assert!(subscription.validate().is_ok());
        subscription.no_local = true;
        assert!(subscription.validate().is_err());
        subscription.filter = "commands/#".into();
        assert!(subscription.validate().is_ok());
    }
}
