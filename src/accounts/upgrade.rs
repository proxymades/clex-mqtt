//! Explicit offline transition from UUID-based account MQTT names.
use anyhow::{Result, ensure};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{mqtt_username, topic_prefix};

#[derive(Debug, Default)]
pub struct NameUpgrade {
    pub accounts: u64,
    pub mqtt_users: u64,
    pub sessions: u64,
    pub messages: u64,
    pub rules: u64,
    pub subscriptions: u64,
}

fn replace_scope(filter: &str, old: &str, new: &str) -> Option<String> {
    let parsed = crate::domain::parse_subscription_filter(filter)?;
    let suffix = parsed.topic_filter.strip_prefix(old)?;
    let offset = filter.len() - parsed.topic_filter.len();
    Some(format!("{}{new}{suffix}", &filter[..offset]))
}

/// Fail closed when an older installation has not performed its explicit
/// account-name transition. The check does not modify runtime data.
pub async fn require_readable_names(pool: &PgPool) -> Result<()> {
    let legacy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mqtt_users u JOIN accounts a ON a.id=u.owner_id WHERE u.username IS DISTINCT FROM a.username||'.'||u.account_label UNION ALL SELECT 1 FROM messages m JOIN accounts a ON a.id=m.owner_id WHERE substring(m.topic FROM 1 FOR octet_length(convert_to('accounts/'||a.id::text||'/','UTF8')))=convert_to('accounts/'||a.id::text||'/','UTF8') UNION ALL SELECT 1 FROM mqtt_acl_rules r JOIN mqtt_users u ON u.id=r.mqtt_user_id JOIN accounts a ON a.id=u.owner_id WHERE left(r.filter,length('accounts/'||a.id::text||'/'))='accounts/'||a.id::text||'/')")
        .fetch_one(pool).await?;
    ensure!(
        !legacy,
        "Legacy account MQTT names require the offline accounts upgrade-names --reset-mqtt-state command"
    );
    Ok(())
}

/// Requires a stopped broker. Existing account MQTT sessions and old account
/// message state are reset; passwords, web accounts and unowned users remain.
pub async fn upgrade_names(pool: &PgPool) -> Result<NameUpgrade> {
    let mut tx = pool.begin().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(crate::INSTANCE_LOCK)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        locked,
        "Stop the broker before upgrading account MQTT names"
    );
    for key in ["broker-state", "access-control"] {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':' || $1,0))",
        )
        .bind(key)
        .execute(&mut *tx)
        .await?;
    }
    let accounts = sqlx::query("SELECT id,username FROM accounts ORDER BY id FOR UPDATE")
        .fetch_all(&mut *tx)
        .await?;
    let mut report = NameUpgrade::default();
    for account in accounts {
        let owner: Uuid = account.try_get("id")?;
        let name: String = account.try_get("username")?;
        let old = format!("accounts/{owner}/");
        let new = topic_prefix(&name);
        let users = sqlx::query("SELECT id,username,account_label FROM mqtt_users WHERE owner_id=$1 ORDER BY id FOR UPDATE")
            .bind(owner).fetch_all(&mut *tx).await?;
        let mut renames = Vec::new();
        for user in users {
            let id: Uuid = user.try_get("id")?;
            let current: String = user.try_get("username")?;
            let label: Option<String> = user.try_get("account_label")?;
            let label =
                label.ok_or_else(|| anyhow::anyhow!("An owned MQTT user has no device label"))?;
            let expected = mqtt_username(&name, &label);
            if current == expected {
                continue;
            }
            ensure!(
                current == format!("acc_{}_{}", owner.simple(), label),
                "An owned MQTT user has an unsupported legacy name"
            );
            let collision: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mqtt_users WHERE username=$1 AND id<>$2)",
            )
            .bind(&expected)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(
                !collision,
                "A readable MQTT login conflicts with an existing user"
            );
            renames.push((id, expected));
        }
        let legacy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE substring(topic FROM 1 FOR octet_length($1::bytea))=$1 UNION ALL SELECT 1 FROM retained WHERE substring(topic FROM 1 FOR octet_length($1::bytea))=$1)")
            .bind(old.as_bytes()).fetch_one(&mut *tx).await?;
        let rules =
            sqlx::query("SELECT id,filter FROM mqtt_acl_rules WHERE position($1 in filter)>0")
                .bind(&old)
                .fetch_all(&mut *tx)
                .await?;
        let subscriptions =
            sqlx::query("SELECT id,filter FROM subscriptions WHERE position($1 in filter)>0")
                .bind(&old)
                .fetch_all(&mut *tx)
                .await?;
        if renames.is_empty() && !legacy && rules.is_empty() && subscriptions.is_empty() {
            continue;
        }
        report.accounts += 1;
        report.sessions += sqlx::query("DELETE FROM sessions WHERE principal IN (SELECT username FROM mqtt_users WHERE owner_id=$1)")
            .bind(owner).execute(&mut *tx).await?.rows_affected();
        // Never silently discard another principal's or a bridge's obligations.
        let referenced: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages m WHERE (m.owner_id=$1 OR substring(m.topic FROM 1 FOR octet_length($2::bytea))=$2) AND (EXISTS(SELECT 1 FROM deliveries d WHERE d.message_id=m.id) OR EXISTS(SELECT 1 FROM incoming_qos2 i WHERE i.message_id=m.id) OR EXISTS(SELECT 1 FROM sessions s WHERE s.will_message_id=m.id) OR EXISTS(SELECT 1 FROM bridge_deliveries b WHERE b.message_id=m.id)))")
            .bind(owner).bind(old.as_bytes()).fetch_one(&mut *tx).await?;
        ensure!(
            !referenced,
            "Old account messages are referenced by another principal or bridge; clear those obligations before resetting account MQTT state"
        );
        sqlx::query("DELETE FROM retained WHERE message_id IN (SELECT id FROM messages WHERE owner_id=$1 OR substring(topic FROM 1 FOR octet_length($2::bytea))=$2) OR substring(topic FROM 1 FOR octet_length($2::bytea))=$2")
            .bind(owner).bind(old.as_bytes()).execute(&mut *tx).await?;
        report.messages += sqlx::query("DELETE FROM messages WHERE owner_id=$1 OR substring(topic FROM 1 FOR octet_length($2::bytea))=$2")
            .bind(owner).bind(old.as_bytes()).execute(&mut *tx).await?.rows_affected();
        for row in rules {
            let filter: String = row.try_get("filter")?;
            if let Some(updated) = replace_scope(&filter, &old, &new) {
                report.rules += sqlx::query("UPDATE mqtt_acl_rules SET filter=$1 WHERE id=$2")
                    .bind(updated)
                    .bind(row.try_get::<Uuid, _>("id")?)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
        }
        for row in subscriptions {
            let filter: String = row.try_get("filter")?;
            if let Some(updated) = replace_scope(&filter, &old, &new) {
                report.subscriptions +=
                    sqlx::query("UPDATE subscriptions SET filter=$1 WHERE id=$2")
                        .bind(updated)
                        .bind(row.try_get::<Uuid, _>("id")?)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
            }
        }
        for (id, expected) in renames {
            report.mqtt_users += sqlx::query("UPDATE mqtt_users SET username=$1 WHERE id=$2")
                .bind(expected)
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
    }
    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_scopes_preserve_group_and_topic_suffix() {
        let old = "accounts/11111111-1111-4111-8111-111111111111/";
        assert_eq!(
            replace_scope(
                &format!("$share/workers/{old}sensor/+"),
                old,
                "accounts/alex/"
            ),
            Some("$share/workers/accounts/alex/sensor/+".into())
        );
        assert_eq!(
            replace_scope("other/accounts/alex/#", old, "accounts/alex/"),
            None
        );
    }
}
