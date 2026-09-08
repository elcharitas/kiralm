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
    ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage, ToolCall,
    ToolCallFunction,
};
use super::{SessionGeneration, SharedState};
use crate::approvals::{self, ApprovalOutcome};
use crate::events::EventTopic;
use crate::policies::{HookPhase, PolicyEffect};
use crate::tools::{self, Permission, ToolStep};

const MAX_ITERATIONS: usize = 8;

fn message_text(message: &ChatMessage) -> Option<String> {
    match message.content.as_ref()? {
        ChatContent::Text(text) => Some(text.clone()),
        ChatContent::Parts(parts) => Some(
            parts
                .iter()
                .filter_map(|part| match part {
                    super::openai::ChatContentPart::Text { text } => Some(text.as_str()),
                    super::openai::ChatContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

fn routing_conversation(messages: &[ChatMessage]) -> String {
    let mut recent = messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .filter_map(message_text)
        .take(4)
        .collect::<Vec<_>>();
    recent.reverse();
    let combined = recent.join("\n");
    combined
        .chars()
        .rev()
        .take(8_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

pub enum AgentError {
    /// A cloud 401 mid-loop, same treatment as the plain proxy path.
    AuthRequired(String),
    Stopped(Vec<ToolStep>),
    HookBlocked(String, Vec<ToolStep>),
    Other(String),
}

/// Runs the loop against `url` (the already-resolved child endpoint,
/// local or cloud) and returns the final assistant response plus every
/// tool call made along the way, in order -- the real step data behind
/// the UI's "Worked for Xm Ys" collapsible.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &SharedState,
    generation: &std::sync::Arc<SessionGeneration>,
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    mut req: ChatCompletionRequest,
    project_root: &Path,
    project_id: &str,
    chat_id: &str,
    permission: Permission,
    enable_thinking: bool,
) -> Result<(ChatCompletionResponse, Vec<ToolStep>), AgentError> {
    req.stream = false;
    let routing_context = routing_conversation(&req.messages);
    req.tools = Some(tools::routed_tool_defs(
        permission,
        &app.mcp,
        &app.config.dsh.bin,
        &routing_context,
    ));
    // Off by default: Qwen3-style reasoning models otherwise burn the
    // whole max_tokens budget on hidden <think> content before ever
    // emitting a call. The composer's "Thinking" toggle controls this.
    req.chat_template_kwargs = Some(serde_json::json!({"enable_thinking": enable_thinking}));

    let mut steps = Vec::new();

    for iteration in 0..MAX_ITERATIONS {
        app.events.publish(
            EventTopic::Agent,
            "model_started",
            Some(chat_id.to_string()),
            serde_json::json!({ "iteration": iteration + 1 }),
        );
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
            return Err(AgentError::Other(super::upstream_error(
                "tool-calling loop",
                status,
                &body,
            )));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(super::upstream_malformed("tool-calling loop", &e)))?;
        let Some(choice) = parsed.choices.first() else {
            return Err(AgentError::Other(
                "upstream returned no choices".to_string(),
            ));
        };
        let tool_calls = choice.message.tool_calls.clone();
        let assistant_content = choice.message.content.clone();

        // A model with no native tool-call support (or a template --jinja
        // couldn't fix) may still emit the widely-used Hermes/Qwen
        // <tool_call>{...}</tool_call> text convention. Recover those so a
        // "dumb" model isn't stuck giving up after one plain-text turn.
        let calls = tool_calls.filter(|c| !c.is_empty()).or_else(|| {
            non_empty(parse_text_tool_calls(
                assistant_content.as_deref().unwrap_or_default(),
            ))
        });
        let Some(calls) = calls else {
            let final_text = assistant_content.as_deref().unwrap_or_default();
            if let Err(message) = app
                .policies
                .run_hooks(
                    HookPhase::RunCompleted,
                    project_id,
                    None,
                    project_root,
                    serde_json::json!({
                        "chat_id": chat_id,
                        "output": final_text,
                        "steps": steps,
                    }),
                )
                .await
            {
                return Err(AgentError::HookBlocked(message, steps));
            }
            app.events.publish(
                EventTopic::Agent,
                "run_completed",
                Some(chat_id.to_string()),
                serde_json::json!({ "steps": steps.len() }),
            );
            return Ok((parsed, steps));
        };

        req.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content.map(ChatContent::Text),
            tool_calls: Some(calls.clone()),
            tool_call_id: None,
        });
        for call in &calls {
            let arguments = serde_json::from_str(&call.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            let target = approvals::approval_target(&call.function.name, &arguments);
            app.events.publish(
                EventTopic::Agent,
                "tool_requested",
                Some(chat_id.to_string()),
                serde_json::json!({
                    "tool_name": call.function.name,
                    "target": target,
                }),
            );
            let policy = app
                .policies
                .evaluate(project_id, &call.function.name, &target);
            if let Some(matched) = &policy {
                app.events.publish(
                    EventTopic::Agent,
                    "policy_matched",
                    Some(chat_id.to_string()),
                    serde_json::json!({
                        "tool_name": call.function.name,
                        "rule_id": matched.rule_id,
                        "rule_name": matched.rule_name,
                        "effect": matched.effect,
                    }),
                );
            }
            let authorization = match policy.map(|matched| matched.effect) {
                Some(PolicyEffect::Deny) => ApprovalOutcome::Denied(format!(
                    "{} is blocked by workspace policy",
                    call.function.name
                )),
                Some(PolicyEffect::Ask) => {
                    app.approvals
                        .request(
                            chat_id,
                            &call.function.name,
                            &call.function.arguments,
                            approvals::ToolRisk::for_tool(&call.function.name),
                        )
                        .await
                }
                Some(PolicyEffect::Allow) if permission != Permission::ReadOnly => {
                    ApprovalOutcome::Allowed
                }
                _ => {
                    approvals::authorize(
                        &app.approvals,
                        chat_id,
                        permission,
                        &call.function.name,
                        &call.function.arguments,
                    )
                    .await
                }
            };
            let (content, mut step) = match authorization {
                ApprovalOutcome::Allowed => {
                    let allowed = app
                        .policies
                        .run_hooks(
                            HookPhase::PreTool,
                            project_id,
                            Some(&call.function.name),
                            project_root,
                            serde_json::json!({
                                "chat_id": chat_id,
                                "arguments": arguments,
                            }),
                        )
                        .await;
                    if let Err(reason) = allowed {
                        tools::denied_step(&call.function.name, &call.function.arguments, &reason)
                    } else {
                        tools::execute(
                            &call.function.name,
                            &call.function.arguments,
                            project_root,
                            Permission::FullAccess,
                            &app.mcp,
                            &app.config.dsh.bin,
                        )
                        .await
                    }
                }
                ApprovalOutcome::Denied(reason) => {
                    tools::denied_step(&call.function.name, &call.function.arguments, &reason)
                }
            };
            if !step.ok {
                step.checkpoint = None;
            }
            if let Some(checkpoint) = step.checkpoint.take() {
                let conversation = app
                    .chats
                    .read()
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .map(|chat| chat.messages.clone());
                match app
                    .checkpoints
                    .create(chat_id, project_id, checkpoint.clone(), conversation)
                {
                    Ok(summary) => step.checkpoint_id = Some(summary.id),
                    Err(message) => {
                        step.checkpoint = Some(checkpoint);
                        app.events.publish(
                            EventTopic::Checkpoint,
                            "persistence_failed",
                            Some(chat_id.to_string()),
                            serde_json::json!({ "error": message }),
                        );
                    }
                }
            }
            let post_hook_error = app
                .policies
                .run_hooks(
                    HookPhase::PostTool,
                    project_id,
                    Some(&call.function.name),
                    project_root,
                    serde_json::json!({
                        "chat_id": chat_id,
                        "ok": step.ok,
                        "result": step.result_summary,
                    }),
                )
                .await
                .err();
            // A skill's own `allowed-tools` frontmatter is only honored
            // once it's actually loaded (a model can't be scoped down
            // before it picks a skill), and for the rest of this turn --
            // narrowing `req.tools` here is what a fresh `tool_defs()`
            // call at loop top wouldn't otherwise do mid-turn.
            if step.ok && call.function.name == "load_skill" {
                restrict_tools_to_loaded_skill(&mut req, &call.function.arguments);
            }
            steps.push(step);
            generation.state.write().steps = steps.clone();
            app.events.publish(
                EventTopic::Agent,
                "tool_finished",
                Some(chat_id.to_string()),
                serde_json::json!({
                    "tool_name": call.function.name,
                    "ok": steps.last().map(|step| step.ok).unwrap_or(false),
                    "result": steps.last().map(|step| &step.result_summary),
                    "checkpoint_id": steps.last().and_then(|step| step.checkpoint_id.as_ref()),
                }),
            );
            req.messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(ChatContent::Text(content)),
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
            });
            if let Some(message) = post_hook_error {
                return Err(AgentError::HookBlocked(message, steps));
            }
        }
        let control = generation.control_point().await;
        if !control.running || control.stop_after_step {
            return Err(AgentError::Stopped(steps));
        }
        for followup in control.followups {
            req.messages.push(ChatMessage::text("user", followup));
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
    let Ok(args) = serde_json::from_str::<serde_json::Value>(load_skill_args_json) else {
        return;
    };
    let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let Some(skill) = crate::skills::get(name) else {
        return;
    };
    let Some(allowed) = skill.allowed_tools else {
        return;
    };
    let allowed: std::collections::HashSet<&str> = allowed
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("load_skill"))
        .collect();
    if let Some(defs) = &mut req.tools {
        defs.retain(|d| {
            d["function"]["name"]
                .as_str()
                .is_some_and(|n| allowed.contains(n))
        });
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

        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            continue;
        };
        let Some(name) = value.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(arguments) = value.get("arguments").or_else(|| value.get("parameters")) else {
            continue;
        };
        let arguments = match arguments {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        calls.push(ToolCall {
            id: format!("fallback-{}", uuid::Uuid::new_v4()),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: name.to_string(),
                arguments,
            },
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
            let guard = crate::config::HOME_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir =
                std::env::temp_dir().join(format!("kiralm-agent-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", &dir);
            TempHome {
                _guard: guard,
                prev,
                dir,
            }
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
            tools: Some(
                tool_names
                    .iter()
                    .map(|n| json!({"function": {"name": n}}))
                    .collect(),
            ),
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
        let mut req = fake_req(&[
            "read_file",
            "list_directory",
            "write_file",
            "run_command",
            "load_skill",
        ]);
        restrict_tools_to_loaded_skill(&mut req, r#"{"name":"reader"}"#);
        let names: Vec<String> = req
            .tools
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        for expected in ["read_file", "list_directory", "load_skill"] {
            assert!(
                names.iter().any(|n| n == expected),
                "{names:?} should contain {expected}"
            );
        }
        assert!(!names.iter().any(|n| n == "write_file"));
        assert!(!names.iter().any(|n| n == "run_command"));
    }

    #[test]
    fn restrict_tools_leaves_the_set_untouched_without_allowed_tools() {
        let _home = TempHome::new();
        crate::skills::create_from_markdown(
            "---\nname: open\ndescription: no restriction.\n---\nBody.\n",
        )
        .unwrap();
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
