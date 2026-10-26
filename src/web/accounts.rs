use super::*;
use crate::accounts::topic_prefix;

pub(super) fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/v1/account/options", get(options))
        .route("/api/v1/account/register", post(register))
        .route(
            "/api/v1/account/session",
            get(current).post(login).delete(logout),
        )
        .route("/api/v1/account/password", post(change_password))
        .route("/api/v1/account/users", get(users).post(create_user))
        .route(
            "/api/v1/account/users/{id}",
            patch(update_user).delete(delete_user),
        )
        .route("/api/v1/account/clients", get(clients))
        .route("/api/v1/account/clients/disconnect", post(disconnect))
        .route("/api/v1/account/messages", get(messages))
        .route("/api/v1/account/retained", get(retained))
        .route("/api/v1/account/retained/delete", post(delete_retained))
        .route("/api/v1/accounts", get(admin_list))
        .route("/api/v1/accounts/{id}", patch(admin_update))
}

fn named_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<_> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value)
        .collect();
    if values.len() != 1 {
        return None;
    }
    let value = values[0];
    (value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)))
    .then(|| value.to_owned())
}

fn account_cookie(name: &str, token: &str, secure: bool, seconds: u32) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={seconds}{}",
        if secure { "; Secure" } else { "" }
    ))
    .expect("safe generated cookie")
}

fn origin(state: &WebState, headers: &HeaderMap) -> ApiResult<()> {
    if !headers.contains_key(header::ORIGIN) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "ORIGIN_REQUIRED",
            "Требуется Origin запроса.",
        ));
    }
    check_origin(state, headers)
}

fn csrf(headers: &HeaderMap, expected: &[u8]) -> ApiResult<()> {
    let token = headers
        .get("x-csrf-token")
        .and_then(|h| h.to_str().ok())
        .ok_or(ApiError(
            StatusCode::FORBIDDEN,
            "CSRF_REQUIRED",
            "Обновите страницу и повторите действие.",
        ))?;
    if auth::token_hash(token) != expected {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "CSRF_INVALID",
            "Обновите страницу и повторите действие.",
        ));
    }
    Ok(())
}

fn preauth(state: &WebState, headers: &HeaderMap) -> ApiResult<()> {
    origin(state, headers)?;
    let token = named_cookie(headers, "clex_account_preauth").ok_or(ApiError(
        StatusCode::FORBIDDEN,
        "CSRF_REQUIRED",
        "Обновите страницу и повторите действие.",
    ))?;
    csrf(headers, &auth::token_hash(&auth::csrf_token(&token)))
}

struct AccountIdentity {
    id: Uuid,
    session: Uuid,
    username: String,
    token: String,
}

async fn account_identity(
    state: &WebState,
    headers: &HeaderMap,
    mutate: bool,
) -> ApiResult<AccountIdentity> {
    let token = named_cookie(headers, "clex_account_session").ok_or_else(ApiError::unauthorized)?;
    let row=sqlx::query("SELECT a.id,a.username,s.id AS session_id,s.csrf_hash FROM account_sessions s JOIN accounts a ON a.id=s.account_id WHERE s.token_hash=$1 AND s.expires_at>now() AND a.enabled")
        .bind(auth::token_hash(&token)).fetch_optional(&state.pool).await.map_err(|_|ApiError::internal())?.ok_or_else(ApiError::unauthorized)?;
    if mutate {
        origin(state, headers)?;
        csrf(headers, &row.get::<Vec<u8>, _>("csrf_hash"))?;
    }
    Ok(AccountIdentity {
        id: row.get("id"),
        session: row.get("session_id"),
        username: row.get("username"),
        token,
    })
}

/// Match the broker's lock order before ownership, quota or credential changes.
/// Revalidate the account/session after acquiring the lock to fence revocation.
async fn begin_account<'a>(
    state: &'a WebState,
    actor: &AccountIdentity,
) -> ApiResult<sqlx::Transaction<'a, sqlx::Postgres>> {
    let mut tx = begin_state(state).await?;
    let current: Option<Uuid>=sqlx::query_scalar("SELECT a.id FROM accounts a JOIN account_sessions s ON s.account_id=a.id WHERE a.id=$1 AND a.enabled AND s.id=$2 AND s.expires_at>now() FOR UPDATE OF a")
        .bind(actor.id).bind(actor.session).fetch_optional(&mut *tx).await.map_err(|_|ApiError::internal())?;
    if current.is_none() {
        return Err(ApiError::unauthorized());
    }
    Ok(tx)
}
async fn begin_state(state: &WebState) -> ApiResult<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':broker-state',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':access-control',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    Ok(tx)
}

async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &AccountIdentity,
    action: &str,
    subject: Option<&str>,
) -> ApiResult<()> {
    write_audit(tx, &format!("account:{}", actor.username), action, subject).await
}
async fn limited(state: &WebState, peer: SocketAddr, headers: &HeaderMap) -> ApiResult<()> {
    let ip = security::client_ip(peer, headers, &state.config.http_trusted_proxy_ips)?;
    let allowed = state
        .account_security
        .lock()
        .await
        .attempt(ip, Instant::now());
    if let Err(report) = allowed {
        if report {
            login_failure(state, "", ip, "limited").await;
        }
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "ACCOUNT_LIMIT",
            "Слишком много попыток. Повторите через минуту.",
        ));
    }
    Ok(())
}
async fn hash(
    state: &WebState,
    password: String,
    mqtt: bool,
) -> ApiResult<(String, Option<crate::scram::Verifier>)> {
    auth::validate_password(&password)
        .map_err(|_| ApiError::bad("Пароль должен содержать 12-256 байт."))?;
    let permit = state
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "PASSWORD_BUSY",
                "Повторите попытку позже.",
            )
        })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Ok::<_, anyhow::Error>((
            auth::hash_password(&password)?,
            if mqtt {
                crate::scram::Verifier::from_password(&password).ok()
            } else {
                None
            },
        ))
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())
}

fn account_name(value: &str) -> ApiResult<String> {
    let value = value.trim().to_ascii_lowercase();
    if !(3..=32).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
    {
        return Err(ApiError::bad(
            "Логин: 3-32 символа, латинские буквы, цифры, _ или -.",
        ));
    }
    Ok(value)
}
fn conflict(error: sqlx::Error) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        ApiError(
            StatusCode::CONFLICT,
            "NAME_EXISTS",
            "Такое имя уже используется.",
        )
    } else {
        ApiError::internal()
    }
}
async fn options(State(state): State<WebState>) -> ApiResult<Response> {
    let token = auth::random_token();
    let mut response=Json(json!({"registrationEnabled":state.config.registration_enabled,"csrfToken":auth::csrf_token(&token)})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        account_cookie(
            "clex_account_preauth",
            &token,
            state.config.cookie_secure,
            600,
        ),
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

async fn new_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> ApiResult<String> {
    let token = auth::random_token();
    sqlx::query("DELETE FROM account_sessions WHERE (account_id=$1 AND expires_at<=now()) OR id IN (SELECT id FROM account_sessions WHERE account_id=$1 ORDER BY created_at DESC,id DESC OFFSET 9)")
        .bind(id).execute(&mut **tx).await.map_err(|_|ApiError::internal())?;
    sqlx::query("INSERT INTO account_sessions(id,account_id,token_hash,csrf_hash,expires_at) VALUES($1,$2,$3,$4,now()+interval '8 hours')")
        .bind(Uuid::new_v4()).bind(id).bind(auth::token_hash(&token)).bind(auth::token_hash(&auth::csrf_token(&token)))
        .execute(&mut **tx).await.map_err(|_|ApiError::internal())?;
    Ok(token)
}
async fn signed_in(
    state: &WebState,
    id: Uuid,
    token: &str,
    status: StatusCode,
) -> ApiResult<Response> {
    let mut profile = profile(state, id).await?;
    profile["csrfToken"] = json!(auth::csrf_token(token));
    let mut response = (status, Json(profile)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        account_cookie(
            "clex_account_session",
            token,
            state.config.cookie_secure,
            28800,
        ),
    );
    Ok(response)
}

async fn register(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> ApiResult<Response> {
    preauth(&state, &headers)?;
    limited(&state, peer, &headers).await?;
    if !state.config.registration_enabled {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "REGISTRATION_DISABLED",
            "Самостоятельная регистрация отключена.",
        ));
    }
    let username = account_name(&input.username)?;
    let (hash, _) = hash(&state, input.password, false).await?;
    let mut tx = access::begin_access(&state.pool).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    if count >= state.config.max_accounts {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "ACCOUNT_CAPACITY",
            "Регистрация временно недоступна.",
        ));
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash)
        .execute(&mut *tx)
        .await
        .map_err(conflict)?;
    let token = new_session(&mut tx, id).await?;
    write_audit(
        &mut tx,
        &format!("account:{username}"),
        "account.register",
        Some(&username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    signed_in(&state, id, &token, StatusCode::CREATED).await
}

async fn login(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> ApiResult<Response> {
    preauth(&state, &headers)?;
    limited(&state, peer, &headers).await?;
    let ip = security::client_ip(peer, &headers, &state.config.http_trusted_proxy_ips)?;
    let username = account_name(&input.username)?;
    if input.password.len() > 256 {
        return Err(ApiError::bad("Некорректные учётные данные."));
    }
    let row = sqlx::query("SELECT id,password_hash FROM accounts WHERE username=$1 AND enabled")
        .bind(&username)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| ApiError::internal())?;
    let hash = row
        .as_ref()
        .map(|r| r.get::<String, _>("password_hash"))
        .unwrap_or_else(|| state.dummy_hash.as_ref().clone());
    let check = hash.clone();
    let permit = state
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "PASSWORD_BUSY",
                "Повторите попытку позже.",
            )
        })?;
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth::verify_password(&input.password, &check)
    })
    .await
    .map_err(|_| ApiError::internal())?;
    let invalid = || {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "INVALID_CREDENTIALS",
            "Неверный логин или пароль.",
        )
    };
    let row = match (row, valid) {
        (Some(row), true) => row,
        _ => {
            login_failure(&state, &username, ip, "failed").await;
            return Err(invalid());
        }
    };
    let id: Uuid = row.get("id");
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE id=$1 AND enabled AND password_hash=$2 FOR UPDATE",
    )
    .bind(id)
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    if current.is_none() {
        return Err(invalid());
    }
    let token = new_session(&mut tx, id).await?;
    login_event(&mut tx, &username, ip, "success").await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    signed_in(&state, id, &token, StatusCode::OK).await
}

fn limits(row: &sqlx::postgres::PgRow) -> Value {
    json!({"maxMqttUsers":row.get::<i32,_>("max_mqtt_users"),"maxConnections":row.get::<i32,_>("max_connections"),"maxSessions":row.get::<i32,_>("max_sessions"),"maxQueuedMessages":row.get::<i32,_>("max_queued_messages"),"maxQueuedBytes":row.get::<i64,_>("max_queued_bytes"),"maxPublishRate":row.get::<i32,_>("max_publish_rate")})
}
async fn profile(state: &WebState, id: Uuid) -> ApiResult<Value> {
    let row=sqlx::query("SELECT a.*,(SELECT count(*) FROM mqtt_users u WHERE u.owner_id=a.id) AS mqtt_users,(SELECT count(*) FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=a.id AND s.online) AS connections,(SELECT count(*) FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=a.id) AS stored_sessions FROM accounts a WHERE a.id=$1 AND a.enabled")
        .bind(id).fetch_optional(&state.pool).await.map_err(|_|ApiError::internal())?.ok_or_else(ApiError::unauthorized)?;
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    let (messages, bytes) = crate::accounts::storage_usage(&mut tx, id, None)
        .await
        .map_err(|_| ApiError::internal())?;
    tx.rollback().await.map_err(|_| ApiError::internal())?;
    Ok(
        json!({"id":id,"username":row.get::<String,_>("username"),"topicPrefix":topic_prefix(&row.get::<String, _>("username")),"limits":limits(&row),"usage":{"mqttUsers":row.get::<i64,_>("mqtt_users"),"connections":row.get::<i64,_>("connections"),"storedSessions":row.get::<i64,_>("stored_sessions"),"storedMessages":messages,"storedBytes":bytes},"connection":{"host":state.config.public_url.host_str(),"tlsPort":state.config.tls_addr.map(|addr|addr.port()),"websocketPath":"/mqtt"}}),
    )
}
async fn current(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, false).await?;
    let mut data = profile(&state, actor.id).await?;
    data["csrfToken"] = json!(auth::csrf_token(&actor.token));
    Ok(Json(data))
}
async fn logout(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Response> {
    let actor = account_identity(&state, &headers, true).await?;
    sqlx::query("DELETE FROM account_sessions WHERE id=$1")
        .bind(actor.session)
        .execute(&state.pool)
        .await
        .map_err(|_| ApiError::internal())?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        account_cookie("clex_account_session", "", state.config.cookie_secure, 0),
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}
async fn change_password(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> ApiResult<StatusCode> {
    let actor = account_identity(&state, &headers, true).await?;
    limited(&state, peer, &headers).await?;
    if input.current_password.len() > 256 {
        return Err(ApiError::bad("Некорректный пароль."));
    }
    let old: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id=$1")
        .bind(actor.id)
        .fetch_one(&state.pool)
        .await
        .map_err(|_| ApiError::internal())?;
    let check = old.clone();
    let permit = state
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "PASSWORD_BUSY",
                "Повторите попытку позже.",
            )
        })?;
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth::verify_password(&input.current_password, &check)
    })
    .await
    .map_err(|_| ApiError::internal())?;
    if !valid {
        return Err(ApiError::bad("Текущий пароль неверен."));
    }
    let (hash, _) = hash(&state, input.new_password, false).await?;
    let mut tx = begin_account(&state, &actor).await?;
    let changed =
        sqlx::query("UPDATE accounts SET password_hash=$2 WHERE id=$1 AND password_hash=$3")
            .bind(actor.id)
            .bind(hash)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?
            .rows_affected();
    if changed != 1 {
        return Err(ApiError::unauthorized());
    }
    sqlx::query("DELETE FROM account_sessions WHERE account_id=$1")
        .bind(actor.id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    audit(&mut tx, &actor, "account.password", None).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceInput {
    label: String,
    password: String,
    publish_filters: Vec<String>,
    subscribe_filters: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceUpdate {
    password: Option<String>,
    enabled: Option<bool>,
    publish_filters: Option<Vec<String>>,
    subscribe_filters: Option<Vec<String>>,
}

fn scoped_filters(username: &str, filters: &[String]) -> ApiResult<Vec<String>> {
    validate_filters(filters)?;
    let prefix = topic_prefix(username);
    if filters.iter().any(|filter| !filter.starts_with(&prefix)) {
        return Err(ApiError::bad(
            "Укажите полный MQTT-фильтр, начинающийся с префикса вашего аккаунта.",
        ));
    }
    Ok(filters.to_vec())
}
async fn users(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, false).await?;
    let rows=sqlx::query("SELECT id,username,account_label,enabled,created_at FROM mqtt_users WHERE owner_id=$1 ORDER BY created_at,id LIMIT 100")
        .bind(actor.id).fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    let rules=sqlx::query("SELECT r.mqtt_user_id,r.filter,r.publish,r.subscribe FROM mqtt_acl_rules r JOIN mqtt_users u ON u.id=r.mqtt_user_id WHERE u.owner_id=$1 AND r.group_id IS NULL AND r.allow ORDER BY r.filter")
        .bind(actor.id).fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    Ok(Json(json!(rows.into_iter().map(|r| {let id:Uuid=r.get("id");json!({"id":id,"username":r.get::<String,_>("username"),"label":r.get::<Option<String>,_>("account_label"),"enabled":r.get::<bool,_>("enabled"),"createdAt":r.get::<DateTime<Utc>,_>("created_at"),"publishFilters":rules.iter().filter(|r|r.get::<Uuid,_>("mqtt_user_id")==id && r.get::<bool,_>("publish")).map(|r|r.get::<String,_>("filter")).collect::<Vec<_>>(),"subscribeFilters":rules.iter().filter(|r|r.get::<Uuid,_>("mqtt_user_id")==id && r.get::<bool,_>("subscribe")).map(|r|r.get::<String,_>("filter")).collect::<Vec<_>>()})}).collect::<Vec<_>>())))
}
async fn create_user(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<DeviceInput>,
) -> ApiResult<Response> {
    let actor = account_identity(&state, &headers, true).await?;
    if input.label.is_empty()
        || input.label.len() > 24
        || !input
            .label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(ApiError::bad(
            "Название: 1-24 символа, латинские буквы, цифры, _ или -.",
        ));
    }
    let publish = scoped_filters(&actor.username, &input.publish_filters)?;
    let subscribe = scoped_filters(&actor.username, &input.subscribe_filters)?;
    let (hash, verifier) = hash(&state, input.password, true).await?;
    let mut tx = begin_account(&state, &actor).await?;
    let full:bool=sqlx::query_scalar("SELECT (SELECT count(*) FROM mqtt_users WHERE owner_id=$1)>=max_mqtt_users FROM accounts WHERE id=$1").bind(actor.id).fetch_one(&mut *tx).await.map_err(|_|ApiError::internal())?;
    if full {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "DEVICE_LIMIT",
            "Достигнут лимит MQTT-учётных записей.",
        ));
    }
    let id = Uuid::new_v4();
    let username = crate::accounts::mqtt_username(&actor.username, &input.label);
    sqlx::query("INSERT INTO mqtt_users(id,owner_id,account_label,username,password_hash,scram_verifier) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(actor.id).bind(&input.label).bind(&username).bind(hash).bind(verifier.map(sqlx::types::Json)).execute(&mut *tx).await.map_err(conflict)?;
    replace_rules(&mut tx, id, &publish, true).await?;
    replace_rules(&mut tx, id, &subscribe, false).await?;
    audit(&mut tx, &actor, "account.mqtt_user.create", Some(&username)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id":id,"username":username})),
    )
        .into_response())
}
async fn owned_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: Uuid,
    id: Uuid,
) -> ApiResult<String> {
    sqlx::query_scalar("SELECT username FROM mqtt_users WHERE id=$1 AND owner_id=$2 FOR UPDATE")
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "USER_NOT_FOUND",
            "Учётная запись не найдена.",
        ))
}
async fn kick(state: &WebState, usernames: &[String]) {
    for client in state.broker.clients().await {
        if usernames.contains(&client.username) {
            state
                .broker
                .disconnect(&client.username, &client.client_id)
                .await;
        }
    }
}
async fn update_user(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<DeviceUpdate>,
) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, true).await?;
    let publish = input
        .publish_filters
        .as_ref()
        .map(|v| scoped_filters(&actor.username, v))
        .transpose()?;
    let subscribe = input
        .subscribe_filters
        .as_ref()
        .map(|v| scoped_filters(&actor.username, v))
        .transpose()?;
    let changed_password = input.password.is_some();
    let hashed = match input.password {
        Some(password) => Some(hash(&state, password, true).await?),
        None => None,
    };
    let mut tx = begin_account(&state, &actor).await?;
    let username = owned_user(&mut tx, actor.id, id).await?;
    let (password, verifier) = hashed
        .map(|(hash, verifier)| (Some(hash), verifier))
        .unwrap_or((None, None));
    sqlx::query("UPDATE mqtt_users SET enabled=coalesce($2,enabled),password_hash=coalesce($3,password_hash),scram_verifier=CASE WHEN $4 THEN $5 ELSE scram_verifier END WHERE id=$1")
        .bind(id).bind(input.enabled).bind(password).bind(changed_password).bind(verifier.map(sqlx::types::Json)).execute(&mut *tx).await.map_err(|_|ApiError::internal())?;
    if let Some(filters) = publish {
        replace_rules(&mut tx, id, &filters, true).await?;
    }
    if let Some(filters) = subscribe {
        replace_rules(&mut tx, id, &filters, false).await?;
    }
    audit(&mut tx, &actor, "account.mqtt_user.update", Some(&username)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    kick(&state, std::slice::from_ref(&username)).await;
    Ok(Json(json!({"username":username})))
}
async fn delete_user(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let actor = account_identity(&state, &headers, true).await?;
    let mut tx = begin_account(&state, &actor).await?;
    let username = owned_user(&mut tx, actor.id, id).await?;
    sqlx::query("DELETE FROM sessions WHERE principal=$1")
        .bind(&username)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    sqlx::query("DELETE FROM mqtt_users WHERE id=$1 AND owner_id=$2")
        .bind(id)
        .bind(actor.id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    audit(&mut tx, &actor, "account.mqtt_user.delete", Some(&username)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    kick(&state, &[username]).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn clients(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, false).await?;
    let live = state.broker.clients().await;
    let rows=sqlx::query("SELECT s.id,s.client_id,s.principal,s.online,s.protocol,s.updated_at,s.expires_at,(SELECT count(*) FROM subscriptions q WHERE q.session_id=s.id) AS subscriptions,(SELECT count(*) FROM deliveries d WHERE d.session_id=s.id) AS queued FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=$1 ORDER BY s.updated_at DESC LIMIT 1000")
        .bind(actor.id).fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    Ok(Json(json!(rows.into_iter().map(|row| {let id:Uuid=row.get("id");json!({"sessionId":id,"clientId":String::from_utf8_lossy(&row.get::<Vec<u8>,_>("client_id")),"username":row.get::<String,_>("principal"),"connected":live.iter().any(|c|c.session_id==id),"protocol":row.get::<i32,_>("protocol"),"lastSeen":row.get::<DateTime<Utc>,_>("updated_at"),"expiresAt":row.get::<Option<DateTime<Utc>>,_>("expires_at"),"subscriptions":row.get::<i64,_>("subscriptions"),"queued":row.get::<i64,_>("queued")})}).collect::<Vec<_>>())))
}
async fn disconnect(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<Disconnect>,
) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, true).await?;
    let mut tx = begin_account(&state, &actor).await?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mqtt_users WHERE owner_id=$1 AND username=$2)",
    )
    .bind(actor.id)
    .bind(&input.username)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    if !owned {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "USER_NOT_FOUND",
            "Учётная запись не найдена.",
        ));
    }
    audit(
        &mut tx,
        &actor,
        "account.client.disconnect",
        Some(&input.username),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    let disconnected = state
        .broker
        .disconnect(&input.username, &input.client_id)
        .await;
    Ok(Json(json!({"disconnected":disconnected})))
}
async fn messages(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, false).await?;
    let page = state.broker.monitor().read(&crate::monitor::MessageFilter {
        topic: Some(format!("{}#", topic_prefix(&actor.username))),
        limit: Some(100),
        ..Default::default()
    });
    Ok(Json(
        json!({"messages":page.messages.into_iter().map(|m|json!({"eventKey":m.preview_key,"receivedAt":m.received_at,"clientId":m.client_id,"username":m.username,"topic":m.topic,"qos":m.qos,"retain":m.retain,"will":m.will,"payloadBase64":m.payload_base64,"payloadBytes":m.payload_bytes,"payloadTruncated":m.payload_truncated})).collect::<Vec<_>>()}),
    ))
}
async fn retained(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, false).await?;
    let rows=sqlx::query("SELECT r.topic,substring(m.payload for 4096) AS preview,octet_length(m.payload) AS bytes,m.qos,m.created_at FROM retained r JOIN messages m ON m.id=r.message_id WHERE substring(r.topic FROM 1 FOR octet_length($1::bytea))=$1 AND (m.expires_at IS NULL OR m.expires_at>now()) ORDER BY m.created_at DESC LIMIT 100")
        .bind(topic_prefix(&actor.username).into_bytes()).fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    Ok(Json(json!(rows.into_iter().map(|r|json!({"topic":String::from_utf8_lossy(&r.get::<Vec<u8>,_>("topic")),"payloadBase64":STANDARD.encode(r.get::<Vec<u8>,_>("preview")),"payloadBytes":r.get::<i32,_>("bytes"),"payloadTruncated":r.get::<i32,_>("bytes")>4096,"qos":r.get::<i32,_>("qos"),"updatedAt":r.get::<DateTime<Utc>,_>("created_at")})).collect::<Vec<_>>())))
}
async fn delete_retained(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<DeleteRetained>,
) -> ApiResult<Json<Value>> {
    let actor = account_identity(&state, &headers, true).await?;
    if !crate::domain::valid_topic_name(&input.topic)
        || !input.topic.starts_with(&topic_prefix(&actor.username))
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "ACCOUNT_SCOPE",
            "Топик находится вне вашего пространства.",
        ));
    }
    let mut tx = begin_account(&state, &actor).await?;
    let deleted = sqlx::query("DELETE FROM retained WHERE topic=$1")
        .bind(input.topic.as_bytes())
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?
        .rows_affected()
        > 0;
    audit(
        &mut tx,
        &actor,
        "account.retained.delete",
        Some(&input.topic),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!({"deleted":deleted})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Limits {
    max_mqtt_users: i32,
    max_connections: i32,
    max_sessions: i32,
    max_queued_messages: i32,
    max_queued_bytes: i64,
    max_publish_rate: i32,
}
impl Limits {
    fn validate(&self) -> ApiResult<()> {
        if !(1..=100).contains(&self.max_mqtt_users)
            || !(1..=100).contains(&self.max_connections)
            || !(self.max_connections..=1000).contains(&self.max_sessions)
            || !(1..=100000).contains(&self.max_queued_messages)
            || !(1024..=67108864).contains(&self.max_queued_bytes)
            || !(1..=1000).contains(&self.max_publish_rate)
        {
            return Err(ApiError::bad(
                "Лимиты аккаунта находятся вне допустимого диапазона.",
            ));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountUpdate {
    enabled: Option<bool>,
    limits: Option<Limits>,
    password: Option<String>,
}
async fn admin_list(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let rows=sqlx::query("SELECT a.*,(SELECT count(*) FROM mqtt_users u WHERE u.owner_id=a.id) AS mqtt_users,(SELECT count(*) FROM sessions s JOIN mqtt_users u ON u.username=s.principal WHERE u.owner_id=a.id AND s.online) AS connections FROM accounts a ORDER BY a.created_at DESC,a.id LIMIT 1000")
        .fetch_all(&state.pool).await.map_err(|_|ApiError::internal())?;
    Ok(Json(json!(rows.iter().map(|r| {let id:Uuid=r.get("id");json!({"id":id,"username":r.get::<String,_>("username"),"enabled":r.get::<bool,_>("enabled"),"createdAt":r.get::<DateTime<Utc>,_>("created_at"),"topicPrefix":topic_prefix(&r.get::<String, _>("username")),"limits":limits(r),"mqttUsers":r.get::<i64,_>("mqtt_users"),"connections":r.get::<i64,_>("connections")})}).collect::<Vec<_>>())))
}
async fn admin_update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<AccountUpdate>,
) -> ApiResult<StatusCode> {
    let actor = identity(&state, &headers, true).await?;
    if let Some(limits) = &input.limits {
        limits.validate()?;
    }
    let password = match input.password {
        Some(password) => Some(hash(&state, password, false).await?.0),
        None => None,
    };
    let changed_password = password.is_some();
    let mut tx = begin_state(&state).await?;
    let row = sqlx::query("SELECT username FROM accounts WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "ACCOUNT_NOT_FOUND",
            "Аккаунт не найден.",
        ))?;
    sqlx::query("UPDATE accounts SET enabled=coalesce($2,enabled),password_hash=coalesce($3,password_hash) WHERE id=$1").bind(id).bind(input.enabled).bind(password).execute(&mut *tx).await.map_err(|_|ApiError::internal())?;
    if let Some(limits) = input.limits {
        sqlx::query("UPDATE accounts SET max_mqtt_users=$2,max_connections=$3,max_sessions=$4,max_queued_messages=$5,max_queued_bytes=$6,max_publish_rate=$7 WHERE id=$1")
            .bind(id).bind(limits.max_mqtt_users).bind(limits.max_connections).bind(limits.max_sessions).bind(limits.max_queued_messages).bind(limits.max_queued_bytes).bind(limits.max_publish_rate).execute(&mut *tx).await.map_err(|_|ApiError::internal())?;
    }
    if changed_password || input.enabled == Some(false) {
        sqlx::query("DELETE FROM account_sessions WHERE account_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    let users: Vec<String> =
        sqlx::query_scalar("SELECT username FROM mqtt_users WHERE owner_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?;
    write_audit(
        &mut tx,
        &actor.username,
        "account.update",
        Some(&row.get::<String, _>("username")),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    kick(&state, &users).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn login_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    username: &str,
    ip: std::net::IpAddr,
    result: &str,
) -> ApiResult<()> {
    let action = match result {
        "success" => "account.login",
        "limited" => "account.login_limited",
        _ => "account.login_failed",
    };
    sqlx::query("INSERT INTO audit_events(actor,action,subject,details) VALUES($1,$2,$3,$4)")
        .bind(format!("account:{username}"))
        .bind(action)
        .bind(username)
        .bind(json!({"ip":ip.to_string(),"result":result}))
        .execute(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(())
}
async fn login_failure(state: &WebState, username: &str, ip: std::net::IpAddr, result: &str) {
    if !state
        .account_security
        .lock()
        .await
        .allow_event(Instant::now())
    {
        return;
    }
    let attempt = async {
        let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
        login_event(&mut tx, username, ip, result).await?;
        tx.commit().await.map_err(|_| ApiError::internal())
    }
    .await;
    if attempt.is_err() {
        tracing::warn!("Could not persist an account login audit event");
    }
    security::prune(state).await;
}
