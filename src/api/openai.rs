//! SPEC.md §2.1: `/v1/chat/completions` (thin reverse proxy to the
//! resolved model's llama-server child) and `/v1/models` (synthesized from
//! the registry, never proxied).

use axum::{
    Json,
    body::{Body, Bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use serde_json::json;

use super::AppState;
use crate::engine::EnsureLoadError;

/// SPEC §2.4 OpenAI error shape: `{"error":{"message","type","param","code"}}`.
fn openai_error(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &str,
    param: Option<&str>,
    code: Option<&str>,
) -> Response {
    let body = json!({
        "error": {
            "message": message.into(),
            "type": error_type,
            "param": param,
            "code": code,
        }
    });
    (status, Json(body)).into_response()
}

/// `GET /v1/models` — every downloaded model regardless of loaded state.
pub async fn list_models(State(state): State<AppState>) -> Response {
    let data: Vec<_> = state
        .engine
        .list_models()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.canonical_id,
                "object": "model",
                "created": m.pulled_at,
                "owned_by": "nodelm",
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// `POST /v1/chat/completions` — resolves `model`, ensures the child is
/// loaded, then forwards the request body byte-for-byte to it. Streaming
/// responses are byte-copied (no re-parsing); non-streaming responses have
/// only the `model` field normalized to the client's requested name.
pub async fn chat_completions(State(state): State<AppState>, body: Bytes) -> Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON body: {e}"),
                "invalid_request_error",
                None,
                None,
            );
        }
    };

    let requested_model = match parsed.get("model").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "you must provide a model parameter",
                "invalid_request_error",
                Some("model"),
                None,
            );
        }
    };
    let stream = parsed
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let canonical_id = match state.engine.resolve(&requested_model) {
        Some(id) => id,
        None => {
            return openai_error(
                StatusCode::NOT_FOUND,
                format!("The model `{requested_model}` does not exist"),
                "invalid_request_error",
                Some("model"),
                Some("model_not_found"),
            );
        }
    };

    let port = match state.engine.ensure_loaded(&canonical_id) {
        Ok(port) => port,
        Err(EnsureLoadError::Unsupported(err)) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                err,
                "invalid_request_error",
                Some("model"),
                Some("model_unsupported"),
            );
        }
        Err(EnsureLoadError::ColdStarting) => {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Model `{requested_model}` is still loading, try again shortly"),
                "server_error",
                None,
                None,
            );
        }
    };

    let upstream = state
        .http
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;

    let upstream_resp = match upstream {
        Ok(r) => r,
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("proxy request to model failed: {e}"),
                "server_error",
                None,
                None,
            );
        }
    };

    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    if stream {
        // Pure byte-copy of the child's SSE stream, no re-parsing.
        let byte_stream = upstream_resp
            .bytes_stream()
            .map_err(std::io::Error::other);
        return Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(byte_stream))
            .unwrap();
    }

    let bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed reading model response: {e}"),
                "server_error",
                None,
                None,
            );
        }
    };

    // Pipe the child's JSON back verbatim except normalizing `model` to
    // the client's requested (possibly alias) name.
    let mut json_body: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(bytes))
                .unwrap();
        }
    };
    if let Some(obj) = json_body.as_object_mut() {
        obj.insert("model".to_string(), json!(requested_model));
    }
    (status, Json(json_body)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        response::{
            IntoResponse,
            sse::{Event, Sse},
        },
        routing::post,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::{
        engine::EngineSupervisor,
        registry::{ModelState, ModelStatus},
    };

    /// Spawns a fake llama-server child on loopback, returns its port.
    async fn spawn_fake_child() -> u16 {
        async fn chat(body: axum::body::Bytes) -> axum::response::Response {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false) {
                let stream = futures_util::stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(Event::default().data("chunk1")),
                    Ok(Event::default().data("chunk2")),
                ]);
                return Sse::new(stream).into_response();
            }
            axum::Json(serde_json::json!({
                "id": "cmpl-1",
                "model": "canonical/should-be-overwritten",
                "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                "usage": {"prompt_tokens":1,"completion_tokens":1}
            }))
            .into_response()
        }
        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    async fn test_gateway() -> (Router, u16) {
        let port = spawn_fake_child().await;
        let engine = Arc::new(EngineSupervisor::new());
        engine.insert(ModelState {
            canonical_id: "test/model:Q4".to_string(),
            size_bytes: 123,
            pulled_at: 1_700_000_000,
            status: ModelStatus::Loaded,
            port: Some(port),
        });
        engine.insert_alias("tm", "test/model:Q4");
        (super::super::router(engine), port)
    }

    #[tokio::test]
    async fn unresolved_model_is_404() {
        let (app, _) = test_gateway().await;
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"nope","messages":[]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "model_not_found");
    }

    #[tokio::test]
    async fn non_streaming_resolves_alias_and_normalizes_model_field() {
        let (app, _) = test_gateway().await;
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"tm","messages":[],"stream":false}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "tm");
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
    }

    #[tokio::test]
    async fn streaming_is_byte_copied() {
        let (app, _) = test_gateway().await;
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"test/model:Q4","messages":[],"stream":true}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("chunk1"));
        assert!(text.contains("chunk2"));
    }

    #[tokio::test]
    async fn list_models_reflects_registry() {
        let (app, _) = test_gateway().await;
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["data"][0]["id"], "test/model:Q4");
        assert_eq!(v["data"][0]["owned_by"], "nodelm");
    }

    #[tokio::test]
    async fn unsupported_architecture_is_400() {
        let port = spawn_fake_child().await;
        let engine = Arc::new(EngineSupervisor::new());
        engine.insert(ModelState {
            canonical_id: "bad/model:Q4".to_string(),
            size_bytes: 1,
            pulled_at: 0,
            status: ModelStatus::UnsupportedArchitecture("wrong number of tensors".to_string()),
            port: Some(port),
        });
        let app = super::super::router(engine);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"bad/model:Q4","messages":[]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("wrong number of tensors")
        );
    }
}
