//! Independently assembled MQTT packets and a client-side SCRAM transcript.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{hmac, pbkdf2};
use sha2::{Digest, Sha256};
use std::num::NonZeroU32;

const METHOD: &str = "SCRAM-SHA-256";
const FIRST: &str = "n,,n=wire-test,r=independent-client-nonce";

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn mqtt5_takeover_reports_the_reason_when_generation_fencing_precedes_registry_cancel()
-> Result<()> {
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        let mut old = h.client(5, "fenced-before-cancel", false).await?;
        let credential = h.store.credential("wire-test").await?.unwrap();
        // Reproduce the gap between committing the replacement generation and
        // registering it to cancel the old connection task.
        h.store
            .open_session_authenticated(
                clex_mqtt::domain::SessionSpec {
                    client_id: b"fenced-before-cancel".to_vec(),
                    principal: "wire-test".to_owned(),
                    clean_start: false,
                    expiry_interval: 300,
                    protocol: 5,
                    will: None,
                    will_delay_interval: 0,
                },
                credential.id,
                &credential.password_hash,
            )
            .await?;
        old.assert_disconnect(0x8e).await?;
        let mut replacement = h.client(5, "fenced-before-cancel", false).await?;
        ensure!(replacement.session_present);
        replacement.send(0xc0, &[]).await?;
        ensure!(replacement.read().await?.0 == 0xd0);
        replacement.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

async fn prepare(h: &Harness) -> Result<()> {
    let credential = h.store.credential("wire-test").await?.unwrap();
    h.store
        .enable_scram(
            &credential,
            clex_mqtt::scram::Verifier::from_password("wire-test-password")?,
        )
        .await
}

async fn begin(h: &Harness, id: &str, method: &str, first: &str) -> Result<Client> {
    let mut client = Client {
        io: TcpStream::connect(h.address).await?,
        version: 5,
        session_present: false,
    };
    let mut properties = vec![0x15];
    string(&mut properties, method);
    properties.push(0x16);
    binary(&mut properties, first.as_bytes());
    // A persistent session and a Will must only be created after authentication.
    properties.extend_from_slice(&[0x11, 0, 0, 1, 44]);
    let mut body = Vec::new();
    string(&mut body, "MQTT");
    body.extend_from_slice(&[5, 4, 0, 60]);
    property_section(&mut body, &properties);
    string(&mut body, id);
    body.push(0);
    string(&mut body, "auth/status");
    binary(&mut body, b"offline");
    client.send(0x10, &body).await?;
    Ok(client)
}

async fn auth(client: &mut Client, reason: u8, method: Option<&str>, data: &str) -> Result<()> {
    let mut properties = Vec::new();
    if let Some(method) = method {
        properties.push(0x15);
        string(&mut properties, method);
    }
    properties.push(0x16);
    binary(&mut properties, data.as_bytes());
    let mut body = vec![reason];
    property_section(&mut body, &properties);
    client.send(0xf0, &body).await
}

fn blob<'a>(body: &'a [u8], at: &mut usize) -> Result<&'a [u8]> {
    ensure!(*at + 2 <= body.len());
    let size = u16::from_be_bytes([body[*at], body[*at + 1]]) as usize;
    *at += 2;
    ensure!(*at + size <= body.len());
    let value = &body[*at..*at + size];
    *at += size;
    Ok(value)
}

fn authentication_data(header: u8, body: &[u8]) -> Result<String> {
    ensure!(matches!(header, 0x20 | 0xf0));
    let mut at = if header == 0x20 { 2 } else { 1 };
    let (size, bytes) = read_variable(&body[at..])?;
    at += bytes;
    ensure!(at + size == body.len());
    let mut method = None;
    let mut data = None;
    while at < body.len() {
        let kind = body[at];
        at += 1;
        match kind {
            0x15 => method = Some(std::str::from_utf8(blob(body, &mut at)?)?.to_owned()),
            0x16 => data = Some(std::str::from_utf8(blob(body, &mut at)?)?.to_owned()),
            0x12 | 0x1f => {
                blob(body, &mut at)?;
            }
            0x21 | 0x22 => at += 2,
            0x27 => at += 4,
            0x2a => at += 1,
            _ => anyhow::bail!("unexpected CONNACK/AUTH property {kind}"),
        }
    }
    ensure!(method.as_deref() == Some(METHOD));
    data.context("missing authentication data")
}

fn read_variable(bytes: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0;
    let mut multiplier = 1;
    for (index, byte) in bytes.iter().take(4).enumerate() {
        value += usize::from(byte & 127) * multiplier;
        if byte & 128 == 0 {
            return Ok((value, index + 1));
        }
        multiplier *= 128;
    }
    anyhow::bail!("invalid variable integer")
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

fn proof(first: &str, challenge: &str, password: &str) -> Result<(String, String)> {
    let fields: Vec<_> = challenge.split(',').collect();
    ensure!(fields.len() == 3);
    let nonce = fields[0].strip_prefix("r=").context("nonce")?;
    ensure!(nonce.starts_with(first.split(",r=").nth(1).context("client nonce")?));
    let salt = STANDARD.decode(fields[1].strip_prefix("s=").context("salt")?)?;
    let iterations = fields[2]
        .strip_prefix("i=")
        .context("iterations")?
        .parse::<u32>()?;
    ensure!((4096..=1_000_000).contains(&iterations));
    let mut salted = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations).unwrap(),
        &salt,
        password.as_bytes(),
        &mut salted,
    );
    let client_key = mac(&salted, b"Client Key");
    let stored = Sha256::digest(&client_key);
    let final_without = format!("c=biws,r={nonce}");
    let message = format!(
        "{},{challenge},{final_without}",
        first.strip_prefix("n,,").unwrap()
    );
    let signature = mac(&stored, message.as_bytes());
    let proof: Vec<_> = client_key
        .iter()
        .zip(signature)
        .map(|(a, b)| a ^ b)
        .collect();
    let server_key = mac(&salted, b"Server Key");
    Ok((
        format!("{final_without},p={}", STANDARD.encode(proof)),
        format!(
            "v={}",
            STANDARD.encode(mac(&server_key, message.as_bytes()))
        ),
    ))
}

async fn connected(h: &Harness, id: &str) -> Result<Client> {
    let mut client = begin(h, id, METHOD, FIRST).await?;
    let (header, body) = client.read().await?;
    ensure!(header == 0xf0 && body[0] == 0x18);
    let challenge = authentication_data(header, &body)?;
    let (final_message, expected) = proof(FIRST, &challenge, "wire-test-password")?;
    auth(&mut client, 0x18, Some(METHOD), &final_message).await?;
    let (header, body) = client.read().await?;
    ensure!(header == 0x20 && body[1] == 0);
    ensure!(
        authentication_data(header, &body)? == expected,
        "server signature differs"
    );
    Ok(client)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn scram_upgrades_existing_password_credentials_and_routes_across_protocols() -> Result<()> {
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        ensure!(
            h.store
                .credential("wire-test")
                .await?
                .unwrap()
                .scram_verifier
                .is_none()
        );
        let legacy = h.client(3, "password-upgrade", true).await?;
        legacy.disconnect().await?;
        ensure!(
            h.store
                .credential("wire-test")
                .await?
                .unwrap()
                .scram_verifier
                .is_some()
        );
        let mut subscriber = connected(&h, "scram-main").await?;
        subscriber.subscribe("auth/data", 1, 1).await?;
        let mut publisher = h.client(4, "legacy-publisher", true).await?;
        publisher
            .publish("auth/data", b"interoperable", 1, 2, false, false)
            .await?;
        ensure!(publisher.read().await?.0 == 0x40);
        let (header, body) = subscriber.read().await?;
        let (_, payload, id) = publication(header, &body, 5)?;
        ensure!(payload == b"interoperable");
        subscriber.ack(4, id).await?;
        subscriber.disconnect().await?;
        publisher.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn scram_reauthentication_keeps_session_and_allows_mqtt_packets_between_steps() -> Result<()>
{
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        prepare(&h).await?;
        let mut client = connected(&h, "reauth-main").await?;
        client.subscribe("auth/reauth", 1, 1).await?;
        let generation: i64 =
            sqlx::query_scalar("SELECT generation FROM sessions WHERE client_id=$1")
                .bind(b"reauth-main".as_slice())
                .fetch_one(h.store.pool())
                .await?;
        for round in 0..2 {
            let first = format!("n,,n=wire-test,r=reauth-nonce-{round}");
            auth(&mut client, 0x19, Some(METHOD), &first).await?;
            let (header, body) = client.read().await?;
            ensure!(body[0] == 0x18);
            let challenge = authentication_data(header, &body)?;
            client.send(0xc0, &[]).await?;
            ensure!(client.read().await? == (0xd0, Vec::new()));
            client
                .publish("auth/reauth", b"during-auth", 1, 10 + round, false, false)
                .await?;
            ensure!(client.read().await?.0 == 0x40);
            let (header, body) = client.read().await?;
            let (_, payload, id) = publication(header, &body, 5)?;
            ensure!(payload == b"during-auth");
            client.ack(4, id).await?;
            let (final_message, expected) = proof(&first, &challenge, "wire-test-password")?;
            auth(&mut client, 0x18, Some(METHOD), &final_message).await?;
            let (header, body) = client.read().await?;
            ensure!(header == 0xf0 && body[0] == 0);
            ensure!(authentication_data(header, &body)? == expected);
        }
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT generation FROM sessions WHERE client_id=$1")
                .bind(b"reauth-main".as_slice())
                .fetch_one(h.store.pool())
                .await?
                == generation
        );
        client.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn scram_initial_failures_create_no_session_will_or_registry_entry() -> Result<()> {
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        prepare(&h).await?;
        let mut unsupported = begin(&h, "unsupported", "other-method", FIRST).await?;
        let (header, body) = unsupported.read().await?;
        ensure!(header == 0x20 && body[1] == 0x8c);
        for mode in [
            "wrong-password",
            "wrong-method",
            "missing-method",
            "wrong-reason",
            "wrong-packet",
            "revoked",
        ] {
            let mut client = begin(&h, mode, METHOD, FIRST).await?;
            let (header, body) = client.read().await?;
            let challenge = authentication_data(header, &body)?;
            let (final_message, _) = proof(
                FIRST,
                &challenge,
                if mode == "wrong-password" {
                    "incorrect-password"
                } else {
                    "wire-test-password"
                },
            )?;
            if mode == "revoked" {
                sqlx::query("UPDATE mqtt_users SET enabled=false WHERE username='wire-test'")
                    .execute(h.store.pool())
                    .await?;
                auth(&mut client, 0x18, Some(METHOD), &final_message).await?;
                let (header, body) = client.read().await?;
                ensure!(header == 0x20 && body[1] == 0x87);
            } else {
                if mode == "wrong-packet" {
                    client.send(0xc0, &[]).await?;
                } else {
                    auth(
                        &mut client,
                        if mode == "wrong-reason" { 0 } else { 0x18 },
                        if mode == "missing-method" {
                            None
                        } else if mode == "wrong-method" {
                            Some("other-method")
                        } else {
                            Some(METHOD)
                        },
                        &final_message,
                    )
                    .await?;
                }
                let (header, body) = client.read().await?;
                ensure!(
                    header == 0x20 && body[1] == if mode == "wrong-password" { 0x86 } else { 0x82 },
                    "incorrect failure for {mode}: {body:?}"
                );
            }
        }
        ensure!(h.broker.clients().await.is_empty());
        for table in ["sessions", "messages", "deliveries"] {
            ensure!(
                sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                    .fetch_one(h.store.pool())
                    .await?
                    == 0,
                "authentication failure created {table}"
            );
        }
        ensure!(
            h.store
                .monitor()
                .read(&Default::default())
                .messages
                .is_empty()
        );
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn scram_reauthentication_rejects_identity_switches_replays_and_unsolicited_auth()
-> Result<()> {
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        prepare(&h).await?;
        let mut basic = h.client(5, "basic-unsolicited", true).await?;
        auth(&mut basic, 0x19, Some(METHOD), FIRST).await?;
        basic.assert_disconnect(0x82).await?;
        let mut switched = connected(&h, "switch-user").await?;
        auth(
            &mut switched,
            0x19,
            Some(METHOD),
            "n,,n=another-user,r=new-nonce",
        )
        .await?;
        switched.assert_disconnect(0x87).await?;
        let mut client = connected(&h, "replay-user").await?;
        auth(&mut client, 0x19, Some(METHOD), FIRST).await?;
        let (header, body) = client.read().await?;
        let challenge = authentication_data(header, &body)?;
        let (old_proof, _) = proof(FIRST, &challenge, "wire-test-password")?;
        auth(&mut client, 0x18, Some(METHOD), &old_proof).await?;
        ensure!(client.read().await?.1[0] == 0);
        auth(&mut client, 0x19, Some(METHOD), FIRST).await?;
        let (header, body) = client.read().await?;
        ensure!(authentication_data(header, &body)? != challenge);
        auth(&mut client, 0x18, Some(METHOD), &old_proof).await?;
        client.assert_disconnect(0x87).await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn scram_challenges_expire_and_password_rotation_fences_preparation() -> Result<()> {
    let h = Harness::start_with(BrokerConfig {
        handshake_timeout: Duration::from_millis(700),
        ..Default::default()
    })
    .await?
    .unwrap();
    let outcome: Result<()> = async {
        prepare(&h).await?;
        let mut initial = begin(&h, "initial-timeout", METHOD, FIRST).await?;
        ensure!(initial.read().await?.0 == 0xf0);
        ensure!(
            timeout(Duration::from_secs(2), initial.io.read_u8())
                .await?
                .is_err()
        );
        let mut client = connected(&h, "reauth-timeout").await?;
        auth(&mut client, 0x19, Some(METHOD), FIRST).await?;
        ensure!(client.read().await?.0 == 0xf0);
        client.assert_disconnect(0x87).await?;
        let old = h.store.credential("wire-test").await?.unwrap();
        sqlx::query("UPDATE mqtt_users SET password_hash=$1,scram_verifier=NULL WHERE id=$2")
            .bind(hash_password("rotated-password")?)
            .bind(old.id)
            .execute(h.store.pool())
            .await?;
        h.store
            .enable_scram(
                &old,
                clex_mqtt::scram::Verifier::from_password("wire-test-password")?,
            )
            .await?;
        ensure!(
            h.store
                .credential("wire-test")
                .await?
                .unwrap()
                .scram_verifier
                .is_none()
        );
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn mqtt5_handshake_assigns_empty_persistent_ids_and_honors_small_packet_limits() -> Result<()>
{
    let h = Harness::start().await?.unwrap();
    let outcome: Result<()> = async {
        let client =
            Client::connect_v5_properties(h.address, "", false, &[0x11, 0, 0, 1, 44]).await?;
        ensure!(!client.session_present);
        let ids: Vec<Vec<u8>> = sqlx::query_scalar("SELECT client_id FROM sessions")
            .fetch_all(h.store.pool())
            .await?;
        ensure!(ids.len() == 1 && ids[0].starts_with(b"clex-"));
        client.disconnect().await?;
        let mut limited = Client {
            io: TcpStream::connect(h.address).await?,
            version: 5,
            session_present: false,
        };
        let mut body = Vec::new();
        string(&mut body, "MQTT");
        body.extend_from_slice(&[5, 0xc2, 0, 60]);
        property_section(&mut body, &[0x27, 0, 0, 0, 2]);
        string(&mut body, "small-limit");
        string(&mut body, "wire-test");
        string(&mut body, "wrong-password");
        limited.send(0x10, &body).await?;
        ensure!(
            timeout(Duration::from_secs(2), limited.io.read_u8())
                .await?
                .is_err(),
            "CONNACK must honor the client's packet size even on authentication failure"
        );
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
