use crate::{auth, broker::Broker, config::Config};
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

mod access;
mod accounts;
mod audit;
mod bridges;
mod certificates;
mod diagnostics;
mod metrics;
mod security;

#[derive(Clone)]
pub struct WebState {
    pub pool: PgPool,
    pub broker: Broker,
    pub config: Arc<Config>,
    pub started: Instant,
    metrics: Arc<crate::metrics::Exporter>,
    login_security: Arc<Mutex<security::LoginSecurity>>,
    account_security: Arc<Mutex<security::LoginSecurity>>,
    password_jobs: Arc<Semaphore>,
    diagnostic_changes: Arc<Mutex<()>>,
    dummy_hash: Arc<String>,
}

impl WebState {
    pub fn new(pool: PgPool, broker: Broker, config: Arc<Config>) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            broker,
            config,
            started: Instant::now(),
            metrics: Default::default(),
            login_security: Default::default(),
            account_security: Default::default(),
            password_jobs: Arc::new(Semaphore::new(4)),
            diagnostic_changes: Default::default(),
            dummy_hash: Arc::new(auth::hash_password(&auth::random_token())?),
        })
    }
}

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct Frontend;

pub fn router(state: WebState) -> Router {
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"alive"})) }),
        )
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics::scrape))
        .route("/mqtt", get(mqtt_websocket))
        .route("/api/v1/session", get(session).post(login).delete(logout))
        .merge(accounts::routes())
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/bridges", get(bridges::list).post(bridges::create))
        .route(
            "/api/v1/bridges/{id}",
            patch(bridges::update).delete(bridges::delete),
        )
        .route("/api/v1/messages", get(messages))
        .route(
            "/api/v1/diagnostics",
            get(diagnostics::list).post(diagnostics::start),
        )
        .route(
            "/api/v1/diagnostics/{id}",
            axum::routing::delete(diagnostics::stop),
        )
        .route(
            "/api/v1/diagnostics/messages/{id}",
            get(diagnostics::message),
        )
        .route("/api/v1/clients", get(clients))
        .route("/api/v1/clients/disconnect", post(disconnect))
        .route("/api/v1/retained", get(retained))
        .route("/api/v1/retained/delete", post(delete_retained))
        .route("/api/v1/users", get(users).post(create_user))
        .route("/api/v1/users/{username}", patch(update_user))
        .route(
            "/api/v1/users/{username}/certificates",
            get(certificates::list).post(certificates::create),
        )
        .route(
            "/api/v1/users/{username}/certificates/{id}",
            patch(certificates::update).delete(certificates::delete),
        )
        .route(
            "/api/v1/groups",
            get(access::groups).post(access::create_group),
        )
        .route(
            "/api/v1/groups/{id}",
            patch(access::update_group).delete(access::delete_group),
        )
        .route("/api/v1/access/check", post(access::check_access))
        .route("/api/v1/audit", get(audit::list))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .fallback(frontend)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(request: axum::extract::Request, next: middleware::Next) -> Response {
    let api = request.uri().path().starts_with("/api/") || request.uri().path() == "/metrics";
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("same-origin"));
    response.headers_mut().insert("content-security-policy", HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"));
    if api {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn mqtt_websocket(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let protocol = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|header| header.to_str().ok())
        .unwrap_or("");
    if !protocol.split(',').any(|part| part.trim() == "mqtt") {
        return Err(ApiError::bad("WebSocket requires the mqtt subprotocol."));
    }
    Ok(ws
        .protocols(["mqtt"])
        .max_message_size(state.config.max_packet_size)
        .max_frame_size(state.config.max_packet_size)
        .on_upgrade(move |socket| websocket_io(socket, state.broker, peer))
        .into_response())
}

async fn websocket_io(socket: WebSocket, broker: Broker, peer: SocketAddr) {
    let (server_io, proxy_io) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        let _ = broker.run_io(server_io, peer).await;
    });
    let (mut sink, mut stream) = socket.split();
    let (mut reader, mut writer) = tokio::io::split(proxy_io);
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        tokio::select! {
            received = reader.read(&mut buffer) => match received {
                Ok(0) | Err(_) => break,
                Ok(size) => if !matches!(tokio::time::timeout(Duration::from_secs(10),sink.send(Message::Binary(buffer[..size].to_vec().into()))).await,Ok(Ok(()))) { break; },
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => if !matches!(tokio::time::timeout(Duration::from_secs(10),writer.write_all(&bytes)).await,Ok(Ok(()))) { break; },
                Some(Ok(Message::Ping(bytes))) => if !matches!(tokio::time::timeout(Duration::from_secs(10),sink.send(Message::Pong(bytes))).await,Ok(Ok(()))) { break; },
                Some(Ok(Message::Pong(_))) => (),
                _ => break,
            }
        }
    }
    let _ = writer.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .is_err()
    {
        tracing::warn!("WebSocket MQTT task exceeded shutdown deadline");
    }
}

#[derive(Debug)]
pub struct ApiError(StatusCode, &'static str, &'static str);
impl ApiError {
    fn internal() -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "Операция не выполнена. Проверьте состояние сервиса.",
        )
    }
    fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Требуется вход в панель.",
        )
    }
    fn bad(message: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error":{"code":self.1,"message":self.2}})),
        )
            .into_response()
    }
}
type ApiResult<T> = Result<T, ApiError>;
struct Identity {
    id: Uuid,
    username: String,
    token: String,
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "clex_session"
                && value.len() == 43
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
            .then(|| value.to_owned())
        })
}

async fn identity(state: &WebState, headers: &HeaderMap, mutate: bool) -> ApiResult<Identity> {
    let token = cookie(headers).ok_or_else(ApiError::unauthorized)?;
    let row = sqlx::query("SELECT s.id, a.username, s.csrf_hash FROM admin_sessions s JOIN admin_users a ON a.id = s.admin_id WHERE s.token_hash = $1 AND s.expires_at > now() AND a.enabled=true")
        .bind(auth::token_hash(&token)).fetch_optional(&state.pool).await.map_err(|_| ApiError::internal())?.ok_or_else(ApiError::unauthorized)?;
    if mutate {
        check_origin(state, headers)?;
        let csrf = headers
            .get("x-csrf-token")
            .and_then(|h| h.to_str().ok())
            .ok_or(ApiError(
                StatusCode::FORBIDDEN,
                "CSRF_REQUIRED",
                "Обновите страницу и повторите действие.",
            ))?;
        let expected: Vec<u8> = row.get("csrf_hash");
        if auth::token_hash(csrf) != expected {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "CSRF_INVALID",
                "Обновите страницу и повторите действие.",
            ));
        }
    }
    Ok(Identity {
        id: row.get("id"),
        username: row.get("username"),
        token,
    })
}

fn check_origin(state: &WebState, headers: &HeaderMap) -> ApiResult<()> {
    if headers.get_all(header::ORIGIN).iter().nth(1).is_some() {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "ORIGIN_DENIED",
            "Укажите единственный Origin запроса.",
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin
            .to_str()
            .map_err(|_| ApiError::bad("Некорректный Origin."))?;
        let dev_origin = state.config.environment != "production"
            && matches!(origin, "http://127.0.0.1:5173" | "http://localhost:5173");
        if origin != state.config.origin() && !dev_origin {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "ORIGIN_DENIED",
                "Источник запроса не разрешён.",
            ));
        }
    }
    Ok(())
}

fn session_cookie(token: &str, secure: bool, clear: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "clex_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        if clear { "" } else { token },
        if clear { 0 } else { 28800 },
        if secure { "; Secure" } else { "" }
    ))
    .expect("Generated cookie contains only safe characters")
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}
async fn login(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> ApiResult<Response> {
    let ip = security::client_ip(peer, &headers, &state.config.http_trusted_proxy_ips)?;
    let allowed = state
        .login_security
        .lock()
        .await
        .attempt(ip, Instant::now());
    if let Err(report) = allowed {
        if report {
            security::failure(&state, &input.username, ip, "limited", "rate_limit").await;
        }
        return Ok(security::limited_response("60"));
    }
    if let Err(error) = check_origin(&state, &headers) {
        security::failure(&state, &input.username, ip, "failed", "origin_denied").await;
        return Err(error);
    }
    if input.username.len() > 128
        || input.username.chars().any(char::is_control)
        || input.password.len() > 256
    {
        security::failure(&state, &input.username, ip, "failed", "invalid_input").await;
        return Err(ApiError::bad("Некорректные учётные данные."));
    }
    let permit = match state.password_jobs.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            security::failure(&state, &input.username, ip, "limited", "password_busy").await;
            return Ok(security::limited_response("1"));
        }
    };
    let row = sqlx::query(
        "SELECT id, username, password_hash FROM admin_users WHERE username = $1 AND enabled=true",
    )
    .bind(&input.username)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| ApiError::internal())?;
    let hash = row
        .as_ref()
        .map(|r| r.get::<String, _>("password_hash"))
        .unwrap_or_else(|| state.dummy_hash.as_ref().clone());
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth::verify_password(&input.password, &hash)
    })
    .await
    .map_err(|_| ApiError::internal())?;
    let row = match (valid, row) {
        (true, Some(row)) => row,
        _ => {
            security::failure(&state, &input.username, ip, "failed", "invalid_credentials").await;
            return Err(ApiError(
                StatusCode::UNAUTHORIZED,
                "INVALID_CREDENTIALS",
                "Неверное имя пользователя или пароль.",
            ));
        }
    };
    let token = auth::random_token();
    let csrf = auth::csrf_token(&token);
    let username: String = row.get("username");
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    sqlx::query("DELETE FROM admin_sessions WHERE expires_at <= now()")
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    sqlx::query("INSERT INTO admin_sessions (id, admin_id, token_hash, csrf_hash, expires_at) VALUES ($1,$2,$3,$4,now()+interval '8 hours')")
        .bind(Uuid::new_v4()).bind(row.get::<Uuid,_>("id")).bind(auth::token_hash(&token)).bind(auth::token_hash(&csrf)).execute(&mut *tx).await.map_err(|_| ApiError::internal())?;
    security::write_login(&mut tx, &username, ip, "success", None).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    security::prune(&state).await;
    let mut response = Json(json!({"username":username,"csrfToken":csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&token, state.config.cookie_secure, false),
    );
    Ok(response)
}

async fn session(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let identity = identity(&state, &headers, false).await?;
    Ok(Json(
        json!({"username":identity.username,"csrfToken":auth::csrf_token(&identity.token)}),
    ))
}

async fn logout(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Response> {
    let identity = identity(&state, &headers, true).await?;
    sqlx::query("DELETE FROM admin_sessions WHERE id=$1")
        .bind(identity.id)
        .execute(&state.pool)
        .await
        .map_err(|_| ApiError::internal())?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie("", state.config.cookie_secure, true),
    );
    Ok(response)
}

async fn ready(State(state): State<WebState>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => Json(json!({"status":"ready"})).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"unavailable"})),
        )
            .into_response(),
    }
}

async fn write_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    subject: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO audit_events (actor, action, subject, details) VALUES ($1,$2,$3,'{}'::jsonb)",
    )
    .bind(actor)
    .bind(action)
    .bind(subject)
    .execute(&mut **tx)
    .await
    .map_err(|_| ApiError::internal())?;
    Ok(())
}

async fn users(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let rows = sqlx::query(
        "SELECT id,username,enabled,created_at,require_client_certificate,owner_id,account_label,(SELECT username FROM accounts a WHERE a.id=mqtt_users.owner_id) AS owner_username,coalesce((SELECT enabled FROM accounts a WHERE a.id=mqtt_users.owner_id),true) AS owner_enabled,(scram_verifier IS NOT NULL) AS scram_ready,(SELECT count(*) FROM mqtt_client_certificates c WHERE c.mqtt_user_id=mqtt_users.id AND c.enabled AND c.expires_at>now()) AS certificate_count FROM mqtt_users ORDER BY username LIMIT 1000",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| ApiError::internal())?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.get("id")).collect();
    let rules = sqlx::query("SELECT mqtt_user_id,filter,publish,subscribe FROM mqtt_acl_rules WHERE mqtt_user_id=ANY($1) AND group_id IS NULL AND allow=true ORDER BY priority DESC,filter")
        .bind(&ids).fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    let memberships = sqlx::query("SELECT ug.mqtt_user_id,g.id,g.name,g.enabled FROM mqtt_user_groups ug JOIN mqtt_access_groups g ON g.id=ug.group_id WHERE ug.mqtt_user_id=ANY($1) ORDER BY g.name")
        .bind(&ids).fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    let mut groups: HashMap<Uuid, Vec<Value>> = HashMap::new();
    for member in memberships {
        groups.entry(member.get("mqtt_user_id")).or_default().push(json!({
            "id":member.get::<Uuid,_>("id"),"name":member.get::<String,_>("name"),"enabled":member.get::<bool,_>("enabled")
        }));
    }
    let data: Vec<Value> = rows.into_iter().map(|row| {
        let id: Uuid = row.get("id");
        let publish: Vec<String> = rules.iter().filter(|rule| rule.get::<Option<Uuid>,_>("mqtt_user_id") == Some(id) && rule.get::<bool,_>("publish")).map(|rule| rule.get("filter")).collect();
        let subscribe: Vec<String> = rules.iter().filter(|rule| rule.get::<Option<Uuid>,_>("mqtt_user_id") == Some(id) && rule.get::<bool,_>("subscribe")).map(|rule| rule.get("filter")).collect();
        json!({"ownerId":row.get::<Option<Uuid>,_>("owner_id"),"ownerUsername":row.get::<Option<String>,_>("owner_username"),"accountLabel":row.get::<Option<String>,_>("account_label"),"accountEnabled":row.get::<bool,_>("owner_enabled"),"username":row.get::<String,_>("username"),"enabled":row.get::<bool,_>("enabled"),"createdAt":row.get::<DateTime<Utc>,_>("created_at"),"publishFilters":publish,"subscribeFilters":subscribe,"groups":groups.remove(&id).unwrap_or_default(),"scramReady":row.get::<bool,_>("scram_ready"),"requireClientCertificate":row.get::<bool,_>("require_client_certificate"),"certificateCount":row.get::<i64,_>("certificate_count")})
    }).collect();
    Ok(Json(json!(data)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NewUser {
    username: String,
    password: String,
    publish_filters: Vec<String>,
    subscribe_filters: Vec<String>,
    #[serde(default)]
    group_ids: Vec<Uuid>,
    #[serde(default)]
    require_client_certificate: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserUpdate {
    enabled: Option<bool>,
    password: Option<String>,
    publish_filters: Option<Vec<String>>,
    subscribe_filters: Option<Vec<String>>,
    group_ids: Option<Vec<Uuid>>,
    require_client_certificate: Option<bool>,
}

fn validate_username(username: &str) -> ApiResult<()> {
    if username.is_empty()
        || username.len() > 128
        || !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b))
    {
        return Err(ApiError::bad(
            "Имя: 1-128 символов, латинские буквы, цифры, _, ., @ или -.",
        ));
    }
    Ok(())
}

fn validate_filters(filters: &[String]) -> ApiResult<()> {
    if filters.len() > 100 {
        return Err(ApiError::bad(
            "Допускается не больше 100 правил каждого типа.",
        ));
    }
    for filter in filters {
        if filter.is_empty() || filter.len() > 65535 || filter.contains('\0') {
            return Err(ApiError::bad("Некорректный фильтр топика."));
        }
        let levels: Vec<_> = filter.split('/').collect();
        for (i, level) in levels.iter().enumerate() {
            if (level.contains('+') && *level != "+")
                || (level.contains('#') && (*level != "#" || i + 1 != levels.len()))
            {
                return Err(ApiError::bad(
                    "Символ + занимает уровень целиком; # допустим только последним уровнем.",
                ));
            }
        }
    }
    Ok(())
}

async fn password_hash(
    state: &WebState,
    password: String,
) -> ApiResult<(String, Option<crate::scram::Verifier>)> {
    auth::validate_password(&password)
        .map_err(|_| ApiError::bad("Пароль должен содержать 12-256 байт."))?;
    let permit = state
        .password_jobs
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::internal())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Ok::<_, anyhow::Error>((
            auth::hash_password(&password)?,
            crate::scram::Verifier::from_password(&password).ok(),
        ))
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())
}

async fn replace_rules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    filters: &[String],
    publish: bool,
) -> ApiResult<()> {
    let delete = if publish {
        "DELETE FROM mqtt_acl_rules WHERE mqtt_user_id=$1 AND publish=true"
    } else {
        "DELETE FROM mqtt_acl_rules WHERE mqtt_user_id=$1 AND subscribe=true"
    };
    sqlx::query(delete)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?;
    for filter in filters {
        sqlx::query("INSERT INTO mqtt_acl_rules (id,mqtt_user_id,filter,publish,subscribe,allow,priority) VALUES ($1,$2,$3,$4,$5,true,0)")
            .bind(Uuid::new_v4()).bind(id).bind(filter).bind(publish).bind(!publish).execute(&mut **tx).await.map_err(|_| ApiError::internal())?;
    }
    Ok(())
}

async fn create_user(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<NewUser>,
) -> ApiResult<Response> {
    let actor = identity(&state, &headers, true).await?;
    validate_username(&input.username)?;
    validate_filters(&input.publish_filters)?;
    validate_filters(&input.subscribe_filters)?;
    let (hash, verifier) = password_hash(&state, input.password).await?;
    let id = Uuid::new_v4();
    let mut tx = access::begin_access(&state.pool).await?;
    let inserted = sqlx::query("INSERT INTO mqtt_users (id,username,password_hash,scram_verifier,require_client_certificate,enabled,superuser) VALUES ($1,$2,$3,$4,$5,true,false)").bind(id).bind(&input.username).bind(hash).bind(verifier.map(sqlx::types::Json)).bind(input.require_client_certificate).execute(&mut *tx).await;
    match inserted {
        Ok(_) => (),
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "USER_EXISTS",
                "Такое имя уже используется.",
            ));
        }
        Err(_) => return Err(ApiError::internal()),
    }
    replace_rules(&mut tx, id, &input.publish_filters, true).await?;
    replace_rules(&mut tx, id, &input.subscribe_filters, false).await?;
    access::replace_memberships(&mut tx, id, &input.group_ids).await?;
    write_audit(
        &mut tx,
        &actor.username,
        "mqtt_user.create",
        Some(&input.username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"username":input.username})),
    )
        .into_response())
}

async fn update_user(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(input): Json<UserUpdate>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    validate_username(&username)?;
    if let Some(filters) = &input.publish_filters {
        validate_filters(filters)?;
    }
    if let Some(filters) = &input.subscribe_filters {
        validate_filters(filters)?;
    }
    let changing_password = input.password.is_some();
    let (hash, verifier) = match input.password {
        Some(password) => {
            let (hash, verifier) = password_hash(&state, password).await?;
            (Some(hash), verifier)
        }
        None => (None, None),
    };
    let mut tx = access::begin_access(&state.pool).await?;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM mqtt_users WHERE username=$1 FOR UPDATE")
        .bind(&username)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "USER_NOT_FOUND",
            "Клиент не найден.",
        ))?;
    sqlx::query("UPDATE mqtt_users SET enabled=COALESCE($2,enabled),password_hash=COALESCE($3,password_hash),scram_verifier=CASE WHEN $4 THEN $5 ELSE scram_verifier END,require_client_certificate=COALESCE($6,require_client_certificate) WHERE id=$1").bind(id).bind(input.enabled).bind(hash).bind(changing_password).bind(verifier.map(sqlx::types::Json)).bind(input.require_client_certificate).execute(&mut *tx).await.map_err(|_| ApiError::internal())?;
    if let Some(filters) = &input.publish_filters {
        replace_rules(&mut tx, id, filters, true).await?;
    }
    if let Some(filters) = &input.subscribe_filters {
        replace_rules(&mut tx, id, filters, false).await?;
    }
    if let Some(groups) = &input.group_ids {
        access::replace_memberships(&mut tx, id, groups).await?;
    }
    write_audit(
        &mut tx,
        &actor.username,
        "mqtt_user.update",
        Some(&username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    // Reconnect reauthenticates and reloads ACL; existing sessions must not retain revoked access.
    for client in state.broker.clients().await {
        if client.username == username {
            state
                .broker
                .disconnect(&client.username, &client.client_id)
                .await;
        }
    }
    Ok(Json(json!({"username":username})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Disconnect {
    username: String,
    client_id: String,
}
async fn disconnect(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<Disconnect>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    let disconnected = state
        .broker
        .disconnect(&input.username, &input.client_id)
        .await;
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    write_audit(
        &mut tx,
        &actor.username,
        "client.disconnect",
        Some(&format!("{} / {}", input.username, input.client_id)),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!({"disconnected":disconnected})))
}

async fn frontend(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path.starts_with("health/") {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":{"code":"NOT_FOUND","message":"Ресурс не найден."}})),
        )
            .into_response();
    }
    let selected = if path.is_empty() { "index.html" } else { path };
    let (file, name) = match Frontend::get(selected) {
        Some(file) => (file, selected),
        None if !selected.contains('.') => match Frontend::get("index.html") {
            Some(file) => (file, "index.html"),
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut response = file.data.into_owned().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime_guess::from_path(name).first_or_octet_stream().as_ref())
            .expect("Known MIME type"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if name.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    response
}

async fn overview(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let counts = sqlx::query("SELECT (SELECT count(*) FROM sessions) AS sessions, (SELECT count(*) FROM subscriptions) AS subscriptions, (SELECT count(*) FROM retained) AS retained, (SELECT count(*) FROM deliveries) AS deliveries")
        .fetch_one(&state.pool).await.map_err(|_| ApiError::internal())?;
    let metrics = state.broker.metrics();
    let mut listeners = vec![
        json!({"name":"MQTT TCP","address":state.config.mqtt_addr.to_string(),"protocol":"MQTT","enabled":true}),
        json!({"name":"Панель управления","address":state.config.http_addr.to_string(),"protocol":"HTTP / WebSocket","enabled":true}),
    ];
    if let Some(address) = state.config.tls_addr {
        listeners.push(json!({"name":"MQTT TLS","address":address.to_string(),"protocol":"TLS","enabled":true}));
    }
    let capabilities = vec![
        json!({"name":"Клиентские сертификаты","status":"available","detail":"mTLS с проверкой CA, привязкой SHA-256 отпечатка к MQTT-учётной записи и отзывом сертификатов через панель."}),
        json!({"name":"MQTT 3.1 / 3.1.1","status":"available","detail":"Подключение, публикации, подписки, сообщения Retain и постоянные сессии."}),
        json!({"name":"MQTT 5.0","status":"available","detail":"QoS 0/1/2, постоянные сессии, сроки хранения, Topic Alias, Subscription Identifier, совместные подписки и расширенная аутентификация."}),
        json!({"name":"Расширенная аутентификация","status":"available","detail":"MQTT 5: SCRAM-SHA-256 с подтверждением сервера и повторной аутентификацией в открытом соединении."}),
        json!({"name":"Совместные подписки","status":"available","detail":"MQTT 5: распределение сообщений по очереди между участниками группы с сохранением состояния в PostgreSQL."}),
        json!({"name":"QoS 0 / 1 / 2","status":"available","detail":"Сохранение очередей и состояний подтверждения в PostgreSQL."}),
        json!({"name":"Учётные записи MQTT и ACL","status":"available","detail":"Раздельные права публикации и подписки; по умолчанию доступ запрещён."}),
        json!({"name":"Группы и проверка доступа","status":"available","detail":"Общие наборы прав, шаблоны для типовых задач и проверка правил для выбранной учётной записи и топика."}),
        json!({"name":"TLS и WebSocket","status":"available","detail":"TLS-порт включается конфигурацией. WebSocket доступен по /mqtt."}),
        json!({"name":"Поток сообщений","status":"available","detail":"Принятые публикации с фильтрами по Client ID, MQTT-логину и топику. Просмотр содержимого, пауза и отдельная вкладка Retain."}),
        json!({"name":"Мосты между брокерами","status":"available","detail":"MQTT 5 TCP/TLS, QoS 1, правила топиков, очередь PostgreSQL и восстановление связи."}),
        json!({"name":"Метрики Prometheus","status":"available","detail":"Защищённый экспорт подключений, пакетов, публикаций, очередей, мостов и времени операций PostgreSQL."}),
        json!({"name":"Диагностика отдельного сообщения","status":"available","detail":"Сбор по источнику или топику: подписки, очередь, отправка и подтверждения QoS. Ограниченный буфер и сеанс до 5 минут."}),
    ];
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"uptimeSeconds":state.started.elapsed().as_secs(),"activeConnections":metrics.connected_clients,"storedSessions":counts.get::<i64,_>("sessions"),"subscriptions":counts.get::<i64,_>("subscriptions"),"retainedMessages":counts.get::<i64,_>("retained"),"queuedMessages":counts.get::<i64,_>("deliveries"),"incomingMessages":metrics.messages_received,"outgoingMessages":metrics.messages_sent,"rejectedConnections":metrics.handshake_failures+metrics.capacity_rejections,"rejectedPublications":metrics.publish_rejected,"database":{"healthy":true},"listeners":listeners,"capabilities":capabilities,"monitoring":{"enabled":state.config.metrics_token.is_some(),"path":"/metrics","databaseSnapshotSeconds":crate::metrics::CACHE_SECONDS}}),
    ))
}

async fn clients(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let live = state.broker.clients().await;
    let rows = sqlx::query("SELECT s.id,s.client_id,s.principal,s.protocol,s.updated_at,s.expires_at,(SELECT count(*) FROM subscriptions q WHERE q.session_id=s.id) AS subscriptions,(SELECT count(*) FROM deliveries d WHERE d.session_id=s.id AND d.phase='queued') AS queued,(SELECT count(*) FROM deliveries d WHERE d.session_id=s.id AND d.phase<>'queued') AS inflight FROM sessions s ORDER BY s.updated_at DESC LIMIT 1000")
        .fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    let data: Vec<Value> = rows.into_iter().map(|row| {
        let id: Uuid = row.get("id");
        let active = live.iter().find(|client| client.session_id == id);
        let protocol: i32 = row.get("protocol");
        let client_id: Vec<u8> = row.get("client_id");
        json!({"sessionId":id,"clientId":String::from_utf8_lossy(&client_id),"username":row.get::<String,_>("principal"),"connected":active.is_some(),"connectedAt":active.map(|client|client.connected_at),"lastSeen":active.map(|client|client.last_packet_at).unwrap_or_else(||row.get::<DateTime<Utc>,_>("updated_at")),"subscriptions":row.get::<i64,_>("subscriptions"),"queued":row.get::<i64,_>("queued"),"inflight":row.get::<i64,_>("inflight"),"remoteAddress":active.map(|client|client.peer.to_string()),"protocol":match protocol { 3=>"3.1", 5=>"5.0", _=>"3.1.1" },"keepAliveSeconds":active.map(|client|client.keep_alive),"certificateFingerprint":active.and_then(|client|client.certificate_fingerprint.clone()),"expiresAt":row.get::<Option<DateTime<Utc>>,_>("expires_at")})
    }).collect();
    Ok(Json(json!(data)))
}

async fn messages(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(filter): Query<crate::monitor::MessageFilter>,
) -> ApiResult<Json<crate::monitor::MessagePage>> {
    identity(&state, &headers, false).await?;
    if filter
        .topic
        .as_ref()
        .is_some_and(|topic| !crate::domain::valid_topic_filter(topic))
    {
        return Err(ApiError::bad(
            "Укажите топик или MQTT-фильтр, например devices/+/status.",
        ));
    }
    if filter.client_id.as_ref().is_some_and(|id| id.len() > 65535)
        || filter
            .username
            .as_ref()
            .is_some_and(|name| name.len() > 128)
        || filter
            .limit
            .is_some_and(|limit| !(1..=100).contains(&limit))
    {
        return Err(ApiError::bad(
            "Проверьте фильтры и размер страницы: от 1 до 100 сообщений.",
        ));
    }
    Ok(Json(state.broker.monitor().read(&filter)))
}

async fn retained(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let rows = sqlx::query("SELECT r.topic,substring(m.payload for 4096) AS preview,octet_length(m.payload) AS bytes,m.qos,m.created_at FROM retained r JOIN messages m ON m.id=r.message_id WHERE m.expires_at IS NULL OR m.expires_at>now() ORDER BY m.created_at DESC LIMIT 100")
        .fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!(rows.into_iter().map(|row| {
        let topic: Vec<u8> = row.get("topic");
        let payload: Vec<u8> = row.get("preview");
        let bytes: i32 = row.get("bytes");
        json!({"topic":String::from_utf8_lossy(&topic),"payloadBase64":STANDARD.encode(payload),"payloadBytes":bytes,"payloadTruncated":bytes>4096,"qos":row.get::<i32,_>("qos"),"updatedAt":row.get::<DateTime<Utc>,_>("created_at")})
    }).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRetained {
    topic: String,
}
async fn delete_retained(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<DeleteRetained>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    if !crate::domain::valid_topic_name(&input.topic) {
        return Err(ApiError::bad("Некорректное имя топика."));
    }
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    let affected = sqlx::query("DELETE FROM retained WHERE topic=$1")
        .bind(input.topic.as_bytes())
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?
        .rows_affected();
    write_audit(
        &mut tx,
        &actor.username,
        "retained.delete",
        Some(&input.topic),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!({"deleted":affected>0})))
}
