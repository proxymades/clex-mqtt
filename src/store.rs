use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use sqlx::{Acquire, PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::domain::{
    CertificateBinding, Credential, Delivery, DeliveryBatch, DeliveryPhase, OpenedSession,
    Publication, PublicationProperties, Qos2Release, SessionSpec, Subscription,
    parse_subscription_filter, topic_matches,
};
use crate::monitor::{MessageBatch, MessageMonitor};
mod bridges;
mod delivery_acks;
mod delivery_reservations;
mod qos0;
mod queue_usage;
use queue_usage::{delete_deliveries_sql, queue_usage, removed_deliveries};

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
    monitor: MessageMonitor,
    diagnostics: crate::diagnostics::Diagnostics,
    metrics: crate::metrics::StorageMetrics,
    max_queued_messages: i64,
    max_queued_bytes: i64,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "database commit result is unknown for operation {operation_id}; reconnect and reconcile before retrying"
)]
pub struct CommitOutcomeUnknown {
    pub operation_id: Uuid,
    #[source]
    pub source: sqlx::Error,
}

#[derive(Debug, thiserror::Error)]
#[error("{scope} quota exceeded")]
pub struct QuotaExceeded {
    pub scope: &'static str,
}

#[derive(Debug, thiserror::Error)]
#[error("session connection was superseded")]
pub struct SessionSuperseded;

const MESSAGE_COLUMNS: &str = "m.id,m.topic,m.payload,m.qos,m.retain,m.properties,m.expires_at";

struct SharedTarget {
    subscription_id: Uuid,
    session_id: Uuid,
    last_sequence: i64,
    qos: u8,
    retain: bool,
    identifier: Option<u32>,
    online: bool,
}

struct DeliveryOptions<'a> {
    qos: u8,
    retain: bool,
    identifiers: Vec<u32>,
    shared_filter: Option<&'a str>,
}

impl Store {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(2))
            .connect(url)
            .await?;
        let store = Self::from_pool(pool);
        store.check_durability().await?;
        Ok(store)
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            monitor: MessageMonitor::default(),
            diagnostics: Default::default(),
            metrics: Default::default(),
            max_queued_messages: 10_000,
            max_queued_bytes: 64 * 1024 * 1024,
        }
    }

    pub fn with_queue_limits(mut self, max_messages: i64, max_bytes: i64) -> Self {
        self.max_queued_messages = max_messages.max(1);
        self.max_queued_bytes = max_bytes.max(1);
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn monitor(&self) -> &MessageMonitor {
        &self.monitor
    }

    pub fn diagnostics(&self) -> &crate::diagnostics::Diagnostics {
        &self.diagnostics
    }

    pub fn metrics(&self) -> &crate::metrics::StorageMetrics {
        &self.metrics
    }

    async fn commit(&self, tx: Transaction<'_, Postgres>, operation_id: Uuid) -> Result<()> {
        let started = std::time::Instant::now();
        let result = commit(tx, operation_id).await;
        self.metrics
            .commit_time
            .observe(started.elapsed().as_secs_f64());
        match &result {
            Ok(_) => {
                self.metrics.commits.inc();
            }
            Err(error) => {
                self.metrics.commit_errors.inc();
                if error.is::<CommitOutcomeUnknown>() {
                    self.metrics.unknown_commits.inc();
                }
            }
        }
        result
    }

    // Serialize this single-node engine's state mutations per schema. Fanout
    // cannot deadlock by locking publisher and subscriber rows in opposite order.
    async fn begin(&self) -> Result<Transaction<'_, Postgres>> {
        let started = std::time::Instant::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL synchronous_commit=on")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':broker-state',0))",
        )
        .execute(&mut *tx)
        .await?;
        self.metrics
            .lock_wait
            .observe(started.elapsed().as_secs_f64());
        Ok(tx)
    }

    pub async fn check_durability(&self) -> Result<()> {
        let fsync: String = sqlx::query_scalar("SHOW fsync")
            .fetch_one(&self.pool)
            .await?;
        let full_page_writes: String = sqlx::query_scalar("SHOW full_page_writes")
            .fetch_one(&self.pool)
            .await?;
        let synchronous_commit: String = sqlx::query_scalar("SHOW synchronous_commit")
            .fetch_one(&self.pool)
            .await?;
        ensure!(
            fsync == "on" && full_page_writes == "on" && synchronous_commit != "off",
            "PostgreSQL durable commit settings are required"
        );
        Ok(())
    }

    pub async fn credential(&self, username: &str) -> Result<Option<Credential>> {
        let row = sqlx::query("SELECT u.id,u.username,u.password_hash,u.scram_verifier,u.require_client_certificate,(u.enabled AND coalesce(a.enabled,true)) AS enabled,u.superuser,u.bound_client_id,u.owner_id,a.username AS owner_username FROM mqtt_users u LEFT JOIN accounts a ON a.id=u.owner_id WHERE u.username=$1")
            .bind(username).fetch_optional(&self.pool).await?;
        row.map(|r| {
            Ok(Credential {
                owner_id: r.try_get("owner_id")?,
                owner_username: r.try_get("owner_username")?,
                id: r.try_get("id")?,
                username: r.try_get("username")?,
                password_hash: r.try_get("password_hash")?,
                scram_verifier: r
                    .try_get::<Option<sqlx::types::Json<crate::scram::Verifier>>, _>(
                        "scram_verifier",
                    )?
                    .map(|value| value.0),
                enabled: r.try_get("enabled")?,
                require_client_certificate: r.try_get("require_client_certificate")?,
                superuser: r.try_get("superuser")?,
                bound_client_id: r.try_get("bound_client_id")?,
            })
        })
        .transpose()
    }

    pub async fn certificate_credential(
        &self,
        certificate: &crate::tls::VerifiedClientCertificate,
    ) -> Result<Option<(Credential, CertificateBinding)>> {
        let row = sqlx::query("SELECT c.id,c.mqtt_user_id,u.username,c.expires_at FROM mqtt_client_certificates c JOIN mqtt_users u ON u.id=c.mqtt_user_id WHERE c.fingerprint=$1 AND c.enabled AND c.expires_at>now() AND u.enabled")
            .bind(certificate.info.fingerprint.as_slice()).fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let username: String = row.try_get("username")?;
        let Some(credential) = self.credential(&username).await? else {
            return Ok(None);
        };
        if credential.id != row.try_get::<Uuid, _>("mqtt_user_id")? {
            return Ok(None);
        }
        Ok(Some((
            credential,
            CertificateBinding {
                id: row.try_get("id")?,
                fingerprint: certificate.info.fingerprint.to_vec(),
                expires_at: certificate.info.expires_at,
            },
        )))
    }

    pub async fn certificate_current(
        &self,
        user: Uuid,
        certificate: &CertificateBinding,
    ) -> Result<bool> {
        if certificate.expires_at <= Utc::now() {
            return Ok(false);
        }
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mqtt_client_certificates WHERE id=$1 AND mqtt_user_id=$2 AND fingerprint=$3 AND enabled AND expires_at>now())")
            .bind(certificate.id).bind(user).bind(&certificate.fingerprint).fetch_one(&self.pool).await?)
    }

    /// Upgrade only the credential that was actually verified. A concurrent
    /// password rotation/revocation must not inherit the old password's keys.
    pub async fn enable_scram(
        &self,
        credential: &Credential,
        verifier: crate::scram::Verifier,
    ) -> Result<()> {
        sqlx::query("UPDATE mqtt_users SET scram_verifier=$3 WHERE id=$1 AND password_hash=$2 AND enabled=true AND scram_verifier IS NULL")
            .bind(credential.id).bind(&credential.password_hash).bind(sqlx::types::Json(verifier))
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn authorize(
        &self,
        username: &str,
        topic_or_filter: &str,
        publish: bool,
    ) -> Result<bool> {
        let action = if publish {
            crate::acl::AccessAction::Publish
        } else {
            crate::acl::AccessAction::Subscribe
        };
        Ok(self
            .access_decision(username, topic_or_filter, action, false)
            .await?
            .allowed)
    }

    async fn access_decision(
        &self,
        username: &str,
        requested: &str,
        action: crate::acl::AccessAction,
        trace: bool,
    ) -> Result<crate::acl::AccessDecision> {
        use crate::acl::{AccessAction, AccessDecision};
        let Some(user) = self.credential(username).await? else {
            return Ok(AccessDecision::simple(false, "user_missing"));
        };
        if !user.enabled {
            return Ok(AccessDecision::simple(false, "user_disabled"));
        }
        if !crate::accounts::permits(
            user.owner_id,
            user.owner_username.as_deref(),
            user.superuser,
            requested,
            action,
        ) {
            return Ok(AccessDecision::simple(false, "account_scope"));
        }
        if user.superuser {
            return Ok(AccessDecision::simple(true, "superuser"));
        }
        let rules =
            crate::acl::load_rules(&self.pool, user.id, action == AccessAction::Publish).await?;
        Ok(crate::acl::evaluate(&rules, requested, action, trace))
    }

    pub async fn check_access(
        &self,
        username: &str,
        requested: &str,
        action: crate::acl::AccessAction,
    ) -> Result<crate::acl::AccessDecision> {
        use crate::acl::{AccessAction, AccessDecision};
        let topic = if action == AccessAction::Subscribe {
            let Some(parsed) = parse_subscription_filter(requested) else {
                return Ok(AccessDecision::simple(false, "invalid_request"));
            };
            parsed.topic_filter
        } else {
            if !crate::domain::valid_topic_name(requested) {
                return Ok(AccessDecision::simple(false, "invalid_request"));
            }
            requested
        };
        self.access_decision(username, topic, action, true).await
    }

    pub async fn authorize_subscription(&self, username: &str, raw_filter: &str) -> Result<bool> {
        let Some(parsed) = parse_subscription_filter(raw_filter) else {
            return Ok(false);
        };
        self.authorize(username, parsed.topic_filter, false).await
    }

    pub async fn open_session(&self, spec: SessionSpec) -> Result<OpenedSession> {
        self.open_session_internal(spec, None).await
    }

    /// Fence credential rotation/revocation while the session transaction opens.
    pub async fn open_session_authenticated(
        &self,
        spec: SessionSpec,
        credential_id: Uuid,
        password_hash: &str,
    ) -> Result<OpenedSession> {
        self.open_session_verified(spec, credential_id, password_hash, None)
            .await
    }

    pub async fn open_session_verified(
        &self,
        spec: SessionSpec,
        credential_id: Uuid,
        password_hash: &str,
        certificate: Option<&CertificateBinding>,
    ) -> Result<OpenedSession> {
        self.open_session_internal(spec, Some((credential_id, password_hash, certificate)))
            .await
    }

    async fn open_session_internal(
        &self,
        spec: SessionSpec,
        expected: Option<(Uuid, &str, Option<&CertificateBinding>)>,
    ) -> Result<OpenedSession> {
        ensure!(
            !spec.client_id.is_empty(),
            "empty internal client identifier"
        );
        ensure!(
            (3..=5).contains(&spec.protocol),
            "unsupported protocol version"
        );
        let mut tx = self.begin().await?;
        let user=sqlx::query("SELECT id,password_hash,enabled,bound_client_id,require_client_certificate FROM mqtt_users WHERE username=$1 FOR SHARE")
            .bind(&spec.principal).fetch_optional(&mut *tx).await?.context("MQTT principal no longer exists")?;
        ensure!(
            user.try_get::<bool, _>("enabled")?,
            "MQTT principal is disabled"
        );
        let bound: Option<Vec<u8>> = user.try_get("bound_client_id")?;
        ensure!(
            bound.is_none_or(|id| id == spec.client_id),
            "client identifier is not authorized"
        );
        if let Some((id, hash, certificate)) = expected {
            ensure!(
                user.try_get::<Uuid, _>("id")? == id
                    && user.try_get::<String, _>("password_hash")? == hash,
                "MQTT credentials changed during authentication"
            );
            ensure!(
                !user.try_get::<bool, _>("require_client_certificate")? || certificate.is_some(),
                "client certificate required"
            );
            if let Some(certificate) = certificate {
                ensure!(
                    certificate.expires_at > Utc::now(),
                    "client certificate expired"
                );
                let current: Option<Uuid> = sqlx::query_scalar("SELECT id FROM mqtt_client_certificates WHERE id=$1 AND mqtt_user_id=$2 AND fingerprint=$3 AND enabled AND expires_at>now() FOR SHARE")
                    .bind(certificate.id).bind(id).bind(&certificate.fingerprint).fetch_optional(&mut *tx).await?;
                ensure!(
                    current.is_some(),
                    "client certificate changed during authentication"
                );
            }
        }
        crate::accounts::admit_connection(&mut tx, &spec).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(jsonb_build_array($1::text,encode($2::bytea,'hex'))::text,0))")
            .bind(&spec.principal).bind(&spec.client_id)
            .execute(&mut *tx)
            .await?;
        let existing = sqlx::query(
            "SELECT id,principal,expires_at,online,expiry_interval,will_message_id,will_delay_interval,will_due_at FROM sessions WHERE client_id=$1 AND principal=$2 FOR UPDATE",
        )
        .bind(&spec.client_id)
        .bind(&spec.principal)
        .fetch_optional(&mut *tx)
        .await?;
        let mut session_present = false;
        let mut session_id = Uuid::new_v4();
        let mut monitor_batch = MessageBatch::default();
        let mut will_traces = Vec::new();
        let source = SessionIdentity {
            client_id: spec.client_id.clone(),
            principal: spec.principal.clone(),
        };
        if let Some(row) = existing {
            let id: Uuid = row.try_get("id")?;
            // A QoS0 attempt belongs to its original connection. Clear it
            // before a new generation can resume this session or route a Will.
            qos0::cleanup_started(&mut tx, Some(id)).await?;
            let principal: String = row.try_get("principal")?;
            ensure!(
                principal == spec.principal,
                "client identifier belongs to another principal"
            );
            let expiry: Option<DateTime<Utc>> = row.try_get("expires_at")?;
            let expired = expiry.is_some_and(|value| value <= Utc::now());
            let online: bool = row.try_get("online")?;
            let will_delay: i64 = row.try_get("will_delay_interval")?;
            let old_expiry_interval: i64 = row.try_get("expiry_interval")?;
            let will_due: Option<DateTime<Utc>> = row.try_get("will_due_at")?;
            if let Some(will_id) = row.try_get::<Option<Uuid>, _>("will_message_id")?
                && (spec.clean_start
                    || expired
                    || old_expiry_interval == 0
                    || (online && will_delay == 0)
                    || (!online && will_due.is_some_and(|value| value <= Utc::now())))
            {
                let mut will = load_publication(&mut tx, will_id).await?;
                if let Some(seconds) = will.properties.message_expiry_interval {
                    will.expires_at =
                        Some(Utc::now() + chrono::Duration::seconds(i64::from(seconds)));
                    sqlx::query("UPDATE messages SET expires_at=$2 WHERE id=$1")
                        .bind(will_id)
                        .bind(will.expires_at)
                        .execute(&mut *tx)
                        .await?;
                }
                let trace =
                    self.diagnostics
                        .capture(will_id, &will, &source.client_id, &source.principal);
                if let Some(trace) = &trace {
                    trace.note("will", None, None);
                }
                let routed = self
                    .route(&mut tx, will_id, &will, &source, trace.as_ref())
                    .await?;
                if let Some(trace) = trace {
                    trace.prepare(&routed);
                    will_traces.push(trace);
                }
                monitor_batch.record(&will, &spec.client_id, &principal);
            }
            if !spec.clean_start && expiry.is_none_or(|value| value > Utc::now()) {
                session_id = id;
                session_present = true;
            } else {
                sqlx::query("DELETE FROM sessions WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let will_id = if let Some(will) = &spec.will {
            will.validate()?;
            ensure!(
                crate::accounts::namespace_allowed(
                    &mut tx,
                    &spec.principal,
                    &will.topic,
                    crate::acl::AccessAction::Publish
                )
                .await?,
                "Will topic is outside account namespace"
            );
            let id = Uuid::new_v4();
            insert_message(&mut tx, id, will, Some(&source)).await?;
            Some(id)
        } else {
            None
        };
        let generation: i64 = if session_present {
            sqlx::query_scalar("UPDATE sessions SET generation=nextval(pg_get_serial_sequence('sessions','generation')), protocol=$2,online=true,expiry_interval=$3,expires_at=NULL,will_message_id=$4,will_delay_interval=$5,will_due_at=NULL,updated_at=now() WHERE id=$1 RETURNING generation")
                .bind(session_id).bind(i32::from(spec.protocol)).bind(i64::from(spec.expiry_interval))
                .bind(will_id).bind(i64::from(spec.will_delay_interval)).fetch_one(&mut *tx).await?
        } else {
            sqlx::query_scalar("INSERT INTO sessions(id,client_id,principal,protocol,online,expiry_interval,will_message_id,will_delay_interval,queued_count,queued_bytes) VALUES($1,$2,$3,$4,true,$5,$6,$7,0,0) RETURNING generation")
                .bind(session_id).bind(&spec.client_id).bind(&spec.principal).bind(i32::from(spec.protocol))
                .bind(i64::from(spec.expiry_interval)).bind(will_id).bind(i64::from(spec.will_delay_interval)).fetch_one(&mut *tx).await?
        };
        cleanup_shared_deliveries(&mut tx).await?;
        crate::accounts::check_principal_storage(&mut tx, &spec.principal).await?;
        let committed = self.commit(tx, session_id).await;
        for trace in will_traces {
            trace.finish(match &committed {
                Ok(()) => "committed",
                Err(e) if e.is::<CommitOutcomeUnknown>() => "commit_unknown",
                Err(_) => "interrupted",
            });
        }
        committed?;
        self.monitor.commit_batch(monitor_batch);
        Ok(OpenedSession {
            session_id,
            generation,
            session_present,
        })
    }

    pub async fn is_session_current(&self, id: Uuid, generation: i64) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=$1 AND generation=$2 AND online)",
        )
        .bind(id)
        .bind(generation)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn close_session(
        &self,
        id: Uuid,
        generation: i64,
        expiry: u32,
        graceful: bool,
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        let row = sqlx::query("SELECT will_message_id,will_delay_interval FROM sessions WHERE id=$1 AND generation=$2 AND online FOR UPDATE")
            .bind(id).bind(generation).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(()) };
        qos0::cleanup_started(&mut tx, Some(id)).await?;
        let will: Option<Uuid> = row.try_get("will_message_id")?;
        let delay: i64 = row.try_get("will_delay_interval")?;
        if graceful || will.is_none() {
            if expiry == 0 {
                sqlx::query("DELETE FROM sessions WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("UPDATE sessions SET online=false,expiry_interval=$2,expires_at=now()+$2::double precision*interval '1 second',will_message_id=NULL,will_due_at=NULL,updated_at=now() WHERE id=$1")
                    .bind(id).bind(i64::from(expiry)).execute(&mut *tx).await?;
            }
        } else {
            sqlx::query("UPDATE sessions SET online=false,expiry_interval=$2,expires_at=now()+$2::double precision*interval '1 second',will_due_at=now()+$3::double precision*interval '1 second',updated_at=now() WHERE id=$1")
                .bind(id).bind(i64::from(expiry)).bind(delay.min(i64::from(expiry))).execute(&mut *tx).await?;
        }
        cleanup_shared_deliveries(&mut tx).await?;
        self.commit(tx, id).await
    }

    pub async fn recover_startup(&self) -> Result<()> {
        self.check_durability().await?;
        let mut tx = self.begin().await?;
        // Counters are derived state. Reconcile lazily after restart as well as
        // after a schema upgrade, using persisted deliveries as the authority.
        sqlx::query("UPDATE sessions SET queued_count=NULL,queued_bytes=NULL")
            .execute(&mut *tx)
            .await?;
        qos0::cleanup_started(&mut tx, None).await?;
        sqlx::query("UPDATE sessions SET online=false,expires_at=now()+expiry_interval::double precision*interval '1 second',will_due_at=CASE WHEN will_message_id IS NOT NULL THEN now()+least(will_delay_interval,expiry_interval)::double precision*interval '1 second' ELSE NULL END,updated_at=now() WHERE online")
            .execute(&mut *tx).await?;
        self.commit(tx, Uuid::new_v4()).await
    }

    pub async fn subscribe(
        &self,
        id: Uuid,
        generation: i64,
        sub: &Subscription,
    ) -> Result<Vec<Delivery>> {
        sub.validate()?;
        let shared = parse_subscription_filter(&sub.filter)
            .context("invalid subscription filter")?
            .share_name
            .is_some();
        let mut tx = self.begin().await?;
        let source = lock_session(&mut tx, id, generation).await?;
        let inner = parse_subscription_filter(&sub.filter)
            .context("invalid subscription filter")?
            .topic_filter;
        ensure!(
            crate::accounts::namespace_allowed(
                &mut tx,
                &source.principal,
                inner,
                crate::acl::AccessAction::Subscribe
            )
            .await?,
            "subscription is outside account namespace"
        );
        if shared {
            let protocol: i32 = sqlx::query_scalar("SELECT protocol FROM sessions WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            ensure!(protocol == 5, "shared subscriptions require MQTT 5");
        }
        let existed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE session_id=$1 AND filter=$2)",
        )
        .bind(id)
        .bind(&sub.filter)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO subscriptions(id,session_id,filter,qos,no_local,retain_as_published,retain_handling,subscription_identifier) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(session_id,filter) DO UPDATE SET qos=EXCLUDED.qos,no_local=EXCLUDED.no_local,retain_as_published=EXCLUDED.retain_as_published,retain_handling=EXCLUDED.retain_handling,subscription_identifier=EXCLUDED.subscription_identifier")
            .bind(Uuid::new_v4()).bind(id).bind(&sub.filter).bind(i32::from(sub.qos)).bind(sub.no_local)
            .bind(sub.retain_as_published).bind(i32::from(sub.retain_handling)).bind(sub.subscription_identifier.map(|v|v as i32)).execute(&mut *tx).await?;
        let mut deliveries = Vec::new();
        if !shared && (sub.retain_handling == 0 || (sub.retain_handling == 1 && !existed)) {
            let rows = sqlx::query(&format!("SELECT {MESSAGE_COLUMNS},m.source_client_id,m.source_principal FROM retained r JOIN messages m ON m.id=r.message_id WHERE m.expires_at IS NULL OR m.expires_at>now()"))
                .fetch_all(&mut *tx).await?;
            for row in rows {
                if sub.no_local
                    && row
                        .try_get::<Option<Vec<u8>>, _>("source_client_id")?
                        .as_ref()
                        == Some(&source.client_id)
                    && row
                        .try_get::<Option<String>, _>("source_principal")?
                        .as_ref()
                        == Some(&source.principal)
                {
                    continue;
                }
                let publication = decode_publication(&row)?;
                if topic_matches(&sub.filter, &publication.topic)
                    && crate::accounts::namespace_allowed(
                        &mut tx,
                        &source.principal,
                        &publication.topic,
                        crate::acl::AccessAction::Receive,
                    )
                    .await?
                {
                    let message_id: Uuid = row.try_get("id")?;
                    let delivery = insert_delivery(
                        &mut tx,
                        id,
                        message_id,
                        &publication,
                        DeliveryOptions {
                            qos: publication.qos.min(sub.qos),
                            retain: true,
                            identifiers: sub.subscription_identifier.into_iter().collect(),
                            shared_filter: None,
                        },
                    )
                    .await?;
                    deliveries.push(delivery);
                }
            }
        }
        self.check_queue(&mut tx, id).await?;
        crate::accounts::check_principal_storage(&mut tx, &source.principal).await?;
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(deliveries)
    }

    pub async fn unsubscribe(&self, id: Uuid, generation: i64, filter: &str) -> Result<bool> {
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let deleted = sqlx::query("DELETE FROM subscriptions WHERE session_id=$1 AND filter=$2")
            .bind(id)
            .bind(filter)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        cleanup_shared_deliveries(&mut tx).await?;
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(deleted)
    }

    pub async fn accept_publication(
        &self,
        id: Uuid,
        generation: i64,
        publication: &Publication,
        source_client_id: &[u8],
    ) -> Result<Vec<Delivery>> {
        let operation_id = Uuid::new_v4();
        self.accept_publication_with_id(operation_id, id, generation, publication, source_client_id)
            .await
    }

    pub async fn accept_publication_with_id(
        &self,
        operation_id: Uuid,
        id: Uuid,
        generation: i64,
        publication: &Publication,
        source_client_id: &[u8],
    ) -> Result<Vec<Delivery>> {
        let received = std::time::Instant::now();
        publication.validate()?;
        let mut tx = self.begin().await?;
        let source = lock_session(&mut tx, id, generation).await?;
        let prior: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id=$1)")
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
        if prior {
            let deliveries =
                fetch_deliveries(&mut tx, "WHERE d.message_id=$1", operation_id, None).await?;
            tx.rollback().await?;
            return Ok(deliveries);
        }
        ensure!(
            source.client_id == source_client_id,
            "publication source must match its session"
        );
        let trace = self.diagnostics.capture_at(
            operation_id,
            publication,
            &source.client_id,
            &source.principal,
            received,
        );
        if let Some(t) = &trace {
            t.note("session_locked", None, None);
        }
        crate::accounts::admit_publication(&mut tx, &source.principal, &publication.topic).await?;
        insert_message(&mut tx, operation_id, publication, Some(&source)).await?;
        let deliveries = self
            .route(&mut tx, operation_id, publication, &source, trace.as_ref())
            .await?;
        if let Some(trace) = &trace {
            trace.prepare(&deliveries);
        }
        let committed = self.commit(tx, operation_id).await;
        if let Some(trace) = trace {
            trace.finish(match &committed {
                Ok(()) => "committed",
                Err(e) if e.is::<CommitOutcomeUnknown>() => "commit_unknown",
                Err(_) => "interrupted",
            });
        }
        committed?;
        self.monitor
            .record(publication, &source.client_id, &source.principal);
        Ok(deliveries)
    }

    pub async fn receive_qos2(
        &self,
        id: Uuid,
        generation: i64,
        packet_id: u16,
        publication: &Publication,
    ) -> Result<bool> {
        ensure!(
            packet_id != 0 && publication.qos == 2,
            "invalid incoming QoS2 publication"
        );
        let received = std::time::Instant::now();
        publication.validate()?;
        let mut tx = self.begin().await?;
        let source = lock_session(&mut tx, id, generation).await?;
        let previous=sqlx::query("SELECT i.released,m.topic,m.payload,m.retain FROM incoming_qos2 i LEFT JOIN messages m ON m.id=i.message_id WHERE i.session_id=$1 AND i.packet_id=$2 FOR UPDATE OF i")
            .bind(id).bind(i32::from(packet_id)).fetch_optional(&mut *tx).await?;
        if let Some(row) = previous
            && !row.try_get::<bool, _>("released")?
        {
            let topic: Vec<u8> = row.try_get("topic")?;
            let payload: Vec<u8> = row.try_get("payload")?;
            let retain: bool = row.try_get("retain")?;
            ensure!(
                topic == publication.topic.as_bytes()
                    && payload == publication.payload
                    && retain == publication.retain,
                "packet identifier is already in use"
            );
            tx.rollback().await?;
            return Ok(false);
        }
        let message_id = Uuid::new_v4();
        let trace = self.diagnostics.capture_at(
            message_id,
            publication,
            &source.client_id,
            &source.principal,
            received,
        );
        if let Some(t) = &trace {
            t.note("session_locked", None, None);
        }
        crate::accounts::admit_publication(&mut tx, &source.principal, &publication.topic).await?;
        insert_message(&mut tx, message_id, publication, Some(&source)).await?;
        sqlx::query("INSERT INTO incoming_qos2(session_id,packet_id,message_id,released) VALUES($1,$2,$3,false) ON CONFLICT(session_id,packet_id) DO UPDATE SET message_id=EXCLUDED.message_id,released=false")
            .bind(id).bind(i32::from(packet_id)).bind(message_id).execute(&mut *tx).await?;
        let usage = sqlx::query("SELECT count(*) AS count,coalesce(sum(octet_length(m.payload)+octet_length(m.topic)+octet_length(m.properties::text)),0)::bigint AS bytes FROM incoming_qos2 i JOIN messages m ON m.id=i.message_id WHERE i.session_id=$1 AND NOT i.released")
            .bind(id).fetch_one(&mut *tx).await?;
        if usage.try_get::<i64, _>("count")? > self.max_queued_messages
            || usage.try_get::<i64, _>("bytes")? > self.max_queued_bytes
        {
            if let Some(t) = &trace {
                t.note("quota_exceeded", Some(id), None);
            }
            return Err(QuotaExceeded {
                scope: "incoming QoS2 session",
            }
            .into());
        }
        crate::accounts::check_principal_storage(&mut tx, &source.principal).await?;
        let committed = self.commit(tx, message_id).await;
        if let Some(trace) = trace {
            trace.finish(match &committed {
                Ok(()) => "await_pubrel",
                Err(e) if e.is::<CommitOutcomeUnknown>() => "commit_unknown",
                Err(_) => "interrupted",
            });
        }
        committed?;
        Ok(true)
    }

    pub async fn release_qos2(
        &self,
        id: Uuid,
        generation: i64,
        packet_id: u16,
    ) -> Result<Qos2Release> {
        let mut tx = self.begin().await?;
        let source = lock_session(&mut tx, id, generation).await?;
        let row=sqlx::query("SELECT message_id,released FROM incoming_qos2 WHERE session_id=$1 AND packet_id=$2 FOR UPDATE")
            .bind(id).bind(i32::from(packet_id)).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(Qos2Release {
                found: false,
                deliveries: vec![],
            });
        };
        if row.try_get::<bool, _>("released")? {
            return Ok(Qos2Release {
                found: true,
                deliveries: vec![],
            });
        }
        let message_id: Uuid = row.try_get("message_id")?;
        let publication = load_publication(&mut tx, message_id).await?;
        let trace = self.diagnostics.resume(message_id);
        if let Some(trace) = &trace {
            trace.note("pubrel_received", None, None);
        }
        let deliveries = self
            .route(&mut tx, message_id, &publication, &source, trace.as_ref())
            .await?;
        sqlx::query("UPDATE incoming_qos2 SET released=true,message_id=NULL WHERE session_id=$1 AND packet_id=$2")
            .bind(id).bind(i32::from(packet_id)).execute(&mut *tx).await?;
        if let Some(trace) = &trace {
            trace.prepare(&deliveries);
        }
        let committed = self.commit(tx, message_id).await;
        if let Some(trace) = trace {
            trace.finish(match &committed {
                Ok(()) => "committed",
                Err(e) if e.is::<CommitOutcomeUnknown>() => "commit_unknown",
                Err(_) => "interrupted",
            });
        }
        committed?;
        self.monitor
            .record(&publication, &source.client_id, &source.principal);
        Ok(Qos2Release {
            found: true,
            deliveries,
        })
    }

    pub async fn pending_deliveries(
        &self,
        id: Uuid,
        generation: i64,
        limit: i64,
    ) -> Result<Vec<Delivery>> {
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        // Once QoS2 PUBLISH is sent, its handshake must finish even after expiry.
        let expired = self.expire_deliveries(&mut tx, Some(id)).await?;
        let rows=sqlx::query("SELECT d.id AS delivery_id,d.session_id,d.sequence,d.packet_id,d.phase,d.qos AS delivery_qos,d.retain AS delivery_retain,d.subscription_identifiers,m.id,m.topic,m.payload,m.qos,m.retain,m.properties,m.expires_at FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.session_id=$1 AND d.phase<>'sending_qos0' ORDER BY d.sequence LIMIT $2")
            .bind(id).bind(limit.clamp(1,10_000)).fetch_all(&mut *tx).await?;
        let deliveries = rows
            .iter()
            .map(decode_delivery)
            .collect::<Result<Vec<_>>>()?;
        self.commit(tx, Uuid::new_v4()).await?;
        self.diagnostics.expired(expired);
        Ok(deliveries)
    }

    /// A read-only fast path for connections without locally tracked deliveries.
    /// A concurrent enqueue still wakes the connection through the normal registry.
    pub async fn has_deliveries(&self, id: Uuid, generation: i64) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deliveries WHERE session_id=$1 AND phase<>'sending_qos0') FROM sessions WHERE id=$1 AND generation=$2 AND online")
            .bind(id).bind(generation).fetch_optional(&self.pool).await?
            .ok_or_else(|| SessionSuperseded.into())
    }

    /// Keep every reserved Packet Identifier visible while materializing only
    /// the next bounded, unsent payload batch. ACK processing stays independent
    /// of the length of the client's unsent backlog.
    pub async fn delivery_batch(
        &self,
        id: Uuid,
        generation: i64,
        sent: &[Uuid],
        can_send_publish: bool,
    ) -> Result<DeliveryBatch> {
        self.load_delivery_batch(id, generation, sent, can_send_publish, false)
            .await
    }

    /// Select bounded payloads and commit their QoS0 attempt ownership in the
    /// same transaction. The returned payloads preserve the original wire order.
    pub async fn outgoing_batch(
        &self,
        id: Uuid,
        generation: i64,
        sent: &[Uuid],
        can_send_publish: bool,
    ) -> Result<DeliveryBatch> {
        self.load_delivery_batch(id, generation, sent, can_send_publish, true)
            .await
    }

    async fn load_delivery_batch(
        &self,
        id: Uuid,
        generation: i64,
        sent: &[Uuid],
        can_send_publish: bool,
        claim_qos0: bool,
    ) -> Result<DeliveryBatch> {
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let expired = self.expire_deliveries(&mut tx, Some(id)).await?;
        let inflight=sqlx::query_as::<_,(Uuid,i32)>("SELECT id,packet_id FROM deliveries WHERE session_id=$1 AND packet_id IS NOT NULL ORDER BY sequence LIMIT 65535")
            .bind(id).fetch_all(&mut *tx).await?.into_iter().map(|(id,packet)| (id,packet as u16)).collect();
        // Bound the delivery side before joining payloads. A cached PostgreSQL
        // plan may otherwise join/sort the entire backlog before applying LIMIT.
        let rows=sqlx::query("WITH pending AS MATERIALIZED (SELECT * FROM deliveries WHERE session_id=$1 AND phase<>'sending_qos0' AND NOT(id=ANY($2)) AND ($3 OR qos=0 OR phase='await_pubcomp') ORDER BY sequence LIMIT 64) SELECT d.id AS delivery_id,d.session_id,d.sequence,d.packet_id,d.phase,d.qos AS delivery_qos,d.retain AS delivery_retain,d.subscription_identifiers,m.id,m.topic,m.payload,m.qos,m.retain,m.properties,m.expires_at FROM pending d JOIN messages m ON m.id=d.message_id ORDER BY d.sequence")
            .bind(id).bind(sent).bind(can_send_publish).fetch_all(&mut *tx).await?;
        let deliveries = rows
            .iter()
            .map(decode_delivery)
            .collect::<Result<Vec<_>>>()?;
        let started_qos0 = if claim_qos0 {
            let ids: Vec<_> = deliveries
                .iter()
                .filter(|delivery| delivery.publication.qos == 0)
                .map(|delivery| delivery.id)
                .collect();
            if ids.is_empty() {
                Vec::new()
            } else {
                qos0::claim(&mut tx, id, &ids).await?
            }
        } else {
            Vec::new()
        };
        self.commit(tx, Uuid::new_v4()).await?;
        self.diagnostics.expired(expired);
        Ok(DeliveryBatch {
            inflight,
            deliveries,
            started_qos0,
        })
    }

    pub async fn reserve_delivery(
        &self,
        id: Uuid,
        generation: i64,
        delivery_id: Uuid,
        packet_id: u16,
    ) -> Result<Delivery> {
        ensure!(packet_id != 0, "invalid packet identifier");
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let row=sqlx::query("UPDATE deliveries SET packet_id=$3,phase=CASE qos WHEN 1 THEN 'await_puback' WHEN 2 THEN 'await_pubrec' ELSE 'queued' END WHERE id=$1 AND session_id=$2 AND phase='queued' AND qos>0 RETURNING id")
            .bind(delivery_id).bind(id).bind(i32::from(packet_id)).fetch_optional(&mut *tx).await?;
        ensure!(row.is_some(), "delivery is no longer queued");
        let mut deliveries = fetch_deliveries(&mut tx, "WHERE d.id=$1", delivery_id, None).await?;
        let delivery = deliveries.pop().context("reserved delivery disappeared")?;
        self.commit(tx, delivery_id).await?;
        Ok(delivery)
    }

    pub async fn advance_qos2(&self, id: Uuid, generation: i64, packet_id: u16) -> Result<bool> {
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let changed=sqlx::query("UPDATE deliveries SET phase='await_pubcomp' WHERE session_id=$1 AND packet_id=$2 AND phase IN ('await_pubrec','await_pubcomp')")
            .bind(id).bind(i32::from(packet_id)).execute(&mut *tx).await?.rows_affected()>0;
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(changed)
    }

    pub async fn acknowledge(
        &self,
        id: Uuid,
        generation: i64,
        packet_id: u16,
        phase: DeliveryPhase,
    ) -> Result<bool> {
        ensure!(
            matches!(
                phase,
                DeliveryPhase::AwaitPubAck
                    | DeliveryPhase::AwaitPubRec
                    | DeliveryPhase::AwaitPubComp
            ),
            "invalid acknowledgement phase"
        );
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        let deleted = removed_deliveries(
            sqlx::query(&delete_deliveries_sql(
                "d.session_id=$1 AND d.packet_id=$2 AND d.phase=$3",
                false,
            ))
            .bind(id)
            .bind(i32::from(packet_id))
            .bind(phase.as_str())
            .fetch_one(&mut *tx)
            .await?,
        )?
        .count
            > 0;
        self.commit(tx, Uuid::new_v4()).await?;
        Ok(deleted)
    }

    pub async fn discard_delivery(
        &self,
        id: Uuid,
        generation: i64,
        delivery_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        lock_session(&mut tx, id, generation).await?;
        removed_deliveries(
            sqlx::query(&delete_deliveries_sql("d.id=$1 AND d.session_id=$2", false))
                .bind(delivery_id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        self.commit(tx, delivery_id).await
    }

    pub async fn maintenance(&self) -> Result<Vec<Delivery>> {
        let mut tx = self.begin().await?;
        // Expired queued messages must release quota before attempting due Wills.
        // Inflight QoS handshakes retain their durable state until completion.
        sqlx::query("DELETE FROM retained r USING messages m WHERE r.message_id=m.id AND m.expires_at<=now()").execute(&mut *tx).await?;
        let expired = self.expire_deliveries(&mut tx, None).await?;
        let wills=sqlx::query("SELECT id,client_id,principal,will_message_id FROM sessions WHERE NOT online AND will_message_id IS NOT NULL AND will_due_at<=now() FOR UPDATE")
            .fetch_all(&mut *tx).await?;
        let mut deliveries = Vec::new();
        let mut monitor_batch = MessageBatch::default();
        let mut will_traces = Vec::new();
        for row in wills {
            let session_id: Uuid = row.try_get("id")?;
            let message_id: Uuid = row.try_get("will_message_id")?;
            let source = SessionIdentity {
                client_id: row.try_get("client_id")?,
                principal: row.try_get("principal")?,
            };
            // A blocked recipient leaves this Will pending, without rolling back
            // independent Wills or expiry/garbage collection in the outer pass.
            let mut will_tx = tx.begin().await?;
            match self
                .route_will(&mut will_tx, session_id, message_id, &source)
                .await
            {
                Ok((routed, publication, trace)) => {
                    if let Some(trace) = trace {
                        will_traces.push(trace);
                    }
                    will_tx.commit().await?;
                    deliveries.extend(routed);
                    monitor_batch.record(&publication, &source.client_id, &source.principal);
                }
                Err(error)
                    if error.is::<QuotaExceeded>()
                        || error.is::<crate::accounts::ScopeDenied>() =>
                {
                    will_tx.rollback().await?
                }
                Err(error) => return Err(error),
            }
        }
        sqlx::query("DELETE FROM sessions WHERE NOT online AND expires_at<=now() AND will_message_id IS NULL").execute(&mut *tx).await?;
        cleanup_shared_deliveries(&mut tx).await?;
        crate::accounts::compact_messages(&mut tx, None).await?;
        // Protect fresh operations until their result can be reconciled after a lost COMMIT response.
        sqlx::query("DELETE FROM bridge_deliveries d USING messages m WHERE d.message_id=m.id AND m.expires_at<=now()").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM messages m WHERE m.created_at<now()-interval '1 hour' AND NOT EXISTS(SELECT 1 FROM retained r WHERE r.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM deliveries d WHERE d.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM incoming_qos2 i WHERE i.message_id=m.id) AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.will_message_id=m.id) AND NOT EXISTS(SELECT 1 FROM bridge_deliveries b WHERE b.message_id=m.id)")
            .execute(&mut *tx).await?;
        let committed = self.commit(tx, Uuid::new_v4()).await;
        for trace in will_traces {
            trace.finish(match &committed {
                Ok(()) => "committed",
                Err(e) if e.is::<CommitOutcomeUnknown>() => "commit_unknown",
                Err(_) => "interrupted",
            });
        }
        committed?;
        self.diagnostics.expired(expired);
        self.monitor.commit_batch(monitor_batch);
        Ok(deliveries)
    }

    async fn route_will(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        message_id: Uuid,
        source: &SessionIdentity,
    ) -> Result<(
        Vec<Delivery>,
        Publication,
        Option<crate::diagnostics::Capture>,
    )> {
        let mut publication = load_publication(tx, message_id).await?;
        if let Some(seconds) = publication.properties.message_expiry_interval {
            publication.expires_at =
                Some(Utc::now() + chrono::Duration::seconds(i64::from(seconds)));
            sqlx::query("UPDATE messages SET expires_at=$2 WHERE id=$1")
                .bind(message_id)
                .bind(publication.expires_at)
                .execute(&mut **tx)
                .await?;
        }
        let trace = self.diagnostics.capture(
            message_id,
            &publication,
            &source.client_id,
            &source.principal,
        );
        if let Some(trace) = &trace {
            trace.note("will", None, None);
        }
        let deliveries = self
            .route(tx, message_id, &publication, source, trace.as_ref())
            .await?;
        if let Some(trace) = &trace {
            trace.prepare(&deliveries);
        }
        sqlx::query("UPDATE sessions SET will_message_id=NULL,will_due_at=NULL WHERE id=$1")
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
        Ok((deliveries, publication, trace))
    }

    async fn check_queue(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
    ) -> Result<()> {
        let (count, bytes) = queue_usage(tx, session_id).await?;
        if count > self.max_queued_messages || bytes > self.max_queued_bytes {
            return Err(QuotaExceeded {
                scope: "outgoing session delivery",
            }
            .into());
        }
        Ok(())
    }

    async fn route(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        message_id: Uuid,
        publication: &Publication,
        source: &SessionIdentity,
        trace: Option<&crate::diagnostics::Capture>,
    ) -> Result<Vec<Delivery>> {
        if !crate::accounts::namespace_allowed(
            tx,
            &source.principal,
            &publication.topic,
            crate::acl::AccessAction::Publish,
        )
        .await?
        {
            return Err(crate::accounts::ScopeDenied.into());
        }
        if publication.is_expired() {
            if let Some(t) = trace {
                t.note("expired", None, None);
            }
            if publication.retain {
                sqlx::query("DELETE FROM retained WHERE topic=$1")
                    .bind(publication.topic.as_bytes())
                    .execute(&mut **tx)
                    .await?;
            }
            return Ok(vec![]);
        }
        if publication.retain {
            if publication.payload.is_empty() {
                sqlx::query("DELETE FROM retained WHERE topic=$1")
                    .bind(publication.topic.as_bytes())
                    .execute(&mut **tx)
                    .await?;
            } else {
                sqlx::query("INSERT INTO retained(topic,message_id) VALUES($1,$2) ON CONFLICT(topic) DO UPDATE SET message_id=EXCLUDED.message_id")
                    .bind(publication.topic.as_bytes()).bind(message_id).execute(&mut **tx).await?;
            }
        }
        if publication.retain
            && let Some(t) = trace
        {
            t.note(
                if publication.payload.is_empty() {
                    "retain_deleted"
                } else {
                    "retain_updated"
                },
                None,
                None,
            );
        }
        self.enqueue_bridges(tx, message_id, publication).await?;
        if let Some(t) = trace {
            t.note("bridge_routing", None, None);
        }
        let rows=sqlx::query("SELECT s.id AS session_id,s.client_id,s.principal,s.online,s.protocol,sub.id AS subscription_id,sub.filter,sub.qos,sub.no_local,sub.retain_as_published,sub.subscription_identifier,sub.shared_last_sequence,u.id AS principal_id,u.superuser,u.owner_id,a.username AS owner_username FROM subscriptions sub JOIN sessions s ON s.id=sub.session_id JOIN mqtt_users u ON u.username=s.principal LEFT JOIN accounts a ON a.id=u.owner_id WHERE u.enabled AND coalesce(a.enabled,true) AND (s.online OR s.expires_at>now()) ORDER BY s.id")
            .fetch_all(&mut **tx).await?;
        let mut targets: HashMap<Uuid, (u8, bool, Vec<u32>, bool)> = HashMap::new();
        let mut shared_groups: BTreeMap<String, Vec<SharedTarget>> = BTreeMap::new();
        let mut shared_authorization = HashMap::new();
        for row in rows {
            let filter: String = row.try_get("filter")?;
            let Some(parsed) = parse_subscription_filter(&filter) else {
                continue;
            };
            if !crate::accounts::permits(
                row.try_get("owner_id")?,
                row.try_get::<Option<String>, _>("owner_username")?
                    .as_deref(),
                row.try_get("superuser")?,
                &publication.topic,
                crate::acl::AccessAction::Receive,
            ) {
                continue;
            }
            if !topic_matches(parsed.topic_filter, &publication.topic) {
                continue;
            }
            if let Some(t) = trace {
                let peer: String = row.try_get("principal")?;
                let client: Vec<u8> = row.try_get("client_id")?;
                t.note(
                    "subscription_match",
                    Some(row.try_get("session_id")?),
                    Some(&format!("{peer}/{}", String::from_utf8_lossy(&client))),
                );
            }
            let client_id: Vec<u8> = row.try_get("client_id")?;
            if row.try_get::<bool, _>("no_local")?
                && client_id == source.client_id
                && row.try_get::<String, _>("principal")? == source.principal
            {
                if let Some(t) = trace {
                    t.note("no_local", Some(row.try_get("session_id")?), None);
                }
                continue;
            }
            let id: Uuid = row.try_get("session_id")?;
            let qos = (row.try_get::<i32, _>("qos")? as u8).min(publication.qos);
            let retain = publication.retain && row.try_get::<bool, _>("retain_as_published")?;
            let online: bool = row.try_get("online")?;
            if !online && qos == 0 {
                if let Some(t) = trace {
                    t.note("offline_qos0", Some(id), None);
                }
                continue;
            }
            if parsed.share_name.is_some() {
                if row.try_get::<i32, _>("protocol")? != 5 {
                    continue;
                }
                let principal_id: Uuid = row.try_get("principal_id")?;
                let key = (principal_id, parsed.topic_filter.to_owned());
                let allowed = if let Some(allowed) = shared_authorization.get(&key) {
                    *allowed
                } else {
                    let allowed = authorize_subscription_tx(
                        tx,
                        principal_id,
                        row.try_get("superuser")?,
                        parsed.topic_filter,
                    )
                    .await?;
                    shared_authorization.insert(key, allowed);
                    allowed
                };
                if let Some(t) = trace {
                    t.note(
                        if allowed {
                            "shared_acl_allowed"
                        } else {
                            "shared_acl_denied"
                        },
                        Some(id),
                        None,
                    );
                }
                if allowed {
                    shared_groups.entry(filter).or_default().push(SharedTarget {
                        subscription_id: row.try_get("subscription_id")?,
                        session_id: id,
                        last_sequence: row.try_get("shared_last_sequence")?,
                        qos,
                        retain,
                        identifier: row
                            .try_get::<Option<i32>, _>("subscription_identifier")?
                            .map(|value| value as u32),
                        online,
                    });
                }
                continue;
            }
            if !online && let Some(t) = trace {
                t.note("offline_queued", Some(id), None);
            }
            let target = targets.entry(id).or_insert((qos, retain, vec![], online));
            target.0 = target.0.max(qos);
            target.1 |= retain;
            if let Some(identifier) = row.try_get::<Option<i32>, _>("subscription_identifier")? {
                target.2.push(identifier as u32);
            }
        }
        let mut ids: Vec<_> = targets.into_iter().collect();
        ids.sort_by_key(|(id, _)| *id);
        let mut deliveries = Vec::with_capacity(ids.len());
        for (id, (qos, retain, identifiers, _)) in ids {
            // Lock target rows in a stable order so concurrent publishers cannot race quotas.
            sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
            deliveries.push(
                insert_delivery(
                    tx,
                    id,
                    message_id,
                    publication,
                    DeliveryOptions {
                        qos,
                        retain,
                        identifiers,
                        shared_filter: None,
                    },
                )
                .await?,
            );
            if let Err(error) = self.check_queue(tx, id).await {
                if let Some(t) = trace {
                    t.note("quota_exceeded", Some(id), None);
                }
                return Err(error);
            }
        }
        for (filter, mut candidates) in shared_groups {
            // Prefer currently connected workers. Durable offline members are a
            // fallback only when the group has no eligible connected member.
            if candidates.iter().any(|candidate| candidate.online) {
                candidates.retain(|candidate| candidate.online);
            }
            candidates.sort_by_key(|candidate| (candidate.last_sequence, candidate.session_id));
            let mut selected = None;
            for candidate in candidates {
                let mut candidate_tx = tx.begin().await?;
                let delivery = insert_delivery(
                    &mut candidate_tx,
                    candidate.session_id,
                    message_id,
                    publication,
                    DeliveryOptions {
                        qos: candidate.qos,
                        retain: candidate.retain,
                        identifiers: candidate.identifier.into_iter().collect(),
                        shared_filter: Some(&filter),
                    },
                )
                .await?;
                match self
                    .check_queue(&mut candidate_tx, candidate.session_id)
                    .await
                {
                    Ok(()) => {
                        sqlx::query("UPDATE subscriptions SET shared_last_sequence=$2 WHERE id=$1")
                            .bind(candidate.subscription_id)
                            .bind(delivery.sequence)
                            .execute(&mut *candidate_tx)
                            .await?;
                        candidate_tx.commit().await?;
                        if !candidate.online
                            && let Some(t) = trace
                        {
                            t.note("offline_queued", Some(candidate.session_id), None);
                        }
                        selected = Some(delivery);
                        break;
                    }
                    Err(error) if error.is::<QuotaExceeded>() => {
                        if let Some(t) = trace {
                            t.note("shared_quota", Some(candidate.session_id), None);
                        }
                        candidate_tx.rollback().await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            deliveries.push(selected.ok_or(QuotaExceeded {
                scope: "shared subscription delivery",
            })?);
        }
        if deliveries.is_empty()
            && let Some(t) = trace
        {
            t.note("no_recipients", None, None);
        }
        crate::accounts::check_routing_storage(tx, message_id, &source.principal).await?;
        Ok(deliveries)
    }
}

async fn authorize_subscription_tx(
    tx: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    superuser: bool,
    requested: &str,
) -> Result<bool> {
    if superuser {
        return Ok(true);
    }
    let rules = crate::acl::load_rules(&mut **tx, principal_id, false).await?;
    Ok(crate::acl::evaluate(
        &rules,
        requested,
        crate::acl::AccessAction::Subscribe,
        false,
    )
    .allowed)
}

async fn cleanup_shared_deliveries(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // Once the last subscription in a group ends, discard only work whose
    // delivery has not started. Inflight QoS 1/2 handshakes still complete.
    removed_deliveries(sqlx::query(&delete_deliveries_sql("d.shared_filter IS NOT NULL AND d.phase='queued' AND NOT EXISTS(SELECT 1 FROM subscriptions sub JOIN sessions s ON s.id=sub.session_id WHERE sub.filter=d.shared_filter AND (s.online OR s.expires_at>now()))",false))
        .fetch_one(&mut **tx).await?)?;
    Ok(())
}

struct SessionIdentity {
    client_id: Vec<u8>,
    principal: String,
}

async fn lock_session(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    generation: i64,
) -> Result<SessionIdentity> {
    let row = sqlx::query(
        "SELECT client_id,principal FROM sessions WHERE id=$1 AND generation=$2 AND online FOR UPDATE",
    )
    .bind(id)
    .bind(generation)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(SessionSuperseded)?;
    Ok(SessionIdentity {
        client_id: row.try_get("client_id")?,
        principal: row.try_get("principal")?,
    })
}

async fn commit(tx: Transaction<'_, Postgres>, operation_id: Uuid) -> Result<()> {
    match tx.commit().await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error,
                sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_)
            ) =>
        {
            Err(CommitOutcomeUnknown {
                operation_id,
                source: error,
            }
            .into())
        }
        Err(error) => Err(error.into()),
    }
}

async fn insert_message(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    publication: &Publication,
    source: Option<&SessionIdentity>,
) -> Result<()> {
    sqlx::query("INSERT INTO messages(id,topic,payload,qos,retain,properties,source_client_id,source_principal,expires_at,owner_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,coalesce((SELECT owner_id FROM mqtt_users WHERE username=$8),(SELECT id FROM accounts WHERE substring($2::bytea FROM 1 FOR octet_length(convert_to('accounts/'||username||'/','UTF8')))=convert_to('accounts/'||username||'/','UTF8'))))")
        .bind(id).bind(publication.topic.as_bytes()).bind(&publication.payload).bind(i32::from(publication.qos)).bind(publication.retain)
        .bind(sqlx::types::Json(&publication.properties)).bind(source.map(|value| &value.client_id)).bind(source.map(|value| &value.principal)).bind(publication.expires_at).execute(&mut **tx).await?;
    Ok(())
}

async fn load_publication(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<Publication> {
    let row = sqlx::query(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.id=$1"
    ))
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    decode_publication(&row)
}

fn decode_publication(row: &sqlx::postgres::PgRow) -> Result<Publication> {
    Ok(Publication {
        topic: String::from_utf8(row.try_get("topic")?).context("stored topic is not UTF-8")?,
        payload: row.try_get("payload")?,
        qos: row.try_get::<i32, _>("qos")? as u8,
        retain: row.try_get("retain")?,
        expires_at: row.try_get("expires_at")?,
        properties: row
            .try_get::<sqlx::types::Json<PublicationProperties>, _>("properties")?
            .0,
    })
}

fn decode_delivery(row: &sqlx::postgres::PgRow) -> Result<Delivery> {
    let mut publication = decode_publication(row)?;
    publication.qos = row.try_get::<i32, _>("delivery_qos")? as u8;
    publication.retain = row.try_get("delivery_retain")?;
    Ok(Delivery {
        id: row.try_get("delivery_id")?,
        session_id: row.try_get("session_id")?,
        sequence: row.try_get("sequence")?,
        publication,
        packet_id: row
            .try_get::<Option<i32>, _>("packet_id")?
            .map(|value| value as u16),
        phase: DeliveryPhase::parse(&row.try_get::<String, _>("phase")?)?,
        subscription_identifiers: row
            .try_get::<sqlx::types::Json<Vec<u32>>, _>("subscription_identifiers")?
            .0,
    })
}

async fn insert_delivery(
    tx: &mut Transaction<'_, Postgres>,
    session_id: Uuid,
    message_id: Uuid,
    publication: &Publication,
    options: DeliveryOptions<'_>,
) -> Result<Delivery> {
    let DeliveryOptions {
        qos,
        retain,
        identifiers,
        shared_filter,
    } = options;
    let id = Uuid::new_v4();
    let sequence:i64=sqlx::query_scalar("WITH inserted AS (INSERT INTO deliveries(id,session_id,message_id,qos,retain,subscription_identifiers,shared_filter,accounted_bytes) SELECT $1,$2,$3,$4,$5,$6,$7,octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text) FROM messages m WHERE m.id=$3 RETURNING sequence,accounted_bytes), adjusted AS (UPDATE sessions SET queued_count=queued_count+1,queued_bytes=queued_bytes+(SELECT accounted_bytes FROM inserted) WHERE id=$2 RETURNING id) SELECT sequence FROM inserted")
        .bind(id).bind(session_id).bind(message_id).bind(i32::from(qos)).bind(retain).bind(sqlx::types::Json(&identifiers)).bind(shared_filter).fetch_one(&mut **tx).await?;
    let mut publication = publication.clone();
    publication.qos = qos;
    publication.retain = retain;
    Ok(Delivery {
        id,
        session_id,
        sequence,
        publication,
        packet_id: None,
        phase: DeliveryPhase::Queued,
        subscription_identifiers: identifiers,
    })
}

async fn fetch_deliveries(
    tx: &mut Transaction<'_, Postgres>,
    condition: &str,
    id: Uuid,
    _limit: Option<i64>,
) -> Result<Vec<Delivery>> {
    let query = format!(
        "SELECT d.id AS delivery_id,d.session_id,d.sequence,d.packet_id,d.phase,d.qos AS delivery_qos,d.retain AS delivery_retain,d.subscription_identifiers,m.id,m.topic,m.payload,m.qos,m.retain,m.properties,m.expires_at FROM deliveries d JOIN messages m ON m.id=d.message_id {condition} ORDER BY d.sequence"
    );
    sqlx::query(&query)
        .bind(id)
        .fetch_all(&mut **tx)
        .await?
        .iter()
        .map(decode_delivery)
        .collect()
}
