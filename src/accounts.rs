//! Account ownership and durable limits, independent of the HTTP interface.
use anyhow::{Result, ensure};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

mod upgrade;
pub use upgrade::{NameUpgrade, require_readable_names, upgrade_names};

use crate::{
    acl::AccessAction,
    domain::{SessionSpec, filter_covers},
    store::QuotaExceeded,
};

#[derive(Debug)]
pub(crate) struct ScopeDenied;
impl std::fmt::Display for ScopeDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("account scope is unavailable")
    }
}
impl std::error::Error for ScopeDenied {}

pub fn topic_prefix(username: &str) -> String {
    format!("accounts/{username}/")
}

pub fn mqtt_username(account: &str, device: &str) -> String {
    format!("{account}.{device}")
}

pub fn permits(
    owner: Option<Uuid>,
    owner_username: Option<&str>,
    superuser: bool,
    requested: &str,
    action: AccessAction,
) -> bool {
    match owner {
        Some(_) => {
            let Some(username) = owner_username else {
                return false;
            };
            if !(3..=32).contains(&username.len())
                || !username
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
            {
                return false;
            }
            let prefix = topic_prefix(username);
            if action == AccessAction::Subscribe {
                filter_covers(&format!("{prefix}#"), requested)
            } else {
                requested.starts_with(&prefix)
            }
        }
        None => superuser || !requested.starts_with("accounts/"),
    }
}

/// Called under the broker-state lock before opening or replacing a session.
pub(crate) async fn admit_connection(
    tx: &mut Transaction<'_, Postgres>,
    spec: &SessionSpec,
) -> Result<()> {
    let account = sqlx::query("SELECT a.id,a.enabled,a.max_connections,a.max_sessions FROM accounts a JOIN mqtt_users u ON u.owner_id=a.id WHERE u.username=$1 FOR SHARE OF a")
        .bind(&spec.principal).fetch_optional(&mut **tx).await?;
    let Some(account) = account else {
        return Ok(());
    };
    ensure!(
        account.try_get::<bool, _>("enabled")?,
        "account is disabled"
    );
    let id: Uuid = account.try_get("id")?;
    let counts: (i64,i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE s.online),count(*) FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=$1 AND NOT (s.principal=$2 AND s.client_id=$3)")
        .bind(id).bind(&spec.principal).bind(&spec.client_id).fetch_one(&mut **tx).await?;
    if counts.0 >= i64::from(account.try_get::<i32, _>("max_connections")?)
        || counts.1 >= i64::from(account.try_get::<i32, _>("max_sessions")?)
    {
        return Err(QuotaExceeded {
            scope: "account connections or stored sessions",
        }
        .into());
    }
    Ok(())
}

/// A durable per-account publication budget. Retries of the same accepted
/// operation and incoming QoS 2 duplicates bypass this function.
pub(crate) async fn admit_publication(
    tx: &mut Transaction<'_, Postgres>,
    username: &str,
    topic: &str,
) -> Result<()> {
    let user=sqlx::query("SELECT u.owner_id,a.username AS owner_username,u.superuser,u.enabled,coalesce(a.enabled,true) AS account_enabled FROM mqtt_users u LEFT JOIN accounts a ON a.id=u.owner_id WHERE u.username=$1")
        .bind(username).fetch_optional(&mut **tx).await?;
    let Some(user) = user else {
        anyhow::bail!("MQTT principal no longer exists");
    };
    let owner: Option<Uuid> = user.try_get("owner_id")?;
    ensure!(
        user.try_get::<bool, _>("enabled")? && user.try_get::<bool, _>("account_enabled")?,
        "MQTT principal is disabled"
    );
    ensure!(
        permits(
            owner,
            user.try_get::<Option<String>, _>("owner_username")?
                .as_deref(),
            user.try_get("superuser")?,
            topic,
            AccessAction::Publish
        ),
        "topic is outside account namespace"
    );
    let Some(owner) = owner else {
        return Ok(());
    };
    let accepted: Option<Uuid> = sqlx::query_scalar("WITH tick AS MATERIALIZED (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint AS bucket) UPDATE accounts SET publish_count=CASE WHEN publish_window=w.bucket THEN publish_count+1 ELSE 1 END,publish_window=w.bucket FROM tick w WHERE id=$1 AND enabled AND max_publish_rate>0 AND (publish_window<>w.bucket OR publish_count<max_publish_rate) RETURNING id")
        .bind(owner).fetch_optional(&mut **tx).await?;
    if accepted.is_none() {
        return Err(QuotaExceeded {
            scope: "account publication rate",
        }
        .into());
    }
    Ok(())
}

/// Count durable obligations, including all delivery copies, incoming QoS 2,
/// Retain and Will. During routing the current incoming QoS 2 is about to be
/// released in the same transaction and must not be counted a second time.
pub(crate) async fn check_storage(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    routing: Option<Uuid>,
) -> Result<()> {
    compact_messages(tx, Some(owner)).await?;
    let limits: (i32, i64) =
        sqlx::query_as("SELECT max_queued_messages,max_queued_bytes FROM accounts WHERE id=$1")
            .bind(owner)
            .fetch_one(&mut **tx)
            .await?;
    let usage = storage_usage(tx, owner, routing).await?;
    if limits.0 < 1 || limits.1 < 1 || usage.0 > i64::from(limits.0) || usage.1 > limits.1 {
        return Err(QuotaExceeded {
            scope: "account durable storage",
        }
        .into());
    }
    Ok(())
}

pub(crate) async fn check_principal_storage(
    tx: &mut Transaction<'_, Postgres>,
    principal: &str,
) -> Result<()> {
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_id FROM mqtt_users WHERE username=$1")
            .bind(principal)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
    if let Some(owner) = owner {
        check_storage(tx, owner, None).await?;
    }
    Ok(())
}

pub(crate) async fn check_routing_storage(
    tx: &mut Transaction<'_, Postgres>,
    message: Uuid,
    principal: &str,
) -> Result<()> {
    let owners: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT id FROM accounts WHERE id IN (SELECT owner_id FROM mqtt_users WHERE owner_id IS NOT NULL AND (username=$2 OR username IN (SELECT s.principal FROM sessions s JOIN deliveries d ON d.session_id=s.id WHERE d.message_id=$1))) OR id IN (SELECT owner_id FROM messages WHERE id=$1) ORDER BY id")
        .bind(message).bind(principal).fetch_all(&mut **tx).await?;
    for owner in owners {
        check_storage(tx, owner, Some(message)).await?;
    }
    Ok(())
}

pub(crate) async fn namespace_allowed(
    tx: &mut Transaction<'_, Postgres>,
    principal: &str,
    requested: &str,
    action: AccessAction,
) -> Result<bool> {
    let row=sqlx::query("SELECT u.owner_id,a.username AS owner_username,u.superuser,coalesce(a.enabled,true) AS account_enabled FROM mqtt_users u LEFT JOIN accounts a ON a.id=u.owner_id WHERE u.username=$1")
        .bind(principal).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(true);
    }; // Trusted internal bridge source.
    Ok(row.try_get::<bool, _>("account_enabled")?
        && permits(
            row.try_get("owner_id")?,
            row.try_get::<Option<String>, _>("owner_username")?
                .as_deref(),
            row.try_get("superuser")?,
            requested,
            action,
        ))
}

pub(crate) async fn storage_usage(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    routing: Option<Uuid>,
) -> Result<(i64, i64)> {
    let usage: (i64,i64)=sqlx::query_as("WITH account_prefix AS (SELECT convert_to('accounts/'||username||'/','UTF8') AS value FROM accounts WHERE id=$1), owned AS (SELECT s.id,s.will_message_id FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=$1), obligations AS (SELECT d.message_id,d.accounted_bytes FROM deliveries d JOIN owned s ON s.id=d.session_id UNION ALL SELECT i.message_id,NULL::bigint FROM incoming_qos2 i JOIN owned s ON s.id=i.session_id WHERE NOT i.released AND ($2::uuid IS NULL OR i.message_id<>$2) UNION ALL SELECT r.message_id,NULL::bigint FROM retained r JOIN account_prefix ap ON substring(r.topic FROM 1 FOR octet_length(ap.value))=ap.value UNION ALL SELECT b.message_id,NULL::bigint FROM bridge_deliveries b JOIN messages m ON m.id=b.message_id WHERE m.owner_id=$1 UNION ALL SELECT s.will_message_id,NULL::bigint FROM owned s WHERE s.will_message_id IS NOT NULL AND ($2::uuid IS NULL OR s.will_message_id<>$2)) SELECT count(*),coalesce(sum(coalesce(o.accounted_bytes,octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text))),0)::bigint FROM obligations o JOIN messages m ON m.id=o.message_id")
        .bind(owner).bind(routing).fetch_one(&mut **tx).await?;
    Ok(usage)
}

pub(crate) async fn compact_messages(
    tx: &mut Transaction<'_, Postgres>,
    owner: Option<Uuid>,
) -> Result<()> {
    sqlx::query("UPDATE messages m SET payload=''::bytea,topic=''::bytea,properties='{}'::jsonb,source_client_id=NULL,source_principal=NULL,compacted=true WHERE m.owner_id IS NOT NULL AND ($1::uuid IS NULL OR m.owner_id=$1) AND NOT m.compacted AND NOT EXISTS(SELECT 1 FROM retained r WHERE r.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM deliveries d WHERE d.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM incoming_qos2 i WHERE i.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.will_message_id=m.id) AND NOT EXISTS(SELECT 1 FROM bridge_deliveries b WHERE b.message_id=m.id)")
        .bind(owner).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readable_names_are_separate_from_internal_ids_and_optional_topic_levels() {
        let owner = Some(Uuid::new_v4());
        assert_eq!(topic_prefix("alex"), "accounts/alex/");
        assert_eq!(mqtt_username("alex", "esp32-01"), "alex.esp32-01");
        assert_ne!(
            mqtt_username("alex_one", "two"),
            mqtt_username("alex", "one_two")
        );
        for topic in [
            "accounts/alex/temperature",
            "accounts/alex/esp32/ask",
            "accounts/alex/devices/room/light",
        ] {
            assert!(permits(
                owner,
                Some("alex"),
                false,
                topic,
                AccessAction::Publish
            ));
        }
        for topic in [
            "accounts/alex2/temperature",
            "accounts/al/temperature",
            "accounts/bob/x",
        ] {
            assert!(!permits(
                owner,
                Some("alex"),
                true,
                topic,
                AccessAction::Publish
            ));
        }
        assert!(!permits(
            owner,
            None,
            true,
            "accounts/alex/x",
            AccessAction::Publish
        ));
        assert!(!permits(
            owner,
            Some("../alex"),
            true,
            "accounts/../alex/x",
            AccessAction::Publish
        ));
        assert!(!permits(
            owner,
            Some("alex"),
            false,
            "accounts/+/#",
            AccessAction::Subscribe
        ));
        assert!(permits(
            owner,
            Some("alex"),
            false,
            "accounts/alex/#",
            AccessAction::Subscribe
        ));
        assert!(!permits(
            None,
            None,
            false,
            "accounts/alex/x",
            AccessAction::Receive
        ));
    }
}
