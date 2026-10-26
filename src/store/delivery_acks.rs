use super::*;

impl Store {
    /// Persist an already received, homogeneous group of confirmations. The
    /// caller emits responses and releases flow slots only after this commits.
    pub async fn confirm_deliveries(
        &self,
        id: Uuid,
        generation: i64,
        packet_ids: &[u16],
        phase: DeliveryPhase,
        advance_qos2: bool,
    ) -> Result<Vec<u16>> {
        ensure!(
            !packet_ids.is_empty()
                && packet_ids.len() <= 32
                && packet_ids.iter().all(|id| *id != 0)
                && packet_ids
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == packet_ids.len(),
            "invalid delivery confirmation batch"
        );
        ensure!(
            matches!(
                phase,
                DeliveryPhase::AwaitPubAck
                    | DeliveryPhase::AwaitPubRec
                    | DeliveryPhase::AwaitPubComp
            ) && (!advance_qos2 || phase == DeliveryPhase::AwaitPubRec),
            "invalid delivery confirmation phase"
        );
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let ids: Vec<i32> = packet_ids.iter().map(|id| i32::from(*id)).collect();
        let changed: Vec<i32> = if advance_qos2 {
            sqlx::query_scalar("UPDATE deliveries SET phase='await_pubcomp' WHERE session_id=$1 AND packet_id=ANY($2) AND phase IN ('await_pubrec','await_pubcomp') RETURNING packet_id")
                .bind(id).bind(&ids).fetch_all(&mut *tx).await?
        } else {
            removed_deliveries(
                sqlx::query(&delete_deliveries_sql(
                    "d.session_id=$1 AND d.packet_id=ANY($2) AND d.phase=$3",
                    true,
                ))
                .bind(id)
                .bind(&ids)
                .bind(phase.as_str())
                .fetch_one(&mut *tx)
                .await?,
            )?
            .packet_ids
        };
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(changed.into_iter().map(|id| id as u16).collect())
    }
}
