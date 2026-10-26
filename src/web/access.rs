use super::*;
use std::collections::HashSet;

pub(super) async fn begin_access(
    pool: &PgPool,
) -> ApiResult<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = pool.begin().await.map_err(|_| ApiError::internal())?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':access-control',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| ApiError::internal())?;
    Ok(tx)
}

pub(super) async fn replace_memberships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<()> {
    if ids.len() > 32 {
        return Err(ApiError::bad(
            "Выберите до 32 групп для одной учётной записи.",
        ));
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let found: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM mqtt_access_groups WHERE id=ANY($1) ORDER BY id FOR SHARE",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| ApiError::internal())?;
    if found.len() != ids.len() {
        return Err(ApiError::bad("Обновите список групп и повторите выбор."));
    }
    sqlx::query("DELETE FROM mqtt_user_groups WHERE mqtt_user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?;
    for id in ids {
        sqlx::query("INSERT INTO mqtt_user_groups(mqtt_user_id,group_id) VALUES($1,$2)")
            .bind(user_id)
            .bind(id)
            .execute(&mut **tx)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    Ok(())
}

pub(super) async fn groups(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    identity(&state, &headers, false).await?;
    let rows = sqlx::query("SELECT g.id,g.name,g.description,g.enabled,g.created_at,(SELECT count(*) FROM mqtt_user_groups ug WHERE ug.group_id=g.id) AS member_count FROM mqtt_access_groups g ORDER BY g.name LIMIT 1000")
        .fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.get("id")).collect();
    let rules = sqlx::query("SELECT group_id,filter,publish,subscribe,allow FROM mqtt_acl_rules WHERE group_id=ANY($1) AND mqtt_user_id IS NULL ORDER BY filter")
        .bind(&ids).fetch_all(&state.pool).await.map_err(|_| ApiError::internal())?;
    let data: Vec<Value> = rows.into_iter().map(|row| {
        let id: Uuid = row.get("id");
        let filters = |publish: bool, allow: bool| -> Vec<String> {
            rules.iter().filter(|rule| rule.get::<Option<Uuid>,_>("group_id") == Some(id)
                && rule.get::<bool,_>(if publish { "publish" } else { "subscribe" }) && rule.get::<bool,_>("allow") == allow)
                .map(|rule| rule.get("filter")).collect()
        };
        json!({"id":id,"name":row.get::<String,_>("name"),"description":row.get::<String,_>("description"),
            "enabled":row.get::<bool,_>("enabled"),"createdAt":row.get::<DateTime<Utc>,_>("created_at"),"memberCount":row.get::<i64,_>("member_count"),
            "publishFilters":filters(true,true),"subscribeFilters":filters(false,true),
            "denyPublishFilters":filters(true,false),"denySubscribeFilters":filters(false,false)})
    }).collect();
    Ok(Json(json!(data)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct GroupChanges {
    name: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    publish_filters: Option<Vec<String>>,
    subscribe_filters: Option<Vec<String>>,
    deny_publish_filters: Option<Vec<String>>,
    deny_subscribe_filters: Option<Vec<String>>,
}

fn validate_group(input: &GroupChanges) -> ApiResult<()> {
    if input.name.as_ref().is_some_and(|name| {
        name.trim().is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control)
    }) {
        return Err(ApiError::bad(
            "Укажите название группы длиной 1-80 символов.",
        ));
    }
    if input
        .description
        .as_ref()
        .is_some_and(|text| text.chars().count() > 1000)
    {
        return Err(ApiError::bad("Описание группы: до 1 000 символов."));
    }
    for filters in [
        &input.publish_filters,
        &input.subscribe_filters,
        &input.deny_publish_filters,
        &input.deny_subscribe_filters,
    ]
    .into_iter()
    .flatten()
    {
        validate_filters(filters)?;
    }
    Ok(())
}

async fn replace_group_rules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    input: &GroupChanges,
) -> ApiResult<()> {
    for (filters, publish, allow) in [
        (&input.publish_filters, true, true),
        (&input.subscribe_filters, false, true),
        (&input.deny_publish_filters, true, false),
        (&input.deny_subscribe_filters, false, false),
    ] {
        let Some(filters) = filters else {
            continue;
        };
        sqlx::query("DELETE FROM mqtt_acl_rules WHERE group_id=$1 AND mqtt_user_id IS NULL AND allow=$2 AND CASE WHEN $3 THEN publish ELSE subscribe END")
            .bind(id).bind(allow).bind(publish).execute(&mut **tx).await.map_err(|_| ApiError::internal())?;
        for filter in filters.iter().collect::<HashSet<_>>() {
            sqlx::query("INSERT INTO mqtt_acl_rules(id,group_id,filter,publish,subscribe,allow,priority) VALUES($1,$2,$3,$4,$5,$6,0)")
                .bind(Uuid::new_v4()).bind(id).bind(filter).bind(publish).bind(!publish).bind(allow)
                .execute(&mut **tx).await.map_err(|_| ApiError::internal())?;
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mqtt_acl_rules WHERE group_id=$1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| ApiError::internal())?;
    if count > 100 {
        return Err(ApiError::bad(
            "В одной группе допускается до 100 правил доступа.",
        ));
    }
    Ok(())
}

fn group_error(error: sqlx::Error) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
    {
        ApiError(
            StatusCode::CONFLICT,
            "GROUP_EXISTS",
            "Такое название группы уже используется.",
        )
    } else {
        ApiError::internal()
    }
}

pub(super) async fn create_group(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<GroupChanges>,
) -> ApiResult<Response> {
    let actor = identity(&state, &headers, true).await?;
    validate_group(&input)?;
    let name = input
        .name
        .as_ref()
        .ok_or_else(|| ApiError::bad("Укажите название группы."))?;
    let id = Uuid::new_v4();
    let mut tx = begin_access(&state.pool).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mqtt_access_groups")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    if count >= 1000 {
        return Err(ApiError::bad("Достигнут лимит 1 000 групп доступа."));
    }
    sqlx::query("INSERT INTO mqtt_access_groups(id,name,description,enabled) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(name)
        .bind(input.description.as_deref().unwrap_or(""))
        .bind(input.enabled.unwrap_or(true))
        .execute(&mut *tx)
        .await
        .map_err(group_error)?;
    replace_group_rules(&mut tx, id, &input).await?;
    write_audit(&mut tx, &actor.username, "access_group.create", Some(name)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    Ok((StatusCode::CREATED, Json(json!({"id":id}))).into_response())
}

async fn member_names(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> ApiResult<HashSet<String>> {
    let names: Vec<String> = sqlx::query_scalar("SELECT u.username FROM mqtt_users u JOIN mqtt_user_groups ug ON ug.mqtt_user_id=u.id WHERE ug.group_id=$1")
        .bind(id).fetch_all(&mut **tx).await.map_err(|_| ApiError::internal())?;
    Ok(names.into_iter().collect())
}

async fn reconnect_members(state: &WebState, names: &HashSet<String>) {
    for client in state.broker.clients().await {
        if names.contains(&client.username) {
            state
                .broker
                .disconnect(&client.username, &client.client_id)
                .await;
        }
    }
}

pub(super) async fn update_group(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<GroupChanges>,
) -> ApiResult<Json<Value>> {
    let actor = identity(&state, &headers, true).await?;
    validate_group(&input)?;
    let mut tx = begin_access(&state.pool).await?;
    let old_name: String =
        sqlx::query_scalar("SELECT name FROM mqtt_access_groups WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or(ApiError(
                StatusCode::NOT_FOUND,
                "GROUP_NOT_FOUND",
                "Группа отсутствует. Обновите список.",
            ))?;
    let affected = member_names(&mut tx, id).await?;
    sqlx::query("UPDATE mqtt_access_groups SET name=COALESCE($2,name),description=COALESCE($3,description),enabled=COALESCE($4,enabled) WHERE id=$1")
        .bind(id).bind(&input.name).bind(&input.description).bind(input.enabled).execute(&mut *tx).await.map_err(group_error)?;
    replace_group_rules(&mut tx, id, &input).await?;
    write_audit(
        &mut tx,
        &actor.username,
        "access_group.update",
        Some(input.name.as_deref().unwrap_or(&old_name)),
    )
    .await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    reconnect_members(&state, &affected).await;
    Ok(Json(json!({"id":id})))
}

pub(super) async fn delete_group(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let actor = identity(&state, &headers, true).await?;
    let mut tx = begin_access(&state.pool).await?;
    let name: String =
        sqlx::query_scalar("SELECT name FROM mqtt_access_groups WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or(ApiError(
                StatusCode::NOT_FOUND,
                "GROUP_NOT_FOUND",
                "Группа отсутствует. Обновите список.",
            ))?;
    let affected = member_names(&mut tx, id).await?;
    sqlx::query("DELETE FROM mqtt_access_groups WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ApiError::internal())?;
    write_audit(&mut tx, &actor.username, "access_group.delete", Some(&name)).await?;
    tx.commit().await.map_err(|_| ApiError::internal())?;
    reconnect_members(&state, &affected).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AccessCheck {
    username: String,
    action: crate::acl::AccessAction,
    topic: String,
}

pub(super) async fn check_access(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(input): Json<AccessCheck>,
) -> ApiResult<Json<Value>> {
    identity(&state, &headers, true).await?;
    validate_username(&input.username)?;
    let valid = match input.action {
        crate::acl::AccessAction::Subscribe => {
            crate::domain::parse_subscription_filter(&input.topic).is_some()
        }
        _ => crate::broker::valid_topic(&input.topic),
    };
    if !valid {
        return Err(ApiError::bad(
            "Укажите MQTT-топик; для подписки допустим MQTT-фильтр.",
        ));
    }
    let store = crate::store::Store::from_pool(state.pool.clone());
    let decision = store
        .check_access(&input.username, &input.topic, input.action)
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(
        json!({"username":input.username,"topic":input.topic,"action":input.action,"decision":decision}),
    ))
}
