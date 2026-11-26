use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use crate::{
    App,
    auth::Identity,
    zone::{self, Resources},
};

type Reply = Result<Json<Value>, ApiError>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/zone", get(list).post(create))
        .route("/zone/{id}", get(get_zone).put(update).delete(remove))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Input {
    id: String,
    limits: Resources,
}

fn parse(input: Result<Json<Input>, JsonRejection>) -> Result<Input, ApiError> {
    input
        .map(|Json(input)| input)
        .map_err(|_| ApiError::bad_request("invalid JSON"))
}

async fn list(State(app): State<Arc<App>>, Extension(actor): Extension<Identity>) -> Reply {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, document FROM zone ORDER BY id")
        .fetch_all(&app.store.0)
        .await?;
    let entries = app.processes.list().await;
    let mut result = Vec::new();
    for (id, row) in rows {
        if id == zone::DEFAULT || actor.allows("", "zone", &id, "read") {
            result.push(describe(&id, &row, &entries)?);
        }
    }
    Ok(Json(json!(result)))
}

async fn get_zone(State(app): State<Arc<App>>, Path(id): Path<String>) -> Reply {
    let row: String = sqlx::query_scalar("SELECT document FROM zone WHERE id=?")
        .bind(&id)
        .fetch_optional(&app.store.0)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(describe(&id, &row, &app.processes.list().await)?))
}

fn describe(id: &str, row: &str, entries: &[Arc<crate::engine::Entry>]) -> Result<Value, ApiError> {
    let limits: Resources = serde_json::from_str(row)?;
    let mut usage = Resources::default();
    let mut count = 0usize;
    for entry in entries {
        let job = entry.job();
        if zone::canonical(&job.config.zone) == id {
            usage = usage.plus(Resources::claim(&job));
            count += 1;
        }
    }
    Ok(json!({"id":id, "limits":limits, "usage":usage, "processes":count}))
}

async fn create(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    input: Result<Json<Input>, JsonRejection>,
) -> Reply {
    save(&app, &actor, parse(input)?, None).await
}

async fn update(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(id): Path<String>,
    input: Result<Json<Input>, JsonRejection>,
) -> Reply {
    save(&app, &actor, parse(input)?, Some(id)).await
}

async fn save(app: &App, actor: &Identity, mut input: Input, path: Option<String>) -> Reply {
    if let Some(id) = &path {
        if input.id.is_empty() {
            input.id.clone_from(id);
        } else if &input.id != id {
            return Err(ApiError::bad_request("zone id cannot change"));
        }
    }
    if !zone::valid_id(&input.id) || !input.limits.valid() {
        return Err(ApiError::bad_request("invalid zone or limits"));
    }
    let mut tx = app.store.write().await?;
    let document = serde_json::to_string(&input.limits)?;
    let rows = if path.is_some() {
        sqlx::query("UPDATE zone SET document=? WHERE id=?")
            .bind(&document)
            .bind(&input.id)
            .execute(&mut *tx)
            .await?
    } else {
        sqlx::query("INSERT INTO zone(id, document) VALUES (?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&input.id)
            .bind(&document)
            .execute(&mut *tx)
            .await?
    };
    if rows.rows_affected() == 0 {
        return Err(if path.is_some() {
            ApiError::not_found()
        } else {
            ApiError::Http(StatusCode::CONFLICT, "zone already exists")
        });
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM zone")
        .fetch_one(&mut *tx)
        .await?;
    if count > 1024 {
        return Err(ApiError::Http(
            StatusCode::SERVICE_UNAVAILABLE,
            "zone capacity reached",
        ));
    }
    super::operations::record(
        &mut tx,
        "zone",
        if path.is_some() { "update" } else { "create" },
        Some(actor.name().unwrap_or("$root")),
        Some(&input.id),
    )
    .await?;
    tx.commit().await?;
    app.ops.emit(
        "zone",
        if path.is_some() { "update" } else { "create" },
        &input.id,
        "",
    );
    Ok(Json(json!({"id":input.id,"limits":input.limits})))
}

async fn remove(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(id): Path<String>,
) -> Reply {
    if id == zone::DEFAULT {
        return Err(ApiError::bad_request("default zone cannot be deleted"));
    }
    let mut tx = app.store.write().await?;
    let in_use: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM process_jobs WHERE json_extract(configuration,'$.zone')=?)",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    if in_use {
        return Err(ApiError::Http(StatusCode::CONFLICT, "zone is in use"));
    }
    if sqlx::query("DELETE FROM zone WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(ApiError::not_found());
    }
    super::operations::record(
        &mut tx,
        "zone",
        "delete",
        Some(actor.name().unwrap_or("$root")),
        Some(&id),
    )
    .await?;
    tx.commit().await?;
    app.ops.emit("zone", "delete", &id, "");
    Ok(Json(json!("OK")))
}
