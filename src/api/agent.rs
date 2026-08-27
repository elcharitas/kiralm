//! The real tool-calling loop: sends the tool
//! defs to the child, executes whatever it calls for real (sandboxed +
//! permission-checked, see `crate::tools`), feeds the result back, and
//! repeats until the model answers with plain text instead of a call.
//!
//! Each turn is a synchronous non-streaming upstream call, so
//! nothing streams to the client until the whole loop finishes (no
//! interim "thinking"/tool-progress frames). Fine for the desktop UI's
//! current needs; add real interim streaming if a multi-tool turn's
//! silent gap ever becomes a problem in practice.

use std::path::Path;

use super::openai::{ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage};
use crate::mcp::McpRegistry;
use crate::tools::{self, Permission, ToolStep};

const MAX_ITERATIONS: usize = 8;

pub enum AgentError {
    /// A cloud 401 mid-loop, same treatment as the plain proxy path.
    AuthRequired(String),
    Other(String),
}

/// Runs the loop against `url` (the already-resolved child endpoint,
/// local or cloud) and returns the final assistant response plus every
/// tool call made along the way, in order -- the real step data behind
/// the UI's "Worked for Xm Ys" collapsible.
pub async fn run(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    mut req: ChatCompletionRequest,
    project_root: &Path,
    permission: Permission,
    enable_thinking: bool,
    mcp: &McpRegistry,
    dsh_bin: &str,
) -> Result<(ChatCompletionResponse, Vec<ToolStep>), AgentError> {
    req.stream = false;
    req.tools = Some(tools::tool_defs(permission, mcp, dsh_bin));
    // Off by default: Qwen3-style reasoning models otherwise burn the
    // whole max_tokens budget on hidden <think> content before ever
    // emitting a call. The composer's "Thinking" toggle controls this.
    req.chat_template_kwargs = Some(serde_json::json!({"enable_thinking": enable_thinking}));

    let mut steps = Vec::new();

    for _ in 0..MAX_ITERATIONS {
        let mut http_req = http.post(url).json(&req);
        if let Some(token) = bearer {
            http_req = http_req.bearer_auth(token);
        }
        let resp = http_req
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("upstream request failed: {e}")))?;

        if bearer.is_some() && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AgentError::AuthRequired(
                "cloud login expired or revoked. run 'kiralm login' again".to_string(),
            ));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!("upstream returned {status}: {body}")));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("upstream returned malformed response: {e}")))?;
        let Some(choice) = parsed.choices.first() else {
            return Err(AgentError::Other("upstream returned no choices".to_string()));
        };
        let tool_calls = choice.message.tool_calls.clone();
        let assistant_content = choice.message.content.clone();

        let Some(calls) = tool_calls.filter(|c| !c.is_empty()) else {
            return Ok((parsed, steps));
        };

        req.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content.map(ChatContent::Text),
            tool_calls: Some(calls.clone()),
            tool_call_id: None,
        });
        for call in &calls {
            let (content, step) =
                tools::execute(&call.function.name, &call.function.arguments, project_root, permission, mcp, dsh_bin).await;
            steps.push(step);
            req.messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(ChatContent::Text(content)),
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
            });
        }
    }

    Err(AgentError::Other(format!(
        "tool-calling loop exceeded {MAX_ITERATIONS} iterations without a final answer"
    )))
}
