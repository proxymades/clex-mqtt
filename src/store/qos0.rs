use super::*;

fn validate_batch(ids: &[Uuid]) -> Result<()> {
    ensure!(
        !ids.is_empty()
            && ids.len() <= 64
            && ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len(),
        "invalid QoS0 delivery batch"
    );
    Ok(())
}

impl Store {
    /// Commit ownership of one bounded, at-most-once attempt before writing to
    /// the peer. Queue quota remains occupied until completion or connection cleanup.
    pub async fn begin_qos0_batch(
        &self,
        id: Uuid,
        generation: i64,
        ids: &[Uuid],
    ) -> Result<Vec<Uuid>> {
        validate_batch(ids)?;
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let claimed = claim(&mut tx, id, ids).await?;
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(claimed)
    }

    pub async fn complete_qos0_batch(&self, id: Uuid, generation: i64, ids: &[Uuid]) -> Result<()> {
        validate_batch(ids)?;
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        removed_deliveries(
            sqlx::query(&delete_deliveries_sql(
                "d.session_id=$1 AND d.id=ANY($2) AND d.qos=0 AND d.phase='sending_qos0'",
                false,
            ))
            .bind(id)
            .bind(ids)
            .fetch_one(&mut *tx)
            .await?,
        )?;
        self.commit(tx, Uuid::new_v4()).await
    }
}

pub(super) async fn claim(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<Uuid>> {
    sqlx::query_scalar("UPDATE deliveries SET phase='sending_qos0' WHERE session_id=$1 AND id=ANY($2) AND qos=0 AND phase='queued' AND packet_id IS NULL RETURNING id")
        .bind(id).bind(ids).fetch_all(&mut **tx).await.map_err(Into::into)
}

/// Called while holding the state lock and the old session row, or during
/// exclusive startup recovery. An attempted QoS0 packet is never retried.
pub(super) async fn cleanup_started(
    tx: &mut Transaction<'_, Postgres>,
    id: Option<Uuid>,
) -> Result<()> {
    let statement = delete_deliveries_sql(
        if id.is_some() {
            "d.session_id=$1 AND d.qos=0 AND d.phase='sending_qos0'"
        } else {
            "d.qos=0 AND d.phase='sending_qos0'"
        },
        false,
    );
    let query = sqlx::query(&statement);
    let row = if let Some(id) = id {
        query.bind(id).fetch_one(&mut **tx).await?
    } else {
        query.fetch_one(&mut **tx).await?
    };
    removed_deliveries(row)?;
    Ok(())
}
