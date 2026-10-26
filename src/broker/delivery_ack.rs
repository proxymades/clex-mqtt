use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OutgoingAck {
    PubAck,
    RejectedPubAck,
    PubRec,
    RejectedPubRec,
    PubComp,
    RejectedPubComp,
}

impl OutgoingAck {
    pub(super) fn decode(packet: &MqttPacket) -> Option<(Self, u16)> {
        let (kind, id) = match packet {
            MqttPacket::V3(v3::Packet::PublishAck { packet_id }) => (Self::PubAck, *packet_id),
            MqttPacket::V5(v5::Packet::PublishAck(ack)) => (
                if u8::from(ack.reason_code) >= 128 {
                    Self::RejectedPubAck
                } else {
                    Self::PubAck
                },
                ack.packet_id,
            ),
            MqttPacket::V3(v3::Packet::PublishReceived { packet_id }) => (Self::PubRec, *packet_id),
            MqttPacket::V5(v5::Packet::PublishReceived(ack)) => (
                if u8::from(ack.reason_code) >= 128 {
                    Self::RejectedPubRec
                } else {
                    Self::PubRec
                },
                ack.packet_id,
            ),
            MqttPacket::V3(v3::Packet::PublishComplete { packet_id }) => {
                (Self::PubComp, *packet_id)
            }
            MqttPacket::V5(v5::Packet::PublishComplete(ack)) => (
                if u8::from(ack.reason_code) >= 128 {
                    Self::RejectedPubComp
                } else {
                    Self::PubComp
                },
                ack.packet_id,
            ),
            _ => return None,
        };
        Some((kind, id.get()))
    }
}

impl Broker {
    pub(super) async fn confirm_delivery_batch<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
        kind: OutgoingAck,
        packet_ids: &[u16],
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let phase = match kind {
            OutgoingAck::PubAck | OutgoingAck::RejectedPubAck => DeliveryPhase::AwaitPubAck,
            OutgoingAck::PubRec | OutgoingAck::RejectedPubRec => DeliveryPhase::AwaitPubRec,
            OutgoingAck::PubComp | OutgoingAck::RejectedPubComp => DeliveryPhase::AwaitPubComp,
        };
        let changed = self
            .store
            .confirm_deliveries(
                connection.session_id,
                connection.generation,
                packet_ids,
                phase,
                kind == OutgoingAck::PubRec,
            )
            .await?;
        for id in &changed {
            if let Some(delivery) = connection.inflight.get(id) {
                self.diagnostics().delivery(
                    *delivery,
                    match kind {
                        OutgoingAck::PubAck => "puback",
                        OutgoingAck::RejectedPubAck => "puback_rejected",
                        OutgoingAck::PubRec => "pubrec",
                        OutgoingAck::RejectedPubRec => "pubrec_rejected",
                        OutgoingAck::PubComp => "pubcomp",
                        OutgoingAck::RejectedPubComp => "pubcomp_rejected",
                    },
                    &connection.client_id,
                    &connection.username,
                    Some(*id),
                );
            }
        }
        if kind == OutgoingAck::PubRec {
            // Successful PUBREC preserves Receive Maximum slots until PUBCOMP.
            // The entire durable phase update precedes every PUBREL on the wire.
            for id in packet_ids {
                self.send(
                    wire,
                    protocol::pub_ack(
                        connection.v5,
                        6,
                        NonZeroU16::new(*id).unwrap(),
                        if changed.contains(id) { 0 } else { 0x92 },
                    ),
                    Some(connection),
                )
                .await?;
                if changed.contains(id)
                    && let Some(delivery) = connection.inflight.get(id)
                {
                    self.diagnostics().delivery(
                        *delivery,
                        "pubrel_sent",
                        &connection.client_id,
                        &connection.username,
                        Some(*id),
                    );
                }
            }
        } else {
            for id in changed {
                connection.flow.remove(&id);
                if let Some(delivery) = connection.inflight.remove(&id) {
                    connection.sent.remove(&delivery);
                }
            }
        }
        Ok(())
    }
}
