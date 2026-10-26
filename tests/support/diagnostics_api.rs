use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL TEST_DATABASE_URL"]
async fn diagnostics_requires_admin_csrf_origin_and_preserves_stopped_results() -> Result<()> {
    let h = Harness::start().await?;
    let outcome: Result<()> = async {
        let client = Client::builder().cookie_store(true).build()?;
        let path = format!("{}/api/v1/diagnostics", h.url);
        let body = json!({"username":"ivan","clientId":"esp32-01","topic":"trace/#","seconds":60});
        ensure!(client.get(&path).send().await?.status() == StatusCode::UNAUTHORIZED);
        ensure!(client.post(&path).json(&body).send().await?.status() == StatusCode::UNAUTHORIZED);
        let login: Value = client
            .post(format!("{}/api/v1/session", h.url))
            .json(&json!({"username":"test-admin","password":"test-admin-password"}))
            .send()
            .await?
            .json()
            .await?;
        let csrf = login["csrfToken"].as_str().context("CSRF")?;
        ensure!(client.post(&path).json(&body).send().await?.status() == StatusCode::FORBIDDEN);
        ensure!(
            client
                .post(&path)
                .header("Origin", "https://invalid.example")
                .header("X-CSRF-Token", csrf)
                .json(&body)
                .send()
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        let request = |method, suffix: &str| {
            client
                .request(method, format!("{path}{suffix}"))
                .header("Origin", &h.url)
                .header("X-CSRF-Token", csrf)
        };
        ensure!(
            request(reqwest::Method::POST, "")
                .json(&json!({"seconds":60}))
                .send()
                .await?
                .status()
                == StatusCode::BAD_REQUEST
        );
        ensure!(
            request(reqwest::Method::POST, "")
                .json(&json!({"topic":"a/#/b","seconds":60}))
                .send()
                .await?
                .status()
                == StatusCode::BAD_REQUEST
        );
        let started = request(reqwest::Method::POST, "")
            .json(&body)
            .send()
            .await?;
        ensure!(started.status() == StatusCode::CREATED);
        let started: Value = started.json().await?;
        let observer = started["id"].as_str().context("observer")?;
        ensure!(
            request(reqwest::Method::POST, "")
                .json(&body)
                .send()
                .await?
                .status()
                == StatusCode::CONFLICT
        );
        let id = Uuid::new_v4();
        let p = clex_mqtt::domain::Publication {
            topic: "trace/a".into(),
            payload: b"private-payload".to_vec(),
            qos: 1,
            retain: false,
            expires_at: None,
            properties: Default::default(),
        };
        h.store
            .diagnostics()
            .capture(id, &p, b"esp32-01", "ivan")
            .context("capture")?
            .finish("committed");
        let detail_url = format!("{path}/messages/{id}");
        ensure!(reqwest::get(&detail_url).await?.status() == StatusCode::UNAUTHORIZED);
        let detail: Value = client.get(&detail_url).send().await?.json().await?;
        ensure!(detail["status"] == "committed" && !detail.to_string().contains("private-payload"));
        ensure!(
            client
                .delete(format!("{path}/{observer}"))
                .send()
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        ensure!(
            request(reqwest::Method::DELETE, &format!("/{observer}"))
                .send()
                .await?
                .status()
                == StatusCode::OK
        );
        let stopped: Value = client.get(&path).send().await?.json().await?;
        ensure!(
            stopped["active"] == false
                && stopped["messages"].as_array().context("messages")?.len() == 1
        );
        ensure!(
            request(reqwest::Method::POST, "")
                .json(&body)
                .send()
                .await?
                .status()
                == StatusCode::CREATED
        );
        ensure!(
            request(reqwest::Method::DELETE, &format!("/{observer}"))
                .send()
                .await?
                .status()
                == StatusCode::BAD_REQUEST
        );
        ensure!(h.store.diagnostics().snapshot().active);
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'diagnostics.%'",
        )
        .fetch_one(&h.pool)
        .await?;
        ensure!(
            count == 3,
            "successful starts/stops audited; rejected mutations leave no success audit"
        );
        Ok(())
    }
    .await;
    let cleanup = h.close().await;
    outcome?;
    cleanup
}
