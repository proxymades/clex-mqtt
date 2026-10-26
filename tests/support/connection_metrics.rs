use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn connection_metrics_distinguish_capacity_rejection_from_authentication() -> Result<()> {
    let h = Harness::start_with(BrokerConfig {
        max_connections: 1,
        ..Default::default()
    })
    .await?
    .context("broker")?;
    let outcome: Result<()> = async {
        let mut first = h.client(5, "first", true).await?;
        ensure!(h.client(5, "over-capacity", true).await.is_err());
        let metrics = h.broker.metrics();
        ensure!(
            metrics.connections_total == 1
                && metrics.connections_authenticated == 1
                && metrics.capacity_rejections == 1
                && metrics.handshake_failures == 0
        );
        first.send(0xc0, &[]).await?;
        ensure!(first.read().await?.0 == 0xd0);
        first.disconnect().await?;
        timeout(Duration::from_secs(2), async {
            loop {
                if h.broker.clients().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let second = h.client(5, "after-close", true).await?;
        ensure!(h.broker.metrics().connections_authenticated == 2);
        second.disconnect().await?;
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
