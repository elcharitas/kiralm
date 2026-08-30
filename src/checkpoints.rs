use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::events::{EventBus, EventTopic};
use crate::tools::{MutationCheckpoint, PathSnapshot};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub chat_id: String,
    pub project_id: String,
    pub operation: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub restored_at: Option<chrono::DateTime<chrono::Utc>>,
    pub entries: Vec<PathSnapshot>,
    #[serde(default)]
    pub conversation: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointSummary {
    pub id: String,
    pub chat_id: String,
    pub project_id: String,
    pub operation: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub restored_at: Option<chrono::DateTime<chrono::Utc>>,
    pub paths: Vec<String>,
    pub entry_count: usize,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreResult {
    pub restored: CheckpointSummary,
    pub safety_checkpoint: CheckpointSummary,
    pub conversation: Option<serde_json::Value>,
}

pub struct RestoreOptions {
    pub current_conversation: Option<serde_json::Value>,
    pub restore_files: bool,
    pub restore_conversation: bool,
}

pub struct CheckpointStore {
    root: PathBuf,
    records: RwLock<HashMap<String, CheckpointRecord>>,
    events: std::sync::Arc<EventBus>,
}

impl CheckpointStore {
    pub fn open(
        root: impl Into<PathBuf>,
        events: std::sync::Arc<EventBus>,
    ) -> anyhow::Result<CheckpointStore> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let mut records = HashMap::new();
        for entry in std::fs::read_dir(&root)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match crate::storage::load_versioned::<CheckpointRecord, _>(
                &path,
                SCHEMA_VERSION,
                migrate,
            ) {
                Ok(Some(record)) => {
                    records.insert(record.id.clone(), record);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("failed to load checkpoint '{}': {error}", path.display())
                }
            }
        }
        Ok(CheckpointStore {
            root,
            records: RwLock::new(records),
            events,
        })
    }

    pub fn create(
        &self,
        chat_id: &str,
        project_id: &str,
        checkpoint: MutationCheckpoint,
        conversation: Option<serde_json::Value>,
    ) -> Result<CheckpointSummary, String> {
        if checkpoint.entries.is_empty() {
            return Err("checkpoint has no entries".to_string());
        }
        for entry in &checkpoint.entries {
            validate_relative_path(&entry.path)?;
        }
        let record = CheckpointRecord {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            project_id: project_id.to_string(),
            operation: checkpoint.operation,
            created_at: chrono::Utc::now(),
            restored_at: None,
            entries: checkpoint.entries,
            conversation,
        };
        self.save(&record)?;
        let summary = summarize(&record);
        self.records.write().insert(record.id.clone(), record);
        self.events.publish(
            EventTopic::Checkpoint,
            "created",
            Some(chat_id.to_string()),
            serde_json::to_value(&summary).unwrap_or_default(),
        );
        Ok(summary)
    }

    pub fn list(&self, chat_id: Option<&str>) -> Vec<CheckpointSummary> {
        let mut records: Vec<_> = self
            .records
            .read()
            .values()
            .filter(|record| chat_id.is_none_or(|id| record.chat_id == id))
            .map(summarize)
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        records
    }

    pub fn restore(
        &self,
        id: &str,
        chat_id: &str,
        project_id: &str,
        project_root: &Path,
        options: RestoreOptions,
    ) -> Result<RestoreResult, String> {
        let record = self
            .records
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("checkpoint '{id}' not found"))?;
        if record.chat_id != chat_id || record.project_id != project_id {
            return Err("checkpoint does not belong to this chat and workspace".to_string());
        }
        let conversation = if options.restore_conversation {
            Some(
                record
                    .conversation
                    .clone()
                    .ok_or_else(|| "checkpoint does not contain conversation state".to_string())?,
            )
        } else {
            None
        };
        let roots = root_paths(&record.entries)?;
        let root_refs: Vec<_> = roots.iter().map(String::as_str).collect();
        let safety = crate::tools::checkpoint_paths("restore_backup", &root_refs, project_root)?;
        let safety_summary =
            self.create(chat_id, project_id, safety, options.current_conversation)?;
        if options.restore_files {
            restore_entries(project_root, &record.entries)?;
        }
        let restored_at = chrono::Utc::now();
        let restored = {
            let mut records = self.records.write();
            let current = records
                .get_mut(id)
                .ok_or_else(|| format!("checkpoint '{id}' disappeared"))?;
            current.restored_at = Some(restored_at);
            self.save(current)?;
            summarize(current)
        };
        self.events.publish(
            EventTopic::Checkpoint,
            "restored",
            Some(chat_id.to_string()),
            serde_json::json!({
                "checkpoint_id": id,
                "chat_id": chat_id,
                "project_id": project_id,
                "safety_checkpoint_id": safety_summary.id.clone(),
            }),
        );
        Ok(RestoreResult {
            restored,
            safety_checkpoint: safety_summary,
            conversation,
        })
    }

    fn save(&self, record: &CheckpointRecord) -> Result<(), String> {
        crate::storage::save_versioned(
            &self.root.join(format!("{}.json", record.id)),
            SCHEMA_VERSION,
            record,
        )
        .map_err(|error| format!("failed to save checkpoint: {error}"))
    }
}

fn summarize(record: &CheckpointRecord) -> CheckpointSummary {
    CheckpointSummary {
        id: record.id.clone(),
        chat_id: record.chat_id.clone(),
        project_id: record.project_id.clone(),
        operation: record.operation.clone(),
        created_at: record.created_at,
        restored_at: record.restored_at,
        paths: root_paths(&record.entries).unwrap_or_default(),
        entry_count: record.entries.len(),
        size_bytes: record
            .entries
            .iter()
            .map(|entry| {
                entry
                    .content
                    .as_ref()
                    .map_or(0, |content| content.len() as u64)
            })
            .sum(),
    }
}

fn root_paths(entries: &[PathSnapshot]) -> Result<Vec<String>, String> {
    let paths: Vec<PathBuf> = entries
        .iter()
        .map(|entry| {
            validate_relative_path(&entry.path)?;
            Ok(PathBuf::from(&entry.path))
        })
        .collect::<Result<_, String>>()?;
    let set: HashSet<_> = paths.iter().cloned().collect();
    let mut roots = Vec::new();
    for path in &paths {
        let mut parent = path.parent();
        let mut nested = false;
        while let Some(value) = parent {
            if set.contains(value) {
                nested = true;
                break;
            }
            parent = value.parent();
        }
        if !nested {
            roots.push(path.display().to_string());
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn restore_entries(root: &Path, entries: &[PathSnapshot]) -> Result<(), String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("failed to open workspace: {error}"))?;
    validate_entry_tree(entries)?;
    let roots = root_paths(entries)?;
    for relative in &roots {
        clear_restore_path(&root, relative)?;
    }

    let mut directories: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == "directory")
        .collect();
    directories.sort_by_key(|entry| Path::new(&entry.path).components().count());
    for entry in directories {
        let target = safe_target(&root, &entry.path)?;
        std::fs::create_dir_all(&target)
            .map_err(|error| format!("failed to restore '{}': {error}", entry.path))?;
        restore_mode(&target, entry.unix_mode)?;
    }

    let mut leaves: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == "file" || entry.kind == "symlink")
        .collect();
    leaves.sort_by_key(|entry| Path::new(&entry.path).components().count());
    for entry in leaves {
        let target = safe_target(&root, &entry.path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("failed to restore '{}': {error}", entry.path))?;
        }
        if entry.kind == "file" {
            std::fs::write(&target, entry.content.as_deref().unwrap_or_default())
                .map_err(|error| format!("failed to restore '{}': {error}", entry.path))?;
            restore_mode(&target, entry.unix_mode)?;
        } else {
            restore_symlink(&target, entry.link_target.as_deref().unwrap_or_default())?;
        }
    }
    Ok(())
}

fn validate_entry_tree(entries: &[PathSnapshot]) -> Result<(), String> {
    let mut kinds = HashMap::new();
    for entry in entries {
        validate_relative_path(&entry.path)?;
        if !matches!(entry.kind.as_str(), "directory" | "file" | "symlink") {
            return Err(format!(
                "checkpoint entry '{}' has an invalid kind",
                entry.path
            ));
        }
        if kinds
            .insert(PathBuf::from(&entry.path), entry.kind.as_str())
            .is_some()
        {
            return Err(format!("checkpoint entry '{}' is duplicated", entry.path));
        }
    }
    for path in kinds.keys() {
        let mut parent = path.parent();
        while let Some(value) = parent {
            if kinds.get(value).is_some_and(|kind| *kind != "directory") {
                return Err(format!(
                    "checkpoint entry '{}' is nested below a non-directory",
                    path.display()
                ));
            }
            parent = value.parent();
        }
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(format!("checkpoint path '{value}' is not relative"));
    }
    let mut count = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => count += 1,
            _ => return Err(format!("checkpoint path '{value}' is unsafe")),
        }
    }
    if count == 0 {
        return Err(format!("checkpoint path '{value}' is unsafe"));
    }
    Ok(())
}

fn safe_target(root: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

fn clear_restore_path(root: &Path, relative: &str) -> Result<(), String> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    let components: Vec<_> = Path::new(relative).components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "failed to inspect '{}': {error}",
                    current.display()
                ));
            }
        };
        let last = index + 1 == components.len();
        if last || metadata.file_type().is_symlink() || !metadata.is_dir() {
            return remove_path(&current);
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to inspect '{}': {error}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .map_err(|error| format!("failed to clear '{}': {error}", path.display()))
}

#[cfg(unix)]
fn restore_symlink(path: &Path, target: &str) -> Result<(), String> {
    std::os::unix::fs::symlink(target, path)
        .map_err(|error| format!("failed to restore '{}': {error}", path.display()))
}

#[cfg(windows)]
fn restore_symlink(path: &Path, target: &str) -> Result<(), String> {
    std::os::windows::fs::symlink_file(target, path)
        .map_err(|error| format!("failed to restore '{}': {error}", path.display()))
}

#[cfg(unix)]
fn restore_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else { return Ok(()) };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("failed to restore permissions: {error}"))
}

#[cfg(not(unix))]
fn restore_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

fn migrate(version: u32, value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => Ok(value),
        _ => anyhow::bail!("no checkpoint migration from schema version {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_reversible_and_project_scoped() {
        let root = std::env::temp_dir().join(format!("kiralm-checkpoint-{}", uuid::Uuid::new_v4()));
        let store_root = root.join("store");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("file.txt"), "before").unwrap();
        let events = std::sync::Arc::new(EventBus::new(32));
        let store = CheckpointStore::open(&store_root, events).unwrap();
        let snapshot =
            crate::tools::checkpoint_paths("write_file", &["file.txt"], &project).unwrap();
        let created = store
            .create("chat", "project", snapshot, Some(serde_json::json!([])))
            .unwrap();
        std::fs::write(project.join("file.txt"), "after").unwrap();
        let result = store
            .restore(
                &created.id,
                "chat",
                "project",
                &project,
                RestoreOptions {
                    current_conversation: Some(
                        serde_json::json!([{"role":"user","content":"now"}]),
                    ),
                    restore_files: true,
                    restore_conversation: true,
                },
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join("file.txt")).unwrap(),
            "before"
        );
        store
            .restore(
                &result.safety_checkpoint.id,
                "chat",
                "project",
                &project,
                RestoreOptions {
                    current_conversation: result.conversation,
                    restore_files: true,
                    restore_conversation: true,
                },
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join("file.txt")).unwrap(),
            "after"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn restore_replaces_symlink_ancestors_without_touching_targets() {
        let root =
            std::env::temp_dir().join(format!("kiralm-checkpoint-link-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let external = root.join("external");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("file.txt"), "external").unwrap();
        std::os::unix::fs::symlink(&external, project.join("nested")).unwrap();

        restore_entries(
            &project,
            &[PathSnapshot {
                path: "nested/file.txt".to_string(),
                kind: "file".to_string(),
                content: Some(b"restored".to_vec()),
                link_target: None,
                unix_mode: None,
            }],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(external.join("file.txt")).unwrap(),
            "external"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("nested/file.txt")).unwrap(),
            "restored"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
