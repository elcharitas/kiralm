//! Anthropic-compatible `POST /v1/messages` (SPEC.md §2.2). Real
//! translation to/from the OpenAI chat-completion shape (`api/openai.rs`)
//! that a `llama-server` child speaks, not a byte passthrough — request and
//! response schemas differ entirely, and streaming needs a stateful
//! event-by-event translator.

use std::convert::Infallible;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::openai::{
    ChatChunkDelta, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    ChatContent, ChatContentPart, ChatMessage as OpenAiMessage, ImageUrl,
};
use super::{ensure_loaded, resolve_model, GatewayError, SharedState};
use crate::cloud;
use crate::registry::ModelRoute;
use crate::state::ModelManifest;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/v1/messages", post(messages))
}

// ---------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<InputMessage>,
    #[serde(default)]
    pub system: Option<String>,
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: InputContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum InputContent {
    Text(String),
    Blocks(Vec<InputBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputBlock {
    Text { text: String },
    Image { source: ImageSource },
    ToolUse {},
    ToolResult {},
}

#[derive(Debug, Deserialize)]
pub struct ImageSource {
    // "type" (always "base64") isn't branched on; only media_type/data
    // feed the data-URI built below.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

// ---------------------------------------------------------------------
// Response types (non-streaming)
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub content: Vec<ResponseBlock>,
    pub model: String,
    pub stop_reason: Option<&'static str>,
    pub stop_sequence: Option<String>,
    pub usage: ResponseUsage,
}

#[derive(Debug, Serialize)]
pub struct ResponseBlock {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ---------------------------------------------------------------------
// Errors (§2.4: Anthropic shape, chosen strictly by route)
// ---------------------------------------------------------------------

pub(crate) fn anthropic_error(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Response {
    let body = json!({
        "type": "error",
        "error": { "type": kind, "message": message.into() },
    });
    (status, Json(body)).into_response()
}

/// Maps a `GatewayError` to its Anthropic-shaped response — the one
/// place this happens, so `AuthRequired` (raised both by `ensure_loaded`
/// and, after routing, by the cloud forward step) gets identical
/// treatment either way.
fn gateway_error_response(e: GatewayError) -> Response {
    match e {
        GatewayError::CachedUnsupported(msg) => {
            anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", msg)
        }
        GatewayError::InsufficientRam(msg) => {
            anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "overloaded_error", msg)
        }
        GatewayError::LoadFailed(msg) => {
            anthropic_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg)
        }
        GatewayError::AuthRequired(msg) => {
            anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", msg)
        }
    }
}

// ---------------------------------------------------------------------
// Request -> OpenAI translation
// ---------------------------------------------------------------------

/// Converts one Anthropic content value to the OpenAI shape. Returns the
/// unsupported block's type name on rejection (§2.2: tool_use/tool_result
/// always 400; image blocks 400 unless the model is vision_capable).
fn convert_content(content: &InputContent, vision_capable: bool) -> Result<ChatContent, &'static str> {
    let blocks = match content {
        InputContent::Text(s) => return Ok(ChatContent::Text(s.clone())),
        InputContent::Blocks(b) => b,
    };

    let has_image = blocks.iter().any(|b| matches!(b, InputBlock::Image { .. }));
    for b in blocks {
        match b {
            InputBlock::ToolUse {} => return Err("tool_use"),
            InputBlock::ToolResult {} => return Err("tool_result"),
            InputBlock::Image { .. } if !vision_capable => return Err("image"),
            _ => {}
        }
    }

    if !has_image {
        // Pure text blocks -> flattened text (§2.2).
        let text = blocks
            .iter()
            .filter_map(|b| match b {
                InputBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(ChatContent::Text(text));
    }

    let parts = blocks
        .iter()
        .map(|b| match b {
            InputBlock::Text { text } => ChatContentPart::Text { text: text.clone() },
            InputBlock::Image { source } => ChatContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{};base64,{}", source.media_type, source.data),
                },
            },
            InputBlock::ToolUse {} | InputBlock::ToolResult {} => unreachable!("rejected above"),
        })
        .collect();
    Ok(ChatContent::Parts(parts))
}

fn build_openai_request(
    req: &MessagesRequest,
    canonical_id: &str,
    vision_capable: bool,
    stream: bool,
) -> Result<ChatCompletionRequest, Response> {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = &req.system {
        messages.push(OpenAiMessage {
            role: "system".to_string(),
            content: ChatContent::Text(system.clone()),
        });
    }
    for m in &req.messages {
        let content = convert_content(&m.content, vision_capable).map_err(|block_type| {
            anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("content block type '{block_type}' is not supported"),
            )
        })?;
        messages.push(OpenAiMessage {
            role: m.role.clone(),
            content,
        });
    }

    Ok(ChatCompletionRequest {
        model: canonical_id.to_string(),
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stop: req.stop_sequences.clone(),
        stream,
    })
}

// ---------------------------------------------------------------------
// §2.3 stop_reason mapping
// ---------------------------------------------------------------------

fn map_stop_reason(
    finish_reason: Option<&str>,
    completion_text: &str,
    stop_sequences: &Option<Vec<String>>,
) -> (Option<&'static str>, Option<String>) {
    match finish_reason {
        Some("length") => (Some("max_tokens"), None),
        Some("stop") | None => {
            let matched = stop_sequences.as_ref().and_then(|seqs| {
                seqs.iter()
                    .find(|s| !s.is_empty() && completion_text.ends_with(s.as_str()))
                    .cloned()
            });
            match matched {
                Some(seq) => (Some("stop_sequence"), Some(seq)),
                None => (Some("end_turn"), None),
            }
        }
        Some(_other) => (Some("end_turn"), None),
    }
}

// ---------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------

pub async fn messages(State(app): State<SharedState>, Json(req): Json<MessagesRequest>) -> Response {
    let Some(canonical_id) = resolve_model(&app, &req.model) else {
        return anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            format!("model '{}' not found", req.model),
        );
    };
    let vision_capable = app
        .registry
        .get_clone(&canonical_id)
        .and_then(|ms| ModelManifest::load(&ms.gguf_path).ok())
        .map(|m| m.vision_capable)
        .unwrap_or(false);

    let stream = req.stream.unwrap_or(false);
    let openai_req = match build_openai_request(&req, &canonical_id, vision_capable, stream) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let route = match ensure_loaded(&app, &canonical_id).await {
        Ok(r) => r,
        Err(e) => return gateway_error_response(e),
    };

    let http = reqwest::Client::new();
    // Same routing branch as `/v1/chat/completions` (§"/v1/chat/
    // completions routing for cloud-sourced models" — the translator
    // below doesn't know or care whether the child is local or cloud,
    // only that `ensure_loaded` handed it a route to call).
    let (url, bearer, openai_req) = match route {
        ModelRoute::Local(port) => (format!("http://127.0.0.1:{port}/v1/chat/completions"), None, openai_req),
        ModelRoute::Cloud => {
            let token = match cloud::ensure_token() {
                Ok(t) => t,
                Err(e) => return gateway_error_response(GatewayError::AuthRequired(e.to_string())),
            };
            let cloud_model_id = app
                .registry
                .get_clone(&canonical_id)
                .and_then(|ms| ms.cloud_model_id)
                .unwrap_or_else(|| canonical_id.clone());
            let mut req = openai_req;
            req.model = cloud_model_id; // nodelm-web expects the catalog slug, not our alias
            (
                format!("{}/api/v1/chat/completions", app.config.cloud.api_base_url.trim_end_matches('/')),
                Some(token),
                req,
            )
        }
    };

    if stream {
        stream_messages(http, url, bearer, openai_req, req.stop_sequences, canonical_id).await
    } else {
        non_stream_messages(http, url, bearer, openai_req, req.stop_sequences).await
    }
}

async fn non_stream_messages(
    http: reqwest::Client,
    url: String,
    bearer: Option<String>,
    openai_req: ChatCompletionRequest,
    stop_sequences: Option<Vec<String>>,
) -> Response {
    let mut req = http.post(&url).json(&openai_req);
    if let Some(token) = &bearer {
        req = req.bearer_auth(token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                format!("upstream request failed: {e}"),
            )
        }
    };
    if bearer.is_some() && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return gateway_error_response(GatewayError::AuthRequired(
            "cloud login expired or revoked — run 'nodelm login' again".to_string(),
        ));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("upstream returned {status}: {body}"),
        );
    }
    let parsed: ChatCompletionResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            return anthropic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                format!("upstream returned malformed response: {e}"),
            )
        }
    };
    let Some(choice) = parsed.choices.into_iter().next() else {
        return anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "upstream returned no choices",
        );
    };
    let text = choice.message.content.unwrap_or_default();
    let (stop_reason, stop_sequence) =
        map_stop_reason(choice.finish_reason.as_deref(), &text, &stop_sequences);
    let usage = parsed.usage.unwrap_or(super::openai::ChatUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
    });

    let body = MessagesResponse {
        id: format!("msg_{}", uuid::Uuid::new_v4()),
        kind: "message",
        role: "assistant",
        content: vec![ResponseBlock { kind: "text", text }],
        model: openai_req.model,
        stop_reason,
        stop_sequence,
        usage: ResponseUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        },
    };
    (StatusCode::OK, Json(body)).into_response()
}

async fn stream_messages(
    http: reqwest::Client,
    url: String,
    bearer: Option<String>,
    openai_req: ChatCompletionRequest,
    stop_sequences: Option<Vec<String>>,
    canonical_id: String,
) -> Response {
    let mut req = http.post(&url).json(&openai_req);
    if let Some(token) = &bearer {
        req = req.bearer_auth(token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                format!("upstream request failed: {e}"),
            )
        }
    };
    if bearer.is_some() && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return gateway_error_response(GatewayError::AuthRequired(
            "cloud login expired or revoked — run 'nodelm login' again".to_string(),
        ));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            format!("upstream returned {status}: {body}"),
        );
    }

    let msg_id = format!("msg_{}", uuid::Uuid::new_v4());
    let stream = translate_stream(resp, msg_id, canonical_id, stop_sequences);
    Sse::new(stream).into_response()
}

/// Stateful SSE translator (§2.2): message_start -> content_block_start ->
/// N content_block_delta -> content_block_stop -> message_delta ->
/// message_stop. A read failure partway through (child crash/OOM) emits an
/// Anthropic `event: error` frame instead of aborting silently, since the
/// stream has already been committed to the client by then (§2.2).
fn translate_stream(
    resp: reqwest::Response,
    msg_id: String,
    model: String,
    stop_sequences: Option<Vec<String>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        yield Ok(sse_json("message_start", json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                // ponytail: llama-server doesn't expose the prompt token
                // count before streaming starts; real input_tokens would
                // need buffering the prompt-processing phase separately.
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            }
        })));
        yield Ok(sse_json("content_block_start", json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" },
        })));

        let mut byte_stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut completion_text = String::new();
        let mut finish_reason: Option<String> = None;
        let mut output_tokens: u32 = 0;
        let mut errored = false;

        'read: loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(bytes)) => bytes,
                Some(Err(e)) => {
                    yield Ok(sse_json("error", json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": format!("upstream stream error: {e}") },
                    })));
                    errored = true;
                    break 'read;
                }
                None => break 'read,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find("\n\n") {
                let frame = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        break 'read;
                    }
                    let parsed: ChatCompletionChunk = match serde_json::from_str(data) {
                        Ok(p) => p,
                        Err(_) => continue, // ignore malformed/keep-alive frames
                    };
                    if let Some(choice) = parsed.choices.into_iter().next() {
                        if let ChatChunkDelta { content: Some(text) } = choice.delta {
                            if !text.is_empty() {
                                completion_text.push_str(&text);
                                output_tokens += 1; // ponytail: chunk count as a token-count proxy
                                yield Ok(sse_json("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": 0,
                                    "delta": { "type": "text_delta", "text": text },
                                })));
                            }
                        }
                        if choice.finish_reason.is_some() {
                            finish_reason = choice.finish_reason;
                            break 'read;
                        }
                    }
                }
            }
        }

        if errored {
            return;
        }

        yield Ok(sse_json("content_block_stop", json!({
            "type": "content_block_stop",
            "index": 0,
        })));

        let (stop_reason, stop_sequence) =
            map_stop_reason(finish_reason.as_deref(), &completion_text, &stop_sequences);
        yield Ok(sse_json("message_delta", json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": stop_sequence },
            "usage": { "output_tokens": output_tokens },
        })));
        yield Ok(sse_json("message_stop", json!({ "type": "message_stop" })));
    }
}

fn sse_json(event: &'static str, payload: serde_json::Value) -> Event {
    Event::default().event(event).data(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::registry::{ModelRegistry, ModelState};
    use crate::state::{ExpertCount, ModelManifest, State};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Writes a manifest sidecar next to a unique temp "gguf" path and
    /// registers a matching `ModelState`, so `messages()`'s vision_capable
    /// lookup (which reads the sidecar off disk) resolves for real.
    fn register_test_model(app: &SharedState, canonical_id: &str, alias: &str, vision_capable: bool) {
        let gguf_path = std::env::temp_dir().join(format!(
            "nodelm-test-{}-{}.gguf",
            canonical_id.replace(['/', ':'], "_"),
            uuid::Uuid::new_v4()
        ));
        let manifest = ModelManifest {
            canonical_id: canonical_id.to_string(),
            filename: gguf_path.file_name().unwrap().to_string_lossy().to_string(),
            sha256: "deadbeef".to_string(),
            size_bytes: 1,
            architecture: "test".to_string(),
            is_moe: false,
            expert_count: None::<ExpertCount>,
            context_length_trained: 4096,
            vision_capable,
            source_ref: canonical_id.to_string(),
            pulled_at: chrono::Utc::now(),
        };
        manifest.write(&gguf_path).expect("write test manifest");

        let mut model = ModelState::from_manifest(&manifest, false);
        model.gguf_path = gguf_path;
        app.registry.insert(model);
        app.state.write().aliases.insert(alias.to_string(), canonical_id.to_string());
    }

    fn mark_loaded(app: &SharedState, canonical_id: &str, port: u16) {
        let mut model = app.registry.get_clone(canonical_id).unwrap();
        model.status = crate::registry::ModelStatus::Loaded;
        model.port = Some(port);
        app.registry.insert(model);
    }

    fn test_state() -> SharedState {
        Arc::new(super::super::AppState::new(
            Config::default(),
            Arc::new(ModelRegistry::new()),
            State::default(),
        ))
    }

    fn router(state: SharedState) -> Router {
        Router::new().route("/v1/messages", post(messages)).with_state(state)
    }

    #[test]
    fn stop_reason_maps_length_to_max_tokens() {
        let (reason, seq) = map_stop_reason(Some("length"), "hello", &None);
        assert_eq!(reason, Some("max_tokens"));
        assert_eq!(seq, None);
    }

    #[test]
    fn stop_reason_maps_matched_stop_sequence() {
        let stops = Some(vec!["END".to_string()]);
        let (reason, seq) = map_stop_reason(Some("stop"), "hello END", &stops);
        assert_eq!(reason, Some("stop_sequence"));
        assert_eq!(seq, Some("END".to_string()));
    }

    #[test]
    fn stop_reason_defaults_to_end_turn() {
        let (reason, seq) = map_stop_reason(Some("stop"), "hello", &None);
        assert_eq!(reason, Some("end_turn"));
        assert_eq!(seq, None);
    }

    #[test]
    fn convert_content_rejects_tool_use() {
        let content = InputContent::Blocks(vec![InputBlock::ToolUse {}]);
        assert_eq!(convert_content(&content, false), Err("tool_use"));
    }

    #[test]
    fn convert_content_rejects_image_without_vision() {
        let content = InputContent::Blocks(vec![InputBlock::Image {
            source: ImageSource {
                kind: "base64".into(),
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        }]);
        assert_eq!(convert_content(&content, false), Err("image"));
    }

    #[test]
    fn convert_content_flattens_plain_text() {
        let content = InputContent::Text("hi there".into());
        match convert_content(&content, false).unwrap() {
            ChatContent::Text(s) => assert_eq!(s, "hi there"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn unresolved_model_is_404_not_found_error() {
        let state = test_state();
        let app = router(state);
        let body = json!({
            "model": "nope",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "not_found_error");
    }

    #[tokio::test]
    async fn tool_use_block_is_400_invalid_request_error() {
        let state = test_state();
        register_test_model(&state, "org/repo:Q4", "alias", false);
        let app = router(state);
        let body = json!({
            "model": "alias",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "tool_use", "id": "1", "name": "x", "input": {}}]}],
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(v["error"]["message"].as_str().unwrap().contains("tool_use"));
    }

    /// Full round trip: a fake upstream axum server plays the role of a
    /// `llama-server` child's non-streaming `/v1/chat/completions`.
    #[tokio::test]
    async fn non_streaming_round_trip_translates_to_anthropic_shape() {
        async fn fake_chat_completions(
            Json(_req): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            Json(json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "message": { "role": "assistant", "content": "hello world" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 2 },
            }))
        }
        let upstream = Router::new().route("/v1/chat/completions", post(fake_chat_completions));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = test_state();
        register_test_model(&state, "org/repo:Q4", "alias", false);
        mark_loaded(&state, "org/repo:Q4", port);
        let app = router(state);

        let body = json!({
            "model": "alias",
            "max_tokens": 16,
            "system": "be nice",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello world");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 5);
        assert_eq!(v["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn streaming_round_trip_emits_anthropic_sse_events() {
        async fn fake_stream() -> Response {
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                body,
            )
                .into_response()
        }
        let upstream = Router::new().route("/v1/chat/completions", axum::routing::post(fake_stream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = test_state();
        register_test_model(&state, "org/repo:Q4", "alias", false);
        mark_loaded(&state, "org/repo:Q4", port);
        let app = router(state);

        let body = json!({
            "model": "alias",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"text\":\"hi\""));
        assert!(text.contains("\"text\":\"!\""));
        assert!(text.contains("event: content_block_stop"));
        assert!(text.contains("event: message_delta"));
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
        assert!(text.contains("event: message_stop"));
    }
}
