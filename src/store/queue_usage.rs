use super::*;

/// All callers hold the schema state lock. Unknown usage (after an upgrade or
/// restart) is rebuilt once from the durable queue before enforcing admission.
pub(super) async fn queue_usage(
    tx: &mut Transaction<'_, Postgres>,
    session_id: Uuid,
) -> Result<(i64, i64)> {
    let usage: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT queued_count,queued_bytes FROM sessions WHERE id=$1")
            .bind(session_id)
            .fetch_one(&mut **tx)
            .await?;
    let usage = if let (Some(count), Some(bytes)) = usage {
        (count, bytes)
    } else {
        sqlx::query_as("UPDATE sessions SET queued_count=q.count,queued_bytes=q.bytes FROM (SELECT count(*) AS count,coalesce(sum(coalesce(d.accounted_bytes,octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text))),0)::bigint AS bytes FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.session_id=$1) q WHERE sessions.id=$1 RETURNING queued_count,queued_bytes")
            .bind(session_id).fetch_one(&mut **tx).await?
    };
    ensure!(usage.0 >= 0 && usage.1 >= 0, "queue usage is inconsistent");
    Ok(usage)
}

/// Only fixed, internal predicates are accepted here. Deletion, quota release
/// and underflow validation share one SQL statement and the caller's transaction.
pub(super) fn delete_deliveries_sql(predicate: &str, packet_ids: bool) -> String {
    removal_sql(predicate, packet_ids, "")
}

pub(super) fn delete_expired_deliveries_sql(session_only: bool) -> String {
    removal_sql(
        if session_only {
            "d.session_id=$1 AND d.phase='queued' AND d.message_id IN (SELECT id FROM expired)"
        } else {
            "d.phase='queued' AND d.message_id IN (SELECT id FROM expired)"
        },
        false,
        "expired AS MATERIALIZED (SELECT id FROM messages WHERE expires_at<=now()),",
    )
}

fn removal_sql(predicate: &str, packet_ids: bool, prefix: &str) -> String {
    let packets = if packet_ids {
        "(SELECT coalesce(array_agg(packet_id),'{}'::int[]) FROM removed)"
    } else {
        "'{}'::int[]"
    };
    format!(
        "WITH {prefix} removed AS (DELETE FROM deliveries d WHERE {predicate} RETURNING d.session_id,d.message_id,d.accounted_bytes,d.packet_id), usage AS (SELECT r.session_id,count(*) AS count,sum(coalesce(r.accounted_bytes,(SELECT octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text) FROM messages m WHERE m.id=r.message_id)))::bigint AS bytes FROM removed r GROUP BY r.session_id), adjusted AS (UPDATE sessions s SET queued_count=s.queued_count-u.count,queued_bytes=s.queued_bytes-u.bytes FROM usage u WHERE s.id=u.session_id RETURNING s.queued_count,s.queued_bytes) SELECT (SELECT count(*) FROM removed) AS removed_count,coalesce((SELECT bool_or(queued_count<0 OR queued_bytes<0) FROM adjusted),false) AS invalid,{packets} AS packet_ids"
    )
}

pub(super) struct RemovedDeliveries {
    pub count: i64,
    pub packet_ids: Vec<i32>,
}

pub(super) fn removed_deliveries(row: sqlx::postgres::PgRow) -> Result<RemovedDeliveries> {
    ensure!(!row.try_get::<bool, _>("invalid")?, "queue usage underflow");
    Ok(RemovedDeliveries {
        count: row.try_get("removed_count")?,
        packet_ids: row.try_get("packet_ids")?,
    })
}

impl Store {
    /// Keep the default expiry statement unchanged. An active observer adds a
    /// bounded projection of removed IDs to that same atomic deletion.
    pub(super) async fn expire_deliveries(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: Option<Uuid>,
    ) -> Result<(Option<Uuid>, Vec<Uuid>, bool)> {
        let observer = self.diagnostics.observer();
        let mut statement = delete_expired_deliveries_sql(session.is_some());
        if observer.is_some() {
            statement = statement.replace("RETURNING d.session_id", "RETURNING d.id,d.session_id");
            statement.push_str(",(SELECT coalesce(array_agg(id),'{}'::uuid[]) FROM (SELECT id FROM removed LIMIT 12800) traced) AS traced_ids");
        }
        let mut query = sqlx::query(&statement);
        if let Some(id) = session {
            query = query.bind(id);
        }
        let row = query.fetch_one(&mut **tx).await?;
        let ids = if observer.is_some() {
            row.try_get("traced_ids")?
        } else {
            vec![]
        };
        let removed = removed_deliveries(row)?;
        Ok((observer, ids, removed.count > 12800))
    }
}
