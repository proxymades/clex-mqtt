use super::*;

pub(super) async fn scrape(State(state): State<WebState>, headers: HeaderMap) -> Response {
    let Some(token) = &state.config.metrics_token else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let authorized = headers.get_all(header::AUTHORIZATION).iter().count() == 1
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| token.accepts(value));
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"metrics\"")],
            "Metrics token required\n",
        )
            .into_response();
    }
    let Some(database) = state.metrics.database(&state.pool).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "Metrics collection in progress\n",
        )
            .into_response();
    };
    match crate::metrics::render(
        &state.broker.metrics(),
        state.broker.storage_metrics(),
        state.broker.monitor().stats(),
        &database,
        &state.pool,
        state.started.elapsed(),
    ) {
        Ok(body) => ([(header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)], body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
