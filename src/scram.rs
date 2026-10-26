//! SCRAM-SHA-256 (RFC 5802/7677). Crypto primitives are provided by ring.
//! Only the salt, iteration count, StoredKey and ServerKey are persisted.
use std::{collections::BTreeMap, num::NonZeroU32};

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use ring::{hmac, pbkdf2};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const METHOD: &str = "SCRAM-SHA-256";
pub const ITERATIONS: u32 = 16_384;
const MAX_MESSAGE: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verifier {
    salt: Vec<u8>,
    iterations: u32,
    stored_key: [u8; 32],
    server_key: [u8; 32],
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verifier([redacted])")
    }
}

impl Verifier {
    pub fn from_password(password: &str) -> Result<Self> {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        Self::derive(password, &salt, ITERATIONS)
    }

    fn derive(password: &str, salt: &[u8], iterations: u32) -> Result<Self> {
        let password = stringprep::saslprep(password)
            .map_err(|_| anyhow::anyhow!("password cannot be prepared for SCRAM"))?;
        ensure!(!password.is_empty(), "empty prepared SCRAM password");
        let mut salted = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(iterations).unwrap(),
            salt,
            password.as_bytes(),
            &mut salted,
        );
        let client_key = mac(&salted, b"Client Key");
        Ok(Self {
            salt: salt.to_vec(),
            iterations,
            stored_key: Sha256::digest(client_key).into(),
            server_key: mac(&salted, b"Server Key"),
        })
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            (8..=64).contains(&self.salt.len()) && (4096..=1_000_000).contains(&self.iterations),
            "invalid SCRAM verifier"
        );
        Ok(())
    }
}

fn mac(key: &[u8], data: &[u8]) -> [u8; 32] {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .try_into()
        .unwrap()
}

fn message(data: &[u8]) -> Result<&str> {
    ensure!(data.len() <= MAX_MESSAGE, "SCRAM message exceeds limit");
    let text = std::str::from_utf8(data)?;
    ensure!(!text.contains('\0'), "invalid SCRAM text");
    Ok(text)
}

fn attributes(text: &str) -> Result<Vec<(char, &str)>> {
    let mut seen = BTreeMap::new();
    let mut result = Vec::new();
    for item in text.split(',') {
        let bytes = item.as_bytes();
        ensure!(
            bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'=',
            "invalid SCRAM attribute"
        );
        let key = bytes[0] as char;
        ensure!(key != 'm', "mandatory SCRAM extension is unsupported");
        ensure!(seen.insert(key, ()).is_none(), "duplicate SCRAM attribute");
        result.push((key, &item[2..]));
    }
    Ok(result)
}

fn name(text: &str) -> Result<String> {
    let mut decoded = String::new();
    let mut rest = text;
    while let Some(index) = rest.find('=') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index..];
        let (value, consumed) = if rest.starts_with("=2C") {
            (',', 3)
        } else if rest.starts_with("=3D") {
            ('=', 3)
        } else {
            anyhow::bail!("invalid SCRAM username escape")
        };
        decoded.push(value);
        rest = &rest[consumed..];
    }
    decoded.push_str(rest);
    let prepared =
        stringprep::saslprep(&decoded).map_err(|_| anyhow::anyhow!("invalid SCRAM username"))?;
    ensure!(
        (1..=128).contains(&prepared.len()),
        "invalid SCRAM username length"
    );
    Ok(prepared.into_owned())
}

pub struct ClientFirst {
    pub username: String,
    gs2: String,
    bare: String,
    nonce: String,
}

impl ClientFirst {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let text = message(data)?;
        let mut parts = text.splitn(3, ',');
        let flag = parts.next().unwrap_or_default();
        ensure!(
            matches!(flag, "n" | "y"),
            "unsupported SCRAM channel binding"
        );
        let authorization = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing GS2 header"))?;
        let bare = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing SCRAM client first"))?;
        let attrs = attributes(bare)?;
        ensure!(
            attrs.len() >= 2 && attrs[0].0 == 'n' && attrs[1].0 == 'r',
            "invalid SCRAM client first order"
        );
        let username = name(attrs[0].1)?;
        if !authorization.is_empty() {
            let authzid = authorization
                .strip_prefix("a=")
                .ok_or_else(|| anyhow::anyhow!("invalid SCRAM authorization identity"))?;
            ensure!(
                name(authzid)? == username,
                "SCRAM authorization identity differs"
            );
        }
        let nonce = attrs[1].1;
        ensure!(
            (1..=1024).contains(&nonce.len()) && nonce.bytes().all(|b| (0x21..=0x7e).contains(&b)),
            "invalid SCRAM client nonce"
        );
        Ok(Self {
            username,
            gs2: format!("{flag},{authorization},"),
            bare: bare.to_owned(),
            nonce: nonce.to_owned(),
        })
    }

    pub fn start(self, verifier: Verifier) -> Result<Exchange> {
        self.start_with_nonce(verifier, &crate::auth::random_token())
    }

    fn start_with_nonce(self, verifier: Verifier, suffix: &str) -> Result<Exchange> {
        verifier.validate()?;
        let nonce = format!("{}{suffix}", self.nonce);
        let server_first = format!(
            "r={nonce},s={},i={}",
            STANDARD.encode(&verifier.salt),
            verifier.iterations
        );
        Ok(Exchange {
            verifier,
            gs2: self.gs2,
            client_first: self.bare,
            nonce,
            server_first,
        })
    }
}

pub struct Exchange {
    verifier: Verifier,
    gs2: String,
    client_first: String,
    nonce: String,
    server_first: String,
}

impl Exchange {
    pub fn challenge(&self) -> &str {
        &self.server_first
    }

    /// Consuming the exchange prevents a second use of a successful proof.
    pub fn finish(self, data: &[u8]) -> Result<String> {
        let text = message(data)?;
        let attrs = attributes(text)?;
        ensure!(
            attrs.len() >= 3
                && attrs[0].0 == 'c'
                && attrs[1].0 == 'r'
                && attrs.last().unwrap().0 == 'p',
            "invalid SCRAM client final order"
        );
        ensure!(
            STANDARD.decode(attrs[0].1)? == self.gs2.as_bytes(),
            "SCRAM channel binding differs"
        );
        ensure!(attrs[1].1 == self.nonce, "SCRAM nonce differs");
        let proof: [u8; 32] = STANDARD
            .decode(attrs.last().unwrap().1)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid SCRAM proof length"))?;
        let (without_proof, _) = text.rsplit_once(",p=").unwrap();
        let auth_message = format!(
            "{},{},{without_proof}",
            self.client_first, self.server_first
        );
        let signature = mac(&self.verifier.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; 32];
        for i in 0..32 {
            client_key[i] = proof[i] ^ signature[i];
        }
        let candidate: [u8; 32] = Sha256::digest(client_key).into();
        ensure!(
            bool::from(candidate.ct_eq(&self.verifier.stored_key)),
            "invalid SCRAM proof"
        );
        Ok(format!(
            "v={}",
            STANDARD.encode(mac(&self.verifier.server_key, auth_message.as_bytes()))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7677_known_answer_and_server_signature() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let verifier = Verifier::derive("pencil", &salt, 4096).unwrap();
        let exchange = ClientFirst::parse(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO")
            .unwrap()
            .start_with_nonce(verifier, "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0")
            .unwrap();
        assert_eq!(
            exchange.challenge(),
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
        );
        assert_eq!(exchange.finish(b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=").unwrap(), "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
    }

    #[test]
    fn malformed_transcripts_and_authorization_switches_are_rejected() {
        for data in [
            "n,,n=user,r=nonce,r=again",
            "n,,m=mandatory,n=user,r=nonce",
            "p=tls-exporter,,n=user,r=nonce",
            "n,a=other,n=user,r=nonce",
            "n,,n=bad=2c,r=nonce",
            "n,,n=user,r=bad nonce",
        ] {
            assert!(ClientFirst::parse(data.as_bytes()).is_err(), "{data}");
        }
        let verifier = Verifier::from_password("a-test-password").unwrap();
        for data in [
            "c=biws,r=wrong,p=AAAA",
            "c=eSws,r=nonce,p=AAAA",
            "c=biws,r=nonce,p=AAAA,p=AAAA",
        ] {
            let exchange = ClientFirst::parse(b"n,,n=user,r=nonce")
                .unwrap()
                .start(verifier.clone())
                .unwrap();
            assert!(exchange.finish(data.as_bytes()).is_err());
        }
    }

    #[test]
    fn salts_are_random_passwords_are_normalized_and_debug_is_redacted() {
        let first = Verifier::from_password("pass\u{00a0}word12").unwrap();
        let second = Verifier::from_password("pass\u{00a0}word12").unwrap();
        assert_ne!(first.salt, second.salt);
        let normalized = Verifier::derive("pass word12", &first.salt, ITERATIONS).unwrap();
        assert_eq!(first.stored_key, normalized.stored_key);
        assert_eq!(format!("{first:?}"), "Verifier([redacted])");
        assert!(Verifier::from_password("bad\0password").is_err());
    }
}
