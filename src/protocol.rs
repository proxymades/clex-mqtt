//! Bounded MQTT framing and conversion between wire packets and persisted messages.
use std::{
    io,
    num::{NonZeroU16, NonZeroU32},
};

use bytes::{Bytes, BytesMut};
use chrono::{Duration, Utc};
use rmqtt_codec::{
    MqttCodec, MqttPacket,
    types::QoS,
    v3, v5,
    version::{ProtocolVersion, VersionCodec},
};
use tokio_util::codec::{Decoder, Encoder};

use crate::domain::{Delivery, Publication, PublicationProperties};

#[derive(Debug, thiserror::Error)]
#[error("MQTT packet exceeds peer maximum packet size")]
pub struct PeerPacketTooLarge;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ProtocolViolation {
    reason: v5::DisconnectReasonCode,
    message: &'static str,
}

fn violation(reason: v5::DisconnectReasonCode, message: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        ProtocolViolation { reason, message },
    )
}

fn protocol_error(message: &'static str) -> io::Error {
    violation(v5::DisconnectReasonCode::ProtocolError, message)
}

pub fn disconnect_reason(error: &io::Error) -> v5::DisconnectReasonCode {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<ProtocolViolation>())
        .map(|error| error.reason)
        .unwrap_or(v5::DisconnectReasonCode::MalformedPacket)
}

/// Checks total frame size and reserved fixed-header bits before invoking the codec.
/// The upstream codecs limit Remaining Length rather than the complete frame.
pub struct BrokerCodec {
    inner: MqttCodec,
    pub version: u8,
    max_in: usize,
    max_out: usize,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    /// rmqtt-codec represents Message Expiry as NonZeroU32. Preserve the valid
    /// MQTT value zero separately while decoding its otherwise identical frame.
    pub last_expiry_zero: bool,
    pub last_will_expiry_zero: bool,
}

impl BrokerCodec {
    pub fn new(max_packet_size: usize) -> Self {
        Self {
            inner: MqttCodec::Version(VersionCodec),
            version: 0,
            max_in: max_packet_size,
            max_out: max_packet_size,
            bytes_received: 0,
            bytes_sent: 0,
            last_expiry_zero: false,
            last_will_expiry_zero: false,
        }
    }

    pub fn set_max_out(&mut self, size: usize) {
        self.max_out = size.min(self.max_in);
    }

    pub fn is_v5(&self) -> bool {
        self.version == 5
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn variable(src: &[u8], at: &mut usize) -> io::Result<usize> {
    let mut value = 0;
    let mut multiplier = 1;
    for index in 0..4 {
        let byte = *src
            .get(*at)
            .ok_or_else(|| invalid("truncated variable integer"))?;
        *at += 1;
        value += usize::from(byte & 127) * multiplier;
        if byte & 128 == 0 {
            if index > 0 && byte == 0 {
                return Err(invalid("non-minimal variable integer"));
            }
            return Ok(value);
        }
        multiplier *= 128;
    }
    Err(invalid("invalid variable integer"))
}

fn field<'a>(src: &'a [u8], at: &mut usize, utf8: bool) -> io::Result<&'a [u8]> {
    let length = src
        .get(*at..*at + 2)
        .ok_or_else(|| invalid("truncated string length"))?;
    let length = u16::from_be_bytes([length[0], length[1]]) as usize;
    *at += 2;
    let value = src
        .get(*at..*at + length)
        .ok_or_else(|| invalid("truncated string"))?;
    *at += length;
    if utf8 {
        let string =
            std::str::from_utf8(value).map_err(|_| invalid("invalid MQTT UTF-8 string"))?;
        if string.chars().any(|c| {
            c == '\0' || matches!(c as u32, 0xfdd0..=0xfdef) || (c as u32 & 0xffff) >= 0xfffe
        }) {
            return Err(invalid("forbidden MQTT UTF-8 code point"));
        }
    }
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropertyContext {
    Connect,
    Will,
    Publish,
    Subscribe,
    Unsubscribe,
    Acknowledgement,
    Disconnect,
    Authentication,
}

impl PropertyContext {
    fn accepts(self, kind: usize) -> bool {
        match self {
            Self::Connect => matches!(
                kind,
                0x11 | 0x15 | 0x16 | 0x17 | 0x19 | 0x21 | 0x22 | 0x26 | 0x27
            ),
            Self::Will => matches!(kind, 0x01 | 0x02 | 0x03 | 0x08 | 0x09 | 0x18 | 0x26),
            Self::Publish => matches!(kind, 0x01 | 0x02 | 0x03 | 0x08 | 0x09 | 0x0b | 0x23 | 0x26),
            Self::Subscribe => matches!(kind, 0x0b | 0x26),
            Self::Unsubscribe => kind == 0x26,
            Self::Acknowledgement => matches!(kind, 0x1f | 0x26),
            Self::Disconnect => matches!(kind, 0x11 | 0x1c | 0x1f | 0x26),
            Self::Authentication => matches!(kind, 0x15 | 0x16 | 0x1f | 0x26),
        }
    }
}

fn properties(src: &mut [u8], at: &mut usize, context: PropertyContext) -> io::Result<bool> {
    let length = variable(src, at)?;
    let end = at
        .checked_add(length)
        .ok_or_else(|| invalid("property length overflow"))?;
    if end > src.len() {
        return Err(invalid("truncated MQTT properties"));
    }
    let mut expiry_zero = false;
    let mut seen = 0u64;
    while *at < end {
        let kind = variable(&src[..end], at)?;
        // MQTT 5 §2.2.2.2 assigns 0x81 to a property on the wrong packet type.
        if !context.accepts(kind) {
            return Err(invalid("property is not valid for this packet type"));
        }
        if context == PropertyContext::Publish && kind == 0x0b {
            return Err(protocol_error(
                "client PUBLISH contains a subscription identifier",
            ));
        }
        if kind != 0x26 && seen & (1u64 << kind) != 0 {
            return Err(protocol_error("singleton property is repeated"));
        }
        seen |= 1u64 << kind;
        match kind {
            0x01 | 0x17 | 0x19 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2a => {
                let value = *src[..end]
                    .get(*at)
                    .ok_or_else(|| invalid("truncated byte property"))?;
                if value > 1 {
                    return Err(protocol_error("invalid boolean property"));
                }
                *at += 1;
            }
            0x02 | 0x11 | 0x18 | 0x27 => {
                if *at + 4 > end {
                    return Err(invalid("truncated integer property"));
                }
                if kind == 0x02 && src[*at..*at + 4] == [0, 0, 0, 0] {
                    src[*at + 3] = 1;
                    expiry_zero = true;
                }
                if kind == 0x27 && src[*at..*at + 4] == [0, 0, 0, 0] {
                    return Err(protocol_error("maximum packet size is zero"));
                }
                *at += 4;
            }
            0x13 | 0x21 | 0x22 | 0x23 => {
                let value = src
                    .get(*at..*at + 2)
                    .filter(|_| *at + 2 <= end)
                    .ok_or_else(|| invalid("truncated integer property"))?;
                if value == [0, 0] {
                    if kind == 0x21 {
                        return Err(protocol_error("receive maximum is zero"));
                    }
                    if kind == 0x23 {
                        return Err(violation(
                            v5::DisconnectReasonCode::TopicAliasInvalid,
                            "topic alias is zero",
                        ));
                    }
                }
                *at += 2;
            }
            0x03 | 0x08 | 0x12 | 0x15 | 0x1a | 0x1c | 0x1f => {
                field(&src[..end], at, true)?;
            }
            0x09 | 0x16 => {
                field(&src[..end], at, false)?;
            }
            0x0b => {
                if variable(&src[..end], at)? == 0 {
                    return Err(protocol_error("subscription identifier is zero"));
                }
            }
            0x26 => {
                field(&src[..end], at, true)?;
                field(&src[..end], at, true)?;
            }
            _ => return Err(invalid("unknown MQTT property")),
        }
        if *at > end {
            return Err(invalid("truncated MQTT property value"));
        }
    }
    if matches!(
        context,
        PropertyContext::Connect | PropertyContext::Authentication
    ) && seen & (1 << 0x16) != 0
        && seen & (1 << 0x15) == 0
    {
        return Err(protocol_error(
            "authentication data has no authentication method",
        ));
    }
    Ok(expiry_zero)
}

/// Fill gaps in the upstream decoder: matching protocol name/level, complete
/// CONNECT consumption, Will flag consistency, UTF-8 constraints and SUB options.
fn validate_body(src: &mut [u8], version: u8) -> io::Result<(bool, bool, bool)> {
    let kind = src[0] >> 4;
    let mut at = 1;
    variable(src, &mut at)?;
    let mut expiry_zero = false;
    let mut will_zero = false;
    let mut assigned_id_without_clean_start = false;
    match kind {
        1 => {
            let name = field(src, &mut at, false)?.to_vec();
            let level = *src
                .get(at)
                .ok_or_else(|| invalid("missing protocol level"))?;
            at += 1;
            if !matches!(
                (name.as_slice(), level),
                (b"MQIsdp", 3) | (b"MQTT", 4) | (b"MQTT", 5)
            ) {
                return Err(invalid("protocol name and level do not match"));
            }
            let flags_at = at;
            let flags = *src
                .get(at)
                .ok_or_else(|| invalid("missing CONNECT flags"))?;
            at += 1;
            if flags & 1 != 0 || flags & 0x18 == 0x18 || flags & 4 == 0 && flags & 0x38 != 0 {
                return Err(invalid("invalid CONNECT Will flags"));
            }
            if level < 5 && flags & 0x40 != 0 && flags & 0x80 == 0 {
                return Err(invalid("password without username"));
            }
            at += 2;
            if at > src.len() {
                return Err(invalid("missing keepalive"));
            }
            if level == 5 {
                properties(src, &mut at, PropertyContext::Connect)?;
            }
            let client_id = field(src, &mut at, true)?;
            if level == 5 && client_id.is_empty() && flags & 2 == 0 {
                // MQTT 5 permits an assigned Client ID with Clean Start=0.
                // Adapt the older restriction in the upstream decoder only;
                // restore the actual flag on the decoded CONNECT below.
                src[flags_at] |= 2;
                assigned_id_without_clean_start = true;
            }
            if flags & 4 != 0 {
                if level == 5 {
                    will_zero = properties(src, &mut at, PropertyContext::Will)?;
                }
                field(src, &mut at, true)?;
                field(src, &mut at, false)?;
            }
            if flags & 0x80 != 0 {
                field(src, &mut at, true)?;
            }
            if flags & 0x40 != 0 {
                field(src, &mut at, false)?;
            }
            if at != src.len() {
                return Err(invalid("trailing CONNECT data"));
            }
        }
        3 => {
            field(src, &mut at, true)?;
            if src[0] & 6 != 0 {
                at += 2;
            }
            if version == 5 {
                expiry_zero = properties(src, &mut at, PropertyContext::Publish)?;
            }
        }
        8 | 10 => {
            at += 2;
            if at > src.len() {
                return Err(invalid("missing packet identifier"));
            }
            if version == 5 {
                properties(
                    src,
                    &mut at,
                    if kind == 8 {
                        PropertyContext::Subscribe
                    } else {
                        PropertyContext::Unsubscribe
                    },
                )?;
            }
            let mut count = 0;
            while at < src.len() {
                let filter = field(src, &mut at, true)?;
                if kind == 8 {
                    let options = *src
                        .get(at)
                        .ok_or_else(|| invalid("missing subscription options"))?;
                    at += 1;
                    if options & 3 == 3
                        || version < 5 && options > 2
                        || version == 5 && (options & 0xc0 != 0 || options & 0x30 == 0x30)
                    {
                        return Err(invalid("invalid subscription options"));
                    }
                    if version == 5 && options & 4 != 0 && filter.starts_with(b"$share/") {
                        return Err(protocol_error("shared subscription has No Local set"));
                    }
                }
                count += 1;
            }
            if count == 0 {
                return Err(invalid("empty subscription list"));
            }
        }
        4..=7 if version == 5 => {
            at += 2;
            if at > src.len() {
                return Err(invalid("missing packet identifier"));
            }
            if at < src.len() {
                at += 1;
                if at < src.len() {
                    properties(src, &mut at, PropertyContext::Acknowledgement)?;
                }
            }
            if at != src.len() {
                return Err(invalid("trailing acknowledgement data"));
            }
        }
        14 | 15 if version == 5 => {
            if at < src.len() {
                at += 1;
                if at < src.len() {
                    properties(
                        src,
                        &mut at,
                        if kind == 14 {
                            PropertyContext::Disconnect
                        } else {
                            PropertyContext::Authentication
                        },
                    )?;
                }
            }
            if at != src.len() {
                return Err(invalid("trailing control packet data"));
            }
        }
        _ => {}
    }
    Ok((expiry_zero, will_zero, assigned_id_without_clean_start))
}

fn frame_size(src: &[u8], limit: usize) -> io::Result<Option<usize>> {
    if src.len() < 2 {
        return Ok(None);
    }
    let mut remaining = 0usize;
    let mut multiplier = 1usize;
    for index in 1..=4 {
        let Some(&byte) = src.get(index) else {
            return Ok(None);
        };
        remaining += usize::from(byte & 127) * multiplier;
        if byte & 128 == 0 {
            if index > 1 && byte == 0 {
                return Err(invalid("non-minimal remaining length"));
            }
            let size = index + 1 + remaining;
            if size > limit {
                return Err(violation(
                    v5::DisconnectReasonCode::PacketTooLarge,
                    "MQTT packet exceeds configured size limit",
                ));
            }
            return Ok(Some(size));
        }
        multiplier *= 128;
    }
    Err(invalid("malformed remaining length"))
}

impl Decoder for BrokerCodec {
    type Item = MqttPacket;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        let Some(size) = frame_size(src, self.max_in)? else {
            return Ok(None);
        };
        let first = src[0];
        let kind = first >> 4;
        let flags = first & 15;
        let valid_flags = match kind {
            3 => flags & 6 != 6 && !(flags & 6 == 0 && flags & 8 != 0),
            6 | 8 | 10 => flags == 2,
            1..=15 => flags == 0,
            _ => false,
        };
        if !valid_flags {
            return Err(invalid("invalid MQTT fixed header flags"));
        }
        if src.len() < size {
            return Ok(None);
        }
        if matches!(self.inner, MqttCodec::Version(_)) {
            if kind != 1 {
                return Err(invalid("first packet must be CONNECT"));
            }
            let Some((MqttPacket::Version(version), _)) = self
                .inner
                .decode(src)
                .map_err(|_| invalid("invalid MQTT protocol version"))?
            else {
                return Err(invalid("incomplete CONNECT protocol header"));
            };
            self.inner = match version {
                ProtocolVersion::MQTT3 => MqttCodec::V3(v3::Codec::new(self.max_in as u32)),
                ProtocolVersion::MQTT5 => {
                    MqttCodec::V5(v5::Codec::new(self.max_in as u32, self.max_out as u32))
                }
            };
            self.version = if matches!(version, ProtocolVersion::MQTT5) {
                5
            } else {
                4
            };
        }
        let (expiry_zero, will_zero, restore_clean_start) =
            validate_body(&mut src[..size], self.version)?;
        self.last_expiry_zero = expiry_zero;
        self.last_will_expiry_zero = will_zero;
        if matches!(kind, 12 | 13) && size != 2 {
            return Err(invalid("PING packet must have zero remaining length"));
        }
        if self.version != 5
            && ((kind == 14 && size != 2) || (matches!(kind, 4..=7 | 11) && size != 4))
        {
            return Err(invalid("invalid control packet length"));
        }
        let before = src.len();
        let Some((mut packet, _)) = self
            .inner
            .decode(src)
            .map_err(|_| invalid("malformed MQTT packet"))?
        else {
            return Err(invalid("incomplete MQTT packet"));
        };
        if before - src.len() != size {
            return Err(invalid("incorrect MQTT frame consumption"));
        }
        self.bytes_received += size as u64;
        if restore_clean_start && let MqttPacket::V5(v5::Packet::Connect(connect)) = &mut packet {
            connect.clean_start = false;
        }
        if let MqttPacket::V3(v3::Packet::Connect(connect)) = &packet {
            self.version = connect.protocol.level();
        }
        Ok(Some(packet))
    }
}

impl Encoder<MqttPacket> for BrokerCodec {
    type Error = io::Error;
    fn encode(&mut self, item: MqttPacket, dst: &mut BytesMut) -> io::Result<()> {
        let mut encoded = BytesMut::new();
        self.inner
            .encode(item, &mut encoded)
            .map_err(|_| invalid("unable to encode MQTT packet"))?;
        if encoded.len() > self.max_out {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                PeerPacketTooLarge,
            ));
        }
        dst.extend_from_slice(&encoded);
        self.bytes_sent += encoded.len() as u64;
        Ok(())
    }
}

pub fn qos(value: u8) -> QoS {
    match value {
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtMostOnce,
    }
}

pub fn publication(publish: &rmqtt_codec::types::Publish) -> Publication {
    let properties = publish
        .properties
        .as_ref()
        .map(|p| PublicationProperties {
            payload_format_indicator: p.is_utf8_payload.then_some(1),
            content_type: p.content_type.as_ref().map(ToString::to_string),
            response_topic: p.response_topic.as_ref().map(ToString::to_string),
            correlation_data: p.correlation_data.as_ref().map(|b| b.to_vec()),
            user_properties: p
                .user_properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            message_expiry_interval: p.message_expiry_interval.map(NonZeroU32::get),
        })
        .unwrap_or_default();
    let expires_at = properties
        .message_expiry_interval
        .map(|secs| Utc::now() + Duration::seconds(i64::from(secs)));
    Publication {
        topic: publish.topic.to_string(),
        payload: publish.payload.to_vec(),
        qos: publish.qos.into(),
        retain: publish.retain,
        expires_at,
        properties,
    }
}

pub fn delivery_packet(delivery: &Delivery, v5: bool, dup: bool) -> MqttPacket {
    let p = &delivery.publication;
    let properties = v5.then(|| v5::PublishProperties {
        topic_alias: None,
        correlation_data: p.properties.correlation_data.clone().map(Bytes::from),
        message_expiry_interval: p.expires_at.and_then(|end| {
            NonZeroU32::new((end - Utc::now()).num_seconds().max(1).min(u32::MAX as i64) as u32)
        }),
        content_type: p.properties.content_type.clone().map(Into::into),
        user_properties: p
            .properties
            .user_properties
            .iter()
            .map(|(k, v)| (k.clone().into(), v.clone().into()))
            .collect(),
        is_utf8_payload: p.properties.payload_format_indicator == Some(1),
        response_topic: p.properties.response_topic.clone().map(Into::into),
        subscription_ids: delivery
            .subscription_identifiers
            .iter()
            .filter_map(|id| NonZeroU32::new(*id))
            .collect(),
    });
    let packet = rmqtt_codec::types::Publish {
        dup,
        retain: p.retain,
        qos: qos(p.qos),
        topic: p.topic.clone().into(),
        packet_id: delivery.packet_id.and_then(NonZeroU16::new),
        payload: Bytes::from(p.payload.clone()),
        properties,
    };
    if v5 {
        MqttPacket::V5(v5::Packet::Publish(Box::new(packet)))
    } else {
        MqttPacket::V3(v3::Packet::Publish(Box::new(packet)))
    }
}

pub fn pub_ack(v5: bool, kind: u8, packet_id: NonZeroU16, reason: u8) -> MqttPacket {
    if v5 {
        match kind {
            4 | 5 => {
                let ack = v5::PublishAck {
                    packet_id,
                    reason_code: reason
                        .try_into()
                        .unwrap_or(v5::PublishAckReason::UnspecifiedError),
                    ..Default::default()
                };
                MqttPacket::V5(if kind == 4 {
                    v5::Packet::PublishAck(ack)
                } else {
                    v5::Packet::PublishReceived(ack)
                })
            }
            _ => {
                let ack = v5::PublishAck2 {
                    packet_id,
                    reason_code: reason.try_into().unwrap_or(v5::PublishAck2Reason::Success),
                    ..Default::default()
                };
                MqttPacket::V5(if kind == 6 {
                    v5::Packet::PublishRelease(ack)
                } else {
                    v5::Packet::PublishComplete(ack)
                })
            }
        }
    } else {
        MqttPacket::V3(match kind {
            4 => v3::Packet::PublishAck { packet_id },
            5 => v3::Packet::PublishReceived { packet_id },
            6 => v3::Packet::PublishRelease { packet_id },
            _ => v3::Packet::PublishComplete { packet_id },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversize_before_payload_arrives() {
        let mut codec = BrokerCodec::new(64);
        let mut bytes = BytesMut::from(&b"\x30\x7f"[..]);
        let error = codec.decode(&mut bytes).unwrap_err();
        assert_eq!(
            disconnect_reason(&error),
            v5::DisconnectReasonCode::PacketTooLarge
        );
    }

    #[test]
    fn rejects_bad_flags_and_nonminimal_length() {
        for wire in [
            &b"\xc1\x00"[..],
            &b"\xc0\x80\x00"[..],
            &b"\xc0\x80\x80\x80\x80"[..],
        ] {
            assert!(
                BrokerCodec::new(1024)
                    .decode(&mut BytesMut::from(wire))
                    .is_err()
            );
        }
    }

    #[test]
    fn detects_all_three_connect_versions_from_fragmented_stream() {
        for (wire, version) in [
            (&b"\x10\x0f\x00\x06MQIsdp\x03\x02\x00\x3c\x00\x01a"[..], 3),
            (&b"\x10\x0d\x00\x04MQTT\x04\x02\x00\x3c\x00\x01a"[..], 4),
            (&b"\x10\x0e\x00\x04MQTT\x05\x02\x00\x3c\x00\x00\x01a"[..], 5),
        ] {
            let mut codec = BrokerCodec::new(1024);
            let mut bytes = BytesMut::new();
            for byte in &wire[..wire.len() - 1] {
                bytes.extend_from_slice(&[*byte]);
                assert!(codec.decode(&mut bytes).unwrap().is_none());
            }
            bytes.extend_from_slice(&wire[wire.len() - 1..]);
            assert!(codec.decode(&mut bytes).unwrap().is_some());
            assert_eq!(codec.version, version);
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn accepts_zero_message_expiry_and_preserves_its_meaning() {
        let mut codec = BrokerCodec::new(1024);
        codec
            .decode(&mut BytesMut::from(
                &b"\x10\x0e\x00\x04MQTT\x05\x02\x00\x3c\x00\x00\x01a"[..],
            ))
            .unwrap()
            .unwrap();
        let packet = codec
            .decode(&mut BytesMut::from(
                &b"\x30\x0a\x00\x01a\x05\x02\x00\x00\x00\x00x"[..],
            ))
            .unwrap()
            .unwrap();
        assert!(matches!(packet, MqttPacket::V5(v5::Packet::Publish(_))));
        assert!(codec.last_expiry_zero);
    }

    #[test]
    fn rejects_connect_trailing_bytes_will_flags_and_v5_subscribe_reserved_bits() {
        for raw in [
            &b"\x10\x0e\x00\x04MQTT\x04\x02\x00\x3c\x00\x01ax"[..],
            &b"\x10\x0d\x00\x04MQTT\x04\x22\x00\x3c\x00\x01a"[..],
        ] {
            assert!(
                BrokerCodec::new(1024)
                    .decode(&mut BytesMut::from(raw))
                    .is_err()
            );
        }
        let mut codec = BrokerCodec::new(1024);
        codec
            .decode(&mut BytesMut::from(
                &b"\x10\x0e\x00\x04MQTT\x05\x02\x00\x3c\x00\x00\x01a"[..],
            ))
            .unwrap()
            .unwrap();
        assert!(
            codec
                .decode(&mut BytesMut::from(
                    &b"\x82\x07\x00\x01\x00\x00\x01a\x40"[..]
                ))
                .is_err()
        );
    }

    fn mqtt5_codec() -> BrokerCodec {
        let mut codec = BrokerCodec::new(1024);
        codec
            .decode(&mut BytesMut::from(
                &b"\x10\x0e\x00\x04MQTT\x05\x02\x00\x3c\x00\x00\x01a"[..],
            ))
            .unwrap()
            .unwrap();
        codec
    }

    #[test]
    fn property_validation_preserves_specific_disconnect_categories() {
        for (header, body, reason) in [
            (
                0x30,
                vec![0, 1, b'a', 8, 3, 0, 1, b'x', 3, 0, 1, b'y'],
                v5::DisconnectReasonCode::ProtocolError,
            ),
            (
                0x30,
                vec![0, 1, b'a', 3, 0x23, 0, 0],
                v5::DisconnectReasonCode::TopicAliasInvalid,
            ),
            (
                0x30,
                vec![0, 1, b'a', 5, 0x18, 0, 0, 0, 1],
                v5::DisconnectReasonCode::MalformedPacket,
            ),
            (
                0x40,
                vec![0, 1, 0, 8, 0x1f, 0, 1, b'x', 0x1f, 0, 1, b'y'],
                v5::DisconnectReasonCode::ProtocolError,
            ),
        ] {
            let mut wire = BytesMut::from([header, body.len() as u8].as_slice());
            wire.extend_from_slice(&body);
            let error = mqtt5_codec().decode(&mut wire).unwrap_err();
            assert_eq!(disconnect_reason(&error), reason);
        }
    }

    #[test]
    fn truncated_byte_property_does_not_consume_payload() {
        for payload in [0, 1, b'x', 255] {
            let mut frame = BytesMut::from(&b"\x30\x06\x00\x01a\x01\x01"[..]);
            frame.extend_from_slice(&[payload]);
            let error = mqtt5_codec().decode(&mut frame).unwrap_err();
            assert_eq!(
                disconnect_reason(&error),
                v5::DisconnectReasonCode::MalformedPacket,
                "truncated property classification depends on payload {payload}"
            );
        }
    }

    #[test]
    fn shared_no_local_is_protocol_error_before_subscribe_processing() {
        let filter = b"$share/workers/commands/#";
        let mut body = vec![0, 1, 0];
        body.extend_from_slice(&(filter.len() as u16).to_be_bytes());
        body.extend_from_slice(filter);
        body.push(5); // QoS 1 and No Local.
        let mut frame = BytesMut::from([0x82, body.len() as u8].as_slice());
        frame.extend_from_slice(&body);
        let error = mqtt5_codec().decode(&mut frame).unwrap_err();
        assert_eq!(
            disconnect_reason(&error),
            v5::DisconnectReasonCode::ProtocolError
        );
    }

    #[test]
    fn rejects_zero_receive_maximum_as_protocol_error() {
        let mut codec = BrokerCodec::new(1024);
        let error = codec
            .decode(&mut BytesMut::from(
                &b"\x10\x11\x00\x04MQTT\x05\x02\x00\x3c\x03\x21\x00\x00\x00\x01a"[..],
            ))
            .unwrap_err();
        assert_eq!(
            disconnect_reason(&error),
            v5::DisconnectReasonCode::ProtocolError
        );
    }
}
