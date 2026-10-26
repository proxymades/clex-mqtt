use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

pub fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn csrf_token(session_token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"clex-mqtt:csrf:v1:");
    hash.update(session_token.as_bytes());
    URL_SAFE_NO_PAD.encode(hash.finalize())
}

pub fn validate_password(password: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        (12..=256).contains(&password.len()),
        "Password must contain 12-256 bytes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_is_salted_and_verifiable() {
        let first = hash_password("local-test-password").unwrap();
        let second = hash_password("local-test-password").unwrap();
        assert_ne!(first, second);
        assert!(verify_password("local-test-password", &first));
        assert!(!verify_password("wrong-password", &first));
        assert!(!verify_password("local-test-password", "invalid"));
    }

    #[test]
    fn session_and_csrf_tokens_are_distinct() {
        let first = random_token();
        let second = random_token();
        assert_ne!(first, second);
        assert_ne!(first, csrf_token(&first));
        assert_eq!(csrf_token(&first), csrf_token(&first));
        assert_ne!(csrf_token(&first), csrf_token(&second));
        assert_eq!(token_hash(&first).len(), 32);
    }
}
