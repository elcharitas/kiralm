use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::oneshot;

use crate::events::{EventBus, EventTopic};
use crate::tools::Permission;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    Read,
    Write,
    ProcessExecution,
    NetworkAccess,
    GitMutation,
    DestructiveFilesystem,
    ExternalMcp,
}

impl ToolRisk {
    pub fn for_tool(name: &str) -> ToolRisk {
        match name {
            "read_file" | "list_directory" | "search_files" | "glob" | "git_diff"
            | "load_skill" => ToolRisk::Read,
            "write_file" | "edit_file" => ToolRisk::Write,
            "run_command" | "run_dsh_profile" => ToolRisk::ProcessExecution,
            "web_fetch" => ToolRisk::NetworkAccess,
            "git_commit" => ToolRisk::GitMutation,
            "delete_file" | "move_file" => ToolRisk::DestructiveFilesystem,
            _ => ToolRisk::ExternalMcp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowForChat,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Allowed,
    Denied(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub id: String,
    pub chat_id: String,
    pub tool_name: String,
    pub risk: ToolRisk,
    pub target: String,
    pub arguments: serde_json::Value,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalAuditRecord {
    pub approval_id: String,
    pub chat_id: String,
    pub tool_name: String,
    pub decision: String,
    pub decided_at: chrono::DateTime<chrono::Utc>,
}

struct PendingEntry {
    record: PendingApproval,
    sender: oneshot::Sender<ApprovalOutcome>,
}

pub struct ApprovalManager {
    pending: Mutex<HashMap<String, PendingEntry>>,
    chat_allow: RwLock<HashMap<(String, String), ()>>,
    audit: RwLock<Vec<ApprovalAuditRecord>>,
    timeout: Duration,
    events: Arc<EventBus>,
}

impl ApprovalManager {
    pub fn new(events: Arc<EventBus>) -> ApprovalManager {
        ApprovalManager::with_timeout(events, Duration::from_secs(300))
    }

    pub fn with_timeout(events: Arc<EventBus>, timeout: Duration) -> ApprovalManager {
        ApprovalManager {
            pending: Mutex::new(HashMap::new()),
            chat_allow: RwLock::new(HashMap::new()),
            audit: RwLock::new(Vec::new()),
            timeout,
            events,
        }
    }

    pub fn list(&self, chat_id: Option<&str>) -> Vec<PendingApproval> {
        self.expire_stale();
        let mut records: Vec<_> = self
            .pending
            .lock()
            .values()
            .filter(|entry| chat_id.is_none_or(|id| entry.record.chat_id == id))
            .map(|entry| entry.record.clone())
            .collect();
        records.sort_by_key(|record| record.requested_at);
        records
    }

    #[cfg(test)]
    pub fn audit_records(&self) -> Vec<ApprovalAuditRecord> {
        self.audit.read().clone()
    }

    pub async fn request(
        &self,
        chat_id: &str,
        tool_name: &str,
        args_json: &str,
        risk: ToolRisk,
    ) -> ApprovalOutcome {
        if self
            .chat_allow
            .read()
            .contains_key(&(chat_id.to_string(), tool_name.to_string()))
        {
            return ApprovalOutcome::Allowed;
        }

        self.expire_stale();
        let now = chrono::Utc::now();
        let id = uuid::Uuid::new_v4().to_string();
        let arguments = serde_json::from_str(args_json).unwrap_or_else(|_| json!({}));
        let record = PendingApproval {
            id: id.clone(),
            chat_id: chat_id.to_string(),
            tool_name: tool_name.to_string(),
            risk,
            target: approval_target(tool_name, &arguments),
            arguments,
            requested_at: now,
            expires_at: now
                + chrono::Duration::from_std(self.timeout)
                    .unwrap_or_else(|_| chrono::Duration::minutes(5)),
        };
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(
            id.clone(),
            PendingEntry {
                record: record.clone(),
                sender,
            },
        );
        self.events.publish(
            EventTopic::Approval,
            "requested",
            Some(id.clone()),
            serde_json::to_value(&record).unwrap_or_else(|_| json!({})),
        );

        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => ApprovalOutcome::Denied("approval was cancelled".to_string()),
            Err(_) => {
                self.finish(&id, ApprovalDecision::Deny, "expired");
                ApprovalOutcome::Denied("approval expired".to_string())
            }
        }
    }

    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> Result<(), String> {
        self.expire_stale();
        self.finish(id, decision, "user")
            .then_some(())
            .ok_or_else(|| format!("approval '{id}' is not pending"))
    }

    pub fn cancel_chat(&self, chat_id: &str) {
        let ids: Vec<_> = self
            .pending
            .lock()
            .values()
            .filter(|entry| entry.record.chat_id == chat_id)
            .map(|entry| entry.record.id.clone())
            .collect();
        for id in ids {
            self.finish(&id, ApprovalDecision::Deny, "cancelled");
        }
    }

    pub fn forget_chat(&self, chat_id: &str) {
        self.cancel_chat(chat_id);
        self.chat_allow
            .write()
            .retain(|(allowed_chat, _), _| allowed_chat != chat_id);
    }

    fn expire_stale(&self) {
        let now = chrono::Utc::now();
        let ids: Vec<_> = self
            .pending
            .lock()
            .values()
            .filter(|entry| entry.record.expires_at <= now)
            .map(|entry| entry.record.id.clone())
            .collect();
        for id in ids {
            self.finish(&id, ApprovalDecision::Deny, "expired");
        }
    }

    fn finish(&self, id: &str, decision: ApprovalDecision, source: &str) -> bool {
        let Some(entry) = self.pending.lock().remove(id) else {
            return false;
        };
        if decision == ApprovalDecision::AllowForChat {
            self.chat_allow.write().insert(
                (entry.record.chat_id.clone(), entry.record.tool_name.clone()),
                (),
            );
        }
        let outcome = match decision {
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowForChat => {
                ApprovalOutcome::Allowed
            }
            ApprovalDecision::Deny => ApprovalOutcome::Denied(
                match source {
                    "expired" => "approval expired",
                    "cancelled" => "generation stopped",
                    _ => "approval denied",
                }
                .to_string(),
            ),
        };
        let decision_name = match decision {
            ApprovalDecision::AllowOnce => "allow_once",
            ApprovalDecision::AllowForChat => "allow_for_chat",
            ApprovalDecision::Deny => match source {
                "expired" => "expired",
                "cancelled" => "cancelled",
                _ => "deny",
            },
        };
        let audit = ApprovalAuditRecord {
            approval_id: entry.record.id.clone(),
            chat_id: entry.record.chat_id.clone(),
            tool_name: entry.record.tool_name.clone(),
            decision: decision_name.to_string(),
            decided_at: chrono::Utc::now(),
        };
        self.audit.write().push(audit.clone());
        self.events.publish(
            EventTopic::Approval,
            "decided",
            Some(entry.record.id),
            json!({
                "chat_id": audit.chat_id,
                "tool_name": audit.tool_name,
                "decision": audit.decision,
            }),
        );
        let _ = entry.sender.send(outcome);
        true
    }
}

pub async fn authorize(
    manager: &ApprovalManager,
    chat_id: &str,
    permission: Permission,
    name: &str,
    args_json: &str,
) -> ApprovalOutcome {
    let risk = ToolRisk::for_tool(name);
    if risk == ToolRisk::Read {
        return ApprovalOutcome::Allowed;
    }
    if permission == Permission::ReadOnly {
        return ApprovalOutcome::Denied(format!(
            "{name} is blocked because this chat is read-only"
        ));
    }
    if permission == Permission::Ask || risk == ToolRisk::ExternalMcp {
        return manager.request(chat_id, name, args_json, risk).await;
    }
    ApprovalOutcome::Allowed
}

fn approval_target(name: &str, args: &serde_json::Value) -> String {
    let string_arg = |key: &str| args.get(key).and_then(|value| value.as_str());
    match name {
        "read_file" | "list_directory" | "search_files" | "glob" | "write_file" | "edit_file"
        | "delete_file" => string_arg("path").unwrap_or(".").to_string(),
        "move_file" => format!(
            "{} -> {}",
            string_arg("from").unwrap_or("?"),
            string_arg("to").unwrap_or("?")
        ),
        "run_command" => string_arg("command").unwrap_or("").to_string(),
        "run_dsh_profile" => string_arg("profile").unwrap_or("").to_string(),
        "web_fetch" => string_arg("url").unwrap_or("").to_string(),
        "git_commit" => string_arg("message").unwrap_or("").to_string(),
        name if name.starts_with("mcp__") => name.to_string(),
        _ => args.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(timeout: Duration) -> Arc<ApprovalManager> {
        Arc::new(ApprovalManager::with_timeout(
            Arc::new(EventBus::new(32)),
            timeout,
        ))
    }

    async fn wait_for_pending(manager: &ApprovalManager, chat_id: &str) -> PendingApproval {
        for _ in 0..100 {
            if let Some(record) = manager.list(Some(chat_id)).into_iter().next() {
                return record;
            }
            tokio::task::yield_now().await;
        }
        panic!("approval did not become pending");
    }

    #[tokio::test]
    async fn deny_has_no_authorization() {
        let manager = manager(Duration::from_secs(2));
        let requester = manager.clone();
        let request = tokio::spawn(async move {
            authorize(&requester, "chat-a", Permission::Ask, "write_file", "{}").await
        });
        let pending = wait_for_pending(&manager, "chat-a").await;
        manager
            .resolve(&pending.id, ApprovalDecision::Deny)
            .unwrap();
        assert!(matches!(request.await.unwrap(), ApprovalOutcome::Denied(_)));
        assert_eq!(manager.audit_records()[0].decision, "deny");
    }

    #[tokio::test]
    async fn allow_once_applies_once() {
        let manager = manager(Duration::from_secs(2));
        let requester = manager.clone();
        let first = tokio::spawn(async move {
            authorize(&requester, "chat-a", Permission::Ask, "write_file", "{}").await
        });
        let pending = wait_for_pending(&manager, "chat-a").await;
        manager
            .resolve(&pending.id, ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(first.await.unwrap(), ApprovalOutcome::Allowed);

        let requester = manager.clone();
        let second = tokio::spawn(async move {
            authorize(&requester, "chat-a", Permission::Ask, "write_file", "{}").await
        });
        let pending = wait_for_pending(&manager, "chat-a").await;
        manager
            .resolve(&pending.id, ApprovalDecision::Deny)
            .unwrap();
        assert!(matches!(second.await.unwrap(), ApprovalOutcome::Denied(_)));
    }

    #[tokio::test]
    async fn chat_policy_does_not_cross_chats() {
        let manager = manager(Duration::from_secs(2));
        let requester = manager.clone();
        let first = tokio::spawn(async move {
            authorize(&requester, "chat-a", Permission::Ask, "write_file", "{}").await
        });
        let pending = wait_for_pending(&manager, "chat-a").await;
        manager
            .resolve(&pending.id, ApprovalDecision::AllowForChat)
            .unwrap();
        assert_eq!(first.await.unwrap(), ApprovalOutcome::Allowed);
        assert_eq!(
            authorize(&manager, "chat-a", Permission::Ask, "write_file", "{}").await,
            ApprovalOutcome::Allowed
        );

        let requester = manager.clone();
        let other = tokio::spawn(async move {
            authorize(&requester, "chat-b", Permission::Ask, "write_file", "{}").await
        });
        let pending = wait_for_pending(&manager, "chat-b").await;
        manager
            .resolve(&pending.id, ApprovalDecision::Deny)
            .unwrap();
        assert!(matches!(other.await.unwrap(), ApprovalOutcome::Denied(_)));
    }

    #[tokio::test]
    async fn listing_survives_reconnect_and_mcp_asks() {
        let manager = manager(Duration::from_secs(2));
        let requester = manager.clone();
        let request = tokio::spawn(async move {
            authorize(
                &requester,
                "chat-a",
                Permission::FullAccess,
                "mcp__mail__send",
                r#"{"to":"person@example.com"}"#,
            )
            .await
        });
        let first = wait_for_pending(&manager, "chat-a").await;
        let listed = manager.list(Some("chat-a"));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, first.id);
        assert_eq!(listed[0].risk, ToolRisk::ExternalMcp);
        manager.resolve(&first.id, ApprovalDecision::Deny).unwrap();
        assert!(matches!(request.await.unwrap(), ApprovalOutcome::Denied(_)));
    }

    #[tokio::test]
    async fn expiration_denies_and_removes_pending_record() {
        let manager = manager(Duration::from_millis(20));
        let outcome = authorize(&manager, "chat-a", Permission::Ask, "write_file", "{}").await;
        assert_eq!(
            outcome,
            ApprovalOutcome::Denied("approval expired".to_string())
        );
        assert!(manager.list(Some("chat-a")).is_empty());
        assert_eq!(manager.audit_records()[0].decision, "expired");
    }
}
