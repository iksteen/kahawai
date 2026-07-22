//! Minimal browse API (HUB-12 first cut): collections, items, item detail
//! with full technical stream info.
//!
//! ponytail: no auth yet — the binary binds this to 127.0.0.1 by default
//! until the users/token slice (HUB-10/11) lands. Do not expose before then.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::Row;

use crate::registry::Registry;

pub fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/api/v1/collections", get(list_collections))
        .route("/api/v1/items", get(list_items))
        .route("/api/v1/items/{id}", get(item_detail))
        .with_state(registry)
}

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn list_collections(State(reg): State<Arc<Registry>>) -> Result<Json<Value>, ApiError> {
    let cols = reg.collections().await.map_err(internal)?;
    Ok(Json(json!({ "collections": cols })))
}

async fn list_items(State(reg): State<Arc<Registry>>) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        "SELECT i.id, i.title, i.year, COUNT(s.item_id) AS sources
         FROM items i LEFT JOIN item_sources s ON s.item_id = i.id
         GROUP BY i.id ORDER BY i.title, i.year",
    )
    .fetch_all(reg.db())
    .await
    .map_err(internal)?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "title": r.get::<String, _>("title"),
                "year": r.get::<Option<i64>, _>("year"),
                "sources": r.get::<i64, _>("sources"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn item_detail(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let item = sqlx::query("SELECT id, title, year FROM items WHERE id = ?")
        .bind(&id)
        .fetch_optional(reg.db())
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such item".to_string()))?;

    let sources = sqlx::query(
        "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE s.item_id = ? ORDER BY f.size DESC",
    )
    .bind(&id)
    .fetch_all(reg.db())
    .await
    .map_err(internal)?;

    let sources: Vec<Value> = sources
        .iter()
        .map(|r| {
            let module_id: String = r.get("module_id");
            let streams: Value =
                serde_json::from_str(r.get::<String, _>("streams_json").as_str())
                    .unwrap_or(Value::Null);
            json!({
                "module_id": module_id,
                "collection_id": r.get::<String, _>("collection_id"),
                "path_rel": r.get::<String, _>("path_rel"),
                "size": r.get::<i64, _>("size"),
                "available": reg.is_connected(&module_id),
                "streams": streams,
            })
        })
        .collect();

    Ok(Json(json!({
        "id": item.get::<String, _>("id"),
        "title": item.get::<String, _>("title"),
        "year": item.get::<Option<i64>, _>("year"),
        "sources": sources,
    })))
}
