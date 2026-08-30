use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{ensure_loaded, resolve_model, GatewayError, SharedState};
use crate::evals::{
    compare_baseline, export_json, export_jsonl, run_suite, EvalCancellation, EvalFuture,
    EvalRunOptions, EvalRunner, InferenceOutput, InferenceRequest, Redactor, RegressionThresholds,
    TokenUsage,
};
use crate::events::EventTopic;
use crate::registry::ModelRoute;

const MAX_CONCURRENCY: usize = 8;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_REDACTIONS: usize = 64;
const MAX_REDACTION_CHARS: usize = 4_096;
const MAX_UPSTREAM_BYTES: usize = 2 * 1024 * 1024;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/evals/suites", get(list_suites))
        .route("/api/evals/suites/:id", get(get_suite).put(put_suite))
        .route("/api/evals/suites/:id/runs", get(list_runs))
        .route("/api/evals/suites/:suite_id/runs/:run_id", get(get_run))
        .route(
            "/api/evals/suites/:suite_id/runs/:run_id/export",
            get(export_run),
        )
        .route("/api/evals/runs", post(start_run))
        .route(
            "/api/evals/jobs/:run_id",
            get(job_status).delete(cancel_run),
        )
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn storage_error(context: &str, error: anyhow::Error) -> Response {
    tracing::warn!("{context}: {error:#}");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, context)
}

async fn list_suites(State(app): State<SharedState>) -> Response {
    match app.eval_store.list_suites() {
        Ok(suites) => Json(json!({"suites": suites})).into_response(),
        Err(error) => storage_error("failed to list evaluation suites", error),
    }
}

async fn get_suite(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    match app.eval_store.load_suite(&id) {
        Ok(Some(suite)) => Json(suite).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "evaluation suite not found"),
        Err(error) => storage_error("failed to load evaluation suite", error),
    }
}

async fn put_suite(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(suite): Json<crate::evals::EvalSuite>,
) -> Response {
    if id != suite.id {
        return api_error(StatusCode::BAD_REQUEST, "suite id does not match path");
    }
    if let Err(error) = suite.validate() {
        return api_error(StatusCode::BAD_REQUEST, error.to_string());
    }
    match app.eval_store.save_suite(&suite) {
        Ok(()) => Json(suite).into_response(),
        Err(error) => storage_error("failed to save evaluation suite", error),
    }
}

async fn list_runs(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    match app.eval_store.list_runs(&id) {
        Ok(runs) => Json(json!({"runs": runs})).into_response(),
        Err(error) => storage_error("failed to list evaluation runs", error),
    }
}

async fn get_run(
    State(app): State<SharedState>,
    Path((suite_id, run_id)): Path<(String, String)>,
) -> Response {
    match app.eval_store.load_run(&suite_id, &run_id) {
        Ok(Some(run)) => Json(run).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "evaluation run not found"),
        Err(error) => storage_error("failed to load evaluation run", error),
    }
}

#[derive(Deserialize)]
struct ExportQuery {
    #[serde(default = "default_export_format")]
    format: String,
}

fn default_export_format() -> String {
    "json".to_string()
}

async fn export_run(
    State(app): State<SharedState>,
    Path((suite_id, run_id)): Path<(String, String)>,
    Query(query): Query<ExportQuery>,
) -> Response {
    let run = match app.eval_store.load_run(&suite_id, &run_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "evaluation run not found");
        }
        Err(error) => return storage_error("failed to load evaluation run", error),
    };
    let redactor = Redactor::default();
    let exported = match query.format.as_str() {
        "json" => export_json(&run, &redactor),
        "jsonl" => export_jsonl(&run, &redactor),
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "export format must be json or jsonl",
            );
        }
    };
    match exported {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                if query.format == "jsonl" {
                    "application/x-ndjson"
                } else {
                    "application/json"
                },
            )],
            body,
        )
            .into_response(),
        Err(error) => storage_error("failed to export evaluation run", error),
    }
}

#[derive(Deserialize)]
struct StartRunRequest {
    suite_id: String,
    #[serde(default)]
    baseline_run_id: Option<String>,
    #[serde(default)]
    thresholds: RegressionThresholds,
    #[serde(default = "default_concurrency")]
    max_concurrency: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    redactions: Vec<String>,
}

fn default_concurrency() -> usize {
    2
}

fn default_timeout_ms() -> u64 {
    120_000
}

async fn start_run(State(app): State<SharedState>, Json(body): Json<StartRunRequest>) -> Response {
    if !(1..=MAX_CONCURRENCY).contains(&body.max_concurrency) {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("max concurrency must be between 1 and {MAX_CONCURRENCY}"),
        );
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&body.timeout_ms) {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("timeout must be between 1 and {MAX_TIMEOUT_MS} ms"),
        );
    }
    if body.redactions.len() > MAX_REDACTIONS
        || body
            .redactions
            .iter()
            .any(|value| value.chars().count() > MAX_REDACTION_CHARS)
    {
        return api_error(StatusCode::BAD_REQUEST, "redaction list is too large");
    }
    if let Err(error) = body.thresholds.validate() {
        return api_error(StatusCode::BAD_REQUEST, error.to_string());
    }
    let suite = match app.eval_store.load_suite(&body.suite_id) {
        Ok(Some(suite)) => suite,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "evaluation suite not found");
        }
        Err(error) => return storage_error("failed to load evaluation suite", error),
    };
    let baseline = match body.baseline_run_id.as_deref() {
        Some(run_id) => match app.eval_store.load_run(&body.suite_id, run_id) {
            Ok(Some(run)) => Some(run),
            Ok(None) => {
                return api_error(StatusCode::NOT_FOUND, "baseline run not found");
            }
            Err(error) => {
                return storage_error("failed to load baseline run", error);
            }
        },
        None => None,
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let cancellation = EvalCancellation::default();
    app.eval_jobs
        .write()
        .insert(run_id.clone(), cancellation.clone());

    let task_app = app.clone();
    let task_run_id = run_id.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let event_bus = task_app.events.clone();
        let event_run_id = task_run_id.clone();
        let progress = Arc::new(move |progress| {
            event_bus.publish(
                EventTopic::Evaluation,
                "evaluation_progress",
                Some(event_run_id.clone()),
                serde_json::to_value(progress).unwrap_or_else(|_| json!({})),
            );
        });
        let redactor = Redactor::new(body.redactions);
        let options = EvalRunOptions {
            max_concurrency: body.max_concurrency,
            default_timeout: Duration::from_millis(body.timeout_ms),
            ..EvalRunOptions::default()
        };
        let runner: Arc<dyn EvalRunner> = Arc::new(GatewayEvalRunner {
            app: task_app.clone(),
        });
        let outcome = run_suite(
            &suite,
            task_run_id.clone(),
            runner,
            options,
            cancellation,
            redactor.clone(),
            Some(progress),
        )
        .await;
        match outcome {
            Ok(mut run) => {
                if let Some(baseline) = baseline {
                    match compare_baseline(&run, &baseline, &body.thresholds) {
                        Ok(comparison) => run.baseline = Some(comparison),
                        Err(error) => {
                            publish_failure(&task_app, &task_run_id, &error);
                            task_app.eval_jobs.write().remove(&task_run_id);
                            return;
                        }
                    }
                }
                if let Err(error) = task_app.eval_store.save_run(&run, &redactor) {
                    publish_failure(&task_app, &task_run_id, &error);
                    task_app.eval_jobs.write().remove(&task_run_id);
                    return;
                }
                task_app.events.publish(
                    EventTopic::Evaluation,
                    "evaluation_complete",
                    Some(task_run_id.clone()),
                    json!({
                        "suite_id": run.suite_id,
                        "metrics": run.metrics,
                        "baseline": run.baseline,
                    }),
                );
                let status = if run.metrics.cancelled > 0 {
                    crate::telemetry::OperationStatus::Cancelled
                } else {
                    crate::telemetry::OperationStatus::Success
                };
                let _ = task_app.telemetry.record_evaluation(
                    "run_suite",
                    status,
                    Some(started.elapsed()),
                    None,
                    None,
                );
            }
            Err(error) => publish_failure(&task_app, &task_run_id, &error),
        }
        task_app.eval_jobs.write().remove(&task_run_id);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "running": true})),
    )
        .into_response()
}

fn publish_failure(app: &SharedState, run_id: &str, error: &anyhow::Error) {
    tracing::warn!("evaluation {run_id} failed: {error:#}");
    app.events.publish(
        EventTopic::Evaluation,
        "evaluation_failed",
        Some(run_id.to_string()),
        json!({"error": "evaluation failed"}),
    );
    let _ = app.telemetry.record_evaluation(
        "run_suite",
        crate::telemetry::OperationStatus::Error,
        None,
        None,
        Some("evaluation failed"),
    );
}

async fn job_status(
    State(app): State<SharedState>,
    Path(run_id): Path<String>,
) -> Json<serde_json::Value> {
    Json(json!({
        "run_id": run_id,
        "running": app.eval_jobs.read().contains_key(&run_id),
    }))
}

async fn cancel_run(State(app): State<SharedState>, Path(run_id): Path<String>) -> Response {
    let cancellation = app.eval_jobs.read().get(&run_id).cloned();
    match cancellation {
        Some(cancellation) => {
            cancellation.cancel();
            StatusCode::NO_CONTENT.into_response()
        }
        None => api_error(StatusCode::NOT_FOUND, "evaluation run is not active"),
    }
}

struct GatewayEvalRunner {
    app: SharedState,
}

impl EvalRunner for GatewayEvalRunner {
    fn infer(&self, request: InferenceRequest) -> EvalFuture {
        let app = self.app.clone();
        Box::pin(async move { gateway_infer(app, request).await })
    }
}

async fn gateway_infer(
    app: SharedState,
    request: InferenceRequest,
) -> anyhow::Result<InferenceOutput> {
    let canonical = resolve_model(&app, &request.model)
        .ok_or_else(|| anyhow::anyhow!("evaluation model was not found"))?;
    let route = ensure_loaded(&app, &canonical)
        .await
        .map_err(gateway_error)?;
    let mut body = json!({
        "model": request.model,
        "messages": [{"role": "user", "content": request.prompt}],
        "temperature": request.settings.temperature,
        "max_tokens": request.settings.max_tokens,
        "stream": false,
    });
    if let Some(seed) = request.settings.seed {
        body["seed"] = json!(seed);
    }
    let (url, bearer, lease) = match route {
        ModelRoute::Local(lease) => (
            format!("http://127.0.0.1:{}/v1/chat/completions", lease.port()),
            None,
            Some(lease),
        ),
        ModelRoute::Cloud => {
            let token = crate::cloud::ensure_token()
                .map_err(|_| anyhow::anyhow!("cloud authentication is required"))?;
            let model = app
                .registry
                .get_clone(&canonical)
                .and_then(|state| state.cloud_model_id)
                .unwrap_or(canonical);
            body["model"] = json!(model);
            (
                format!(
                    "{}/api/v1/chat/completions",
                    app.config.cloud.api_base_url.trim_end_matches('/')
                ),
                Some(token),
                None,
            )
        }
    };
    let client = reqwest::Client::new();
    let mut outgoing = client.post(&url).json(&body);
    if let Some(token) = bearer {
        outgoing = outgoing.bearer_auth(token);
    }
    let mut response = outgoing.send().await.map_err(|error| {
        anyhow::anyhow!(super::upstream_unreachable("evaluation inference", &error))
    })?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        anyhow::anyhow!(super::upstream_unreachable("evaluation inference", &error))
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_UPSTREAM_BYTES {
            return Err(anyhow::anyhow!(
                "evaluation inference response exceeded the capture limit"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    drop(lease);
    if !status.is_success() {
        return Err(anyhow::anyhow!(super::upstream_error(
            "evaluation inference",
            status,
            &String::from_utf8_lossy(&bytes),
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(super::upstream_malformed("evaluation inference", &error))
    })?;
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("evaluation inference returned no text"))?
        .to_string();
    Ok(InferenceOutput {
        text,
        usage: TokenUsage {
            prompt_tokens: value
                .pointer("/usage/prompt_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            completion_tokens: value
                .pointer("/usage/completion_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        },
        tool_trace: None,
    })
}

fn gateway_error(error: GatewayError) -> anyhow::Error {
    match error {
        GatewayError::CachedUnsupported(message)
        | GatewayError::InsufficientRam(message)
        | GatewayError::LoadFailed(message)
        | GatewayError::AuthRequired(message) => anyhow::anyhow!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> SharedState {
        let (tunnel_listener, _) = tokio::sync::mpsc::channel(1);
        Arc::new(crate::api::AppState::new(
            crate::config::Config::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            crate::state::State::default(),
            crate::settings::Settings::default(),
            crate::chats::Chats::default(),
            tunnel_listener,
        ))
    }

    #[tokio::test]
    async fn saves_and_lists_validated_suites() {
        let app = routes().with_state(test_state());
        let suite = json!({
            "id": "smoke",
            "name": "Smoke",
            "default_model": "model-a",
            "cases": [{
                "id": "hello",
                "prompt": "say hello",
                "assertions": [{"type": "contains", "expected": "hello"}],
            }],
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/evals/suites/smoke")
                    .header("content-type", "application/json")
                    .body(Body::from(suite.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/evals/suites")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["suites"][0]["id"], "smoke");
    }
}
