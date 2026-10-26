use super::*;
use crate::tls::{CertificateInfo, fingerprint_text};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NewCertificate {
    label: String,
    certificate_pem: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CertificateUpdate {
    enabled: bool,
}

fn missing() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "CERTIFICATE_NOT_FOUND",
        "Сертификат не найден.",
    )
}

pub(super) async fn list(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let user =
        sqlx::query("SELECT id,require_client_certificate FROM mqtt_users WHERE username=$1")
            .bind(&username)
            .fetch_optional(&state.pool)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or_else(missing)?;
    let rows = sqlx::query("SELECT id,label,subject,fingerprint,expires_at,enabled,created_at FROM mqtt_client_certificates WHERE mqtt_user_id=$1 ORDER BY created_at DESC LIMIT 16")
        .bind(user.get::<Uuid,_>("id")).fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    let certificates: Vec<Value> = rows.into_iter().map(|row|json!({
        "id":row.get::<Uuid,_>("id"),"label":row.get::<String,_>("label"),"subject":row.get::<String,_>("subject"),
        "fingerprint":fingerprint_text(&row.get::<Vec<u8>,_>("fingerprint")),"expiresAt":row.get::<DateTime<Utc>,_>("expires_at"),
        "enabled":row.get::<bool,_>("enabled"),"createdAt":row.get::<DateTime<Utc>,_>("created_at")
    })).collect();
    Ok(Json(
        json!({"certificates":certificates,"requireClientCertificate":user.get::<bool,_>("require_client_certificate"),
        "tls":{"mode":state.config.tls_client_auth.as_str(),"address":state.config.tls_addr.map(|address|address.to_string())}}),
    ))
}

async fn lock_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    username: &str,
) -> ApiResult<Uuid> {
    sqlx::query_scalar("SELECT id FROM mqtt_users WHERE username=$1 FOR UPDATE")
        .bind(username)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or_else(missing)
}

pub(super) async fn create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(input): Json<NewCertificate>,
) -> ApiResult<Response> {
    let actor = identity(&state, &headers, true).await?;
    let label = input.label.trim();
    if label.is_empty() || label.chars().count() > 80 || label.chars().any(char::is_control) {
        return Err(ApiError::bad("Название сертификата: 1-80 символов."));
    }
    let certificate=CertificateInfo::from_pem(&input.certificate_pem)
        .map_err(|_|ApiError::bad("Добавьте один действующий открытый сертификат клиента в формате PEM с назначением clientAuth."))?;
    let mut tx = access::begin_access(&state.pool).await?;
    let user = lock_user(&mut tx, &username).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mqtt_client_certificates WHERE mqtt_user_id=$1")
            .bind(user)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?;
    if count >= 16 {
        return Err(ApiError::bad(
            "Для учётной записи доступно до 16 сертификатов. Удалите завершившие работу сертификаты.",
        ));
    }
    let id = Uuid::new_v4();
    let inserted=sqlx::query("INSERT INTO mqtt_client_certificates(id,mqtt_user_id,fingerprint,label,subject,expires_at) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(user).bind(certificate.fingerprint.as_slice()).bind(label).bind(&certificate.subject).bind(certificate.expires_at).execute(&mut *tx).await;
    if let Err(error) = inserted {
        if error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "CERTIFICATE_EXISTS",
                "Этот сертификат уже зарегистрирован.",
            ));
        }
        return Err(ApiError::internal());
    }
    write_audit(
        &mut tx,
        &actor.username,
        "client_certificate.create",
        Some(&username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok((StatusCode::CREATED, Json(json!({"id":id}))).into_response())
}

pub(super) async fn update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((username, id)): Path<(String, Uuid)>,
    Json(input): Json<CertificateUpdate>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    let mut tx = access::begin_access(&state.pool).await?;
    let user = lock_user(&mut tx, &username).await?;
    let fingerprint:Option<Vec<u8>>=sqlx::query_scalar("UPDATE mqtt_client_certificates SET enabled=$3 WHERE id=$1 AND mqtt_user_id=$2 AND (NOT $3 OR expires_at>now()) RETURNING fingerprint")
        .bind(id).bind(user).bind(input.enabled).fetch_optional(&mut *tx).await.map_err(|_|ApiError::internal())?;
    let fingerprint = fingerprint.ok_or_else(missing)?;
    write_audit(
        &mut tx,
        &actor.username,
        "client_certificate.update",
        Some(&username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    if !input.enabled {
        state
            .broker
            .disconnect_certificate(&username, &fingerprint_text(&fingerprint))
            .await;
    }
    Ok(Json(json!({"id":id})))
}

pub(super) async fn delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((username, id)): Path<(String, Uuid)>,
) -> ApiResult<StatusCode> {
    let actor = identity(&state, &headers, true).await?;
    let mut tx = access::begin_access(&state.pool).await?;
    let user = lock_user(&mut tx, &username).await?;
    let fingerprint:Vec<u8>=sqlx::query_scalar("DELETE FROM mqtt_client_certificates WHERE id=$1 AND mqtt_user_id=$2 RETURNING fingerprint")
        .bind(id).bind(user).fetch_optional(&mut *tx).await.map_err(|_|ApiError::internal())?.ok_or_else(missing)?;
    write_audit(
        &mut tx,
        &actor.username,
        "client_certificate.delete",
        Some(&username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    state
        .broker
        .disconnect_certificate(&username, &fingerprint_text(&fingerprint))
        .await;
    Ok(StatusCode::NO_CONTENT)
}
