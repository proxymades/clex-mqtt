use super::*;
use clex_mqtt::admission::Rate;

fn protected_config(burst: u32) -> BrokerConfig {
    let mut config = wire_config();
    config.auth_concurrency = 1;
    config.connect_limits.account = Rate {
        burst,
        refill: Duration::from_secs(60),
    };
    config
}

async fn account_user(h: &Harness, owner: Uuid, name: &str) -> Result<()> {
    let hash = h
        .store
        .credential("wire-test")
        .await?
        .context("fixture credential")?
        .password_hash;
    sqlx::query("INSERT INTO mqtt_users(id,username,password_hash,owner_id) VALUES($1,$2,$3,$4)")
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(hash)
        .bind(owner)
        .execute(h.store.pool())
        .await?;
    sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,publish,subscribe,allow) SELECT $1,u.id,'accounts/'||a.username||'/#',true,true,true FROM mqtt_users u JOIN accounts a ON a.id=u.owner_id WHERE u.username=$2")
        .bind(Uuid::new_v4()).bind(name).execute(h.store.pool()).await?;
    Ok(())
}
async fn owner(h: &Harness, name: &str) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts(id,username,password_hash) VALUES($1,$2,'test-only-web-hash')",
    )
    .bind(id)
    .bind(name)
    .execute(h.store.pool())
    .await?;
    Ok(id)
}

async fn connect_reply(
    address: SocketAddr,
    version: u8,
    id: &str,
    username: &str,
) -> Result<(Client, u8)> {
    let mut client = Client {
        io: TcpStream::connect(address).await?,
        version,
        session_present: false,
    };
    let mut body = Vec::new();
    string(&mut body, "MQTT");
    body.push(version);
    body.push(0xc2);
    // Admission fixtures can remain idle during password-heavy setup on shared runners.
    body.extend_from_slice(&0u16.to_be_bytes());
    if version == 5 {
        body.push(0);
    }
    string(&mut body, id);
    string(&mut body, username);
    string(&mut body, "wire-test-password");
    client.send(0x10, &body).await?;
    let (header, reply) = client.read().await?;
    ensure!(header == 0x20 && reply.len() >= 2, "CONNACK expected");
    Ok((client, reply[1]))
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn account_connection_limit_precedes_password_work_and_preserves_live_delivery() -> Result<()>
{
    let h = Harness::start_with(protected_config(2))
        .await?
        .context("broker")?;
    let outcome: Result<()> = async {
        let a = owner(&h, "alice").await?;
        let b = owner(&h, "bob").await?;
        account_user(&h, a, "alice.one").await?;
        account_user(&h, a, "alice.two").await?;
        account_user(&h, b, "bob.one").await?;
        let mut reader = h.client(5, "stable-reader", true).await?;
        reader.subscribe("guard/value", 0, 1).await?;
        let mut publisher = h.client(5, "stable-publisher", true).await?;
        for name in ["alice.one", "alice.two"] {
            let (client, reason) = connect_reply(h.address, 5, name, name).await?;
            ensure!(reason == 0);
            client.disconnect().await?;
        }
        let checks = h.broker.metrics().password_checks;
        for index in 0..12 {
            let version = if index % 2 == 0 { 5 } else { 4 };
            let name = if index % 3 == 0 {
                "alice.two"
            } else {
                "alice.one"
            };
            let (_client, reason) =
                connect_reply(h.address, version, &format!("rotating-{index}"), name).await?;
            ensure!(
                reason == if version == 5 { 0x9f } else { 3 },
                "appropriate MQTT admission refusal"
            );
        }
        ensure!(
            h.broker.metrics().password_checks == checks,
            "throttled CONNECT must skip password verification"
        );
        ensure!(h.broker.metrics().account_connection_rate_rejections == 12);
        let (mut bob, reason) = connect_reply(h.address, 5, "other-account", "bob.one").await?;
        ensure!(reason == 0, "another account retains its connection budget");
        bob.send(0xc0, &[]).await?;
        ensure!(bob.read().await?.0 == 0xd0);
        publisher
            .publish("guard/value", b"still-delivers", 1, 7, false, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x40);
        let (header, payload) = reader.read().await?;
        ensure!(header >> 4 == 3 && payload.ends_with(b"still-delivers"));
        reader.send(0xc0, &[]).await?;
        ensure!(reader.read().await?.0 == 0xd0);
        bob.disconnect().await?;
        publisher.disconnect().await?;
        reader.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn hundred_connected_accounts_keep_publishing_while_another_account_is_throttled()
-> Result<()> {
    let h = Harness::start_with(protected_config(1))
        .await?
        .context("broker")?;
    let outcome: Result<()> = async {
        let (mut reader, reason) =
            connect_reply(h.address, 5, "stable-observer", "wire-test").await?;
        ensure!(reason == 0);
        reader.subscribe("accounts/#", 0, 1).await?;
        let mut clients = Vec::new();
        for index in 0..100 {
            let name = format!("client-{index}");
            let id = owner(&h, &name).await?;
            let username = format!("{name}.device");
            account_user(&h, id, &username).await?;
            let (client, reason) = connect_reply(h.address, 5, &name, &username).await?;
            ensure!(reason == 0);
            clients.push(client);
        }
        let id = owner(&h, "frequent").await?;
        account_user(&h, id, "frequent.device").await?;
        let (client, reason) =
            connect_reply(h.address, 5, "frequent-first", "frequent.device").await?;
        ensure!(reason == 0);
        client.disconnect().await?;
        let checks = h.broker.metrics().password_checks;
        let address = h.address;
        let attempts = tokio::spawn(async move {
            for index in 0..30 {
                let (_client, reason) =
                    connect_reply(address, 5, &format!("frequent-{index}"), "frequent.device")
                        .await?;
                ensure!(reason == 0x9f);
            }
            Ok::<(), anyhow::Error>(())
        });
        for (index, client) in clients.iter_mut().enumerate() {
            client
                .publish(
                    &format!("accounts/client-{index}/value"),
                    &index.to_be_bytes(),
                    0,
                    0,
                    false,
                    false,
                )
                .await?;
            client.send(0xc0, &[]).await?;
            ensure!(client.read().await?.0 == 0xd0);
        }
        let mut topics = std::collections::HashSet::new();
        for _ in 0..100 {
            let (header, body) = reader.read().await?;
            ensure!(
                header >> 4 == 3,
                "expected PUBLISH, received {header:#04x} {body:?}"
            );
            let length = usize::from(u16::from_be_bytes([body[0], body[1]]));
            topics.insert(String::from_utf8(body[2..2 + length].to_vec())?);
        }
        ensure!(
            topics.len() == 100,
            "all connected accounts keep delivering"
        );
        attempts.await??;
        ensure!(h.broker.metrics().password_checks == checks);
        ensure!(h.broker.metrics().account_connection_rate_rejections == 30);
        for client in clients {
            client.disconnect().await?;
        }
        reader.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
