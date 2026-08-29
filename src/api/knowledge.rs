use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;
use crate::events::EventTopic;
use crate::knowledge::IndexCancellation;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/knowledge", get(status))
        .route(
            "/api/knowledge/index",
            post(start_index).delete(cancel_index),
        )
        .route("/api/knowledge/search", post(search))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn status(State(app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({
        "snapshot": app.knowledge.snapshot(),
        "running": app.knowledge_job.lock().is_some(),
        "recovered_corrupt_metadata": app
            .knowledge
            .recovered_corrupt_metadata(),
    }))
}

async fn start_index(State(app): State<SharedState>) -> Response {
    let projects: Vec<_> = app
        .state
        .read()
        .projects
        .iter()
        .map(|(id, project)| {
            (id.clone(), std::path::PathBuf::from(&project.path))
        })
        .collect();
    let mut roots = Vec::new();
    for (project_id, root) in projects {
        roots.push(root);
        if let Some(preset) = app.workspace_preset_for(&project_id) {
            roots.extend(preset.knowledge_roots);
        }
    }
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        return error(StatusCode::BAD_REQUEST, "no workspaces are registered");
    }
    let cancellation = IndexCancellation::default();
    {
        let mut active = app.knowledge_job.lock();
        if active.is_some() {
            return error(StatusCode::CONFLICT, "knowledge indexing is running");
        }
        *active = Some(cancellation.clone());
    }

    let knowledge = app.knowledge.clone();
    let events = app.events.clone();
    let app_task = app.clone();
    tokio::spawn(async move {
        let progress_events = events.clone();
        let result = tokio::task::spawn_blocking(move || {
            knowledge.index_roots(&roots, &cancellation, |progress| {
                progress_events.publish(
                    EventTopic::Knowledge,
                    "knowledge_progress",
                    None,
                    serde_json::to_value(progress).unwrap_or_else(|_| json!({})),
                );
            })
        })
        .await;
        *app_task.knowledge_job.lock() = None;
        match result {
            Ok(Ok(report)) => app_task.events.publish(
                EventTopic::Knowledge,
                "knowledge_complete",
                None,
                serde_json::to_value(report).unwrap_or_else(|_| json!({})),
            ),
            Ok(Err(error)) => app_task.events.publish(
                EventTopic::Knowledge,
                "knowledge_failed",
                None,
                json!({"error": error.to_string()}),
            ),
            Err(error) => app_task.events.publish(
                EventTopic::Knowledge,
                "knowledge_failed",
                None,
                json!({"error": error.to_string()}),
            ),
        };
    });

    (StatusCode::ACCEPTED, Json(json!({"started": true}))).into_response()
}

async fn cancel_index(State(app): State<SharedState>) -> Response {
    let active = app.knowledge_job.lock().clone();
    match active {
        Some(cancellation) => {
            cancellation.cancel();
            StatusCode::NO_CONTENT.into_response()
        }
        None => error(StatusCode::NOT_FOUND, "knowledge indexing is not running"),
    }
}

#[derive(Deserialize)]
struct SearchRequest {
    project_id: String,
    query: String,
    limit: Option<usize>,
}

async fn search(State(app): State<SharedState>, Json(body): Json<SearchRequest>) -> Response {
    if body.query.len() > 10_000 {
        return error(StatusCode::BAD_REQUEST, "knowledge query is too long");
    }
    let root = app
        .state
        .read()
        .projects
        .get(&body.project_id)
        .map(|project| std::path::PathBuf::from(&project.path));
    let Some(root) = root else {
        return error(StatusCode::NOT_FOUND, "workspace not found");
    };
    let mut roots = vec![root];
    if let Some(preset) = app.workspace_preset_for(&body.project_id) {
        roots.extend(preset.knowledge_roots);
    }
    match app
        .knowledge
        .search_roots(&body.query, &roots, body.limit.unwrap_or(8).clamp(1, 20))
    {
        Ok(hits) => Json(json!({"hits": hits})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state(base: &std::path::Path) -> SharedState {
        let project_root = base.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(project_root.join("guide.md"), "workspace retrieval marker").unwrap();
        let now = chrono::Utc::now();
        let mut state = crate::state::State::default();
        state.projects.insert(
            "project-1".to_string(),
            crate::state::Project {
                id: "project-1".to_string(),
                name: "Project".to_string(),
                path: project_root.to_string_lossy().to_string(),
                created_at: now,
                last_active_at: now,
            },
        );
        let knowledge = Arc::new(
            crate::knowledge::KnowledgeIndex::open(
                base.join("metadata/index.json"),
                crate::knowledge::DeterministicEmbedder::default(),
                crate::knowledge::IndexLimits::default(),
            )
            .unwrap(),
        );
        let (tunnel_listener, _) = tokio::sync::mpsc::channel(1);
        Arc::new(crate::api::AppState::new_with_knowledge(
            crate::config::Config::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            state,
            crate::settings::Settings::default(),
            crate::chats::Chats::default(),
            tunnel_listener,
            crate::engine::EngineCapabilities::default(),
            knowledge,
        ))
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn indexes_and_searches_a_registered_workspace() {
        let base =
            std::env::temp_dir().join(format!("kiralm-knowledge-api-{}", uuid::Uuid::new_v4()));
        let state = test_state(&base);
        let app = routes().with_state(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/knowledge/index")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        for _ in 0..100 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/knowledge")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let value = json_body(response).await;
            if value["running"] == false {
                assert_eq!(value["snapshot"]["files"], 1);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/knowledge/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "project_id": "project-1",
                            "query": "retrieval",
                            "limit": 3,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = json_body(response).await;
        assert_eq!(value["hits"][0]["source"]["path"], "guide.md");
        std::fs::remove_dir_all(base).unwrap();
    }
}
