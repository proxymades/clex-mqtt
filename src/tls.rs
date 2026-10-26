//! TLS verification stays inside rustls; MQTT identity is bound to the exact
//! SHA-256 fingerprint of the verified leaf, rather than its chosen CN.
use std::{io::BufReader, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rustls::{RootCertStore, pki_types::CertificateDer, server::WebPkiClientVerifier};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;
use x509_parser::prelude::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientAuth {
    #[default]
    None,
    Optional,
    Required,
}

impl std::str::FromStr for ClientAuth {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "optional" => Ok(Self::Optional),
            "required" => Ok(Self::Required),
            _ => Err("TLS client auth must be none, optional or required"),
        }
    }
}

impl ClientAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

pub fn validate_client_auth(mode: ClientAuth, listener: bool, ca: bool) -> Result<()> {
    ensure!(
        mode == ClientAuth::None || listener && ca,
        "TLS client authentication requires CLEX_TLS_ADDR and CLEX_TLS_CLIENT_CA_FILE"
    );
    ensure!(
        mode != ClientAuth::None || !ca,
        "Set CLEX_TLS_CLIENT_AUTH to optional or required when configuring a client CA"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub struct CertificateInfo {
    pub fingerprint: [u8; 32],
    pub subject: String,
    pub expires_at: DateTime<Utc>,
}

impl CertificateInfo {
    fn from_der(der: &[u8]) -> Result<Self> {
        ensure!(
            der.len() <= 12 * 1024,
            "client certificate exceeds size limit"
        );
        let (remaining, certificate) = parse_x509_certificate(der)
            .map_err(|_| anyhow::anyhow!("invalid X.509 client certificate"))?;
        ensure!(
            remaining.is_empty() && certificate.validity().is_valid(),
            "client certificate is outside its validity period"
        );
        ensure!(
            !certificate.is_ca(),
            "a client leaf certificate is required"
        );
        if let Some(usage) = certificate
            .extended_key_usage()
            .map_err(|_| anyhow::anyhow!("invalid extended key usage"))?
        {
            ensure!(
                usage.value.client_auth || usage.value.any,
                "certificate must allow TLS client authentication"
            );
        }
        let expires_at = DateTime::from_timestamp(certificate.validity().not_after.timestamp(), 0)
            .context("invalid client certificate expiry")?;
        let subject = certificate.subject().to_string();
        ensure!(subject.len() <= 1024, "certificate subject exceeds limit");
        Ok(Self {
            fingerprint: Sha256::digest(der).into(),
            subject,
            expires_at,
        })
    }

    pub fn from_pem(pem: &str) -> Result<Self> {
        ensure!(
            pem.len() <= 12 * 1024,
            "client certificate PEM exceeds limit"
        );
        let items = rustls_pemfile::read_all(&mut BufReader::new(pem.as_bytes()))
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(items.len() == 1, "provide one public client certificate");
        let rustls_pemfile::Item::X509Certificate(certificate) = &items[0] else {
            anyhow::bail!("provide a public client certificate")
        };
        Self::from_der(certificate.as_ref())
    }
}

/// Created only from a completed rustls connection that verified client auth.
#[derive(Clone, Debug)]
pub struct VerifiedClientCertificate {
    pub(crate) info: CertificateInfo,
}

pub fn verified_client(
    connection: &rustls::ServerConnection,
) -> Result<Option<VerifiedClientCertificate>> {
    ensure!(
        !connection.is_handshaking(),
        "TLS handshake must complete before MQTT authentication"
    );
    connection
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|certificate| {
            CertificateInfo::from_der(certificate.as_ref())
                .map(|info| VerifiedClientCertificate { info })
        })
        .transpose()
}

pub fn fingerprint_text(fingerprint: &[u8]) -> String {
    fingerprint
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn acceptor(
    cert: &Path,
    key: &Path,
    mode: ClientAuth,
    client_ca: Option<&Path>,
) -> Result<TlsAcceptor> {
    validate_client_auth(mode, true, client_ca.is_some())?;
    let certificates = read_certificates(cert)?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(
        std::fs::File::open(key).context("Could not open TLS private key")?,
    ))?
    .context("TLS private key missing")?;
    let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?;
    let builder = if mode == ClientAuth::None {
        builder.with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        for certificate in read_certificates(client_ca.context("Client CA missing")?)? {
            roots.add(certificate)?;
        }
        let verifier = WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        );
        let verifier = if mode == ClientAuth::Optional {
            verifier.allow_unauthenticated()
        } else {
            verifier
        };
        builder.with_client_cert_verifier(verifier.build()?)
    };
    let mut config = builder.with_single_cert(certificates, key)?;
    // Every connection rechecks the chain and fingerprint; TLS resumption must
    // not retain an old certificate authorization across account revocation.
    if mode != ClientAuth::None {
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
    }
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = rustls_pemfile::certs(&mut BufReader::new(
        std::fs::File::open(path).context("Could not open TLS certificate file")?,
    ))
    .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(!certificates.is_empty(), "TLS certificate file is empty");
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_require_an_explicit_listener_and_ca() {
        assert_eq!(
            "optional".parse::<ClientAuth>().unwrap(),
            ClientAuth::Optional
        );
        assert!("invalid".parse::<ClientAuth>().is_err());
        for mode in [ClientAuth::Optional, ClientAuth::Required] {
            assert!(validate_client_auth(mode, true, true).is_ok());
            assert!(validate_client_auth(mode, true, false).is_err());
            assert!(validate_client_auth(mode, false, true).is_err());
        }
        assert!(validate_client_auth(ClientAuth::None, true, false).is_ok());
        assert!(validate_client_auth(ClientAuth::None, true, true).is_err());
    }
}
