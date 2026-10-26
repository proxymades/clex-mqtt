use super::*;

#[derive(Default, Deserialize)]
pub(super) struct AuditQuery {
    before: Option<i64>,
    limit: Option<i64>,
    search: Option<String>,
    result: Option<String>,
    ip: Option<String>,
}

pub(super) async fn list(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=100).contains(&limit) || query.before.is_some_and(|id| id <= 0) {
        return Err(ApiError::bad("Некорректная страница журнала."));
    }
    let search = query.search.as_deref().unwrap_or("").trim();
    if search.len() > 256 || search.chars().any(char::is_control) {
        return Err(ApiError::bad(
            "Поисковый запрос слишком длинный или содержит управляющие символы.",
        ));
    }
    if query
        .result
        .as_deref()
        .is_some_and(|r| !matches!(r, "success" | "failed" | "limited"))
    {
        return Err(ApiError::bad("Некорректный результат входа."));
    }
    let ip = query
        .ip
        .as_deref()
        .map(|ip| {
            ip.parse()
                .map(security::normalize_ip)
                .map(|ip| ip.to_string())
        })
        .transpose()
        .map_err(|_| ApiError::bad("Некорректный IP-адрес."))?;
    security::prune(&state).await;
    let rows = sqlx::query("SELECT id,actor,action,subject,created_at,details->>'ip' AS ip,COALESCE(details->>'result',CASE WHEN action='admin.login' THEN 'success' END) AS result,details->>'reason' AS reason FROM audit_events WHERE ($1::bigint IS NULL OR id<$1) AND ($2='' OR strpos(lower(actor || ' ' || action || ' ' || coalesce(subject,'')),lower($2))>0) AND ($3::text IS NULL OR COALESCE(details->>'result',CASE WHEN action='admin.login' THEN 'success' END)=$3) AND ($4::text IS NULL OR details->>'ip'=$4) ORDER BY id DESC LIMIT $5")
        .bind(query.before).bind(search).bind(query.result).bind(ip).bind(limit)
        .fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    Ok(Json(json!(rows.into_iter().map(|row| json!({
        "id":row.get::<i64,_>("id"),"actor":row.get::<String,_>("actor"),
        "action":row.get::<String,_>("action"),"target":row.get::<Option<String>,_>("subject"),
        "createdAt":row.get::<DateTime<Utc>,_>("created_at"),"ip":row.get::<Option<String>,_>("ip"),
        "result":row.get::<Option<String>,_>("result"),"reason":row.get::<Option<String>,_>("reason"),
    })).collect::<Vec<_>>())))
}
