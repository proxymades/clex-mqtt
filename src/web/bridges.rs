use super::*;
use crate::bridge::{MAX_BRIDGES, Rule};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Input {
    name: String,
    endpoint: String,
    #[serde(default)]
    username: String,
    password: Option<String>,
    #[serde(default)]
    clear_password: bool,
    ca_pem: Option<String>,
    rules: Vec<Rule>,
    #[serde(default = "yes")]
    enabled: bool,
    revision: Option<i64>,
}
fn yes() -> bool {
    true
}
async fn seed(
    state: &WebState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    input: &Input,
) -> ApiResult<()> {
    if input.enabled {
        crate::store::Store::from_pool(state.pool.clone()).with_queue_limits(state.config.max_pending as i64,state.config.max_pending_bytes as i64)
            .seed_bridge_retained(tx,id,&input.rules).await.map_err(|error| if error.is::<crate::store::QuotaExceeded>() { ApiError::bad("Начальные значения Retain превышают квоту очереди моста. Уточните фильтры топиков.") } else { ApiError::internal() })?;
    }
    Ok(())
}
fn missing() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "BRIDGE_NOT_FOUND",
        "Мост уже удалён.",
    )
}
fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "BRIDGE_CHANGED",
        "Настройки моста изменились. Обновите данные и откройте форму снова.",
    )
}

fn validate(input: &Input) -> ApiResult<()> {
    if input.name.trim().is_empty()
        || input.name.chars().count() > 80
        || input.name.chars().any(char::is_control)
    {
        return Err(ApiError::bad("Название моста: 1-80 символов."));
    }
    let url = url::Url::parse(&input.endpoint)
        .map_err(|_| ApiError::bad("Укажите mqtt://host:1883 или mqtts://host:8883."))?;
    if input.endpoint.len() > 512
        || !matches!(url.scheme(), "mqtt" | "mqtts")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.port() == Some(0)
    {
        return Err(ApiError::bad(
            "Адрес моста должен содержать протокол, имя сервера и порт. Логин и пароль задаются отдельными полями.",
        ));
    }
    if input.username.len() > 128
        || input.username.contains('\0')
        || input
            .password
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > 1024 || p.contains('\0'))
        || (input.password.is_some() && (input.username.is_empty() || input.clear_password))
    {
        return Err(ApiError::bad("Укажите логин и пароль длиной 1-1024 байта."));
    }
    if input.rules.is_empty()
        || input.rules.len() > 16
        || input.rules.iter().any(|rule| rule.validate().is_err())
    {
        return Err(ApiError::bad(
            "Добавьте 1-16 правил. Префиксы завершаются символом /; в фильтре доступны + и #.",
        ));
    }
    if let Some(pem) = &input.ca_pem {
        if url.scheme() != "mqtts" || pem.len() > 12 * 1024 {
            return Err(ApiError::bad(
                "Открытый CA-сертификат PEM до 12 КБ используется с mqtts://.",
            ));
        }
        let items = rustls_pemfile::read_all(&mut pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApiError::bad("Проверьте формат открытого CA-сертификата PEM."))?;
        let mut roots = rustls::RootCertStore::empty();
        if items.is_empty() {
            return Err(ApiError::bad("Добавьте открытый CA-сертификат PEM."));
        }
        for item in items {
            match item {
                rustls_pemfile::Item::X509Certificate(cert) => {
                    roots
                        .add(cert)
                        .map_err(|_| ApiError::bad("Проверьте CA-сертификат."))?;
                }
                _ => return Err(ApiError::bad("Поле CA принимает открытые сертификаты PEM.")),
            }
        }
    }
    Ok(())
}

pub(super) async fn list(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let rows = sqlx::query("SELECT b.id,b.name,b.endpoint,b.username,b.password IS NOT NULL AS has_password,b.ca_pem,b.rules,b.enabled,b.revision,b.status,b.last_error,b.connected_at,b.received,b.sent,b.created_at,(SELECT count(*) FROM bridge_deliveries d WHERE d.bridge_id=b.id) AS queued FROM mqtt_bridges b ORDER BY b.created_at,b.id LIMIT 16").fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    let bridges: Vec<_> = rows.iter().map(|row| json!({
        "id":row.get::<Uuid,_>("id"),"name":row.get::<String,_>("name"),"endpoint":row.get::<String,_>("endpoint"),"username":row.get::<String,_>("username"),"hasPassword":row.get::<bool,_>("has_password"),"caPem":row.get::<Option<String>,_>("ca_pem"),"rules":row.get::<Value,_>("rules"),"enabled":row.get::<bool,_>("enabled"),"revision":row.get::<i64,_>("revision"),"status":row.get::<String,_>("status"),"lastError":row.get::<Option<String>,_>("last_error"),"connectedAt":row.get::<Option<DateTime<Utc>>,_>("connected_at"),"received":row.get::<i64,_>("received"),"sent":row.get::<i64,_>("sent"),"queued":row.get::<i64,_>("queued"),"createdAt":row.get::<DateTime<Utc>,_>("created_at")
    })).collect();
    Ok(Json(
        json!({"bridges":bridges,"secretKeyReady":state.config.bridge_key.is_some(),"maxQueuedMessages":state.config.max_pending,"maxQueuedBytes":state.config.max_pending_bytes}),
    ))
}

pub(super) async fn create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> ApiResult<Response> {
    let actor = identity(&state, &headers, true).await?;
    validate(&input)?;
    let id = Uuid::new_v4();
    let password = seal(&state, id, input.password.as_deref())?;
    let mut tx = access::begin_access(&state.pool).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mqtt_bridges")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    if count >= MAX_BRIDGES {
        return Err(ApiError::bad("Доступно до 16 мостов."));
    }
    let result = sqlx::query("INSERT INTO mqtt_bridges(id,name,endpoint,username,password,ca_pem,rules,enabled,status) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(id).bind(input.name.trim()).bind(&input.endpoint).bind(&input.username).bind(password).bind(&input.ca_pem).bind(serde_json::to_value(&input.rules).map_err(|_|ApiError::internal())?).bind(input.enabled).bind(if input.enabled {"connecting"} else {"disabled"}).execute(&mut *tx).await;
    mutation_result(result)?;
    seed(&state, &mut tx, id, &input).await?;
    write_audit(
        &mut tx,
        &actor.username,
        "bridge.create",
        Some(input.name.trim()),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok((StatusCode::CREATED, Json(json!({"id":id}))).into_response())
}

fn seal(state: &WebState, id: Uuid, password: Option<&str>) -> ApiResult<Option<Vec<u8>>> {
    password
        .map(|password| {
            state
                .config
                .bridge_key
                .as_ref()
                .ok_or_else(|| {
                    ApiError::bad(
                        "Для сохранения пароля задайте CLEX_BRIDGE_SECRET_KEY в окружении сервера.",
                    )
                })?
                .seal(id, password)
                .map_err(|_| ApiError::internal())
        })
        .transpose()
}
fn mutation_result(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> ApiResult<()> {
    match result {
        Ok(_) => Ok(()),
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            Err(ApiError(
                StatusCode::CONFLICT,
                "BRIDGE_EXISTS",
                "Это название моста уже используется.",
            ))
        }
        Err(_) => Err(ApiError::internal()),
    }
}

pub(super) async fn update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    validate(&input)?;
    let mut tx = access::begin_access(&state.pool).await?;
    let row = sqlx::query(
        "SELECT revision,password,endpoint,username FROM mqtt_bridges WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?
    .ok_or_else(missing)?;
    if input.revision != Some(row.get("revision")) {
        return Err(conflict());
    }
    let password = if input.clear_password || input.username.is_empty() {
        None
    } else if let Some(password) = input.password.as_deref() {
        seal(&state, id, Some(password))?
    } else {
        row.get::<Option<Vec<u8>>, _>("password")
    };
    if password.is_some()
        && (row.get::<String, _>("endpoint") != input.endpoint
            || row.get::<String, _>("username") != input.username)
        && input.password.is_none()
    {
        return Err(ApiError::bad(
            "При смене сервера или логина укажите пароль для нового подключения.",
        ));
    }
    let result = sqlx::query("UPDATE mqtt_bridges SET name=$2,endpoint=$3,username=$4,password=$5,ca_pem=$6,rules=$7,enabled=$8,revision=revision+1,status=$9,last_error=NULL WHERE id=$1")
        .bind(id).bind(input.name.trim()).bind(&input.endpoint).bind(&input.username).bind(password).bind(&input.ca_pem).bind(serde_json::to_value(&input.rules).map_err(|_|ApiError::internal())?).bind(input.enabled).bind(if input.enabled {"connecting"} else {"disabled"}).execute(&mut *tx).await;
    mutation_result(result)?;
    seed(&state, &mut tx, id, &input).await?;
    write_audit(
        &mut tx,
        &actor.username,
        "bridge.update",
        Some(input.name.trim()),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!({"updated":true})))
}

pub(super) async fn delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    let mut tx = access::begin_access(&state.pool).await?;
    let name: String = sqlx::query_scalar("DELETE FROM mqtt_bridges WHERE id=$1 RETURNING name")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or_else(missing)?;
    write_audit(&mut tx, &actor.username, "bridge.delete", Some(&name)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!({"deleted":true})))
}
