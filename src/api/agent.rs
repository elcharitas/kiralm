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

use super::openai::{
    ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage, ToolCall, ToolCallFunction,
};
use crate::approvals::{self, ApprovalManager, ApprovalOutcome};
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
#[allow(clippy::too_many_arguments)]
pub async fn run(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    mut req: ChatCompletionRequest,
    project_root: &Path,
    chat_id: &str,
    permission: Permission,
    approvals: &ApprovalManager,
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
            .map_err(|e| AgentError::Other(super::upstream_unreachable("tool-calling loop", &e)))?;

        if bearer.is_some() && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AgentError::AuthRequired(
                "cloud login expired or revoked. run 'kiralm login' again".to_string(),
            ));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(super::upstream_error("tool-calling loop", status, &body)));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(super::upstream_malformed("tool-calling loop", &e)))?;
        let Some(choice) = parsed.choices.first() else {
            return Err(AgentError::Other("upstream returned no choices".to_string()));
        };
        let tool_calls = choice.message.tool_calls.clone();
        let assistant_content = choice.message.content.clone();

        // A model with no native tool-call support (or a template --jinja
        // couldn't fix) may still emit the widely-used Hermes/Qwen
        // <tool_call>{...}</tool_call> text convention. Recover those so a
        // "dumb" model isn't stuck giving up after one plain-text turn.
        let calls = tool_calls
            .filter(|c| !c.is_empty())
            .or_else(|| non_empty(parse_text_tool_calls(assistant_content.as_deref().unwrap_or_default())));
        let Some(calls) = calls else {
            return Ok((parsed, steps));
        };

        req.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content.map(ChatContent::Text),
            tool_calls: Some(calls.clone()),
            tool_call_id: None,
        });
        for call in &calls {
            let authorization = approvals::authorize(
                approvals,
                chat_id,
                permission,
                &call.function.name,
                &call.function.arguments,
            )
            .await;
            let (content, step) = match authorization {
                ApprovalOutcome::Allowed => {
                    tools::execute(
                        &call.function.name,
                        &call.function.arguments,
                        project_root,
                        Permission::FullAccess,
                        mcp,
                        dsh_bin,
                    )
                    .await
                }
                ApprovalOutcome::Denied(reason) => {
                    tools::denied_step(&call.function.name, &call.function.arguments, &reason)
                }
            };
            // A skill's own `allowed-tools` frontmatter is only honored
            // once it's actually loaded (a model can't be scoped down
            // before it picks a skill), and for the rest of this turn --
            // narrowing `req.tools` here is what a fresh `tool_defs()`
            // call at loop top wouldn't otherwise do mid-turn.
            if step.ok && call.function.name == "load_skill" {
                restrict_tools_to_loaded_skill(&mut req, &call.function.arguments);
            }
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

/// Narrows `req.tools` to a just-loaded skill's declared `allowed-tools`
/// (plus `load_skill` itself, so the model can still switch skills), if
/// it declared any. A skill with no `allowed-tools` leaves the tool set
/// untouched -- the frontmatter field is opt-in, not a default deny-all.
fn restrict_tools_to_loaded_skill(req: &mut ChatCompletionRequest, load_skill_args_json: &str) {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(load_skill_args_json) else { return };
    let Some(name) = args.get("name").and_then(|v| v.as_str()) else { return };
    let Some(skill) = crate::skills::get(name) else { return };
    let Some(allowed) = skill.allowed_tools else { return };
    let allowed: std::collections::HashSet<&str> =
        allowed.iter().map(String::as_str).chain(std::iter::once("load_skill")).collect();
    if let Some(defs) = &mut req.tools {
        defs.retain(|d| d["function"]["name"].as_str().is_some_and(|n| allowed.contains(n)));
    }
}

fn non_empty(calls: Vec<ToolCall>) -> Option<Vec<ToolCall>> {
    (!calls.is_empty()).then_some(calls)
}

/// Only this exact tag is trusted, so an ordinary answer that happens to
/// contain JSON-looking prose can't misfire into executing a tool.
fn parse_text_tool_calls(content: &str) -> Vec<ToolCall> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    let mut calls = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else { break };
        let body = after[..end].trim();
        rest = &after[end + CLOSE.len()..];

        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else { continue };
        let Some(name) = value.get("name").and_then(|v| v.as_str()) else { continue };
        let Some(arguments) = value.get("arguments").or_else(|| value.get("parameters")) else { continue };
        let arguments = match arguments {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        calls.push(ToolCall {
            id: format!("fallback-{}", uuid::Uuid::new_v4()),
            kind: "function".to_string(),
            function: ToolCallFunction { name: name.to_string(), arguments },
        });
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: std::path::PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            let guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("kiralm-agent-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", &dir);
            TempHome { _guard: guard, prev, dir }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fake_req(tool_names: &[&str]) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test".to_string(),
            messages: vec![],
            max_tokens: 16,
            temperature: None,
            top_p: None,
            top_k: None,
            stop: None,
            stream: false,
            tools: Some(tool_names.iter().map(|n| json!({"function": {"name": n}})).collect()),
            chat_template_kwargs: None,
        }
    }

    #[test]
    fn restrict_tools_narrows_to_a_skills_allowed_tools_plus_load_skill() {
        let _home = TempHome::new();
        crate::skills::create_from_markdown(
            "---\nname: reader\ndescription: read-only helper.\nallowed-tools: [read_file, list_directory]\n---\nBody.\n",
        )
        .unwrap();
        let mut req = fake_req(&["read_file", "list_directory", "write_file", "run_command", "load_skill"]);
        restrict_tools_to_loaded_skill(&mut req, r#"{"name":"reader"}"#);
        let names: Vec<String> =
            req.tools.unwrap().iter().filter_map(|d| d["function"]["name"].as_str().map(str::to_string)).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        for expected in ["read_file", "list_directory", "load_skill"] {
            assert!(names.iter().any(|n| n == expected), "{names:?} should contain {expected}");
        }
        assert!(!names.iter().any(|n| n == "write_file"));
        assert!(!names.iter().any(|n| n == "run_command"));
    }

    #[test]
    fn restrict_tools_leaves_the_set_untouched_without_allowed_tools() {
        let _home = TempHome::new();
        crate::skills::create_from_markdown("---\nname: open\ndescription: no restriction.\n---\nBody.\n").unwrap();
        let mut req = fake_req(&["read_file", "write_file", "load_skill"]);
        restrict_tools_to_loaded_skill(&mut req, r#"{"name":"open"}"#);
        assert_eq!(req.tools.unwrap().len(), 3);
    }

    #[test]
    fn restrict_tools_is_a_no_op_for_an_unknown_skill() {
        let _home = TempHome::new();
        let mut req = fake_req(&["read_file", "write_file"]);
        restrict_tools_to_loaded_skill(&mut req, r#"{"name":"nope"}"#);
        assert_eq!(req.tools.unwrap().len(), 2);
    }

    #[test]
    fn parse_text_tool_calls_recovers_a_single_tagged_call() {
        let content = r#"Sure, let me check.
<tool_call>
{"name": "read_file", "arguments": {"path": "src/main.rs"}}
</tool_call>"#;
        let calls = parse_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn parse_text_tool_calls_recovers_multiple_calls_in_one_turn() {
        let content = r#"<tool_call>{"name": "a", "arguments": {}}</tool_call>
<tool_call>{"name": "b", "parameters": {"x": 1}}</tool_call>"#;
        let calls = parse_text_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "a");
        assert_eq!(calls[1].function.name, "b");
        assert_eq!(calls[1].function.arguments, r#"{"x":1}"#);
    }

    #[test]
    fn parse_text_tool_calls_ignores_ordinary_prose_and_malformed_bodies() {
        assert!(parse_text_tool_calls("Here's a JSON example: {\"name\": \"x\"}").is_empty());
        assert!(parse_text_tool_calls("<tool_call>not json</tool_call>").is_empty());
        assert!(parse_text_tool_calls("<tool_call>{\"arguments\": {}}</tool_call>").is_empty());
        assert!(parse_text_tool_calls("no tags here at all").is_empty());
    }
}
