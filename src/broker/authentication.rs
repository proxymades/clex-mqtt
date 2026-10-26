use super::*;
use crate::{domain::Credential, scram};

pub(super) struct Authentication {
    pub credential: Credential,
    pub reply: Option<String>,
    pub certificate: Option<crate::domain::CertificateBinding>,
}

pub(super) struct Reauthentication {
    exchange: scram::Exchange,
    pub deadline: Instant,
}

impl Broker {
    pub(super) async fn authenticate_connect<T>(
        &self,
        wire: &mut Wire<T>,
        info: &ConnectInfo,
        peer_certificate: Option<&crate::tls::VerifiedClientCertificate>,
    ) -> Result<Authentication>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if info
            .auth_method
            .as_deref()
            .is_some_and(|method| method != scram::METHOD)
        {
            self.connack(wire, false, 0x8c, None).await?;
            anyhow::bail!("unsupported MQTT authentication method");
        }
        let first = if info.auth_method.is_some() {
            let parsed = scram::ClientFirst::parse(info.auth_data.as_deref().unwrap_or_default());
            let Ok(first) = parsed else {
                self.connack(wire, false, 0x86, None).await?;
                anyhow::bail!("invalid MQTT authentication data");
            };
            if info
                .username
                .as_ref()
                .is_some_and(|username| username != &first.username)
            {
                self.connack(wire, false, 0x86, None).await?;
                anyhow::bail!("MQTT authentication identities differ");
            }
            Some(first)
        } else {
            None
        };
        let mapped = if let Some(certificate) = peer_certificate {
            let Some(mapped) = self.store.certificate_credential(certificate).await? else {
                self.connack(wire, false, 0x87, None).await?;
                anyhow::bail!("client certificate is not authorized");
            };
            Some(mapped)
        } else {
            None
        };
        let username = first
            .as_ref()
            .map(|first| first.username.as_str())
            .or(info.username.as_deref())
            .or_else(|| {
                mapped
                    .as_ref()
                    .map(|(credential, _)| credential.username.as_str())
            });
        let Some(username) = username.filter(|username| valid_utf8_field(username)) else {
            self.connack(wire, false, 0x86, None).await?;
            anyhow::bail!("MQTT authentication required");
        };
        let Some(credential) = self
            .store
            .credential(username)
            .await?
            .filter(|user| user.enabled)
        else {
            self.connack(wire, false, 0x86, None).await?;
            anyhow::bail!("MQTT authentication failed");
        };
        let certificate = if let Some((mapped, binding)) = mapped {
            if mapped.id != credential.id {
                self.connack(wire, false, 0x87, None).await?;
                anyhow::bail!("client certificate identity differs");
            }
            Some(binding)
        } else {
            None
        };
        if credential.require_client_certificate && certificate.is_none() {
            self.connack(wire, false, 0x87, None).await?;
            anyhow::bail!("client certificate required");
        }
        if credential
            .bound_client_id
            .as_ref()
            .is_some_and(|id| id != info.client_id.as_bytes())
        {
            self.connack(wire, false, 0x87, None).await?;
            anyhow::bail!("client identifier is not authorized");
        }
        if let Some(first) = first {
            let Some(verifier) = credential.scram_verifier.clone() else {
                self.connack(wire, false, 0x86, None).await?;
                anyhow::bail!("SCRAM credential requires password preparation");
            };
            let exchange = first.start(verifier)?;
            let deadline = Instant::now() + self.config.handshake_timeout;
            self.send_auth(
                wire,
                v5::AuthReasonCode::ContinueAuth,
                exchange.challenge(),
                None,
            )
            .await?;
            let before = wire.codec().bytes_received;
            let incoming = tokio::time::timeout_at(deadline, wire.next())
                .await
                .context("MQTT authentication exchange timed out")?
                .ok_or_else(|| anyhow!("connection closed during authentication"))?;
            let packet = match incoming {
                Ok(packet) => packet,
                Err(error) => {
                    let reason = u8::from(protocol::disconnect_reason(&error));
                    let _ = self.connack(wire, false, reason, None).await;
                    return Err(error.into());
                }
            };
            self.counters
                .bytes_received
                .fetch_add(wire.codec().bytes_received - before, Ordering::Relaxed);
            self.counters
                .messages_received
                .fetch_add(1, Ordering::Relaxed);
            if matches!(packet, MqttPacket::V5(v5::Packet::Disconnect(_))) {
                anyhow::bail!("client cancelled MQTT authentication");
            }
            let MqttPacket::V5(v5::Packet::Auth(auth)) = packet else {
                self.connack(wire, false, 0x82, None).await?;
                anyhow::bail!("expected AUTH during MQTT authentication");
            };
            if auth.reason_code != v5::AuthReasonCode::ContinueAuth
                || auth.auth_method.as_deref() != Some(scram::METHOD)
            {
                self.connack(wire, false, 0x82, None).await?;
                anyhow::bail!("invalid MQTT authentication sequence");
            }
            let Ok(reply) = exchange.finish(auth.auth_data.as_deref().unwrap_or_default()) else {
                self.connack(wire, false, 0x86, None).await?;
                anyhow::bail!("MQTT authentication failed");
            };
            return Ok(Authentication {
                credential,
                reply: Some(reply),
                certificate,
            });
        }
        if certificate.is_some() && info.password.is_none() {
            return Ok(Authentication {
                credential,
                reply: None,
                certificate,
            });
        }
        let Some(password) = info
            .password
            .as_deref()
            .and_then(|value| std::str::from_utf8(value).ok())
        else {
            self.connack(wire, false, 0x86, None).await?;
            anyhow::bail!("MQTT authentication required");
        };
        let permit = timeout(
            self.config.handshake_timeout,
            self.auth.clone().acquire_owned(),
        )
        .await
        .context("authentication queue timeout")??;
        let password = password.to_owned();
        let hash = credential.password_hash.clone();
        let upgrade = credential.scram_verifier.is_none();
        let (verified, verifier) = timeout(
            self.config.handshake_timeout,
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let verified = crate::auth::verify_password(&password, &hash);
                let verifier = if verified && upgrade {
                    scram::Verifier::from_password(&password).ok()
                } else {
                    None
                };
                (verified, verifier)
            }),
        )
        .await
        .context("authentication timeout")??;
        if !verified {
            self.connack(wire, false, 0x86, None).await?;
            anyhow::bail!("MQTT authentication failed");
        }
        if let Some(verifier) = verifier {
            self.store.enable_scram(&credential, verifier).await?;
        }
        Ok(Authentication {
            credential,
            reply: None,
            certificate,
        })
    }

    async fn send_auth<T>(
        &self,
        wire: &mut Wire<T>,
        reason: v5::AuthReasonCode,
        data: &str,
        connection: Option<&Connection>,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.send(
            wire,
            MqttPacket::V5(v5::Packet::Auth(v5::Auth {
                reason_code: reason,
                auth_method: Some(scram::METHOD.into()),
                auth_data: Some(data.as_bytes().to_vec().into()),
                ..Default::default()
            })),
            connection,
        )
        .await
    }

    pub(super) async fn reauthenticate<T>(
        &self,
        wire: &mut Wire<T>,
        connection: &mut Connection,
        auth: v5::Auth,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let continuing = auth.reason_code == v5::AuthReasonCode::ContinueAuth;
        let starting = auth.reason_code == v5::AuthReasonCode::ReAuth;
        if !connection.enhanced_auth
            || auth.auth_method.as_deref() != Some(scram::METHOD)
            || !(starting && connection.reauthentication.is_none()
                || continuing && connection.reauthentication.is_some())
        {
            self.send_disconnect(wire, v5::DisconnectReasonCode::ProtocolError)
                .await;
            anyhow::bail!("invalid MQTT reauthentication sequence");
        }
        let result: Result<()> = async {
            ensure!(
                self.credentials_current(connection).await?,
                "MQTT credential changed during reauthentication"
            );
            let data = auth.auth_data.as_deref().unwrap_or_default();
            if starting {
                let first = scram::ClientFirst::parse(data)?;
                ensure!(
                    first.username == connection.username,
                    "MQTT reauthentication identity differs"
                );
                let verifier = self
                    .store
                    .credential(&connection.username)
                    .await?
                    .and_then(|credential| credential.scram_verifier)
                    .ok_or_else(|| anyhow!("SCRAM credential unavailable"))?;
                let exchange = first.start(verifier)?;
                let deadline = Instant::now() + self.config.handshake_timeout;
                self.send_auth(
                    wire,
                    v5::AuthReasonCode::ContinueAuth,
                    exchange.challenge(),
                    Some(connection),
                )
                .await?;
                connection.reauthentication = Some(Reauthentication { exchange, deadline });
            } else {
                let pending = connection.reauthentication.take().unwrap();
                ensure!(
                    Instant::now() < pending.deadline,
                    "MQTT reauthentication timed out"
                );
                let reply = pending.exchange.finish(data)?;
                ensure!(
                    self.credentials_current(connection).await?,
                    "MQTT credential changed during reauthentication"
                );
                self.send_auth(wire, v5::AuthReasonCode::Success, &reply, Some(connection))
                    .await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            self.send_disconnect(wire, v5::DisconnectReasonCode::NotAuthorized)
                .await;
        }
        result
    }
}
