use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::SharedState;
use crate::platform::{DiagnosticReport, ExecutableRequirement, PreflightOptions, Severity};
use crate::telemetry::{HealthComponent, HealthInput, HealthStatus};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/metrics", get(metrics))
        .route("/api/telemetry", get(telemetry))
        .route("/api/diagnostics", get(diagnostics))
}

pub(crate) fn preflight_options(config: &crate::config::Config) -> PreflightOptions {
    PreflightOptions {
        listener: config.http_listen.clone(),
        required_executables: vec![
            ExecutableRequirement {
                name: config.llama_server_bin.clone(),
                required: true,
                probe_args: vec!["--version".to_string()],
            },
            ExecutableRequirement {
                name: config.tunnel.cloudflared_bin.clone(),
                required: false,
                probe_args: vec!["--version".to_string()],
            },
            ExecutableRequirement {
                name: config.dsh.bin.clone(),
                required: false,
                probe_args: vec!["--version".to_string()],
            },
        ],
        ..PreflightOptions::default()
    }
}

async fn health(State(app): State<SharedState>) -> Response {
    let summary = app.telemetry.health_summary();
    let status = if summary.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(summary)).into_response()
}

async fn metrics(State(app): State<SharedState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        app.telemetry.render_prometheus(),
    )
        .into_response()
}

async fn telemetry(State(app): State<SharedState>) -> Json<crate::telemetry::TelemetrySnapshot> {
    Json(app.telemetry.snapshot())
}

async fn diagnostics(State(app): State<SharedState>) -> Response {
    let options = preflight_options(&app.config);
    let report =
        match tokio::task::spawn_blocking(move || crate::platform::run_preflight(&options)).await {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!("platform preflight task failed: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "platform diagnostics failed"})),
                )
                    .into_response();
            }
        };
    update_health(&app, &report);
    Json(json!({
        "platform": report,
        "telemetry": app.telemetry.snapshot(),
    }))
    .into_response()
}

fn update_health(app: &SharedState, report: &DiagnosticReport) {
    let storage_ready = report.paths.iter().all(|path| path.writable);
    let storage_warning = report.paths.iter().any(|path| {
        path.available_bytes
            .is_some_and(|available| available < crate::platform::DEFAULT_MIN_FREE_SPACE_BYTES)
    });
    set_health(
        app,
        HealthComponent::Storage,
        storage_ready,
        true,
        storage_warning,
        (!storage_ready).then_some("managed storage is not writable"),
    );
    set_health(
        app,
        HealthComponent::Cache,
        storage_ready,
        false,
        storage_warning,
        storage_warning.then_some("managed storage has low free space"),
    );
    let engine_ready = report
        .executables
        .iter()
        .filter(|executable| executable.required)
        .all(|executable| executable.found && executable.probe_exit_code == Some(0));
    set_health(
        app,
        HealthComponent::Engine,
        engine_ready,
        true,
        false,
        (!engine_ready).then_some("required inference engine is unavailable"),
    );
    let has_error = report
        .findings
        .iter()
        .any(|finding| finding.severity == Severity::Error);
    set_health(
        app,
        HealthComponent::Daemon,
        !has_error,
        true,
        !has_error && !report.findings.is_empty(),
        has_error.then_some("platform preflight found blocking issues"),
    );
}

fn set_health(
    app: &SharedState,
    component: HealthComponent,
    ready: bool,
    required: bool,
    degraded: bool,
    detail: Option<&str>,
) {
    app.telemetry.set_health(HealthInput {
        component,
        status: if !ready {
            HealthStatus::Unhealthy
        } else if degraded {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        },
        ready,
        required,
        detail,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::Arc;
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
    async fn health_metrics_and_request_correlation_are_exposed() {
        let state = test_state();
        let app = crate::api::build_router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("x-request-id", "request-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-request-id"], "request-123");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("kiralm_operations_total"));
        assert!(body.contains("kiralm_ready 1"));
    }
}
