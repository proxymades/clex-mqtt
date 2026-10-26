use super::*;
use crate::bridge::{Direction, Rule, can_cross};

impl Store {
    pub async fn seed_bridge_retained(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        rules: &[Rule],
    ) -> Result<()> {
        if !rules.iter().any(|rule| rule.direction == Direction::Out) {
            return Ok(());
        }
        let mut cursor = Vec::<u8>::new();
        loop {
            let rows = sqlx::query("SELECT m.id,m.topic,m.properties FROM retained r JOIN messages m ON m.id=r.message_id WHERE r.topic>$1 AND (m.expires_at IS NULL OR m.expires_at>now()) ORDER BY r.topic LIMIT 32").bind(&cursor).fetch_all(&mut **tx).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                cursor = row.try_get("topic")?;
                let publication = Publication {
                    topic: String::from_utf8(cursor.clone())?,
                    payload: Vec::new(),
                    qos: 1,
                    retain: true,
                    expires_at: None,
                    properties: serde_json::from_value(row.try_get("properties")?)?,
                };
                if !can_cross(&publication, id) {
                    continue;
                }
                for rule in rules.iter().filter(|r| r.direction == Direction::Out) {
                    if let Some(topic) = rule.map(&publication.topic) {
                        sqlx::query("INSERT INTO bridge_deliveries(id,bridge_id,message_id,topic) VALUES($1,$2,$3,$4) ON CONFLICT(bridge_id,message_id,topic) DO NOTHING")
                        .bind(Uuid::new_v4()).bind(id).bind(row.get::<Uuid,_>("id")).bind(topic.as_bytes()).execute(&mut **tx).await?;
                    }
                }
            }
            self.check_bridge_queue(tx, id).await?;
        }
        self.check_bridge_queue(tx, id).await
    }

    async fn check_bridge_queue(&self, tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
        let usage = sqlx::query("SELECT count(*) AS count,coalesce(sum(octet_length(m.payload)+octet_length(d.topic)+octet_length(m.properties::text)),0)::bigint AS bytes FROM bridge_deliveries d JOIN messages m ON m.id=d.message_id WHERE d.bridge_id=$1")
            .bind(id).fetch_one(&mut **tx).await?;
        if usage.get::<i64, _>("count") > self.max_queued_messages
            || usage.get::<i64, _>("bytes") > self.max_queued_bytes
        {
            return Err(QuotaExceeded {
                scope: "bridge outgoing delivery",
            }
            .into());
        }
        Ok(())
    }

    pub(super) async fn enqueue_bridges(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        message_id: Uuid,
        publication: &Publication,
    ) -> Result<()> {
        let rows =
            sqlx::query("SELECT id,rules FROM mqtt_bridges WHERE enabled ORDER BY id LIMIT 16")
                .fetch_all(&mut **tx)
                .await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            if !can_cross(publication, id) {
                continue;
            }
            let rules: Vec<Rule> = serde_json::from_value(row.try_get("rules")?)?;
            let mut inserted = false;
            for rule in rules.iter().filter(|r| r.direction == Direction::Out) {
                if let Some(topic) = rule.map(&publication.topic) {
                    sqlx::query("INSERT INTO bridge_deliveries(id,bridge_id,message_id,topic) VALUES($1,$2,$3,$4) ON CONFLICT(bridge_id,message_id,topic) DO NOTHING")
                        .bind(Uuid::new_v4()).bind(id).bind(message_id).bind(topic.as_bytes()).execute(&mut **tx).await?;
                    inserted = true;
                }
            }
            if inserted {
                self.check_bridge_queue(tx, id).await?;
            }
        }
        Ok(())
    }

    pub async fn import_bridge(
        &self,
        id: Uuid,
        revision: i64,
        rules: &[Rule],
        publication: &Publication,
    ) -> Result<Vec<Delivery>> {
        publication.validate()?;
        let mut tx = self.begin().await?;
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mqtt_bridges WHERE id=$1 AND revision=$2 AND enabled)",
        )
        .bind(id)
        .bind(revision)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(current, "Bridge configuration changed");
        let source = SessionIdentity {
            client_id: format!("clex-bridge-{id}").into_bytes(),
            principal: format!("@bridge:{id}"),
        };
        let mut deliveries = Vec::new();
        let mut publications = Vec::new();
        let operation = Uuid::new_v4();
        let mut topics = std::collections::HashSet::new();
        for rule in rules.iter().filter(|r| r.direction == Direction::In) {
            if let Some(topic) = rule.map(&publication.topic) {
                if !topics.insert(topic.clone()) {
                    continue;
                }
                let mut mapped = publication.clone();
                mapped.topic = topic;
                let message_id = Uuid::new_v4();
                insert_message(&mut tx, message_id, &mapped, Some(&source)).await?;
                deliveries.extend(
                    self.route(&mut tx, message_id, &mapped, &source, None)
                        .await?,
                );
                publications.push(mapped);
            }
        }
        if !publications.is_empty() {
            sqlx::query("UPDATE mqtt_bridges SET received=received+1 WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        self.commit(tx, operation).await?;
        for publication in publications {
            self.monitor
                .record(&publication, &source.client_id, &source.principal);
        }
        Ok(deliveries)
    }

    pub async fn next_bridge_delivery(
        &self,
        id: Uuid,
        revision: i64,
    ) -> Result<Option<(Uuid, Publication)>> {
        let row = sqlx::query("SELECT d.id,d.topic AS mapped_topic,m.topic,m.payload,m.qos,m.retain,m.expires_at,m.properties FROM bridge_deliveries d JOIN messages m ON m.id=d.message_id JOIN mqtt_bridges b ON b.id=d.bridge_id WHERE b.id=$1 AND b.revision=$2 AND b.enabled AND (m.expires_at IS NULL OR m.expires_at>now()) ORDER BY d.sequence LIMIT 1")
            .bind(id).bind(revision).fetch_optional(&self.pool).await?;
        row.map(|row| {
            let publication = Publication {
                topic: String::from_utf8(row.try_get("mapped_topic")?)?,
                payload: row.try_get("payload")?,
                qos: 1,
                retain: row.try_get("retain")?,
                expires_at: row.try_get("expires_at")?,
                properties: serde_json::from_value(row.try_get("properties")?)?,
            };
            Ok((row.try_get("id")?, publication))
        })
        .transpose()
    }
}
