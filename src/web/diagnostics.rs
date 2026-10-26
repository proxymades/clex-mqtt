use super::*;

pub(super) async fn list(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<crate::diagnostics::TracePage>> {
    identity(&state, &headers, false).await?;
    Ok(Json(state.broker.diagnostics().snapshot()))
}
pub(super) async fn message(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<crate::diagnostics::TraceMessage>> {
    identity(&state, &headers, false).await?;
    state
        .broker
        .diagnostics()
        .message(id)
        .map(Json)
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "TRACE_EXPIRED",
            "Запись вышла за пределы буфера. Выберите сообщение из текущего списка.",
        ))
}
pub(super) async fn start(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(filter): Json<crate::diagnostics::TraceFilter>,
) -> ApiResult<Response> {
    let actor = identity(&state, &headers, true).await?;
    let _changes = state.diagnostic_changes.lock().await;
    if !filter.valid() {
        return Err(ApiError::bad(
            "Выберите источник или топик, длительность 1-300 с. Client ID и топик – до 1 024 байт, пользователь – до 128.",
        ));
    }
    // Serialize administrative observer changes, keeping audit writes durable.
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':diagnostics',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    if state.broker.diagnostics().snapshot().active {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "TRACE_ACTIVE",
            "Диагностика уже запущена. Завершите текущий сеанс или дождитесь окончания.",
        ));
    }
    write_audit(&mut tx, &actor.username, "diagnostics.start", None).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    // start is synchronous; all requests in this process share the same observer.
    state.broker.diagnostics().start(filter).map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "TRACE_ACTIVE",
            "Диагностика уже запущена. Обновите её состояние.",
        )
    })?;
    Ok((
        StatusCode::CREATED,
        Json(state.broker.diagnostics().snapshot()),
    )
        .into_response())
}
pub(super) async fn stop(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<crate::diagnostics::TracePage>> {
    let actor = identity(&state, &headers, true).await?;
    let _changes = state.diagnostic_changes.lock().await;
    if state.broker.diagnostics().snapshot().id != Some(id) {
        return Err(ApiError::bad("Обновите текущий сеанс диагностики."));
    }
    let mut tx = state.pool.begin().await.map_err(|_| ApiError::internal())?;
    write_audit(
        &mut tx,
        &actor.username,
        "diagnostics.stop",
        Some(&id.to_string()),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    if !state.broker.diagnostics().stop(id) {
        return Err(ApiError::bad("Обновите текущий сеанс диагностики."));
    }
    Ok(Json(state.broker.diagnostics().snapshot()))
}
