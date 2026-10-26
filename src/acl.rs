//! One ACL evaluator for MQTT authorization, shared routing and the access inspector.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, Postgres, Row};
use uuid::Uuid;

use crate::domain::{filter_covers, topic_matches};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessAction {
    Publish,
    Subscribe,
    Receive,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRule {
    pub id: Uuid,
    pub filter: String,
    pub allow: bool,
    pub source: &'static str,
    pub group_id: Option<Uuid>,
    pub group_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleMatch {
    #[serde(flatten)]
    pub rule: AccessRule,
    pub covers_request: bool,
    pub overlaps_request: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessDecision {
    pub allowed: bool,
    pub reason: &'static str,
    pub matching_rules: Vec<RuleMatch>,
    pub matching_total: usize,
}

impl AccessDecision {
    pub(crate) fn simple(allowed: bool, reason: &'static str) -> Self {
        Self {
            allowed,
            reason,
            matching_rules: Vec::new(),
            matching_total: 0,
        }
    }
}

pub(crate) async fn load_rules<'e, E>(
    executor: E,
    user_id: Uuid,
    publish: bool,
) -> Result<Vec<AccessRule>>
where
    E: Executor<'e, Database = Postgres>,
{
    let rows = sqlx::query("SELECT r.id,r.filter,r.allow,r.mqtt_user_id,r.group_id,g.name AS group_name FROM mqtt_acl_rules r LEFT JOIN mqtt_access_groups g ON g.id=r.group_id WHERE CASE WHEN $2 THEN r.publish ELSE r.subscribe END AND ((r.group_id IS NULL AND (r.mqtt_user_id=$1 OR r.mqtt_user_id IS NULL)) OR (r.mqtt_user_id IS NULL AND g.enabled AND EXISTS(SELECT 1 FROM mqtt_user_groups ug WHERE ug.mqtt_user_id=$1 AND ug.group_id=r.group_id))) ORDER BY r.priority DESC,r.allow ASC,r.filter,r.id")
        .bind(user_id).bind(publish).fetch_all(executor).await?;
    rows.into_iter()
        .map(|row| {
            let group_id: Option<Uuid> = row.try_get("group_id")?;
            let owner: Option<Uuid> = row.try_get("mqtt_user_id")?;
            Ok(AccessRule {
                id: row.try_get("id")?,
                filter: row.try_get("filter")?,
                allow: row.try_get("allow")?,
                source: if group_id.is_some() {
                    "group"
                } else if owner.is_some() {
                    "personal"
                } else {
                    "global"
                },
                group_id,
                group_name: row.try_get("group_name")?,
            })
        })
        .collect()
}

pub(crate) fn evaluate(
    rules: &[AccessRule],
    requested: &str,
    action: AccessAction,
    trace: bool,
) -> AccessDecision {
    let mut result = AccessDecision::simple(false, "no_matching_rule");
    let mut granted = false;
    let mut denied = false;
    for rule in rules {
        let covers = if action == AccessAction::Subscribe {
            filter_covers(&rule.filter, requested)
        } else {
            topic_matches(&rule.filter, requested)
        };
        let overlaps =
            action == AccessAction::Subscribe && filters_overlap(&rule.filter, requested);
        if covers || (!rule.allow && overlaps) {
            granted |= rule.allow && covers;
            denied |= !rule.allow;
            if trace {
                result.matching_total += 1;
                if result.matching_rules.len() < 100 {
                    result.matching_rules.push(RuleMatch {
                        rule: rule.clone(),
                        covers_request: covers,
                        overlaps_request: overlaps,
                    });
                }
            }
        }
    }
    result.allowed = granted && !denied;
    result.reason = if denied {
        "explicit_deny"
    } else if granted {
        "rule_allowed"
    } else {
        "no_matching_rule"
    };
    result
}

fn filters_overlap(left: &str, right: &str) -> bool {
    let a: Vec<_> = left.split('/').collect();
    let b: Vec<_> = right.split('/').collect();
    if (a[0].starts_with('$') && matches!(b[0], "#" | "+"))
        || (b[0].starts_with('$') && matches!(a[0], "#" | "+"))
    {
        return false;
    }
    let mut index = 0;
    loop {
        match (a.get(index), b.get(index)) {
            (Some(&"#"), _) | (_, Some(&"#")) => return true,
            (None, None) => return true,
            (Some(x), Some(y)) if x == y || *x == "+" || *y == "+" => {}
            _ => return false,
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rule(filter: &str, allow: bool) -> AccessRule {
        AccessRule {
            id: Uuid::new_v4(),
            filter: filter.into(),
            allow,
            source: "group",
            group_id: Some(Uuid::new_v4()),
            group_name: Some("Workshop".into()),
        }
    }
    #[test]
    fn deny_overlap_blocks_broad_subscription_but_preserves_other_topics() {
        let rules = [rule("site/#", true), rule("site/private/#", false)];
        let broad = evaluate(&rules, "site/#", AccessAction::Subscribe, true);
        assert!(!broad.allowed);
        assert_eq!(broad.reason, "explicit_deny");
        assert_eq!(broad.matching_total, 2);
        assert!(evaluate(&rules, "site/public/status", AccessAction::Receive, false).allowed);
        assert!(!evaluate(&rules, "site/private/status", AccessAction::Publish, false).allowed);
        assert!(!evaluate(&rules, "$SYS/status", AccessAction::Receive, false).allowed);
    }
    #[test]
    fn trace_is_bounded_and_preserves_exact_mqtt_levels() {
        let rules = vec![rule("a/+", true); 110];
        let decision = evaluate(&rules, "a/", AccessAction::Publish, true);
        assert!(decision.allowed);
        assert_eq!(decision.matching_total, 110);
        assert_eq!(decision.matching_rules.len(), 100);
        assert!(!evaluate(&rules, "a", AccessAction::Publish, false).allowed);
        assert!(!evaluate(&[], "a/", AccessAction::Publish, false).allowed);
    }
}
