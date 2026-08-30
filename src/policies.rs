use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::events::{EventBus, EventTopic};

const SCHEMA_VERSION: u32 = 1;
const MAX_HOOK_OUTPUT: usize = 8_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub project_id: Option<String>,
    pub effect: PolicyEffect,
    pub tool_pattern: String,
    pub target_pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    PreTool,
    PostTool,
    RunCompleted,
    CheckpointRestored,
    TerminalStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    Command,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyHook {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub project_id: Option<String>,
    pub phase: HookPhase,
    pub tool_pattern: String,
    pub kind: HookKind,
    pub target: String,
    pub blocking: bool,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyDocument {
    pub revision: u64,
    pub rules: Vec<PolicyRule>,
    pub hooks: Vec<PolicyHook>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyMatch {
    pub rule_id: String,
    pub rule_name: String,
    pub effect: PolicyEffect,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookResult {
    pub hook_id: String,
    pub hook_name: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub output: String,
}

pub struct PolicyStore {
    path: PathBuf,
    document: RwLock<PolicyDocument>,
    events: Arc<EventBus>,
    http: reqwest::Client,
}

impl PolicyStore {
    pub fn open(path: impl Into<PathBuf>, events: Arc<EventBus>) -> anyhow::Result<PolicyStore> {
        let path = path.into();
        let document =
            crate::storage::load_versioned(&path, SCHEMA_VERSION, migrate)?.unwrap_or_default();
        validate_document(&document).map_err(anyhow::Error::msg)?;
        Ok(PolicyStore {
            path,
            document: RwLock::new(document),
            events,
            http: reqwest::Client::new(),
        })
    }

    pub fn document(&self) -> PolicyDocument {
        self.document.read().clone()
    }

    pub fn replace(
        &self,
        expected_revision: u64,
        mut rules: Vec<PolicyRule>,
        mut hooks: Vec<PolicyHook>,
    ) -> Result<PolicyDocument, String> {
        for rule in &mut rules {
            if rule.id.trim().is_empty() {
                rule.id = uuid::Uuid::new_v4().to_string();
            }
        }
        for hook in &mut hooks {
            if hook.id.trim().is_empty() {
                hook.id = uuid::Uuid::new_v4().to_string();
            }
        }
        let mut current = self.document.write();
        if current.revision != expected_revision {
            return Err(format!(
                "policy revision changed from {expected_revision} to {}",
                current.revision
            ));
        }
        let next = PolicyDocument {
            revision: current.revision + 1,
            rules,
            hooks,
        };
        validate_document(&next)?;
        crate::storage::save_versioned(&self.path, SCHEMA_VERSION, &next)
            .map_err(|error| format!("failed to save policies: {error}"))?;
        *current = next.clone();
        self.events.publish(
            EventTopic::Policy,
            "updated",
            None,
            serde_json::json!({ "revision": next.revision }),
        );
        Ok(next)
    }

    pub fn evaluate(&self, project_id: &str, tool_name: &str, target: &str) -> Option<PolicyMatch> {
        let document = self.document.read();
        for effect in [PolicyEffect::Deny, PolicyEffect::Ask, PolicyEffect::Allow] {
            if let Some(rule) = document.rules.iter().find(|rule| {
                rule.enabled
                    && rule.effect == effect
                    && rule.project_id.as_deref().is_none_or(|id| id == project_id)
                    && wildcard_match(&rule.tool_pattern, tool_name)
                    && wildcard_match(&rule.target_pattern, target)
            }) {
                let matched = PolicyMatch {
                    rule_id: rule.id.clone(),
                    rule_name: rule.name.clone(),
                    effect: rule.effect,
                };
                self.events.publish(
                    EventTopic::Policy,
                    "matched",
                    Some(rule.id.clone()),
                    serde_json::json!({
                        "project_id": project_id,
                        "tool_name": tool_name,
                        "target": target,
                        "effect": effect,
                    }),
                );
                return Some(matched);
            }
        }
        None
    }

    pub async fn run_hooks(
        &self,
        phase: HookPhase,
        project_id: &str,
        tool_name: Option<&str>,
        root: &Path,
        payload: serde_json::Value,
    ) -> Result<Vec<HookResult>, String> {
        let hooks: Vec<_> = self
            .document
            .read()
            .hooks
            .iter()
            .filter(|hook| {
                hook.enabled
                    && hook.phase == phase
                    && hook.project_id.as_deref().is_none_or(|id| id == project_id)
                    && wildcard_match(&hook.tool_pattern, tool_name.unwrap_or_default())
            })
            .cloned()
            .collect();
        let mut results = Vec::new();
        for hook in hooks {
            let result = match hook.kind {
                HookKind::Command => run_command_hook(&hook, root, phase, &payload).await,
                HookKind::Http => self.run_http_hook(&hook, phase, &payload).await,
            };
            self.events.publish(
                EventTopic::Policy,
                "hook_finished",
                Some(hook.id.clone()),
                serde_json::json!({
                    "project_id": project_id,
                    "phase": phase,
                    "tool_name": tool_name,
                    "ok": result.ok,
                    "status": result.status,
                }),
            );
            let failed_blocking = hook.blocking && !result.ok;
            let message = result.output.clone();
            results.push(result);
            if failed_blocking {
                return Err(format!(
                    "hook '{}' blocked the operation: {message}",
                    hook.name
                ));
            }
        }
        Ok(results)
    }

    async fn run_http_hook(
        &self,
        hook: &PolicyHook,
        phase: HookPhase,
        payload: &serde_json::Value,
    ) -> HookResult {
        let timeout = Duration::from_millis(hook.timeout_ms.clamp(100, 120_000));
        let request = async {
            let mut response = self
                .http
                .post(&hook.target)
                .json(&serde_json::json!({
                    "phase": phase,
                    "payload": payload,
                }))
                .send()
                .await?;
            let status = response.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                let remaining = (MAX_HOOK_OUTPUT + 1).saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if bytes.len() > MAX_HOOK_OUTPUT {
                    break;
                }
            }
            Ok::<_, reqwest::Error>((status, String::from_utf8_lossy(&bytes).to_string()))
        };
        let response = tokio::time::timeout(timeout, request).await;
        match response {
            Ok(Ok((status, output))) => HookResult {
                hook_id: hook.id.clone(),
                hook_name: hook.name.clone(),
                ok: status.is_success(),
                status: Some(status.as_u16()),
                output: truncate(output),
            },
            Ok(Err(error)) => hook_error(hook, format!("request failed: {error}")),
            Err(_) => hook_error(hook, "request timed out".to_string()),
        }
    }
}

async fn run_command_hook(
    hook: &PolicyHook,
    root: &Path,
    phase: HookPhase,
    payload: &serde_json::Value,
) -> HookResult {
    let mut command = shell_command(&hook.target);
    command
        .current_dir(root)
        .env("KIRALM_HOOK_PHASE", phase_name(phase))
        .env("KIRALM_HOOK_PAYLOAD", payload.to_string())
        .kill_on_drop(true);
    let timeout = Duration::from_millis(hook.timeout_ms.clamp(100, 120_000));
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            HookResult {
                hook_id: hook.id.clone(),
                hook_name: hook.name.clone(),
                ok: output.status.success(),
                status: output.status.code().map(|code| code as u16),
                output: truncate(text),
            }
        }
        Ok(Err(error)) => hook_error(hook, format!("command failed: {error}")),
        Err(_) => hook_error(hook, "command timed out".to_string()),
    }
}

#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("cmd.exe");
    process.args(["/D", "/S", "/C", command]);
    process
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/sh");
    process.args(["-lc", command]);
    process
}

fn hook_error(hook: &PolicyHook, output: String) -> HookResult {
    HookResult {
        hook_id: hook.id.clone(),
        hook_name: hook.name.clone(),
        ok: false,
        status: None,
        output,
    }
}

fn validate_document(document: &PolicyDocument) -> Result<(), String> {
    if document.rules.len() > 500 || document.hooks.len() > 200 {
        return Err("policy document is too large".to_string());
    }
    for rule in &document.rules {
        if rule.name.trim().is_empty() || rule.name.len() > 120 {
            return Err("every policy rule needs a short name".to_string());
        }
        if rule.tool_pattern.trim().is_empty() || rule.target_pattern.trim().is_empty() {
            return Err(format!("policy rule '{}' has an empty pattern", rule.name));
        }
    }
    for hook in &document.hooks {
        if hook.name.trim().is_empty() || hook.name.len() > 120 {
            return Err("every policy hook needs a short name".to_string());
        }
        if hook.target.trim().is_empty() {
            return Err(format!("policy hook '{}' has no target", hook.name));
        }
        if hook.kind == HookKind::Http
            && !hook.target.starts_with("http://")
            && !hook.target.starts_with("https://")
        {
            return Err(format!("policy hook '{}' needs an HTTP URL", hook.name));
        }
        if !(100..=120_000).contains(&hook.timeout_ms) {
            return Err(format!(
                "policy hook '{}' has an invalid timeout",
                hook.name
            ));
        }
    }
    Ok(())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<_> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut position = 0;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 && !value.starts_with(part) {
            return false;
        }
        if index == parts.len() - 1 {
            return value[position..].ends_with(part);
        }
        let Some(offset) = value[position..].find(part) else {
            return false;
        };
        position += offset + part.len();
    }
    true
}

fn phase_name(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::PreTool => "pre_tool",
        HookPhase::PostTool => "post_tool",
        HookPhase::RunCompleted => "run_completed",
        HookPhase::CheckpointRestored => "checkpoint_restored",
        HookPhase::TerminalStarted => "terminal_started",
    }
}

fn truncate(mut value: String) -> String {
    if value.len() > MAX_HOOK_OUTPUT {
        let mut boundary = MAX_HOOK_OUTPUT;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
        value.push_str("\n… [truncated]");
    }
    value
}

fn migrate(version: u32, value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => Ok(value),
        _ => anyhow::bail!("no policy migration from schema version {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_precedes_broader_allow() {
        let root = std::env::temp_dir().join(format!("kiralm-policy-{}", uuid::Uuid::new_v4()));
        let store = PolicyStore::open(&root, Arc::new(EventBus::new(32))).unwrap();
        store
            .replace(
                0,
                vec![
                    PolicyRule {
                        id: String::new(),
                        name: "allow source".to_string(),
                        enabled: true,
                        project_id: None,
                        effect: PolicyEffect::Allow,
                        tool_pattern: "write_*".to_string(),
                        target_pattern: "src/*".to_string(),
                    },
                    PolicyRule {
                        id: String::new(),
                        name: "protect secrets".to_string(),
                        enabled: true,
                        project_id: None,
                        effect: PolicyEffect::Deny,
                        tool_pattern: "*".to_string(),
                        target_pattern: "*.env*".to_string(),
                    },
                ],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(
            store
                .evaluate("project", "write_file", "src/.env")
                .unwrap()
                .effect,
            PolicyEffect::Deny
        );
        let _ = std::fs::remove_file(root);
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let value = "é".repeat(MAX_HOOK_OUTPUT);
        let truncated = truncate(value);
        assert!(truncated.ends_with("[truncated]"));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[tokio::test]
    async fn blocking_command_hook_stops_the_operation() {
        let path =
            std::env::temp_dir().join(format!("kiralm-policy-hook-{}.json", uuid::Uuid::new_v4()));
        let store = PolicyStore::open(&path, Arc::new(EventBus::new(32))).unwrap();
        store
            .replace(
                0,
                Vec::new(),
                vec![PolicyHook {
                    id: String::new(),
                    name: "reject".to_string(),
                    enabled: true,
                    project_id: None,
                    phase: HookPhase::PreTool,
                    tool_pattern: "write_*".to_string(),
                    kind: HookKind::Command,
                    target: "exit 9".to_string(),
                    blocking: true,
                    timeout_ms: 2_000,
                }],
            )
            .unwrap();
        let result = store
            .run_hooks(
                HookPhase::PreTool,
                "project",
                Some("write_file"),
                Path::new("."),
                serde_json::json!({}),
            )
            .await;
        assert!(result.unwrap_err().contains("blocked the operation"));
        let _ = std::fs::remove_file(path);
    }
}
