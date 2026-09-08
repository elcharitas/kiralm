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

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route(
            "/api/projects/:project_id/terminals",
            get(list).post(create),
        )
        .route("/api/terminals/:id", get(get_terminal).delete(close))
        .route("/api/terminals/:id/input", post(input))
        .route("/api/terminals/:id/resize", post(resize))
        .route("/api/terminals/:id/stream", get(stream))
}

#[derive(Deserialize)]
struct CreateTerminalRequest {
    title: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_cols() -> u16 {
    100
}

fn default_rows() -> u16 {
    30
}

async fn create(
    State(app): State<SharedState>,
    Path(project_id): Path<String>,
    Json(body): Json<CreateTerminalRequest>,
) -> Response {
    let root = match project_path(&app, &project_id) {
        Ok(root) => root,
        Err(response) => return response,
    };
    match app.terminals.create(
        &project_id,
        &root,
        body.title.as_deref(),
        body.cols,
        body.rows,
    ) {
        Ok(terminal) => {
            let hook = app
                .policies
                .run_hooks(
                    crate::policies::HookPhase::TerminalStarted,
                    &project_id,
                    None,
                    &root,
                    serde_json::to_value(&terminal).unwrap_or_default(),
                )
                .await;
            if let Err(message) = hook {
                let _ = app.terminals.close(&terminal.id);
                return error(StatusCode::FORBIDDEN, message);
            }
            (StatusCode::CREATED, Json(terminal)).into_response()
        }
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn list(State(app): State<SharedState>, Path(project_id): Path<String>) -> Response {
    if let Err(response) = project_path(&app, &project_id) {
        return response;
    }
    Json(json!({
        "terminals": app.terminals.list(Some(&project_id)),
    }))
    .into_response()
}

async fn get_terminal(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    match app.terminals.get(&id) {
        Some(terminal) => Json(terminal).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("terminal '{id}' not found")),
    }
}

#[derive(Deserialize)]
struct TerminalInputRequest {
    data: String,
}

async fn input(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<TerminalInputRequest>,
) -> Response {
    match app.terminals.input(&id, body.data.as_bytes()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

#[derive(Deserialize)]
struct ResizeRequest {
    cols: u16,
    rows: u16,
}

async fn resize(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<ResizeRequest>,
) -> Response {
    match app.terminals.resize(&id, body.cols, body.rows) {
        Ok(terminal) => Json(terminal).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn close(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    match app.terminals.close(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => error(StatusCode::NOT_FOUND, message),
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    after: u64,
}

async fn stream(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
) -> Response {
    let mut receiver = match app.terminals.subscribe(&id) {
        Ok(receiver) => receiver,
        Err(message) => return error(StatusCode::NOT_FOUND, message),
    };
    let replay = match app.terminals.replay(&id, query.after) {
        Ok(replay) => replay,
        Err(message) => return error(StatusCode::NOT_FOUND, message),
    };
    let events = async_stream::stream! {
        let mut last_sequence = query.after;
        for chunk in replay {
            last_sequence = last_sequence.max(chunk.sequence);
            yield Ok::<Event, Infallible>(sse_event(&chunk));
        }
        loop {
            match receiver.recv().await {
                Ok(chunk) if chunk.sequence > last_sequence => {
                    last_sequence = chunk.sequence;
                    yield Ok(sse_event(&chunk));
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

fn sse_event(chunk: &crate::terminals::TerminalOutput) -> Event {
    Event::default()
        .json_data(chunk)
        .unwrap_or_else(|_| Event::default())
}

#[allow(clippy::result_large_err)]
fn project_path(app: &SharedState, id: &str) -> Result<std::path::PathBuf, Response> {
    app.state
        .read()
        .projects
        .get(id)
        .map(|project| std::path::PathBuf::from(&project.path))
        .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("project '{id}' not found")))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}
