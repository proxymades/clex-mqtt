use super::*;

impl Store {
    /// Reserve a bounded set of outgoing QoS handshakes atomically. The caller
    /// may send their PUBLISH packets only after this transaction commits.
    pub async fn reserve_deliveries(
        &self,
        id: Uuid,
        generation: i64,
        reservations: &[(Uuid, u16)],
    ) -> Result<()> {
        ensure!(
            !reservations.is_empty()
                && reservations.len() <= 64
                && reservations.iter().all(|(_, packet)| *packet != 0)
                && reservations
                    .iter()
                    .map(|(id, _)| id)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == reservations.len()
                && reservations
                    .iter()
                    .map(|(_, packet)| packet)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == reservations.len(),
            "invalid delivery reservation batch"
        );
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let delivery_ids: Vec<_> = reservations.iter().map(|(id, _)| *id).collect();
        let packet_ids: Vec<_> = reservations
            .iter()
            .map(|(_, packet)| i32::from(*packet))
            .collect();
        let changed = sqlx::query("UPDATE deliveries d SET packet_id=r.packet_id,phase=CASE d.qos WHEN 1 THEN 'await_puback' ELSE 'await_pubrec' END FROM UNNEST($2::uuid[],$3::int[]) AS r(id,packet_id) WHERE d.session_id=$1 AND d.id=r.id AND d.phase='queued' AND d.qos IN (1,2) AND d.packet_id IS NULL")
            .bind(id).bind(&delivery_ids).bind(&packet_ids).execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == reservations.len() as u64,
            "delivery reservation batch is no longer queued"
        );
        self.commit(tx, Uuid::new_v4()).await
    }
}
