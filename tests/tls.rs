//! Real binary, real TLS handshakes and independent MQTT frames. All database
//! state lives in a random schema of TEST_DATABASE_URL; keys are temporary.
use anyhow::{Context, Result, ensure};
use clex_mqtt::{
    auth,
    domain::{CertificateBinding, SessionSpec},
    migrations,
    store::Store,
    tls::{CertificateInfo, ClientAuth},
};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    io::BufReader, net::SocketAddr, process::Stdio, str::FromStr, sync::Arc, time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    time::{sleep, timeout},
};
use tokio_rustls::TlsConnector;
use url::Url;
use uuid::Uuid;

#[path = "support/pki.rs"]
mod pki;
use pki::Pki;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
struct Wire {
    io: Box<dyn Io>,
}

fn binary(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}
fn string(out: &mut Vec<u8>, value: &str) {
    binary(out, value.as_bytes());
}

impl Wire {
    async fn send(&mut self, header: u8, body: &[u8]) -> Result<()> {
        let mut bytes = vec![header];
        let mut size = body.len();
        loop {
            let mut next = (size % 128) as u8;
            size /= 128;
            if size > 0 {
                next |= 128;
            }
            bytes.push(next);
            if size == 0 {
                break;
            }
        }
        bytes.extend_from_slice(body);
        self.io.write_all(&bytes).await?;
        Ok(())
    }
    async fn read(&mut self) -> Result<(u8, Vec<u8>)> {
        timeout(Duration::from_secs(3), async {
            let header = self.io.read_u8().await?;
            let mut length = 0;
            let mut multiplier = 1;
            for _ in 0..4 {
                let next = self.io.read_u8().await?;
                length += usize::from(next & 127) * multiplier;
                if next & 128 == 0 {
                    let mut body = vec![0; length];
                    self.io.read_exact(&mut body).await?;
                    return Ok((header, body));
                }
                multiplier *= 128;
            }
            anyhow::bail!("invalid MQTT length")
        })
        .await
        .context("TLS MQTT response timed out")?
    }
    async fn connect(
        &mut self,
        version: u8,
        id: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<u8> {
        let mut body = Vec::new();
        string(&mut body, if version == 3 { "MQIsdp" } else { "MQTT" });
        body.extend_from_slice(&[
            version,
            2 | if username.is_some() { 128 } else { 0 } | if password.is_some() { 64 } else { 0 },
            0,
            30,
        ]);
        if version == 5 {
            body.push(0);
        }
        string(&mut body, id);
        if let Some(username) = username {
            string(&mut body, username);
        }
        if let Some(password) = password {
            string(&mut body, password);
        }
        self.send(0x10, &body).await?;
        let (header, body) = self.read().await?;
        ensure!(header == 0x20 && body.len() >= 2);
        Ok(body[1])
    }
    async fn ping(&mut self) -> Result<()> {
        self.send(0xc0, &[]).await?;
        ensure!(self.read().await? == (0xd0, vec![]));
        Ok(())
    }
    async fn disconnect(mut self) -> Result<()> {
        self.send(0xe0, &[]).await?;
        self.io.shutdown().await?;
        Ok(())
    }
    async fn rejected(&mut self) -> Result<()> {
        let (header, body) = self.read().await?;
        ensure!(header == 0xe0 && body.first() == Some(&0x87));
        Ok(())
    }
}

struct Harness {
    root: PgPool,
    pool: PgPool,
    schema: String,
    database_url: Url,
    child: Option<Child>,
    mqtt: SocketAddr,
    tls: SocketAddr,
    http: String,
    client: Client,
    csrf: String,
    pki: Pki,
}

fn address() -> Result<SocketAddr> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

impl Harness {
    async fn new() -> Result<Self> {
        let pki = Pki::new()?;
        let value =
            std::env::var("TEST_DATABASE_URL").context("TLS tests require TEST_DATABASE_URL")?;
        let options = PgConnectOptions::from_str(&value)?;
        ensure!(
            options.get_database().unwrap_or_default().contains("test"),
            "Use a dedicated test database"
        );
        let root = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await?;
        let schema = format!("tls_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&root)
            .await?;
        let path = format!("SET search_path TO {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query(&path).execute(conn).await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        migrations::apply(&pool).await?;
        let hash = auth::hash_password("tls-test-password")?;
        sqlx::query("INSERT INTO admin_users(id,username,password_hash) VALUES($1,'tls-admin',$2)")
            .bind(Uuid::new_v4())
            .bind(&hash)
            .execute(&pool)
            .await?;
        for username in ["alpha-user", "other-user"] {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO mqtt_users(id,username,password_hash) VALUES($1,$2,$3)")
                .bind(id)
                .bind(username)
                .bind(&hash)
                .execute(&pool)
                .await?;
            sqlx::query("INSERT INTO mqtt_acl_rules(id,mqtt_user_id,filter,publish,subscribe) VALUES($1,$2,'tls/#',true,true)").bind(Uuid::new_v4()).bind(id).execute(&pool).await?;
        }
        let mut database_url = Url::parse(&value)?;
        database_url
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        Ok(Self {
            root,
            pool,
            schema,
            database_url,
            child: None,
            mqtt: address()?,
            tls: address()?,
            http: format!("http://{}", address()?),
            client: Client::builder()
                .cookie_store(true)
                .timeout(Duration::from_secs(4))
                .build()?,
            csrf: String::new(),
            pki,
        })
    }

    async fn start(&mut self, mode: ClientAuth) -> Result<()> {
        self.stop().await?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_clex-mqtt"));
        command
            .arg("serve")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("DATABASE_URL", self.database_url.as_str())
            .env("CLEX_ENV", "test")
            .env("CLEX_MQTT_ADDR", self.mqtt.to_string())
            .env("CLEX_TLS_ADDR", self.tls.to_string())
            .env(
                "CLEX_HTTP_ADDR",
                Url::parse(&self.http)?.socket_addrs(|| None)?[0].to_string(),
            )
            .env("CLEX_PUBLIC_URL", &self.http)
            .env("CLEX_TLS_CERT_FILE", self.pki.path("server.pem"))
            .env("CLEX_TLS_KEY_FILE", self.pki.path("server.key"))
            .env("CLEX_TLS_CLIENT_AUTH", mode.as_str())
            .env("CLEX_SHUTDOWN_TIMEOUT_SECS", "3")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if mode != ClientAuth::None {
            command.env("CLEX_TLS_CLIENT_CA_FILE", self.pki.path("ca.pem"));
        }
        self.child = Some(command.spawn()?);
        for _ in 0..100 {
            if self
                .client
                .get(format!("{}/health/ready", self.http))
                .send()
                .await
                .is_ok_and(|r| r.status() == StatusCode::OK)
            {
                let login: Value = self
                    .client
                    .post(format!("{}/api/v1/session", self.http))
                    .json(&json!({"username":"tls-admin","password":"tls-test-password"}))
                    .send()
                    .await?
                    .json()
                    .await?;
                self.csrf = login["csrfToken"]
                    .as_str()
                    .context("TLS test admin login")?
                    .to_owned();
                return Ok(());
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                anyhow::bail!("TLS test service exited during startup");
            }
            sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("TLS service startup timed out")
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.http))
            .header("Origin", &self.http)
            .header("X-CSRF-Token", &self.csrf)
    }
    async fn register(&self, user: &str, name: &str) -> Result<Uuid> {
        let response = self
            .request(Method::POST, &format!("/api/v1/users/{user}/certificates"))
            .json(&json!({"label":name,"certificatePem":self.pki.pem(name)?}))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["id"].as_str().context("certificate id")?.parse()?)
    }
    async fn tls_wire(&self, name: Option<&str>) -> Result<Wire> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in pki::certificates(&self.pki.path("ca.pem"))? {
            roots.add(cert)?;
        }
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots);
        let config = if let Some(name) = name {
            let key = rustls_pemfile::private_key(&mut BufReader::new(std::fs::File::open(
                self.pki.path(&format!("{name}.key")),
            )?))?
            .context("test key")?;
            builder.with_client_auth_cert(
                pki::certificates(&self.pki.path(&format!("{name}.pem")))?,
                key,
            )?
        } else {
            builder.with_no_client_auth()
        };
        let stream = TlsConnector::from(Arc::new(config))
            .connect(
                rustls::pki_types::ServerName::try_from("localhost")?,
                TcpStream::connect(self.tls).await?,
            )
            .await?;
        Ok(Wire {
            io: Box::new(stream),
        })
    }
    async fn plain(&self) -> Result<Wire> {
        Ok(Wire {
            io: Box::new(TcpStream::connect(self.mqtt).await?),
        })
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take()
            && child.try_wait()?.is_none()
        {
            #[cfg(unix)]
            {
                let status = Command::new("kill")
                    .arg("-TERM")
                    .arg(child.id().unwrap().to_string())
                    .status()
                    .await?;
                ensure!(status.success());
            }
            #[cfg(not(unix))]
            child.start_kill()?;
            timeout(Duration::from_secs(6), child.wait()).await??;
        }
        Ok(())
    }
    async fn close(mut self) -> Result<()> {
        self.stop().await?;
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.root)
            .await?;
        self.root.close().await;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL and OpenSSL"]
async fn binary_mtls_chain_binding_policy_rotation_and_revocation() -> Result<()> {
    let mut h = Harness::new().await?;
    let outcome: Result<()> = async {
        h.start(ClientAuth::Optional).await?;
        let url = format!("{}/api/v1/users/alpha-user/certificates", h.http);
        ensure!(Client::new().get(&url).send().await?.status() == StatusCode::UNAUTHORIZED);
        ensure!(
            h.client
                .post(&url)
                .json(&json!({"label":"alpha","certificatePem":h.pki.pem("alpha")?}))
                .send()
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        ensure!(
            h.request(Method::POST, "/api/v1/users/alpha-user/certificates")
                .header("Origin", "https://invalid.example")
                .json(&json!({"label":"alpha","certificatePem":h.pki.pem("alpha")?}))
                .send()
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        for name in ["ca", "server", "expired"] {
            ensure!(
                h.request(Method::POST, "/api/v1/users/alpha-user/certificates")
                    .json(&json!({"label":name,"certificatePem":h.pki.pem(name)?}))
                    .send()
                    .await?
                    .status()
                    == StatusCode::BAD_REQUEST
            );
        }
        let alpha = h.register("alpha-user", "alpha").await?;
        let beta = h.register("alpha-user", "beta").await?;
        h.register("other-user", "gamma").await?;
        ensure!(
            h.request(Method::POST, "/api/v1/users/other-user/certificates")
                .json(&json!({"label":"duplicate","certificatePem":h.pki.pem("alpha")?}))
                .send()
                .await?
                .status()
                == StatusCode::CONFLICT
        );
        let listed: Value = h
            .request(Method::GET, "/api/v1/users/alpha-user/certificates")
            .send()
            .await?
            .json()
            .await?;
        ensure!(listed["certificates"].as_array().unwrap().len() == 2);
        ensure!(
            !listed.to_string().contains("PRIVATE KEY")
                && !listed.to_string().contains("certificatePem")
        );
        for version in [3, 4, 5] {
            let mut wire = h.tls_wire(Some("alpha")).await?;
            ensure!(
                wire.connect(version, &format!("cert-v{version}"), None, None)
                    .await?
                    == 0
            );
            wire.ping().await?;
            wire.disconnect().await?;
        }
        for name in ["untrusted", "expired", "server"] {
            let result = async {
                let mut wire = h.tls_wire(Some(name)).await?;
                wire.connect(5, "invalid-leaf", None, None).await
            }
            .await;
            ensure!(result.is_err(), "TLS must reject {name}");
        }
        let mut unknown = h.tls_wire(Some("unregistered")).await?;
        ensure!(
            unknown
                .connect(
                    5,
                    "unregistered",
                    Some("alpha-user"),
                    Some("tls-test-password")
                )
                .await?
                == 0x87
        );
        let mut switched = h.tls_wire(Some("alpha")).await?;
        ensure!(
            switched
                .connect(5, "switched", Some("other-user"), Some("tls-test-password"))
                .await?
                == 0x87
        );
        let mut password = h.tls_wire(None).await?;
        ensure!(
            password
                .connect(
                    5,
                    "optional-password",
                    Some("other-user"),
                    Some("tls-test-password")
                )
                .await?
                == 0
        );
        password.disconnect().await?;
        ensure!(
            h.request(Method::PATCH, "/api/v1/users/alpha-user")
                .json(&json!({"requireClientCertificate":true}))
                .send()
                .await?
                .status()
                == StatusCode::OK
        );
        let mut plain = h.plain().await?;
        ensure!(
            plain
                .connect(
                    5,
                    "plain-bypass",
                    Some("alpha-user"),
                    Some("tls-test-password")
                )
                .await?
                == 0x87
        );
        let mut no_cert = h.tls_wire(None).await?;
        ensure!(
            no_cert
                .connect(
                    5,
                    "tls-bypass",
                    Some("alpha-user"),
                    Some("tls-test-password")
                )
                .await?
                == 0x87
        );
        let mut left = h.tls_wire(Some("alpha")).await?;
        ensure!(left.connect(5, "alpha-live", None, None).await? == 0);
        let mut rotated = h.tls_wire(Some("beta")).await?;
        ensure!(rotated.connect(5, "beta-live", None, None).await? == 0);
        let mut other = h.tls_wire(Some("gamma")).await?;
        ensure!(other.connect(5, "other-live", None, None).await? == 0);
        // Exercise actual subscribe/publish after certificate-only authentication.
        let mut sub = vec![0, 1, 0];
        string(&mut sub, "tls/value");
        sub.push(1);
        left.send(0x82, &sub).await?;
        ensure!(left.read().await?.0 == 0x90);
        let mut publication = Vec::new();
        string(&mut publication, "tls/value");
        publication.extend_from_slice(&[0, 2, 0, 0, 128, 255]);
        rotated.send(0x32, &publication).await?;
        ensure!(rotated.read().await?.0 == 0x40);
        let (header, body) = left.read().await?;
        ensure!(header == 0x32 && body.ends_with(&[0, 128, 255]));
        ensure!(
            h.request(
                Method::PATCH,
                &format!("/api/v1/users/alpha-user/certificates/{alpha}")
            )
            .json(&json!({"enabled":false}))
            .send()
            .await?
            .status()
                == StatusCode::OK
        );
        left.rejected().await?;
        rotated.ping().await?;
        other.ping().await?;
        let mut revoked = h.tls_wire(Some("alpha")).await?;
        ensure!(revoked.connect(5, "revoked", None, None).await? == 0x87);
        ensure!(
            h.request(
                Method::PATCH,
                &format!("/api/v1/users/alpha-user/certificates/{alpha}")
            )
            .json(&json!({"enabled":true}))
            .send()
            .await?
            .status()
                == StatusCode::OK
        );
        let mut restored = h.tls_wire(Some("alpha")).await?;
        ensure!(restored.connect(5, "restored", None, None).await? == 0);
        restored.disconnect().await?;
        ensure!(
            h.request(
                Method::DELETE,
                &format!("/api/v1/users/alpha-user/certificates/{beta}")
            )
            .send()
            .await?
            .status()
                == StatusCode::NO_CONTENT
        );
        rotated.rejected().await?;
        other.ping().await?;
        other.disconnect().await?;
        let store = Store::from_pool(h.pool.clone());
        let credential = store.credential("alpha-user").await?.unwrap();
        let spec = || SessionSpec {
            client_id: b"fenced-certificate".to_vec(),
            principal: "alpha-user".into(),
            clean_start: true,
            expiry_interval: 0,
            protocol: 5,
            will: None,
            will_delay_interval: 0,
        };
        ensure!(
            store
                .open_session_authenticated(spec(), credential.id, &credential.password_hash)
                .await
                .is_err()
        );
        let info = CertificateInfo::from_pem(&h.pki.pem("alpha")?)?;
        let binding = CertificateBinding {
            id: alpha,
            fingerprint: info.fingerprint.to_vec(),
            expires_at: info.expires_at,
        };
        sqlx::query("UPDATE mqtt_client_certificates SET enabled=false WHERE id=$1")
            .bind(alpha)
            .execute(&h.pool)
            .await?;
        ensure!(
            store
                .open_session_verified(
                    spec(),
                    credential.id,
                    &credential.password_hash,
                    Some(&binding)
                )
                .await
                .is_err()
        );
        let audit: Value = h
            .request(Method::GET, "/api/v1/audit")
            .send()
            .await?
            .json()
            .await?;
        ensure!(
            audit
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["action"] == "client_certificate.delete")
        );
        h.start(ClientAuth::Required).await?;
        let result = async {
            let mut wire = h.tls_wire(None).await?;
            wire.connect(
                5,
                "required-no-cert",
                Some("other-user"),
                Some("tls-test-password"),
            )
            .await
        }
        .await;
        ensure!(result.is_err());
        let mut strict = h.tls_wire(Some("gamma")).await?;
        ensure!(strict.connect(5, "required-valid", None, None).await? == 0);
        strict.disconnect().await?;
        h.start(ClientAuth::None).await?;
        let mut normal = h.tls_wire(None).await?;
        ensure!(
            normal
                .connect(
                    5,
                    "normal-tls",
                    Some("other-user"),
                    Some("tls-test-password")
                )
                .await?
                == 0
        );
        normal.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
