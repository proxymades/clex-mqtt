use std::str::FromStr;

use anyhow::{Result, ensure};
use clex_mqtt::{domain::*, migrations, store::Store};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;
#[path = "support/accounts_storage.rs"]
mod accounts_storage;
#[path = "support/bridge_storage.rs"]
mod bridge_storage;
#[path = "support/delivery_batches.rs"]
mod delivery_batches;
#[path = "support/diagnostics_storage.rs"]
mod diagnostics_storage;
#[path = "support/qos0.rs"]
mod qos0;
#[path = "support/queue_usage.rs"]
mod queue_usage;

struct TestDatabase {
    root: PgPool,
    store: Store,
    schema: String,
}

impl TestDatabase {
    async fn create() -> Result<Self> {
        Self::create_with_schema(false).await
    }

    async fn create_with_schema(before_queue_usage: bool) -> Result<Self> {
        let url = std::env::var("TEST_DATABASE_URL")?;
        let options = PgConnectOptions::from_str(&url)?;
        ensure!(
            options
                .get_database()
                .unwrap_or_default()
                .to_lowercase()
                .contains("test"),
            "storage tests require a dedicated database with 'test' in its name"
        );
        let root = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await?;
        let schema = format!("storage_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&root)
            .await?;
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        let mut migrator = migrations::migrator()?;
        if before_queue_usage {
            let previous: Vec<_> = migrator
                .iter()
                .take_while(|migration| migration.description != "queue_usage_counters")
                .cloned()
                .collect();
            ensure!(
                previous.len() < migrator.iter().count(),
                "queue usage migration is missing"
            );
            migrator.migrations = std::borrow::Cow::Owned(previous);
        }
        migrator.run(&pool).await?;
        for username in ["publisher", "subscriber"] {
            sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,enabled,superuser) VALUES($1,$2,'test-only-not-an-authentication-hash',true,false)")
                .bind(Uuid::new_v4()).bind(username).execute(&pool).await?;
        }
        Ok(Self {
            root,
            store: Store::from_pool(pool),
            schema,
        })
    }

    async fn cleanup(self) -> Result<()> {
        let accounting = queue_usage::assert_consistent(&self.store, false).await;
        self.store.pool().close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.root)
            .await?;
        self.root.close().await;
        accounting?;
        Ok(())
    }
}

fn session(client: &str, principal: &str, clean_start: bool) -> SessionSpec {
    SessionSpec {
        client_id: client.as_bytes().to_vec(),
        principal: principal.into(),
        clean_start,
        expiry_interval: 3600,
        protocol: 5,
        will: None,
        will_delay_interval: 0,
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn group_permissions_are_inherited_scoped_and_match_access_inspector() -> Result<()> {
    use clex_mqtt::acl::AccessAction;
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let group_id = Uuid::new_v4();
        sqlx::query("INSERT INTO mqtt_access_groups(id,name) VALUES($1,'Workshop')").bind(group_id).execute(store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_user_groups(mqtt_user_id,group_id) SELECT id,$1 FROM mqtt_users WHERE username='publisher'")
            .bind(group_id).execute(store.pool()).await?;
        for (filter, allow) in [("plant/#",true),("plant/secret/#",false)] {
            sqlx::query("INSERT INTO mqtt_acl_rules(id,group_id,filter,publish,subscribe,allow) VALUES($1,$2,$3,true,true,$4)")
                .bind(Uuid::new_v4()).bind(group_id).bind(filter).bind(allow).execute(store.pool()).await?;
        }
        ensure!(store.authorize("publisher","plant/public/state",true).await?);
        ensure!(!store.authorize("subscriber","plant/public/state",true).await?, "group rules must not become global rules");
        for username in ["publisher","subscriber"] {
            sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,publish,subscribe,allow) SELECT $1,id,'plant/secret/#',true,true,true FROM mqtt_users WHERE username=$2")
                .bind(Uuid::new_v4()).bind(username).execute(store.pool()).await?;
        }
        ensure!(store.authorize("subscriber","plant/secret/state",true).await?, "another account's group deny must stay isolated");
        ensure!(!store.authorize("publisher","plant/secret/state",true).await?, "group deny must override a personal allow");
        for (topic, action, expected) in [("plant/public/state",AccessAction::Publish,true),("plant/secret/state",AccessAction::Publish,false),
            ("plant/#",AccessAction::Subscribe,false),("$share/workers/plant/public/#",AccessAction::Subscribe,true),
            ("plant/public/state",AccessAction::Receive,true),("$SYS/status",AccessAction::Receive,false)] {
            let result = store.check_access("publisher",topic,action).await?;
            ensure!(result.allowed == expected);
            let live = match action { AccessAction::Subscribe => store.authorize_subscription("publisher",topic).await?,
                _ => store.authorize("publisher",topic,action==AccessAction::Publish).await? };
            ensure!(live == result.allowed, "inspector and MQTT runtime must agree");
        }
        let denied = store.check_access("publisher","plant/#",AccessAction::Subscribe).await?;
        ensure!(denied.reason == "explicit_deny" && denied.matching_rules.iter().any(|rule| !rule.rule.allow && rule.rule.group_id == Some(group_id)));
        sqlx::query("UPDATE mqtt_access_groups SET enabled=false WHERE id=$1").bind(group_id).execute(store.pool()).await?;
        ensure!(!store.authorize("publisher","plant/public/state",true).await?);
        ensure!(store.authorize("publisher","plant/secret/state",true).await?, "disabling a group must preserve independent personal rights");
        sqlx::query("UPDATE mqtt_access_groups SET enabled=true WHERE id=$1").bind(group_id).execute(store.pool()).await?;
        sqlx::query("DELETE FROM mqtt_user_groups WHERE group_id=$1").bind(group_id).execute(store.pool()).await?;
        ensure!(!store.authorize("publisher","plant/public/state",true).await?);
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_selection_rechecks_group_permissions_inside_routing_transaction() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let group = Uuid::new_v4();
        sqlx::query("INSERT INTO mqtt_access_groups(id,name) VALUES($1,'Shared consumers')").bind(group).execute(store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_user_groups(mqtt_user_id,group_id) SELECT id,$1 FROM mqtt_users WHERE username='subscriber'").bind(group).execute(store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,group_id,filter,subscribe) VALUES($1,$2,'group-work/#',true)").bind(Uuid::new_v4()).bind(group).execute(store.pool()).await?;
        let member = store.open_session(session("group-member","subscriber",true)).await?;
        let source = store.open_session(session("group-source","publisher",true)).await?;
        ensure!(store.authorize_subscription("subscriber","$share/workers/group-work/#").await?);
        store.subscribe(member.session_id,member.generation,&subscription("$share/workers/group-work/#",1)).await?;
        let first = store.accept_publication(source.session_id,source.generation,&publication("group-work/value",1,b"first",false),b"group-source").await?;
        ensure!(first.len()==1 && first[0].session_id==member.session_id);
        sqlx::query("UPDATE mqtt_access_groups SET enabled=false WHERE id=$1").bind(group).execute(store.pool()).await?;
        let revoked = store.accept_publication(source.session_id,source.generation,&publication("group-work/value",1,b"revoked",false),b"group-source").await?;
        ensure!(revoked.is_empty(),"shared selector must exclude a member whose only group permission was revoked");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

fn publication(topic: &str, qos: u8, payload: &[u8], retain: bool) -> Publication {
    Publication {
        topic: topic.into(),
        payload: payload.into(),
        qos,
        retain,
        expires_at: None,
        properties: PublicationProperties::default(),
    }
}

fn subscription(filter: &str, qos: u8) -> Subscription {
    Subscription {
        filter: filter.into(),
        qos,
        no_local: false,
        retain_as_published: false,
        retain_handling: 0,
        subscription_identifier: Some(17),
    }
}

async fn allow_subscription(store: &Store, username: &str, filter: &str) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,$3,true,true FROM mqtt_users WHERE username=$2")
        .bind(id).bind(username).bind(filter).execute(store.pool()).await?;
    Ok(id)
}

async fn complete_qos1(store: &Store, session: &OpenedSession, delivery: &Delivery) -> Result<()> {
    store
        .reserve_delivery(session.session_id, session.generation, delivery.id, 1)
        .await?;
    ensure!(
        store
            .acknowledge(
                session.session_id,
                session.generation,
                1,
                DeliveryPhase::AwaitPubAck,
            )
            .await?,
        "QoS 1 delivery must complete"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_authorization_parses_once_and_preserves_literal_topic_acl() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "topic").await?;
        ensure!(
            store.authorize_subscription("subscriber", "$share/inner/topic").await?
                && !store.authorize("subscriber", "$share/inner/topic", false).await?,
            "a subscription envelope must be removed only for subscription authorization"
        );
        allow_subscription(store, "subscriber", "$share/inner/#").await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'topic',true,false FROM mqtt_users WHERE username='subscriber'")
            .bind(Uuid::new_v4()).execute(store.pool()).await?;
        ensure!(
            store.authorize("subscriber", "$share/inner/topic", false).await?
                && !store.authorize_subscription("subscriber", "$share/inner/topic").await?
                && store.authorize_subscription("subscriber", "$share/outer/$share/inner/topic").await?
                && !store.authorize_subscription("subscriber", "$share/invalid").await?,
            "a nested shared prefix is a literal inner topic filter with its own ACL"
        );
        let publisher = store.open_session(session("literal-publisher", "publisher", true)).await?;
        let worker = store.open_session(session("literal-worker", "subscriber", true)).await?;
        store.subscribe(worker.session_id, worker.generation,
            &subscription("$share/outer/$share/inner/topic", 1)).await?;
        let delivered = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("$share/inner/topic", 1, b"literal topic", false), b"literal-publisher").await?;
        ensure!(delivered.len() == 1 && delivered[0].session_id == worker.session_id,
            "shared selection must authorize the already parsed literal inner filter");
        complete_qos1(store, &worker, &delivered[0]).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,subscribe,allow) SELECT $1,id,'$share/inner/topic',true,false FROM mqtt_users WHERE username='subscriber'")
            .bind(Uuid::new_v4()).execute(store.pool()).await?;
        ensure!(!store.authorize("subscriber", "$share/inner/topic", false).await?
            && !store.authorize_subscription("subscriber", "$share/outer/$share/inner/topic").await?,
            "a literal topic deny must apply to both delivery and nested subscription authorization");
        let revoked = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("$share/inner/topic", 1, b"revoked", false), b"literal-publisher").await?;
        ensure!(revoked.is_empty(), "revoked shared members must not receive new assignments");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn last_shared_unsubscribe_discards_queued_work_and_keeps_inflight_handshake() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "cleanup/#").await?;
        let publisher = store.open_session(session("cleanup-publisher", "publisher", true)).await?;
        store.accept_publication(publisher.session_id, publisher.generation,
            &publication("cleanup/retained", 2, b"retained before subscription", true), b"cleanup-publisher").await?;
        let worker = store.open_session(session("cleanup-worker", "subscriber", true)).await?;
        let replay = store.subscribe(worker.session_id, worker.generation,
            &subscription("$share/workers/cleanup/#", 2)).await?;
        ensure!(replay.is_empty(), "shared subscriptions must not replay retained messages");
        let first = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("cleanup/inflight", 2, b"finish this handshake", false), b"cleanup-publisher").await?;
        store.reserve_delivery(worker.session_id, worker.generation, first[0].id, 81).await?;
        store.advance_qos2(worker.session_id, worker.generation, 81).await?;
        let queued = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("cleanup/queued", 1, b"discard unstarted work", false), b"cleanup-publisher").await?;
        store.subscribe(worker.session_id, worker.generation, &subscription("ordinary/#", 1)).await?;
        let ordinary = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("ordinary/task", 1, b"keep ordinary work", false), b"cleanup-publisher").await?;
        ensure!(store.unsubscribe(worker.session_id, worker.generation, "$share/workers/cleanup/#").await?,
            "last shared membership must be removed");
        let pending = store.pending_deliveries(worker.session_id, worker.generation, 10).await?;
        ensure!(pending.len() == 2
            && pending.iter().all(|delivery| delivery.id != queued[0].id)
            && pending.iter().any(|delivery| delivery.id == ordinary[0].id)
            && pending.iter().any(|delivery| delivery.id == first[0].id
                && delivery.phase == DeliveryPhase::AwaitPubComp && delivery.packet_id == Some(81)),
            "last-member cleanup must discard only queued shared work and preserve inflight and ordinary deliveries");
        ensure!(store.acknowledge(worker.session_id, worker.generation, 81, DeliveryPhase::AwaitPubComp).await?,
            "inflight shared QoS 2 delivery must complete after unsubscribe");
        complete_qos1(store, &worker, &ordinary[0]).await?;
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_qos2_release_and_recovery_keep_one_pinned_worker() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "qos2/#").await?;
        let publisher = store.open_session(session("shared-qos2-publisher", "publisher", true)).await?;
        let mut workers = Vec::new();
        for client in ["shared-qos2-first", "shared-qos2-second"] {
            let worker = store.open_session(session(client, "subscriber", true)).await?;
            store.subscribe(worker.session_id, worker.generation, &subscription("$share/workers/qos2/#", 2)).await?;
            workers.push((client, worker));
        }
        let task = publication("qos2/task", 2, b"one durable assignment", false);
        ensure!(store.receive_qos2(publisher.session_id, publisher.generation, 91, &task).await?
            && !store.receive_qos2(publisher.session_id, publisher.generation, 91, &task).await?,
            "duplicate incoming QoS 2 PUBLISH must keep one pending message");
        let restarted = Store::from_pool(store.pool().clone());
        restarted.recover_startup().await?;
        let publisher = restarted.open_session(session("shared-qos2-publisher", "publisher", false)).await?;
        for (client, worker) in &mut workers {
            *worker = restarted.open_session(session(client, "subscriber", false)).await?;
        }
        let released = restarted.release_qos2(publisher.session_id, publisher.generation, 91).await?;
        ensure!(released.found && released.deliveries.len() == 1,
            "recovered PUBREL must atomically select exactly one shared worker");
        let chosen = released.deliveries[0].clone();
        let (client, worker) = workers.iter().find(|(_, worker)| worker.session_id == chosen.session_id).unwrap();
        restarted.reserve_delivery(worker.session_id, worker.generation, chosen.id, 301).await?;
        ensure!(restarted.advance_qos2(worker.session_id, worker.generation, 301).await?
            && restarted.advance_qos2(worker.session_id, worker.generation, 301).await?,
            "duplicate PUBREC must retain the same PUBREL handshake");
        let cursor: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id,shared_last_sequence FROM subscriptions ORDER BY id")
            .fetch_all(restarted.pool()).await?;
        let recovered = Store::from_pool(store.pool().clone());
        recovered.recover_startup().await?;
        let publisher = recovered.open_session(session("shared-qos2-publisher", "publisher", false)).await?;
        let worker = recovered.open_session(session(client, "subscriber", false)).await?;
        let repeated = recovered.release_qos2(publisher.session_id, publisher.generation, 91).await?;
        let pending = recovered.pending_deliveries(worker.session_id, worker.generation, 10).await?;
        ensure!(repeated.found && repeated.deliveries.is_empty() && pending.len() == 1
            && pending[0].id == chosen.id && pending[0].packet_id == Some(301)
            && pending[0].phase == DeliveryPhase::AwaitPubComp,
            "repeated inbound PUBREL and recovery must preserve the selected worker, delivery and outbound packet id");
        let after: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id,shared_last_sequence FROM subscriptions ORDER BY id")
            .fetch_all(recovered.pool()).await?;
        let delivery_count: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries")
            .fetch_one(recovered.pool()).await?;
        ensure!(cursor == after && delivery_count == 1,
            "duplicate PUBREL must neither advance rotation nor assign another worker");
        ensure!(recovered.acknowledge(worker.session_id, worker.generation, 301, DeliveryPhase::AwaitPubComp).await?,
            "recovered shared QoS 2 exchange must finish");
        let completed = recovered.release_qos2(publisher.session_id, publisher.generation, 91).await?;
        ensure!(completed.found && completed.deliveries.is_empty(),
            "late repeated PUBREL must not recreate completed shared work");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn persistent_shared_membership_is_dormant_until_mqtt5_returns() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "version/#").await?;
        let publisher = store
            .open_session(session("version-publisher", "publisher", true))
            .await?;
        let mut worker = store
            .open_session(session("version-worker", "subscriber", true))
            .await?;
        let mut ordinary = subscription("version/#", 1);
        ordinary.subscription_identifier = Some(201);
        store
            .subscribe(worker.session_id, worker.generation, &ordinary)
            .await?;
        let mut shared = subscription("$share/workers/version/#", 1);
        shared.subscription_identifier = Some(202);
        store
            .subscribe(worker.session_id, worker.generation, &shared)
            .await?;
        for protocol in [4, 3] {
            store
                .close_session(worker.session_id, worker.generation, 3600, true)
                .await?;
            let mut spec = session("version-worker", "subscriber", false);
            spec.protocol = protocol;
            worker = store.open_session(spec).await?;
            ensure!(
                worker.session_present
                    && store
                        .subscribe(worker.session_id, worker.generation, &shared)
                        .await
                        .is_err(),
                "older protocol must resume the session but reject new shared subscriptions"
            );
            let delivered = store
                .accept_publication(
                    publisher.session_id,
                    publisher.generation,
                    &publication("version/task", 1, &[protocol], false),
                    b"version-publisher",
                )
                .await?;
            ensure!(
                delivered.len() == 1 && delivered[0].subscription_identifiers == vec![201],
                "older protocol must retain ordinary routing while shared membership is dormant"
            );
            complete_qos1(store, &worker, &delivered[0]).await?;
        }
        let cursor: i64 = sqlx::query_scalar(
            "SELECT shared_last_sequence FROM subscriptions WHERE session_id=$1 AND filter=$2",
        )
        .bind(worker.session_id)
        .bind(&shared.filter)
        .fetch_one(store.pool())
        .await?;
        ensure!(
            cursor == 0,
            "dormant shared membership must not consume a routing turn"
        );
        store
            .close_session(worker.session_id, worker.generation, 3600, true)
            .await?;
        worker = store
            .open_session(session("version-worker", "subscriber", false))
            .await?;
        let delivered = store
            .accept_publication(
                publisher.session_id,
                publisher.generation,
                &publication("version/task", 1, b"mqtt5 restored", false),
                b"version-publisher",
            )
            .await?;
        ensure!(
            worker.session_present
                && delivered.len() == 2
                && delivered
                    .iter()
                    .any(|delivery| delivery.subscription_identifiers == vec![201])
                && delivered
                    .iter()
                    .any(|delivery| delivery.subscription_identifiers == vec![202]),
            "MQTT 5 reconnect must reactivate the exact retained shared membership"
        );
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_round_robin_survives_completed_deliveries_and_keeps_independent_copies()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "shared/#").await?;
        let publisher = store.open_session(session("shared-publisher", "publisher", true)).await?;
        let mut members = Vec::new();
        for index in 0..3 {
            let member = store.open_session(session(&format!("worker-{index}"), "subscriber", true)).await?;
            let mut sub = subscription("$share/workers/shared/#", 1);
            sub.subscription_identifier = Some(100 + index);
            store.subscribe(member.session_id, member.generation, &sub).await?;
            members.push(member);
        }
        let mut counts = [0; 3];
        for index in 0..9 {
            let reopened = Store::from_pool(store.pool().clone());
            let delivered = reopened.accept_publication(publisher.session_id, publisher.generation,
                &publication("shared/task", 1, &[index], false), b"shared-publisher").await?;
            ensure!(delivered.len() == 1, "one member must receive each group publication");
            let chosen = members.iter().position(|member| member.session_id == delivered[0].session_id).unwrap();
            ensure!(delivered[0].subscription_identifiers == vec![100 + chosen as u32], "shared delivery must carry only the chosen member identifier");
            counts[chosen] += 1;
            complete_qos1(&reopened, &members[chosen], &delivered[0]).await?;
            let mut sub = subscription("$share/workers/shared/#", 1);
            sub.subscription_identifier = Some(100 + chosen as u32);
            reopened.subscribe(members[chosen].session_id, members[chosen].generation, &sub).await?;
        }
        ensure!(counts == [3, 3, 3], "durable rotation must survive ACK, Store recreation and resubscription");
        let member = &members[0];
        for (filter, identifier) in [("shared/#", 201), ("$share/other/shared/#", 202), ("$share/workers/shared/+", 203)] {
            let mut sub = subscription(filter, 1);
            sub.subscription_identifier = Some(identifier);
            sub.retain_as_published = identifier == 202;
            store.subscribe(member.session_id, member.generation, &sub).await?;
        }
        let delivered = store.accept_publication(publisher.session_id, publisher.generation,
            &publication("shared/task", 1, b"independent copies", true), b"shared-publisher").await?;
        ensure!(delivered.len() == 4, "ordinary subscription and every exact shared group/filter pair must each receive a copy");
        for identifier in [201, 202, 203] {
            let copy = delivered.iter().find(|delivery| delivery.subscription_identifiers == vec![identifier]).unwrap();
            ensure!(copy.session_id == member.session_id && copy.publication.retain == (identifier == 202), "independent copies must preserve their own subscription options");
        }
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_online_preference_offline_fallback_and_assignment_recovery() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        allow_subscription(store, "subscriber", "jobs/#").await?;
        let publisher = store
            .open_session(session("jobs-publisher", "publisher", true))
            .await?;
        let first = store
            .open_session(session("jobs-first", "subscriber", true))
            .await?;
        let second = store
            .open_session(session("jobs-second", "subscriber", true))
            .await?;
        for member in [&first, &second] {
            store
                .subscribe(
                    member.session_id,
                    member.generation,
                    &subscription("$share/workers/jobs/#", 1),
                )
                .await?;
        }
        store
            .close_session(first.session_id, first.generation, 3600, true)
            .await?;
        let delivered = store
            .accept_publication(
                publisher.session_id,
                publisher.generation,
                &publication("jobs/task", 1, b"online", false),
                b"jobs-publisher",
            )
            .await?;
        ensure!(
            delivered.len() == 1 && delivered[0].session_id == second.session_id,
            "online worker must take precedence over offline members"
        );
        complete_qos1(store, &second, &delivered[0]).await?;
        store
            .close_session(second.session_id, second.generation, 3600, true)
            .await?;
        let qos0 = store
            .accept_publication(
                publisher.session_id,
                publisher.generation,
                &publication("jobs/task", 0, b"offline qos0", false),
                b"jobs-publisher",
            )
            .await?;
        ensure!(
            qos0.is_empty(),
            "offline members receive durable QoS 1/2 work only"
        );
        let operation = Uuid::new_v4();
        let task = publication("jobs/task", 1, b"durable assignment", false);
        let offline = store
            .accept_publication_with_id(
                operation,
                publisher.session_id,
                publisher.generation,
                &task,
                b"jobs-publisher",
            )
            .await?;
        ensure!(
            offline.len() == 1 && offline[0].session_id == first.session_id,
            "offline fallback must follow the same durable rotation"
        );
        let restarted = Store::from_pool(store.pool().clone());
        let resumed = restarted
            .open_session(session("jobs-first", "subscriber", false))
            .await?;
        let repeated = restarted
            .accept_publication_with_id(
                operation,
                publisher.session_id,
                publisher.generation,
                &task,
                b"jobs-publisher",
            )
            .await?;
        ensure!(
            repeated.len() == 1 && repeated[0].id == offline[0].id,
            "reconciliation must preserve the original shared assignment"
        );
        let pending = restarted
            .pending_deliveries(resumed.session_id, resumed.generation, 10)
            .await?;
        ensure!(
            resumed.session_present && pending.len() == 1 && pending[0].id == offline[0].id,
            "assigned work must survive an offline worker reconnect"
        );
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn shared_capacity_skips_full_workers_and_rolls_back_all_state_when_full() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(1, 4096);
        allow_subscription(&store, "subscriber", "capacity/#").await?;
        let publisher = store
            .open_session(session("capacity-publisher", "publisher", true))
            .await?;
        let mut workers = Vec::new();
        for index in 0..3 {
            let worker = store
                .open_session(session(&format!("capacity-{index}"), "subscriber", true))
                .await?;
            store
                .subscribe(
                    worker.session_id,
                    worker.generation,
                    &subscription("$share/workers/capacity/#", 1),
                )
                .await?;
            workers.push(worker);
        }
        store
            .close_session(workers[2].session_id, workers[2].generation, 3600, true)
            .await?;
        let mut accepted = Vec::new();
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let delivered = store
                .accept_publication(
                    publisher.session_id,
                    publisher.generation,
                    &publication("capacity/task", 1, payload, true),
                    b"capacity-publisher",
                )
                .await?;
            ensure!(
                delivered.len() == 1,
                "each accepted task must have one selected worker"
            );
            accepted.push(delivered[0].clone());
        }
        ensure!(
            accepted[0].session_id != accepted[1].session_id,
            "full worker must allow another connected worker to take work"
        );
        let before: Vec<(Uuid, i64)> =
            sqlx::query_as("SELECT id,shared_last_sequence FROM subscriptions ORDER BY id")
                .fetch_all(store.pool())
                .await?;
        let operation = Uuid::new_v4();
        let task = publication("capacity/task", 1, b"retry", true);
        let rejected = store
            .accept_publication_with_id(
                operation,
                publisher.session_id,
                publisher.generation,
                &task,
                b"capacity-publisher",
            )
            .await;
        ensure!(
            rejected.is_err_and(|error| error.is::<clex_mqtt::store::QuotaExceeded>()),
            "full online group must reject acceptance rather than queue work to an offline member"
        );
        let after: Vec<(Uuid, i64)> =
            sqlx::query_as("SELECT id,shared_last_sequence FROM subscriptions ORDER BY id")
                .fetch_all(store.pool())
                .await?;
        let retained: Vec<u8> = sqlx::query_scalar(
            "SELECT m.payload FROM retained r JOIN messages m ON m.id=r.message_id",
        )
        .fetch_one(store.pool())
        .await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id=$1)")
            .bind(operation)
            .fetch_one(store.pool())
            .await?;
        ensure!(
            before == after && retained == b"second" && !exists,
            "failed acceptance must roll back cursor, retained value and source message"
        );
        let chosen = workers
            .iter()
            .find(|worker| worker.session_id == accepted[1].session_id)
            .unwrap();
        complete_qos1(&store, chosen, &accepted[1]).await?;
        let retried = store
            .accept_publication_with_id(
                operation,
                publisher.session_id,
                publisher.generation,
                &task,
                b"capacity-publisher",
            )
            .await?;
        ensure!(
            retried.len() == 1 && retried[0].session_id == chosen.session_id,
            "selection must skip the earlier full worker and use available online capacity"
        );
        let repeated = store
            .accept_publication_with_id(
                operation,
                publisher.session_id,
                publisher.generation,
                &task,
                b"capacity-publisher",
            )
            .await?;
        ensure!(
            repeated.len() == 1 && repeated[0].id == retried[0].id,
            "idempotent retry must preserve the successful shared choice"
        );
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn qos2_duplicate_release_and_restart_preserve_one_delivery() -> Result<()> {
    let db = TestDatabase::create().await?;
    let publisher = db
        .store
        .open_session(session("publisher-id", "publisher", true))
        .await?;
    let subscriber = db
        .store
        .open_session(session("subscriber-id", "subscriber", true))
        .await?;
    db.store
        .subscribe(
            subscriber.session_id,
            subscriber.generation,
            &subscription("devices/#", 2),
        )
        .await?;
    db.store
        .close_session(subscriber.session_id, subscriber.generation, 3600, true)
        .await?;
    let mut message = publication("devices/one", 2, &[0, 255, 1, 0], true);
    message.properties.user_properties =
        vec![("a".into(), "first".into()), ("a".into(), "second".into())];
    ensure!(
        db.store
            .receive_qos2(publisher.session_id, publisher.generation, 65535, &message)
            .await?,
        "first QoS2 packet should be accepted"
    );
    ensure!(
        !db.store
            .receive_qos2(publisher.session_id, publisher.generation, 65535, &message)
            .await?,
        "duplicate QoS2 packet must not create a message"
    );
    let release = db
        .store
        .release_qos2(publisher.session_id, publisher.generation, 65535)
        .await?;
    ensure!(
        release.found && release.deliveries.len() == 1,
        "release must atomically enqueue one subscriber delivery"
    );
    let repeated = db
        .store
        .release_qos2(publisher.session_id, publisher.generation, 65535)
        .await?;
    ensure!(
        repeated.found && repeated.deliveries.is_empty(),
        "duplicate PUBREL must not route twice"
    );
    let restarted = Store::from_pool(db.store.pool().clone());
    let resumed = restarted
        .open_session(session("subscriber-id", "subscriber", false))
        .await?;
    ensure!(
        resumed.session_present,
        "persistent offline session must resume"
    );
    let deliveries = restarted
        .pending_deliveries(resumed.session_id, resumed.generation, 100)
        .await?;
    ensure!(
        deliveries.len() == 1 && deliveries[0].publication.payload == message.payload,
        "binary publication must survive reconnect"
    );
    ensure!(
        deliveries[0].publication.properties.user_properties == message.properties.user_properties,
        "ordered duplicate user properties must survive storage"
    );
    ensure!(
        deliveries[0].subscription_identifiers == vec![17],
        "subscription identifier should remain per recipient"
    );
    let reserved = restarted
        .reserve_delivery(
            resumed.session_id,
            resumed.generation,
            deliveries[0].id,
            65535,
        )
        .await?;
    ensure!(
        reserved.phase == DeliveryPhase::AwaitPubRec,
        "outbound QoS2 must wait for PUBREC"
    );
    ensure!(
        restarted
            .advance_qos2(resumed.session_id, resumed.generation, 65535)
            .await?,
        "PUBREC must advance durable state"
    );
    let after_pubrec = restarted
        .pending_deliveries(resumed.session_id, resumed.generation, 100)
        .await?;
    ensure!(
        after_pubrec[0].phase == DeliveryPhase::AwaitPubComp
            && after_pubrec[0].packet_id == Some(65535),
        "PUBREL stage and unsigned packet id must survive reads"
    );
    ensure!(
        !restarted
            .acknowledge(
                resumed.session_id,
                resumed.generation,
                65535,
                DeliveryPhase::AwaitPubAck
            )
            .await?,
        "wrong acknowledgement must not remove delivery"
    );
    ensure!(
        restarted
            .acknowledge(
                resumed.session_id,
                resumed.generation,
                65535,
                DeliveryPhase::AwaitPubComp
            )
            .await?,
        "PUBCOMP must release delivery"
    );
    ensure!(
        restarted
            .pending_deliveries(resumed.session_id, resumed.generation, 100)
            .await?
            .is_empty(),
        "completed delivery should be absent"
    );
    db.cleanup().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn retained_is_independent_and_empty_payload_deletes_it() -> Result<()> {
    let db = TestDatabase::create().await?;
    let mut spec = session("publisher-clean", "publisher", true);
    spec.expiry_interval = 0;
    let publisher = db.store.open_session(spec).await?;
    db.store
        .accept_publication(
            publisher.session_id,
            publisher.generation,
            &publication("state/node", 1, b"online", true),
            b"publisher-clean",
        )
        .await?;
    db.store
        .close_session(publisher.session_id, publisher.generation, 0, true)
        .await?;
    let subscriber = db
        .store
        .open_session(session("retained-reader", "subscriber", true))
        .await?;
    let retained = db
        .store
        .subscribe(
            subscriber.session_id,
            subscriber.generation,
            &subscription("state/#", 1),
        )
        .await?;
    ensure!(
        retained.len() == 1 && retained[0].publication.retain,
        "retained publication must outlive clean source session"
    );
    let publisher = db
        .store
        .open_session(session("publisher-clean", "publisher", true))
        .await?;
    db.store
        .accept_publication(
            publisher.session_id,
            publisher.generation,
            &publication("state/node", 1, b"", true),
            b"publisher-clean",
        )
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM retained")
        .fetch_one(db.store.pool())
        .await?;
    ensure!(
        count == 0,
        "zero-length retained publication must delete retained row"
    );
    db.cleanup().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn quota_failure_rolls_back_retained_and_all_fanout() -> Result<()> {
    let db = TestDatabase::create().await?;
    let bounded = db.store.clone().with_queue_limits(1, 1024);
    let publisher = bounded
        .open_session(session("quota-publisher", "publisher", true))
        .await?;
    let subscriber = bounded
        .open_session(session("quota-reader", "subscriber", true))
        .await?;
    bounded
        .subscribe(
            subscriber.session_id,
            subscriber.generation,
            &subscription("quota/#", 1),
        )
        .await?;
    bounded
        .accept_publication(
            publisher.session_id,
            publisher.generation,
            &publication("quota/state", 1, b"before", true),
            b"quota-publisher",
        )
        .await?;
    let empty = bounded
        .open_session(session("quota-empty-reader", "subscriber", true))
        .await?;
    let mut future = subscription("quota/#", 1);
    future.retain_handling = 2;
    bounded
        .subscribe(empty.session_id, empty.generation, &future)
        .await?;
    let rejected = bounded
        .accept_publication(
            publisher.session_id,
            publisher.generation,
            &publication("quota/state", 1, b"must-rollback", true),
            b"quota-publisher",
        )
        .await;
    ensure!(
        rejected.is_err(),
        "full durable queue must reject acceptance"
    );
    let byte_bounded = bounded.clone().with_queue_limits(1000, 1024);
    let rejected = byte_bounded
        .accept_publication(
            publisher.session_id,
            publisher.generation,
            &publication("quota/state", 1, &vec![0xa5; 2048], true),
            b"quota-publisher",
        )
        .await;
    ensure!(
        rejected.is_err_and(|error| error.is::<clex_mqtt::store::QuotaExceeded>()),
        "byte quota must roll back the complete fanout"
    );
    let observed = bounded.monitor().read(&Default::default());
    ensure!(
        observed.messages.len() == 1 && observed.messages[0].payload_base64 == "YmVmb3Jl",
        "rolled-back publication must not appear in the message stream"
    );
    let payload: Vec<u8> =
        sqlx::query_scalar("SELECT m.payload FROM retained r JOIN messages m ON m.id=r.message_id")
            .fetch_one(bounded.pool())
            .await?;
    ensure!(
        payload == b"before",
        "failed acceptance must not overwrite retained state"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries")
        .fetch_one(bounded.pool())
        .await?;
    ensure!(
        count == 1,
        "failed acceptance must not partially add deliveries"
    );
    db.cleanup().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn takeover_fences_old_connection_and_cleanstart_keeps_generation_monotonic() -> Result<()> {
    let db = TestDatabase::create().await?;
    let first = db
        .store
        .open_session(session("same-id", "publisher", true))
        .await?;
    let second = db
        .store
        .open_session(session("same-id", "publisher", false))
        .await?;
    ensure!(
        second.session_id == first.session_id
            && second.generation > first.generation
            && second.session_present,
        "resume must advance generation"
    );
    ensure!(
        db.store
            .accept_publication(
                first.session_id,
                first.generation,
                &publication("fence", 1, b"stale", false),
                b"same-id"
            )
            .await
            .is_err(),
        "old connection cannot publish"
    );
    db.store
        .close_session(first.session_id, first.generation, 0, true)
        .await?;
    ensure!(
        db.store
            .is_session_current(second.session_id, second.generation)
            .await?,
        "old close cannot remove new connection"
    );
    let clean = db
        .store
        .open_session(session("same-id", "publisher", true))
        .await?;
    ensure!(
        !clean.session_present
            && clean.session_id != second.session_id
            && clean.generation > second.generation,
        "clean start must replace state without recycling fencing generation"
    );
    let other = db
        .store
        .open_session(session("same-id", "subscriber", false))
        .await?;
    ensure!(
        !other.session_present && other.session_id != clean.session_id,
        "another principal must receive an independent session for the same raw Client ID"
    );
    ensure!(
        db.store
            .is_session_current(clean.session_id, clean.generation)
            .await?,
        "opening another principal's Client ID must preserve the original connection"
    );
    db.cleanup().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_scoped_client_ids_preserve_no_local_and_offline_queues() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let left = store
            .open_session(session("esp32-01", "publisher", true))
            .await?;
        let right = store
            .open_session(session("esp32-01", "subscriber", true))
            .await?;
        ensure!(left.session_id != right.session_id);
        let mut no_local = subscription("common/value", 1);
        no_local.no_local = true;
        store
            .subscribe(left.session_id, left.generation, &no_local)
            .await?;
        store
            .subscribe(right.session_id, right.generation, &no_local)
            .await?;
        let routed = store
            .accept_publication(
                left.session_id,
                left.generation,
                &publication("common/value", 1, b"other account sees this", true),
                b"esp32-01",
            )
            .await?;
        ensure!(
            routed.len() == 1 && routed[0].session_id == right.session_id,
            "No Local must compare the account and Client ID together"
        );
        ensure!(
            store
                .subscribe(left.session_id, left.generation, &no_local)
                .await?
                .is_empty(),
            "own Retain publication must respect No Local"
        );
        ensure!(
            store
                .subscribe(right.session_id, right.generation, &no_local)
                .await?
                .len()
                == 1,
            "another account's Retain publication must survive the same raw Client ID"
        );
        store
            .subscribe(
                left.session_id,
                left.generation,
                &subscription("private/left", 1),
            )
            .await?;
        store
            .subscribe(
                right.session_id,
                right.generation,
                &subscription("private/right", 1),
            )
            .await?;
        store
            .close_session(left.session_id, left.generation, 3600, true)
            .await?;
        store
            .close_session(right.session_id, right.generation, 3600, true)
            .await?;
        let sender = store
            .open_session(session("sender", "publisher", true))
            .await?;
        for (topic, body) in [
            ("private/left", b"left".as_slice()),
            ("private/right", b"right".as_slice()),
        ] {
            store
                .accept_publication(
                    sender.session_id,
                    sender.generation,
                    &publication(topic, 1, body, false),
                    b"sender",
                )
                .await?;
        }
        let restarted = Store::from_pool(store.pool().clone());
        let resumed_left = restarted
            .open_session(session("esp32-01", "publisher", false))
            .await?;
        let resumed_right = restarted
            .open_session(session("esp32-01", "subscriber", false))
            .await?;
        ensure!(resumed_left.session_present && resumed_left.session_id == left.session_id);
        ensure!(resumed_right.session_present && resumed_right.session_id == right.session_id);
        let pending_left = restarted
            .pending_deliveries(resumed_left.session_id, resumed_left.generation, 10)
            .await?;
        let pending_right = restarted
            .pending_deliveries(resumed_right.session_id, resumed_right.generation, 10)
            .await?;
        ensure!(
            pending_left
                .iter()
                .any(|delivery| delivery.publication.payload == b"left")
                && !pending_left
                    .iter()
                    .any(|delivery| delivery.publication.payload == b"right")
        );
        ensure!(
            pending_right
                .iter()
                .any(|delivery| delivery.publication.payload == b"right")
                && !pending_right
                    .iter()
                    .any(|delivery| delivery.publication.payload == b"left")
        );
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn qos1_internal_operation_retry_is_idempotent() -> Result<()> {
    let db = TestDatabase::create().await?;
    let publisher = db
        .store
        .open_session(session("idempotent-publisher", "publisher", true))
        .await?;
    let subscriber = db
        .store
        .open_session(session("idempotent-reader", "subscriber", true))
        .await?;
    db.store
        .subscribe(
            subscriber.session_id,
            subscriber.generation,
            &subscription("retry/#", 1),
        )
        .await?;
    let operation_id = Uuid::new_v4();
    let message = publication("retry/one", 1, b"data", false);
    for _ in 0..2 {
        db.store
            .accept_publication_with_id(
                operation_id,
                publisher.session_id,
                publisher.generation,
                &message,
                b"idempotent-publisher",
            )
            .await?;
    }
    let deliveries = db
        .store
        .pending_deliveries(subscriber.session_id, subscriber.generation, 100)
        .await?;
    ensure!(
        deliveries.len() == 1,
        "reconciling one DB operation must not repeat fanout"
    );
    db.cleanup().await
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn maintenance_expires_queued_messages_before_attempting_a_will() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(1, 4096);
        let publisher = store
            .open_session(session("expiry-publisher", "publisher", true))
            .await?;
        let subscriber = store
            .open_session(session("expiry-reader", "subscriber", true))
            .await?;
        store
            .subscribe(
                subscriber.session_id,
                subscriber.generation,
                &subscription("maintenance/#", 1),
            )
            .await?;
        store
            .close_session(subscriber.session_id, subscriber.generation, 3600, true)
            .await?;
        store
            .accept_publication(
                publisher.session_id,
                publisher.generation,
                &publication(
                    "maintenance/expired",
                    1,
                    b"expired queued publication",
                    true,
                ),
                b"expiry-publisher",
            )
            .await?;
        sqlx::query("UPDATE messages SET expires_at=now()-interval '1 second' WHERE topic=$1")
            .bind(b"maintenance/expired".as_slice())
            .execute(store.pool())
            .await?;

        let mut owner_spec = session("expiry-will-owner", "publisher", true);
        owner_spec.will = Some(publication(
            "maintenance/will",
            1,
            b"will after queue cleanup",
            true,
        ));
        let owner = store.open_session(owner_spec).await?;
        store
            .close_session(owner.session_id, owner.generation, 3600, false)
            .await?;
        sqlx::query("UPDATE sessions SET will_due_at=now()-interval '1 second' WHERE id=$1")
            .bind(owner.session_id)
            .execute(store.pool())
            .await?;

        let deliveries = store.maintenance().await?;
        ensure!(
            deliveries.len() == 1 && deliveries[0].publication.topic == "maintenance/will",
            "expired queued publication must free capacity before Will delivery"
        );
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries WHERE session_id=$1")
            .bind(subscriber.session_id)
            .fetch_one(store.pool())
            .await?;
        ensure!(
            queued == 1,
            "only the Will should remain in the recipient queue"
        );
        let expired_retained: i64 =
            sqlx::query_scalar("SELECT count(*) FROM retained WHERE topic=$1")
                .bind(b"maintenance/expired".as_slice())
                .fetch_one(store.pool())
                .await?;
        ensure!(
            expired_retained == 0,
            "expired retained reference must be cleaned in the same maintenance pass"
        );
        let pending: bool =
            sqlx::query_scalar("SELECT will_message_id IS NOT NULL FROM sessions WHERE id=$1")
                .bind(owner.session_id)
                .fetch_one(store.pool())
                .await?;
        ensure!(!pending, "successfully routed Will must be marked complete");
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn maintenance_retries_blocked_will_without_stalling_other_wills_or_cleanup() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = db.store.clone().with_queue_limits(1, 4096);
        let publisher = store.open_session(session("isolated-publisher", "publisher", true)).await?;
        let blocked = store.open_session(session("blocked-reader", "subscriber", true)).await?;
        let healthy = store.open_session(session("healthy-reader", "subscriber", true)).await?;
        store.subscribe(blocked.session_id, blocked.generation, &subscription("blocked/#", 1)).await?;
        store.subscribe(healthy.session_id, healthy.generation, &subscription("healthy/#", 1)).await?;
        store.accept_publication(publisher.session_id, publisher.generation,
            &publication("blocked/fill", 1, b"keep this accepted delivery", false), b"isolated-publisher").await?;

        let expired = store.open_session(session("expired-session", "subscriber", true)).await?;
        store.close_session(expired.session_id, expired.generation, 3600, true).await?;
        sqlx::query("UPDATE sessions SET expires_at=now()-interval '1 second' WHERE id=$1")
            .bind(expired.session_id).execute(store.pool()).await?;
        store.accept_publication(publisher.session_id, publisher.generation,
            &publication("unsubscribed/expired", 0, b"garbage collect this expired retained message", true), b"isolated-publisher").await?;
        let expired_message: Uuid = sqlx::query_scalar("UPDATE messages SET expires_at=now()-interval '1 second',created_at=now()-interval '2 hours' WHERE topic=$1 RETURNING id")
            .bind(b"unsubscribed/expired".as_slice()).fetch_one(store.pool()).await?;

        let mut blocked_spec = session("blocked-will-owner", "publisher", true);
        blocked_spec.will = Some(publication("blocked/will", 1, b"retry this Will", true));
        let blocked_owner = store.open_session(blocked_spec).await?;
        let mut healthy_spec = session("healthy-will-owner", "publisher", true);
        healthy_spec.will = Some(publication("healthy/will", 1, b"deliver this independent Will", true));
        let healthy_owner = store.open_session(healthy_spec).await?;
        for owner in [&blocked_owner, &healthy_owner] {
            store.close_session(owner.session_id, owner.generation, 3600, false).await?;
            sqlx::query("UPDATE sessions SET will_due_at=now()-interval '1 second' WHERE id=$1")
                .bind(owner.session_id).execute(store.pool()).await?;
        }

        let deliveries = store.maintenance().await?;
        ensure!(deliveries.len() == 1 && deliveries[0].session_id == healthy.session_id,
            "one blocked Will must not prevent an independent Will from being delivered");
        let blocked_pending: bool = sqlx::query_scalar("SELECT will_message_id IS NOT NULL FROM sessions WHERE id=$1")
            .bind(blocked_owner.session_id).fetch_one(store.pool()).await?;
        let healthy_pending: bool = sqlx::query_scalar("SELECT will_message_id IS NOT NULL FROM sessions WHERE id=$1")
            .bind(healthy_owner.session_id).fetch_one(store.pool()).await?;
        ensure!(blocked_pending && !healthy_pending, "only the blocked Will should remain pending for retry");
        let blocked_retained: i64 = sqlx::query_scalar("SELECT count(*) FROM retained WHERE topic=$1")
            .bind(b"blocked/will".as_slice()).fetch_one(store.pool()).await?;
        ensure!(blocked_retained == 0, "failed Will fanout must roll back its retained update");
        let observed = store.monitor().read(&Default::default());
        ensure!(observed.messages.iter().any(|event| event.will && event.topic == "healthy/will")
            && !observed.messages.iter().any(|event| event.topic == "blocked/will"),
            "only a committed independent Will may appear in the message stream");
        let expired_session_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=$1)")
            .bind(expired.session_id).fetch_one(store.pool()).await?;
        let expired_message_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id=$1)")
            .bind(expired_message).fetch_one(store.pool()).await?;
        ensure!(!expired_session_exists && !expired_message_exists,
            "session expiry and orphan-message cleanup must progress while a Will is blocked");

        let queued = store.pending_deliveries(blocked.session_id, blocked.generation, 10).await?;
        ensure!(queued.len() == 1 && queued[0].publication.topic == "blocked/fill",
            "previously accepted delivery must remain unchanged");
        store.reserve_delivery(blocked.session_id, blocked.generation, queued[0].id, 44).await?;
        store.acknowledge(blocked.session_id, blocked.generation, 44, DeliveryPhase::AwaitPubAck).await?;
        let retried = store.maintenance().await?;
        ensure!(retried.len() == 1 && retried[0].session_id == blocked.session_id
            && retried[0].publication.topic == "blocked/will", "pending Will must succeed once capacity is freed");
        let healthy_count: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries WHERE session_id=$1")
            .bind(healthy.session_id).fetch_one(store.pool()).await?;
        ensure!(healthy_count == 1, "retry must not duplicate the already completed independent Will");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
