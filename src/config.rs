use anyhow::{Context, ensure};
use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use url::Url;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub environment: String,
    pub mqtt_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub public_url: Url,
    pub cookie_secure: bool,
    pub http_trusted_proxy_ips: Vec<IpAddr>,
    pub database_connections: u32,
    pub registration_enabled: bool,
    pub max_accounts: i64,
    pub max_packet_size: usize,
    pub max_connections: usize,
    pub max_inflight: u16,
    pub max_pending: usize,
    pub max_pending_bytes: usize,
    pub tls_addr: Option<SocketAddr>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_client_auth: crate::tls::ClientAuth,
    pub tls_client_ca: Option<PathBuf>,
    pub shutdown_timeout: Duration,
    pub bridge_key: Option<crate::bridge::SecretKey>,
    pub metrics_token: Option<crate::metrics::TokenDigest>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let environment = value("CLEX_ENV", "development");
        ensure!(
            matches!(environment.as_str(), "development" | "test" | "production"),
            "CLEX_ENV must be development, test or production"
        );
        let production = environment == "production";
        let mqtt_addr: SocketAddr = parse("CLEX_MQTT_ADDR", "127.0.0.1:1883")?;
        let http_addr: SocketAddr = parse("CLEX_HTTP_ADDR", "127.0.0.1:8085")?;
        let public_url = Url::parse(&value("CLEX_PUBLIC_URL", "http://127.0.0.1:8085"))
            .context("CLEX_PUBLIC_URL must be a valid HTTP(S) URL")?;
        ensure!(
            matches!(public_url.scheme(), "http" | "https") && public_url.host_str().is_some(),
            "CLEX_PUBLIC_URL must be an absolute HTTP(S) URL"
        );
        ensure!(
            public_url.username().is_empty()
                && public_url.password().is_none()
                && public_url.query().is_none()
                && public_url.fragment().is_none()
                && public_url.path() == "/",
            "CLEX_PUBLIC_URL must contain an origin only"
        );
        let cookie_secure = parse(
            "CLEX_COOKIE_SECURE",
            if production { "true" } else { "false" },
        )?;
        let tls_addr = optional("CLEX_TLS_ADDR")
            .map(|s| s.parse())
            .transpose()
            .context("Invalid CLEX_TLS_ADDR")?;
        let tls_cert = optional("CLEX_TLS_CERT_FILE").map(PathBuf::from);
        let tls_key = optional("CLEX_TLS_KEY_FILE").map(PathBuf::from);
        let tls_client_auth = parse("CLEX_TLS_CLIENT_AUTH", "none")?;
        let tls_client_ca = optional("CLEX_TLS_CLIENT_CA_FILE").map(PathBuf::from);
        crate::tls::validate_client_auth(
            tls_client_auth,
            tls_addr.is_some(),
            tls_client_ca.is_some(),
        )?;
        ensure!(
            tls_addr.is_none() || (tls_cert.is_some() && tls_key.is_some()),
            "TLS listener requires both CLEX_TLS_CERT_FILE and CLEX_TLS_KEY_FILE"
        );
        if production {
            ensure!(
                cookie_secure && public_url.scheme() == "https",
                "Production administration requires HTTPS and secure cookies"
            );
            ensure!(
                http_addr.ip().is_loopback(),
                "Production HTTP must listen on loopback behind a HTTPS reverse proxy"
            );
            ensure!(
                mqtt_addr.ip().is_loopback()
                    || parse::<bool>("CLEX_ALLOW_PLAINTEXT_MQTT", "false")?,
                "Public plaintext MQTT requires explicit CLEX_ALLOW_PLAINTEXT_MQTT=true; prefer TLS"
            );
        }
        let config = Self {
            metrics_token: optional("CLEX_METRICS_TOKEN")
                .map(|token| crate::metrics::TokenDigest::parse(&token))
                .transpose()?,
            bridge_key: optional("CLEX_BRIDGE_SECRET_KEY")
                .map(|value| crate::bridge::SecretKey::parse(&value))
                .transpose()?,
            database_url: env::var("DATABASE_URL").context(
                "DATABASE_URL is required; run scripts/local_setup.py for local development",
            )?,
            environment,
            mqtt_addr,
            http_addr,
            public_url,
            cookie_secure,
            http_trusted_proxy_ips: trusted_proxy_ips(&value("CLEX_HTTP_TRUSTED_PROXY_IPS", ""))?,
            registration_enabled: parse("CLEX_REGISTRATION_ENABLED", "false")?,
            max_accounts: parse("CLEX_MAX_ACCOUNTS", "100")?,
            database_connections: parse("CLEX_DB_MAX_CONNECTIONS", "10")?,
            max_packet_size: parse("CLEX_MAX_PACKET_BYTES", "1048576")?,
            max_connections: parse("CLEX_MAX_CONNECTIONS", "1000")?,
            max_inflight: parse("CLEX_MAX_INFLIGHT", "32")?,
            max_pending: parse("CLEX_MAX_PENDING", "10000")?,
            max_pending_bytes: parse("CLEX_MAX_PENDING_BYTES", "67108864")?,
            tls_addr,
            tls_cert,
            tls_key,
            tls_client_auth,
            tls_client_ca,
            shutdown_timeout: Duration::from_secs(parse("CLEX_SHUTDOWN_TIMEOUT_SECS", "10")?),
        };
        ensure!(
            (2..=100).contains(&config.database_connections),
            "CLEX_DB_MAX_CONNECTIONS must be in 2..=100"
        );
        ensure!(
            (128..=268435455).contains(&config.max_packet_size),
            "Invalid CLEX_MAX_PACKET_BYTES"
        );
        ensure!(
            config.max_connections > 0 && config.max_inflight > 0 && config.max_pending > 0,
            "Connection and queue limits must be positive"
        );
        ensure!(
            config.max_pending_bytes > 0,
            "CLEX_MAX_PENDING_BYTES must be positive"
        );
        ensure!(
            (1..=1000).contains(&config.max_accounts),
            "CLEX_MAX_ACCOUNTS must be in 1..=1000"
        );
        Ok(config)
    }

    pub fn origin(&self) -> String {
        self.public_url.origin().ascii_serialization()
    }
}

fn value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}
fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}
fn parse<T: std::str::FromStr>(name: &str, default: &str) -> anyhow::Result<T> {
    value(name, default)
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid {name}"))
}

fn trusted_proxy_ips(value: &str) -> anyhow::Result<Vec<IpAddr>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let addresses: Result<Vec<IpAddr>, _> =
        value.split(',').map(|part| part.trim().parse()).collect();
    let addresses = addresses
        .context("CLEX_HTTP_TRUSTED_PROXY_IPS must contain comma-separated IP addresses")?;
    ensure!(
        addresses.len() <= 16,
        "At most 16 trusted proxy addresses are supported"
    );
    Ok(addresses
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(ip) => ip
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(ip)),
            ip => ip,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_proxies_require_explicit_ip_addresses() {
        assert!(trusted_proxy_ips("").unwrap().is_empty());
        assert_eq!(trusted_proxy_ips("127.0.0.1, ::1").unwrap().len(), 2);
        for invalid in ["localhost", "127.0.0.1/8", "*", "127.0.0.1,"] {
            assert!(trusted_proxy_ips(invalid).is_err());
        }
    }
}
