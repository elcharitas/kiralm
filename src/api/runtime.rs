use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;
use crate::events::{AppEvent, EventTopic};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/runtime/events", get(list_events))
        .route("/api/runtime/events/stream", get(stream_events))
        .route(
            "/api/sessions/:id/generation/control",
            post(control_generation),
        )
        .route("/api/checkpoints", get(list_checkpoints))
        .route("/api/checkpoints/:id/restore", post(restore_checkpoint))
        .route("/api/policies", get(get_policies).put(put_policies))
}

#[derive(Clone, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
    topic: Option<EventTopic>,
    resource_id: Option<String>,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

fn default_event_limit() -> usize {
    500
}

async fn list_events(
    State(app): State<SharedState>,
    Query(query): Query<EventQuery>,
) -> Json<serde_json::Value> {
    Json(json!({
        "events": app.events.history(
            query.after,
            query.topic,
            query.resource_id.as_deref(),
            query.limit,
        ),
    }))
}

async fn stream_events(
    State(app): State<SharedState>,
    Query(query): Query<EventQuery>,
) -> Response {
    let mut receiver = app.events.subscribe();
    let replay = app.events.history(
        query.after,
        query.topic,
        query.resource_id.as_deref(),
        query.limit,
    );
    let events = async_stream::stream! {
        let mut last_sequence = query.after;
        for event in replay {
            last_sequence = last_sequence.max(event.sequence);
            yield Ok::<Event, Infallible>(sse_event(&event));
        }
        loop {
            match receiver.recv().await {
                Ok(event)
                    if event.sequence > last_sequence
                        && query.topic.is_none_or(|topic| event.topic == topic)
                        && query.resource_id.as_deref().is_none_or(|id| {
                            event.resource_id.as_deref() == Some(id)
                        }) =>
                {
                    last_sequence = event.sequence;
                    yield Ok(sse_event(&event));
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(10)))
        .into_response()
}

fn sse_event(event: &AppEvent) -> Event {
    Event::default()
        .json_data(event)
        .unwrap_or_else(|_| Event::default())
}

#[derive(Deserialize)]
struct GenerationControlRequest {
    action: String,
    text: Option<String>,
}

async fn control_generation(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GenerationControlRequest>,
) -> Response {
    match app.update_generation_control(&id, &body.action, body.text.as_deref()) {
        Ok(state) => Json(state).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

#[derive(Deserialize)]
struct CheckpointQuery {
    chat_id: Option<String>,
}

async fn list_checkpoints(
    State(app): State<SharedState>,
    Query(query): Query<CheckpointQuery>,
) -> Json<serde_json::Value> {
    Json(json!({
        "checkpoints": app.checkpoints.list(query.chat_id.as_deref()),
    }))
}

#[derive(Deserialize)]
struct RestoreCheckpointRequest {
    chat_id: String,
    project_id: String,
    #[serde(default = "default_restore_mode")]
    mode: String,
}

fn default_restore_mode() -> String {
    "both".to_string()
}

async fn restore_checkpoint(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RestoreCheckpointRequest>,
) -> Response {
    let project = app.state.read().projects.get(&body.project_id).cloned();
    let Some(project) = project else {
        return error(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", body.project_id),
        );
    };
    let root = std::path::PathBuf::from(project.path);
    let (restore_files, restore_conversation) = match body.mode.as_str() {
        "files" => (true, false),
        "conversation" => (false, true),
        "both" => (true, true),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "restore mode must be files, conversation, or both",
            )
        }
    };
    let current_conversation = app
        .chats
        .read()
        .chats
        .iter()
        .find(|chat| chat.id == body.chat_id)
        .map(|chat| chat.messages.clone());
    let restored = match app.checkpoints.restore(
        &id,
        &body.chat_id,
        &body.project_id,
        &root,
        crate::checkpoints::RestoreOptions {
            current_conversation,
            restore_files,
            restore_conversation,
        },
    ) {
        Ok(result) => result,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if let Some(messages) = restored.conversation.clone() {
        let mut chats = app.chats.write();
        let Some(chat) = chats.chats.iter_mut().find(|chat| chat.id == body.chat_id) else {
            return error(StatusCode::NOT_FOUND, "chat not found");
        };
        chat.messages = messages;
        chat.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if let Err(save_error) = chats.save() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, save_error.to_string());
        }
    }
    let hook_warning = app
        .policies
        .run_hooks(
            crate::policies::HookPhase::CheckpointRestored,
            &body.project_id,
            None,
            &root,
            serde_json::to_value(&restored).unwrap_or_default(),
        )
        .await
        .err();
    Json(json!({
        "result": restored,
        "hook_warning": hook_warning,
    }))
    .into_response()
}

async fn get_policies(State(app): State<SharedState>) -> Json<crate::policies::PolicyDocument> {
    Json(app.policies.document())
}

#[derive(Deserialize)]
struct PutPoliciesRequest {
    expected_revision: u64,
    rules: Vec<crate::policies::PolicyRule>,
    hooks: Vec<crate::policies::PolicyHook>,
}

async fn put_policies(
    State(app): State<SharedState>,
    Json(body): Json<PutPoliciesRequest>,
) -> Response {
    match app
        .policies
        .replace(body.expected_revision, body.rules, body.hooks)
    {
        Ok(document) => Json(document).into_response(),
        Err(message) => error(StatusCode::CONFLICT, message),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}
