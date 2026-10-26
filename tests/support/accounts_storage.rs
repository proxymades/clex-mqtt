use super::*;
use anyhow::Context;
use clex_mqtt::accounts::topic_prefix;

async fn owners(store: &Store) -> Result<(Uuid, Uuid)> {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    for (id, name) in [(a, "alice"), (b, "bob")] {
        sqlx::query("INSERT INTO accounts(id,username,password_hash,max_publish_rate) VALUES($1,$2,'test-only-not-an-authentication-hash',1000)").bind(id).bind(name).execute(store.pool()).await?;
    }
    sqlx::query("UPDATE mqtt_users SET owner_id=$1 WHERE username IN ('publisher','subscriber')")
        .bind(a)
        .execute(store.pool())
        .await?;
    sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,owner_id) VALUES($1,'outside','test-only-not-an-authentication-hash',$2)").bind(Uuid::new_v4()).bind(b).execute(store.pool()).await?;
    sqlx::query("INSERT INTO mqtt_users(id,username,password_hash) VALUES($1,'legacy','test-only-not-an-authentication-hash')").bind(Uuid::new_v4()).execute(store.pool()).await?;
    sqlx::query("INSERT INTO mqtt_acl_rules(id,filter,publish,subscribe,allow) VALUES($1,'#',true,true,true)").bind(Uuid::new_v4()).execute(store.pool()).await?;
    Ok((a, b))
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_namespaces_bound_acl_routing_retained_wills_and_qos2_release() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let (a, _b) = owners(store).await?;
        let ap = topic_prefix("alice");
        let bp = topic_prefix("bob");
        let source = store
            .open_session(session("source", "publisher", true))
            .await?;
        let reader = store
            .open_session(session("reader", "subscriber", true))
            .await?;
        let outside = store
            .open_session(session("reader", "outside", true))
            .await?;
        let legacy = store
            .open_session(session("reader", "legacy", true))
            .await?;
        ensure!(
            !store
                .authorize("publisher", &format!("{bp}state"), true)
                .await?
        );
        ensure!(!store.authorize_subscription("publisher", "#").await?);
        ensure!(
            store
                .authorize_subscription("publisher", &format!("$share/workers/{ap}#"))
                .await?
        );
        ensure!(
            store
                .subscribe(reader.session_id, reader.generation, &subscription("#", 1))
                .await
                .is_err()
        );
        store
            .subscribe(
                reader.session_id,
                reader.generation,
                &subscription(&format!("{ap}#"), 1),
            )
            .await?;
        store
            .subscribe(
                outside.session_id,
                outside.generation,
                &subscription(&format!("{bp}#"), 1),
            )
            .await?;
        store
            .subscribe(legacy.session_id, legacy.generation, &subscription("#", 1))
            .await?;
        let delivered = store
            .accept_publication(
                source.session_id,
                source.generation,
                &publication(&format!("{ap}state"), 1, b"private", true),
                b"source",
            )
            .await?;
        ensure!(
            delivered.len() == 1 && delivered[0].session_id == reader.session_id,
            "private publication escaped its account"
        );
        ensure!(
            store
                .pending_deliveries(outside.session_id, outside.generation, 10)
                .await?
                .is_empty()
        );
        ensure!(
            store
                .pending_deliveries(legacy.session_id, legacy.generation, 10)
                .await?
                .is_empty()
        );
        ensure!(
            store
                .subscribe(legacy.session_id, legacy.generation, &subscription("#", 1))
                .await?
                .is_empty(),
            "legacy wildcard replay exposed private Retain"
        );
        ensure!(
            store
                .accept_publication(
                    source.session_id,
                    source.generation,
                    &publication(&format!("{bp}state"), 1, b"forbidden", true),
                    b"source"
                )
                .await
                .is_err()
        );
        let mut invalid = session("invalid-will", "publisher", true);
        invalid.will = Some(publication(&format!("{bp}will"), 1, b"forbidden", false));
        ensure!(store.open_session(invalid).await.is_err());
        let q2 = publication(&format!("{ap}later"), 2, b"accepted before disable", false);
        ensure!(
            store
                .receive_qos2(source.session_id, source.generation, 7, &q2)
                .await?
        );
        sqlx::query("UPDATE accounts SET enabled=false WHERE id=$1")
            .bind(a)
            .execute(store.pool())
            .await?;
        ensure!(
            !store
                .credential("publisher")
                .await?
                .context("credential")?
                .enabled
        );
        ensure!(
            !store
                .authorize("publisher", &format!("{ap}state"), true)
                .await?
        );
        ensure!(
            store
                .open_session(session("denied", "publisher", true))
                .await
                .is_err()
        );
        ensure!(
            store
                .release_qos2(source.session_id, source.generation, 7)
                .await
                .is_err(),
            "disabled account completed a delayed publication"
        );
        store
            .accept_publication(
                outside.session_id,
                outside.generation,
                &publication(&format!("{bp}state"), 1, b"other account", false),
                b"reader",
            )
            .await?;
        sqlx::query("UPDATE accounts SET enabled=true WHERE id=$1")
            .bind(a)
            .execute(store.pool())
            .await?;
        let released = store
            .release_qos2(source.session_id, source.generation, 7)
            .await?;
        ensure!(released.found && released.deliveries.len() == 1);
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_storage_quota_is_atomic_across_retain_delivery_incoming_qos2_and_will()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome:Result<()>=async {
        let store=&db.store;
        let (a,_)=owners(store).await?;let prefix=topic_prefix("alice");
        sqlx::query("UPDATE accounts SET max_queued_messages=1 WHERE id=$1").bind(a).execute(store.pool()).await?;
        let source=store.open_session(session("source","publisher",true)).await?;
        let reader=store.open_session(session("reader","subscriber",true)).await?;
        let filter=format!("{prefix}#");
        store.subscribe(reader.session_id,reader.generation,&subscription(&filter,2)).await?;
        let rejected=store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}state"),1,b"two obligations",true),b"source").await.unwrap_err();
        ensure!(rejected.is::<clex_mqtt::store::QuotaExceeded>());
        let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM messages),(SELECT count(*) FROM retained),(SELECT count(*) FROM deliveries)").fetch_one(store.pool()).await?;
        ensure!(counts==(0,0,0),"quota failure partially committed routing");
        let queued=store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}state"),1,b"one",false),b"source").await?;
        let rejected=store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}state"),1,b"overflow",false),b"source").await.unwrap_err();
        ensure!(rejected.is::<clex_mqtt::store::QuotaExceeded>());
        store.discard_delivery(reader.session_id,reader.generation,queued[0].id).await?;
        sqlx::query("UPDATE accounts SET max_queued_bytes=1024 WHERE id=$1").bind(a).execute(store.pool()).await?;
        ensure!(store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}state"),1,&[0xaa;2048],false),b"source").await.is_err());
        sqlx::query("UPDATE accounts SET max_queued_bytes=8388608 WHERE id=$1").bind(a).execute(store.pool()).await?;
        let q2=publication(&format!("{prefix}q2"),2,b"one incoming",false);
        ensure!(store.receive_qos2(source.session_id,source.generation,42,&q2).await?);
        let before:(i64,i32)=sqlx::query_as("SELECT publish_window,publish_count FROM accounts WHERE id=$1").bind(a).fetch_one(store.pool()).await?;
        ensure!(!store.receive_qos2(source.session_id,source.generation,42,&q2).await?);
        let after:(i64,i32)=sqlx::query_as("SELECT publish_window,publish_count FROM accounts WHERE id=$1").bind(a).fetch_one(store.pool()).await?;
        ensure!(before==after,"duplicate incoming QoS 2 consumed publication budget");
        let released=store.release_qos2(source.session_id,source.generation,42).await?;
        ensure!(released.deliveries.len()==1,"handoff from incoming QoS 2 double-counted durable storage");
        ensure!(store.release_qos2(source.session_id,source.generation,42).await?.deliveries.is_empty());
        store.discard_delivery(reader.session_id,reader.generation,released.deliveries[0].id).await?;
        store.unsubscribe(reader.session_id,reader.generation,&filter).await?;
        store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}retained"),1,b"retained",true),b"source").await?;
        ensure!(store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}second"),1,b"excess retained",true),b"source").await.is_err(),"Retain bypassed account storage limit");
        store.accept_publication(source.session_id,source.generation,&publication(&format!("{prefix}retained"),1,b"replacement",true),b"source").await?;
        let mut spec=session("will-session","publisher",true);
        spec.will=Some(publication(&format!("{prefix}will"),1,b"will",false));
        ensure!(store.open_session(spec).await.is_err(),"Will bypassed account storage limit");
        let sessions:i64=sqlx::query_scalar("SELECT count(*) FROM sessions WHERE client_id=$1").bind(b"will-session".as_slice()).fetch_one(store.pool()).await?;
        ensure!(sessions==0,"failed Will quota left an open session");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_connection_and_persistent_session_limits_allow_fenced_takeover() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let (a, _) = owners(store).await?;
        sqlx::query("UPDATE accounts SET max_connections=1,max_sessions=1 WHERE id=$1")
            .bind(a)
            .execute(store.pool())
            .await?;
        let initial = store
            .open_session(session("same", "publisher", true))
            .await?;
        let resumed = store
            .open_session(session("same", "publisher", false))
            .await?;
        ensure!(
            resumed.session_id == initial.session_id && resumed.generation != initial.generation
        );
        ensure!(
            store
                .open_session(session("different", "subscriber", true))
                .await
                .is_err()
        );
        store.open_session(session("same", "outside", true)).await?;
        store
            .close_session(resumed.session_id, resumed.generation, 3600, true)
            .await?;
        ensure!(
            store
                .open_session(session("different", "subscriber", true))
                .await
                .is_err(),
            "persistent session limit was bypassed while offline"
        );
        let reconnect = store
            .open_session(session("same", "publisher", false))
            .await?;
        store
            .close_session(reconnect.session_id, reconnect.generation, 0, true)
            .await?;
        store
            .open_session(session("different", "subscriber", true))
            .await?;
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

use serde_json::Value;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_completed_messages_keep_only_bounded_receipts_and_remain_idempotent() -> Result<()>
{
    let db = TestDatabase::create().await?;
    let outcome:Result<()>=async {
        let store=&db.store;let (a,_)=owners(store).await?;let prefix=topic_prefix("alice");
        sqlx::query("UPDATE accounts SET max_queued_messages=1,max_queued_bytes=1024 WHERE id=$1").bind(a).execute(store.pool()).await?;
        let source=store.open_session(session("source","publisher",true)).await?;
        let operation=Uuid::new_v4();
        let message=publication(&format!("{prefix}transient"),1,&[0xab;2048],false);
        ensure!(store.accept_publication_with_id(operation,source.session_id,source.generation,&message,b"source").await?.is_empty());
        let receipt:(bool,i32,i32,Value)=sqlx::query_as("SELECT compacted,octet_length(payload),octet_length(topic),properties FROM messages WHERE id=$1").bind(operation).fetch_one(store.pool()).await?;
        ensure!(receipt==(true,0,0,serde_json::json!({})),"unreferenced body escaped durable storage bounds");
        let before:(i64,i32)=sqlx::query_as("SELECT publish_window,publish_count FROM accounts WHERE id=$1").bind(a).fetch_one(store.pool()).await?;
        let reader=store.open_session(session("reader","subscriber",true)).await?;
        store.subscribe(reader.session_id,reader.generation,&subscription(&format!("{prefix}#"),1)).await?;
        ensure!(store.accept_publication_with_id(operation,source.session_id,source.generation,&message,b"source").await?.is_empty(),"compaction forgot an accepted operation");
        let after:(i64,i32)=sqlx::query_as("SELECT publish_window,publish_count FROM accounts WHERE id=$1").bind(a).fetch_one(store.pool()).await?;
        ensure!(before==after);
        ensure!(store.pending_deliveries(reader.session_id,reader.generation,10).await?.is_empty());
        ensure!(store.accept_publication(source.session_id,source.generation,&message,b"source").await.is_err(),"referenced payload exceeded the byte limit");
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_disabled_will_does_not_block_other_accounts_or_maintenance() -> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let (a, _b) = owners(store).await?;
        let mut left = session("will-a", "publisher", true);
        left.will = Some(publication(
            &format!("{}will", topic_prefix("alice")),
            1,
            b"a",
            false,
        ));
        let left = store.open_session(left).await?;
        let mut right = session("will-b", "outside", true);
        right.will = Some(publication(
            &format!("{}will", topic_prefix("bob")),
            1,
            b"b",
            false,
        ));
        let right = store.open_session(right).await?;
        store
            .close_session(left.session_id, left.generation, 3600, false)
            .await?;
        store
            .close_session(right.session_id, right.generation, 3600, false)
            .await?;
        sqlx::query("UPDATE accounts SET enabled=false WHERE id=$1")
            .bind(a)
            .execute(store.pool())
            .await?;
        store.maintenance().await?;
        let pending: bool =
            sqlx::query_scalar("SELECT will_message_id IS NOT NULL FROM sessions WHERE id=$1")
                .bind(left.session_id)
                .fetch_one(store.pool())
                .await?;
        let complete: bool =
            sqlx::query_scalar("SELECT will_message_id IS NULL FROM sessions WHERE id=$1")
                .bind(right.session_id)
                .fetch_one(store.pool())
                .await?;
        ensure!(
            pending && complete,
            "one blocked account stalled independent Will publication"
        );
        sqlx::query("UPDATE accounts SET enabled=true WHERE id=$1")
            .bind(a)
            .execute(store.pool())
            .await?;
        store.maintenance().await?;
        let complete: bool =
            sqlx::query_scalar("SELECT will_message_id IS NULL FROM sessions WHERE id=$1")
                .bind(left.session_id)
                .fetch_one(store.pool())
                .await?;
        ensure!(complete);
        Ok(())
    }
    .await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn readable_name_upgrade_is_atomic_idempotent_and_keeps_passwords_and_legacy_users()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let outcome: Result<()> = async {
        let store = &db.store;
        let owner = Uuid::new_v4();
        let user = Uuid::new_v4();
        let old_login = format!("acc_{}_esp32-01", owner.simple());
        let old_prefix = format!("accounts/{owner}/");
        sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES($1,'alex','test-only')").bind(owner).execute(store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,owner_id,account_label) VALUES($1,$2,'keep-this-hash',$3,'esp32-01')").bind(user).bind(&old_login).bind(owner).execute(store.pool()).await?;
        sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,publish,subscribe,allow) VALUES($1,$2,$3,true,true,true)").bind(Uuid::new_v4()).bind(user).bind(format!("{old_prefix}#")).execute(store.pool()).await?;
        // Prepare an old durable session and Retain without using the new namespace gate.
        let session_id = Uuid::new_v4();
        let message = Uuid::new_v4();
        sqlx::query("INSERT INTO sessions(id,client_id,principal,protocol,online,expiry_interval,queued_count,queued_bytes) VALUES($1,'device',$2,5,false,3600,0,0)").bind(session_id).bind(&old_login).execute(store.pool()).await?;
        sqlx::query("INSERT INTO subscriptions(id,session_id,filter,qos,no_local,retain_as_published,retain_handling) VALUES($1,$2,$3,1,false,false,0)").bind(Uuid::new_v4()).bind(session_id).bind(format!("{old_prefix}#")).execute(store.pool()).await?;
        sqlx::query("INSERT INTO messages(id,topic,payload,qos,retain,properties,owner_id) VALUES($1,$2,'test',1,true,'{}',$3)").bind(message).bind(format!("{old_prefix}value").as_bytes()).bind(owner).execute(store.pool()).await?;
        sqlx::query("INSERT INTO retained(topic,message_id) VALUES($1,$2)").bind(format!("{old_prefix}value").as_bytes()).bind(message).execute(store.pool()).await?;
        ensure!(clex_mqtt::accounts::require_readable_names(store.pool()).await.is_err());
        // A global-name collision must roll back every change, including deleted sessions.
        let collision = Uuid::new_v4();
        sqlx::query("INSERT INTO mqtt_users(id,username,password_hash) VALUES($1,'alex.esp32-01','legacy-hash')").bind(collision).execute(store.pool()).await?;
        ensure!(clex_mqtt::accounts::upgrade_names(store.pool()).await.is_err());
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE id=$1").bind(session_id).fetch_one(store.pool()).await?;
        ensure!(sessions == 1 && store.credential(&old_login).await?.is_some());
        sqlx::query("DELETE FROM mqtt_users WHERE id=$1").bind(collision).execute(store.pool()).await?;

        let foreign = store.open_session(session("outside-scope", "subscriber", true)).await?;
        let delivery = Uuid::new_v4();
        sqlx::query("INSERT INTO deliveries(id,session_id,message_id,qos,retain,accounted_bytes) SELECT $1,$2,id,1,true,octet_length(payload)+octet_length(topic)+octet_length(properties::text) FROM messages WHERE id=$3").bind(delivery).bind(foreign.session_id).bind(message).execute(store.pool()).await?;
        sqlx::query("UPDATE sessions SET queued_count=1,queued_bytes=(SELECT accounted_bytes FROM deliveries WHERE id=$1) WHERE id=$2").bind(delivery).bind(foreign.session_id).execute(store.pool()).await?;
        ensure!(clex_mqtt::accounts::upgrade_names(store.pool()).await.is_err(), "upgrade discarded another principal's obligation");
        let sessions_after_failure: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE id=$1").bind(session_id).fetch_one(store.pool()).await?;
        ensure!(sessions_after_failure == 1 && store.credential(&old_login).await?.is_some(), "failed transition did not roll back session deletion");
        sqlx::query("DELETE FROM sessions WHERE id=$1").bind(foreign.session_id).execute(store.pool()).await?;
        let mut held = store.pool().acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)").bind(clex_mqtt::INSTANCE_LOCK).execute(&mut *held).await?;
        ensure!(clex_mqtt::accounts::upgrade_names(store.pool()).await.is_err(), "upgrade ignored an active broker instance");
        sqlx::query("SELECT pg_advisory_unlock($1)").bind(clex_mqtt::INSTANCE_LOCK).execute(&mut *held).await?;
        drop(held);
        let legacy_hash = store.credential("publisher").await?.context("legacy credential")?.password_hash;
        let changed = clex_mqtt::accounts::upgrade_names(store.pool()).await?;
        ensure!(changed.accounts == 1 && changed.mqtt_users == 1 && changed.sessions == 1 && changed.messages == 1);
        ensure!(store.credential(&old_login).await?.is_none());
        let credential = store.credential("alex.esp32-01").await?.context("new credential")?;
        ensure!(credential.id == user && credential.owner_id == Some(owner) && credential.password_hash == "keep-this-hash");
        ensure!(store.credential("publisher").await?.context("legacy")?.password_hash == legacy_hash);
        let rule: String = sqlx::query_scalar("SELECT filter FROM mqtt_acl_rules WHERE mqtt_user_id=$1").bind(user).fetch_one(store.pool()).await?;
        ensure!(rule == "accounts/alex/#");
        ensure!(store.authorize("alex.esp32-01", "accounts/alex/value", true).await?);
        ensure!(!store.authorize("alex.esp32-01", &format!("{old_prefix}value"), true).await?);
        clex_mqtt::accounts::require_readable_names(store.pool()).await?;
        let unchanged = clex_mqtt::accounts::upgrade_names(store.pool()).await?;
        ensure!(unchanged.accounts == 0 && unchanged.mqtt_users == 0 && unchanged.sessions == 0 && unchanged.messages == 0);
        Ok(())
    }.await;
    let cleanup = db.cleanup().await;
    outcome?;
    cleanup
}
