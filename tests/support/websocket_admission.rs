use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn upgrade(base: &str, ip: &str) -> Result<(tokio::net::TcpStream, String)> {
    let url = url::Url::parse(base)?;
    let mut stream =
        tokio::net::TcpStream::connect(("127.0.0.1", url.port().context("port")?)).await?;
    let request = format!(
        "GET /mqtt HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: mqtt\r\nX-Real-IP: {ip}\r\n\r\n",
        url.host_str().context("host")?
    );
    stream.write_all(request.as_bytes()).await?;
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !reply.ends_with(b"\r\n\r\n") {
            ensure!(reply.len() < 8192);
            reply.push(stream.read_u8().await?);
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    Ok((stream, String::from_utf8(reply)?))
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn websocket_admission_uses_trusted_source_and_releases_aborted_upgrades() -> Result<()> {
    let h = Harness::start_with_proxy(false, true).await?;
    let outcome: Result<()> = async {
        let (first, reply) = upgrade(&h.url, "192.0.2.1").await?;
        ensure!(reply.starts_with("HTTP/1.1 101"));
        let (second, reply) = upgrade(&h.url, "192.0.2.1").await?;
        ensure!(reply.starts_with("HTTP/1.1 101"));
        let (_third, reply) = upgrade(&h.url, "192.0.2.1").await?;
        ensure!(reply.starts_with("HTTP/1.1 429"));
        let (other, reply) = upgrade(&h.url, "192.0.2.2").await?;
        ensure!(reply.starts_with("HTTP/1.1 101"));
        ensure!(h.broker.metrics().handshake_capacity_rejections == 1);
        drop(first);
        drop(second);
        drop(other);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (candidate, reply) = upgrade(&h.url, "192.0.2.1").await?;
                if reply.starts_with("HTTP/1.1 101") {
                    drop(candidate);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn websocket_spoofed_headers_cannot_rotate_an_untrusted_source_budget() -> Result<()> {
    let h = Harness::start_with_proxy(false, false).await?;
    let outcome: Result<()> = async {
        let (first, reply) = upgrade(&h.url, "192.0.2.10").await?;
        ensure!(reply.starts_with("HTTP/1.1 101"));
        let (second, reply) = upgrade(&h.url, "192.0.2.11").await?;
        ensure!(reply.starts_with("HTTP/1.1 101"));
        let (_third, reply) = upgrade(&h.url, "192.0.2.12").await?;
        ensure!(reply.starts_with("HTTP/1.1 429"));
        drop(first);
        drop(second);
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
