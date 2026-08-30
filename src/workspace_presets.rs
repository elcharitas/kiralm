use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::kiralm_home;

const SCHEMA_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 100;
const MAX_ID_BYTES: usize = 256;
const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_KNOWLEDGE_ROOTS: usize = 32;
const MAX_MODEL_IDS: usize = 128;
const MAX_MCP_SERVERS: usize = 128;

pub fn workspace_presets_path() -> PathBuf {
    kiralm_home().join("workspace-presets.json")
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePermission {
    ReadOnly,
    Ask,
    FullAccess,
}

impl WorkspacePermission {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "ask" => Some(Self::Ask),
            "full-access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOptimizationProfile {
    Balanced,
    Throughput,
    Latency,
    Memory,
}

impl WorkspaceOptimizationProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "balanced" => Some(Self::Balanced),
            "throughput" => Some(Self::Throughput),
            "latency" => Some(Self::Latency),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Throughput => "throughput",
            Self::Latency => "latency",
            Self::Memory => "memory",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkspacePresetInput {
    pub name: String,
    pub project_id: Option<String>,
    pub preferred_model: Option<String>,
    pub permission: WorkspacePermission,
    pub optimization_profile: WorkspaceOptimizationProfile,
    pub knowledge_roots: Vec<PathBuf>,
    pub hot_model_ids: Vec<String>,
    pub enabled_mcp_servers: Vec<String>,
    pub instruction_text: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkspacePreset {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub preferred_model: Option<String>,
    pub permission: WorkspacePermission,
    pub optimization_profile: WorkspaceOptimizationProfile,
    pub knowledge_roots: Vec<PathBuf>,
    pub hot_model_ids: Vec<String>,
    pub enabled_mcp_servers: Vec<String>,
    pub instruction_text: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Replace,
    Merge,
}

#[derive(Debug)]
pub enum WorkspacePresetError {
    Invalid(String),
    NotFound(String),
    NameConflict(String),
    RevisionConflict {
        id: String,
        expected: u64,
        actual: u64,
    },
    StoreRevisionConflict {
        expected: u64,
        actual: u64,
    },
    Storage(anyhow::Error),
}

impl fmt::Display for WorkspacePresetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid workspace preset: {message}"),
            Self::NotFound(id) => write!(formatter, "workspace preset '{id}' was not found"),
            Self::NameConflict(name) => {
                write!(formatter, "workspace preset name '{name}' already exists")
            }
            Self::RevisionConflict {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "workspace preset '{id}' revision conflict: expected {expected}, got {actual}"
            ),
            Self::StoreRevisionConflict { expected, actual } => write!(
                formatter,
                "workspace preset store revision conflict: expected {expected}, got {actual}"
            ),
            Self::Storage(error) => write!(formatter, "workspace preset storage failed: {error}"),
        }
    }
}

impl std::error::Error for WorkspacePresetError {}

impl From<anyhow::Error> for WorkspacePresetError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

pub type WorkspacePresetResult<T> = Result<T, WorkspacePresetError>;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct WorkspacePresetDocument {
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    presets: Vec<WorkspacePreset>,
}

pub struct WorkspacePresetStore {
    path: PathBuf,
    document: RwLock<WorkspacePresetDocument>,
}

impl WorkspacePresetStore {
    pub fn load() -> WorkspacePresetResult<Self> {
        Self::open(workspace_presets_path())
    }

    pub fn open(path: impl Into<PathBuf>) -> WorkspacePresetResult<Self> {
        let path = path.into();
        let loaded =
            crate::storage::load_versioned(&path, SCHEMA_VERSION, migrate_workspace_presets)?;
        let mut document = loaded.unwrap_or_default();
        normalize_document(&mut document)?;
        Ok(Self {
            path,
            document: RwLock::new(document),
        })
    }

    pub fn revision(&self) -> u64 {
        self.document.read().revision
    }

    pub fn list(&self) -> Vec<WorkspacePreset> {
        self.document.read().presets.clone()
    }

    pub fn get(&self, id: &str) -> Option<WorkspacePreset> {
        self.document
            .read()
            .presets
            .iter()
            .find(|preset| preset.id == id)
            .cloned()
    }

    pub fn create(&self, input: WorkspacePresetInput) -> WorkspacePresetResult<WorkspacePreset> {
        let input = normalize_input(input)?;
        let now = Utc::now();
        let preset = WorkspacePreset {
            id: uuid::Uuid::new_v4().to_string(),
            name: input.name,
            project_id: input.project_id,
            preferred_model: input.preferred_model,
            permission: input.permission,
            optimization_profile: input.optimization_profile,
            knowledge_roots: input.knowledge_roots,
            hot_model_ids: input.hot_model_ids,
            enabled_mcp_servers: input.enabled_mcp_servers,
            instruction_text: input.instruction_text,
            created_at: now,
            updated_at: now,
            revision: 1,
        };
        let mut guard = self.document.write();
        ensure_name_available(&guard, &preset.name, None)?;
        let mut next = guard.clone();
        next.presets.push(preset.clone());
        advance_document(&mut next)?;
        self.persist_transition(guard.revision, &next)?;
        *guard = next;
        Ok(preset)
    }

    pub fn update(
        &self,
        id: &str,
        expected_revision: u64,
        input: WorkspacePresetInput,
    ) -> WorkspacePresetResult<WorkspacePreset> {
        validate_id(id, "id")?;
        let input = normalize_input(input)?;
        let mut guard = self.document.write();
        let current = find_preset(&guard, id)?.clone();
        ensure_revision(&current, expected_revision)?;
        ensure_name_available(&guard, &input.name, Some(id))?;
        let updated = WorkspacePreset {
            id: current.id.clone(),
            name: input.name,
            project_id: input.project_id,
            preferred_model: input.preferred_model,
            permission: input.permission,
            optimization_profile: input.optimization_profile,
            knowledge_roots: input.knowledge_roots,
            hot_model_ids: input.hot_model_ids,
            enabled_mcp_servers: input.enabled_mcp_servers,
            instruction_text: input.instruction_text,
            created_at: current.created_at,
            updated_at: Utc::now(),
            revision: current.revision.checked_add(1).ok_or_else(|| {
                WorkspacePresetError::Invalid("record revision overflowed".to_string())
            })?,
        };
        let mut next = guard.clone();
        let slot = next
            .presets
            .iter_mut()
            .find(|preset| preset.id == id)
            .expect("the preset was found before cloning");
        *slot = updated.clone();
        advance_document(&mut next)?;
        self.persist_transition(guard.revision, &next)?;
        *guard = next;
        Ok(updated)
    }

    pub fn delete(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> WorkspacePresetResult<WorkspacePreset> {
        validate_id(id, "id")?;
        let mut guard = self.document.write();
        let current = find_preset(&guard, id)?.clone();
        ensure_revision(&current, expected_revision)?;
        let mut next = guard.clone();
        next.presets.retain(|preset| preset.id != id);
        advance_document(&mut next)?;
        self.persist_transition(guard.revision, &next)?;
        *guard = next;
        Ok(current)
    }

    pub fn duplicate(
        &self,
        id: &str,
        expected_revision: u64,
        new_name: &str,
    ) -> WorkspacePresetResult<WorkspacePreset> {
        validate_id(id, "id")?;
        let mut guard = self.document.write();
        let current = find_preset(&guard, id)?.clone();
        ensure_revision(&current, expected_revision)?;
        let name = normalize_name(new_name)?;
        ensure_name_available(&guard, &name, None)?;
        let now = Utc::now();
        let duplicate = WorkspacePreset {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            created_at: now,
            updated_at: now,
            revision: 1,
            ..current
        };
        let mut next = guard.clone();
        next.presets.push(duplicate.clone());
        advance_document(&mut next)?;
        self.persist_transition(guard.revision, &next)?;
        *guard = next;
        Ok(duplicate)
    }

    pub fn export_json(&self) -> WorkspacePresetResult<String> {
        let document = self.document.read();
        let mut value = serde_json::to_value(&*document).map_err(anyhow::Error::from)?;
        value
            .as_object_mut()
            .expect("workspace preset documents serialize as objects")
            .insert("schema_version".to_string(), SCHEMA_VERSION.into());
        serde_json::to_string_pretty(&value)
            .map_err(anyhow::Error::from)
            .map_err(Into::into)
    }

    pub fn import_json(
        &self,
        json: &str,
        mode: ImportMode,
        expected_store_revision: u64,
    ) -> WorkspacePresetResult<Vec<WorkspacePreset>> {
        let mut imported = parse_import(json)?;
        normalize_document(&mut imported)?;
        let mut guard = self.document.write();
        if guard.revision != expected_store_revision {
            return Err(WorkspacePresetError::StoreRevisionConflict {
                expected: expected_store_revision,
                actual: guard.revision,
            });
        }
        let imported_presets = imported.presets.clone();
        let mut next = match mode {
            ImportMode::Replace => WorkspacePresetDocument {
                revision: guard.revision,
                presets: imported.presets,
            },
            ImportMode::Merge => {
                ensure_merge_available(&guard, &imported.presets)?;
                let mut next = guard.clone();
                next.presets.extend(imported.presets);
                next
            }
        };
        advance_document(&mut next)?;
        self.persist_transition(guard.revision, &next)?;
        *guard = next;
        Ok(imported_presets)
    }

    /// Returns a stable snapshot and never changes downstream state.
    /// Callers should apply every field as one transaction.
    pub fn resolve_for_apply(
        &self,
        id: &str,
        expected_revision: Option<u64>,
    ) -> WorkspacePresetResult<WorkspacePreset> {
        validate_id(id, "id")?;
        let document = self.document.read();
        let preset = find_preset(&document, id)?;
        if let Some(expected) = expected_revision {
            ensure_revision(preset, expected)?;
        }
        Ok(preset.clone())
    }

    fn persist_transition(
        &self,
        expected_revision: u64,
        next: &WorkspacePresetDocument,
    ) -> WorkspacePresetResult<()> {
        let disk = crate::storage::load_versioned::<WorkspacePresetDocument, _>(
            &self.path,
            SCHEMA_VERSION,
            migrate_workspace_presets,
        )?
        .unwrap_or_default();
        if disk.revision != expected_revision {
            return Err(WorkspacePresetError::StoreRevisionConflict {
                expected: expected_revision,
                actual: disk.revision,
            });
        }
        crate::storage::save_versioned(&self.path, SCHEMA_VERSION, next)?;
        Ok(())
    }
}

fn find_preset<'a>(
    document: &'a WorkspacePresetDocument,
    id: &str,
) -> WorkspacePresetResult<&'a WorkspacePreset> {
    document
        .presets
        .iter()
        .find(|preset| preset.id == id)
        .ok_or_else(|| WorkspacePresetError::NotFound(id.to_string()))
}

fn ensure_revision(preset: &WorkspacePreset, expected_revision: u64) -> WorkspacePresetResult<()> {
    if preset.revision == expected_revision {
        return Ok(());
    }
    Err(WorkspacePresetError::RevisionConflict {
        id: preset.id.clone(),
        expected: expected_revision,
        actual: preset.revision,
    })
}

fn ensure_name_available(
    document: &WorkspacePresetDocument,
    name: &str,
    except_id: Option<&str>,
) -> WorkspacePresetResult<()> {
    let key = normalized_name_key(name);
    if document.presets.iter().any(|preset| {
        Some(preset.id.as_str()) != except_id && normalized_name_key(&preset.name) == key
    }) {
        return Err(WorkspacePresetError::NameConflict(name.to_string()));
    }
    Ok(())
}

fn ensure_merge_available(
    current: &WorkspacePresetDocument,
    imported: &[WorkspacePreset],
) -> WorkspacePresetResult<()> {
    let current_ids: HashSet<_> = current.presets.iter().map(|preset| &preset.id).collect();
    for preset in imported {
        if current_ids.contains(&preset.id) {
            return Err(WorkspacePresetError::Invalid(format!(
                "preset id '{}' already exists",
                preset.id
            )));
        }
        ensure_name_available(current, &preset.name, None)?;
    }
    Ok(())
}

fn advance_document(document: &mut WorkspacePresetDocument) -> WorkspacePresetResult<()> {
    document.revision = document
        .revision
        .checked_add(1)
        .ok_or_else(|| WorkspacePresetError::Invalid("store revision overflowed".to_string()))?;
    normalize_document(document)
}

fn normalize_document(document: &mut WorkspacePresetDocument) -> WorkspacePresetResult<()> {
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for preset in &mut document.presets {
        normalize_preset(preset)?;
        if !ids.insert(preset.id.clone()) {
            return Err(WorkspacePresetError::Invalid(format!(
                "duplicate preset id '{}'",
                preset.id
            )));
        }
        if !names.insert(normalized_name_key(&preset.name)) {
            return Err(WorkspacePresetError::NameConflict(preset.name.clone()));
        }
    }
    document.presets.sort_by(|left, right| {
        normalized_name_key(&left.name)
            .cmp(&normalized_name_key(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(())
}

fn normalize_preset(preset: &mut WorkspacePreset) -> WorkspacePresetResult<()> {
    validate_id(&preset.id, "id")?;
    if preset.revision == 0 {
        return Err(WorkspacePresetError::Invalid(
            "record revision must be at least 1".to_string(),
        ));
    }
    if preset.updated_at < preset.created_at {
        return Err(WorkspacePresetError::Invalid(
            "updated_at cannot precede created_at".to_string(),
        ));
    }
    let input = normalize_input(WorkspacePresetInput {
        name: std::mem::take(&mut preset.name),
        project_id: preset.project_id.take(),
        preferred_model: preset.preferred_model.take(),
        permission: preset.permission,
        optimization_profile: preset.optimization_profile,
        knowledge_roots: std::mem::take(&mut preset.knowledge_roots),
        hot_model_ids: std::mem::take(&mut preset.hot_model_ids),
        enabled_mcp_servers: std::mem::take(&mut preset.enabled_mcp_servers),
        instruction_text: std::mem::take(&mut preset.instruction_text),
    })?;
    preset.name = input.name;
    preset.project_id = input.project_id;
    preset.preferred_model = input.preferred_model;
    preset.permission = input.permission;
    preset.optimization_profile = input.optimization_profile;
    preset.knowledge_roots = input.knowledge_roots;
    preset.hot_model_ids = input.hot_model_ids;
    preset.enabled_mcp_servers = input.enabled_mcp_servers;
    preset.instruction_text = input.instruction_text;
    Ok(())
}

fn normalize_input(mut input: WorkspacePresetInput) -> WorkspacePresetResult<WorkspacePresetInput> {
    input.name = normalize_name(&input.name)?;
    input.project_id = normalize_optional_id(input.project_id, "project_id")?;
    input.preferred_model = normalize_optional_value(input.preferred_model, "preferred_model")?;
    input.knowledge_roots = normalize_roots(input.knowledge_roots)?;
    input.hot_model_ids = normalize_values(input.hot_model_ids, "hot_model_ids", MAX_MODEL_IDS)?;
    input.enabled_mcp_servers = normalize_values(
        input.enabled_mcp_servers,
        "enabled_mcp_servers",
        MAX_MCP_SERVERS,
    )?;
    input.instruction_text = input.instruction_text.replace("\r\n", "\n");
    if input.instruction_text.len() > MAX_INSTRUCTION_BYTES {
        return Err(WorkspacePresetError::Invalid(format!(
            "instruction_text exceeds {MAX_INSTRUCTION_BYTES} bytes"
        )));
    }
    Ok(input)
}

fn normalize_name(value: &str) -> WorkspacePresetResult<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(WorkspacePresetError::Invalid(
            "name cannot be empty".to_string(),
        ));
    }
    if normalized.len() > MAX_NAME_BYTES {
        return Err(WorkspacePresetError::Invalid(format!(
            "name exceeds {MAX_NAME_BYTES} bytes"
        )));
    }
    Ok(normalized)
}

fn normalized_name_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn normalize_optional_id(
    value: Option<String>,
    field: &str,
) -> WorkspacePresetResult<Option<String>> {
    value
        .map(|value| {
            let normalized = value.trim().to_string();
            validate_id(&normalized, field)?;
            Ok(normalized)
        })
        .transpose()
}

fn normalize_optional_value(
    value: Option<String>,
    field: &str,
) -> WorkspacePresetResult<Option<String>> {
    value
        .map(|value| normalize_value(&value, field))
        .transpose()
}

fn normalize_values(
    values: Vec<String>,
    field: &str,
    max_items: usize,
) -> WorkspacePresetResult<Vec<String>> {
    if values.len() > max_items {
        return Err(WorkspacePresetError::Invalid(format!(
            "{field} exceeds {max_items} items"
        )));
    }
    let mut normalized = values
        .iter()
        .map(|value| normalize_value(value, field))
        .collect::<WorkspacePresetResult<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_value(value: &str, field: &str) -> WorkspacePresetResult<String> {
    let normalized = value.trim().to_string();
    if normalized.is_empty() {
        return Err(WorkspacePresetError::Invalid(format!(
            "{field} contains an empty value"
        )));
    }
    if normalized.len() > MAX_ID_BYTES || normalized.chars().any(char::is_control) {
        return Err(WorkspacePresetError::Invalid(format!(
            "{field} contains an invalid value"
        )));
    }
    Ok(normalized)
}

fn validate_id(value: &str, field: &str) -> WorkspacePresetResult<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(WorkspacePresetError::Invalid(format!(
            "{field} contains an invalid identifier"
        )));
    }
    Ok(())
}

fn normalize_roots(roots: Vec<PathBuf>) -> WorkspacePresetResult<Vec<PathBuf>> {
    if roots.len() > MAX_KNOWLEDGE_ROOTS {
        return Err(WorkspacePresetError::Invalid(format!(
            "knowledge_roots exceeds {MAX_KNOWLEDGE_ROOTS} items"
        )));
    }
    let mut normalized = roots
        .iter()
        .map(|root| normalize_root(root))
        .collect::<WorkspacePresetResult<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_root(root: &Path) -> WorkspacePresetResult<PathBuf> {
    let Some(text) = root.to_str() else {
        return Err(WorkspacePresetError::Invalid(
            "knowledge_roots must use UTF-8 paths".to_string(),
        ));
    };
    if text.len() > MAX_PATH_BYTES || !root.is_absolute() {
        return Err(WorkspacePresetError::Invalid(
            "knowledge_roots must contain bounded absolute paths".to_string(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() || normalized.as_os_str().is_empty() {
                    return Err(WorkspacePresetError::Invalid(
                        "knowledge_roots cannot escape the filesystem root".to_string(),
                    ));
                }
            }
        }
    }
    if !normalized.is_absolute() {
        return Err(WorkspacePresetError::Invalid(
            "knowledge_roots must remain absolute after normalization".to_string(),
        ));
    }
    Ok(normalized)
}

fn parse_import(json: &str) -> WorkspacePresetResult<WorkspacePresetDocument> {
    let mut value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| WorkspacePresetError::Invalid(error.to_string()))?;
    let object = value.as_object().ok_or_else(|| {
        WorkspacePresetError::Invalid("import root must be an object".to_string())
    })?;
    let version = match object.get("schema_version") {
        Some(value) => value.as_u64().ok_or_else(|| {
            WorkspacePresetError::Invalid(
                "schema_version must be a non-negative integer".to_string(),
            )
        })?,
        None => 0,
    };
    if version > u64::from(SCHEMA_VERSION) {
        return Err(WorkspacePresetError::Invalid(format!(
            "import uses schema version {version}, but this build supports up to {SCHEMA_VERSION}"
        )));
    }
    let mut current = u32::try_from(version)
        .map_err(|_| WorkspacePresetError::Invalid("schema_version is too large".to_string()))?;
    while current < SCHEMA_VERSION {
        value = migrate_workspace_presets(current, value)?;
        current += 1;
        value
            .as_object_mut()
            .expect("workspace preset migrations return objects")
            .insert("schema_version".to_string(), current.into());
    }
    serde_json::from_value(value).map_err(|error| WorkspacePresetError::Invalid(error.to_string()))
}

fn migrate_workspace_presets(
    version: u32,
    mut value: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => {
            let object = value
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("workspace preset root must be an object"))?;
            if !object.contains_key("presets") {
                if let Some(presets) = object.remove("workspace_presets") {
                    object.insert("presets".to_string(), presets);
                }
            }
            object
                .entry("revision".to_string())
                .or_insert_with(|| 0.into());
            let now = Utc::now().to_rfc3339();
            let presets = object
                .entry("presets".to_string())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            let presets = presets
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("presets must be an array"))?;
            for preset in presets {
                migrate_legacy_preset(preset, &now)?;
            }
            Ok(value)
        }
        _ => anyhow::bail!("no workspace preset migration from schema version {version}"),
    }
}

fn migrate_legacy_preset(value: &mut serde_json::Value, now: &str) -> anyhow::Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("workspace preset must be an object"))?;
    rename_field(object, "workspace_id", "project_id");
    rename_field(object, "model", "preferred_model");
    rename_field(object, "optimization", "optimization_profile");
    rename_field(object, "hot_models", "hot_model_ids");
    rename_field(object, "mcp_servers", "enabled_mcp_servers");
    rename_field(object, "instructions", "instruction_text");
    if let Some(permission) = object.get_mut("permission") {
        if let Some(permission) = permission.as_str() {
            *object.get_mut("permission").expect("permission exists") =
                permission.replace('_', "-").into();
        }
    }
    insert_default(object, "project_id", serde_json::Value::Null);
    insert_default(object, "preferred_model", serde_json::Value::Null);
    insert_default(object, "permission", "read-only".into());
    insert_default(object, "optimization_profile", "balanced".into());
    insert_default(object, "knowledge_roots", serde_json::json!([]));
    insert_default(object, "hot_model_ids", serde_json::json!([]));
    insert_default(object, "enabled_mcp_servers", serde_json::json!([]));
    insert_default(object, "instruction_text", "".into());
    insert_default(object, "created_at", now.into());
    insert_default(object, "updated_at", now.into());
    insert_default(object, "revision", 1.into());
    Ok(())
}

fn rename_field(object: &mut serde_json::Map<String, serde_json::Value>, old: &str, new: &str) {
    if !object.contains_key(new) {
        if let Some(value) = object.remove(old) {
            object.insert(new.to_string(), value);
        }
    }
}

fn insert_default(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
) {
    object.entry(key.to_string()).or_insert(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kiralm-workspace-presets-{name}-{}.json",
            uuid::Uuid::new_v4()
        ))
    }

    fn input(name: &str) -> WorkspacePresetInput {
        WorkspacePresetInput {
            name: name.to_string(),
            project_id: Some("project-1".to_string()),
            preferred_model: Some("org/model:q4".to_string()),
            permission: WorkspacePermission::Ask,
            optimization_profile: WorkspaceOptimizationProfile::Balanced,
            knowledge_roots: vec![PathBuf::from("/tmp/project/docs")],
            hot_model_ids: vec!["model-b".to_string(), "model-a".to_string()],
            enabled_mcp_servers: vec!["source".to_string()],
            instruction_text: "Use project conventions.".to_string(),
        }
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("json.v0.bak"));
    }

    #[test]
    fn creates_updates_lists_and_deletes_presets() {
        let path = test_path("crud");
        let store = WorkspacePresetStore::open(&path).unwrap();
        let second = store.create(input("Zulu")).unwrap();
        let first = store.create(input("  Alpha   workspace ")).unwrap();

        assert_eq!(store.list()[0].name, "Alpha workspace");
        assert_eq!(store.list()[1].id, second.id);
        assert_eq!(first.hot_model_ids, vec!["model-a", "model-b"]);

        let mut changed = input("Beta");
        changed.permission = WorkspacePermission::FullAccess;
        let changed = store.update(&first.id, 1, changed).unwrap();
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.permission, WorkspacePermission::FullAccess);
        assert_eq!(
            store.resolve_for_apply(&first.id, Some(2)).unwrap(),
            changed
        );

        assert_eq!(store.delete(&second.id, 1).unwrap().id, second.id);
        assert_eq!(store.list(), vec![changed]);
        cleanup(&path);
    }

    #[test]
    fn rejects_stale_record_and_store_revisions() {
        let path = test_path("conflict");
        let store = WorkspacePresetStore::open(&path).unwrap();
        let preset = store.create(input("One")).unwrap();
        store.update(&preset.id, 1, input("One updated")).unwrap();

        assert!(matches!(
            store.update(&preset.id, 1, input("Stale")),
            Err(WorkspacePresetError::RevisionConflict { actual: 2, .. })
        ));
        assert!(matches!(
            store.import_json(&store.export_json().unwrap(), ImportMode::Replace, 1),
            Err(WorkspacePresetError::StoreRevisionConflict { actual: 2, .. })
        ));
        cleanup(&path);
    }

    #[test]
    fn duplicates_without_reusing_identity_or_revision() {
        let path = test_path("duplicate");
        let store = WorkspacePresetStore::open(&path).unwrap();
        let original = store.create(input("Original")).unwrap();
        let duplicate = store
            .duplicate(&original.id, original.revision, "Original copy")
            .unwrap();

        assert_ne!(duplicate.id, original.id);
        assert_eq!(duplicate.revision, 1);
        assert_eq!(duplicate.created_at, duplicate.updated_at);
        assert_eq!(duplicate.preferred_model, original.preferred_model);
        assert!(matches!(
            store.create(input(" original   COPY ")),
            Err(WorkspacePresetError::NameConflict(_))
        ));
        cleanup(&path);
    }

    #[test]
    fn rejects_invalid_paths_permissions_and_profiles() {
        let path = test_path("invalid");
        let store = WorkspacePresetStore::open(&path).unwrap();
        let mut invalid_path = input("Bad path");
        invalid_path.knowledge_roots = vec![PathBuf::from("relative/docs")];
        assert!(matches!(
            store.create(invalid_path),
            Err(WorkspacePresetError::Invalid(_))
        ));

        let mut exported = serde_json::json!({
            "schema_version": 1,
            "revision": 0,
            "presets": [{
                "id": "preset-1",
                "name": "Bad permission",
                "project_id": null,
                "preferred_model": null,
                "permission": "write-everything",
                "optimization_profile": "turbo",
                "knowledge_roots": [],
                "hot_model_ids": [],
                "enabled_mcp_servers": [],
                "instruction_text": "",
                "created_at": "2026-08-29T00:00:00Z",
                "updated_at": "2026-08-29T00:00:00Z",
                "revision": 1
            }]
        });
        let error = store
            .import_json(&exported.to_string(), ImportMode::Replace, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant"));
        exported["presets"][0]["permission"] = "ask".into();
        let error = store
            .import_json(&exported.to_string(), ImportMode::Replace, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant"));
        assert!(WorkspacePermission::parse("write-everything").is_none());
        assert!(WorkspaceOptimizationProfile::parse("turbo").is_none());
        cleanup(&path);
    }

    #[test]
    fn merge_import_adds_non_conflicting_presets() {
        let source_path = test_path("merge-source");
        let source = WorkspacePresetStore::open(&source_path).unwrap();
        source.create(input("Imported")).unwrap();
        let exported = source.export_json().unwrap();

        let target_path = test_path("merge-target");
        let target = WorkspacePresetStore::open(&target_path).unwrap();
        target.create(input("Existing")).unwrap();
        target
            .import_json(&exported, ImportMode::Merge, target.revision())
            .unwrap();
        let names: Vec<_> = target
            .list()
            .into_iter()
            .map(|preset| preset.name)
            .collect();
        assert_eq!(names, vec!["Existing", "Imported"]);
        cleanup(&source_path);
        cleanup(&target_path);
    }

    #[test]
    fn migrates_legacy_files_and_preserves_a_backup() {
        let path = test_path("migration");
        std::fs::write(
            &path,
            serde_json::json!({
                "workspace_presets": [{
                    "id": "legacy-1",
                    "name": "Legacy",
                    "workspace_id": "project-1",
                    "model": "org/model:q4",
                    "permission": "full_access",
                    "optimization": "memory",
                    "knowledge_roots": ["/tmp/project"],
                    "hot_models": ["model-1"],
                    "mcp_servers": ["source"],
                    "instructions": "Legacy instructions"
                }]
            })
            .to_string(),
        )
        .unwrap();

        let store = WorkspacePresetStore::open(&path).unwrap();
        let preset = &store.list()[0];
        assert_eq!(preset.project_id.as_deref(), Some("project-1"));
        assert_eq!(preset.permission, WorkspacePermission::FullAccess);
        assert_eq!(
            preset.optimization_profile,
            WorkspaceOptimizationProfile::Memory
        );
        assert!(path.with_extension("json.v0.bak").exists());
        cleanup(&path);
    }

    #[test]
    fn rejects_corrupt_and_future_files_without_replacing_them() {
        let corrupt_path = test_path("corrupt");
        std::fs::write(&corrupt_path, "{not-json").unwrap();
        assert!(matches!(
            WorkspacePresetStore::open(&corrupt_path),
            Err(WorkspacePresetError::Storage(_))
        ));
        assert_eq!(std::fs::read_to_string(&corrupt_path).unwrap(), "{not-json");

        let future_path = test_path("future");
        let future = r#"{"schema_version":2,"revision":0,"presets":[]}"#;
        std::fs::write(&future_path, future).unwrap();
        assert!(WorkspacePresetStore::open(&future_path).is_err());
        assert_eq!(std::fs::read_to_string(&future_path).unwrap(), future);
        cleanup(&corrupt_path);
        cleanup(&future_path);
    }

    #[test]
    fn export_is_deterministic_and_replace_import_round_trips() {
        let source_path = test_path("export-source");
        let source = WorkspacePresetStore::open(&source_path).unwrap();
        source.create(input("Zulu")).unwrap();
        source.create(input("Alpha")).unwrap();

        let first = source.export_json().unwrap();
        let second = source.export_json().unwrap();
        assert_eq!(first, second);
        assert!(first.find("Alpha").unwrap() < first.find("Zulu").unwrap());

        let target_path = test_path("export-target");
        let target = WorkspacePresetStore::open(&target_path).unwrap();
        target
            .import_json(&first, ImportMode::Replace, target.revision())
            .unwrap();
        let target_names: Vec<_> = target
            .list()
            .into_iter()
            .map(|preset| preset.name)
            .collect();
        assert_eq!(target_names, vec!["Alpha", "Zulu"]);
        cleanup(&source_path);
        cleanup(&target_path);
    }
}
