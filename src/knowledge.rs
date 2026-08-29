#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage;

const KNOWLEDGE_SCHEMA_VERSION: u32 = 1;

pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    fn fingerprint(&self) -> String {
        std::any::type_name::<Self>().to_string()
    }
}

#[derive(Clone)]
pub struct DeterministicEmbedder {
    dimensions: usize,
}

impl DeterministicEmbedder {
    pub fn new(dimensions: usize) -> anyhow::Result<Self> {
        if dimensions == 0 {
            anyhow::bail!("embedding dimensions must be greater than zero");
        }
        Ok(Self { dimensions })
    }
}

impl Default for DeterministicEmbedder {
    fn default() -> Self {
        Self { dimensions: 128 }
    }
}

impl Embedder for DeterministicEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut vector = vec![0.0; self.dimensions];
        for token in tokenize(text) {
            let digest = Sha256::digest(token.as_bytes());
            let index =
                u64::from_le_bytes(digest[..8].try_into().unwrap()) as usize % self.dimensions;
            let sign = if digest[8] & 1 == 0 { 1.0 } else { -1.0 };
            vector[index] += sign;
        }
        normalize(&mut vector);
        Ok(vector)
    }

    fn fingerprint(&self) -> String {
        format!("deterministic-sha256-v1:{}", self.dimensions)
    }
}

#[derive(Clone, Debug)]
pub struct IndexLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub chunk_lines: usize,
    pub chunk_overlap_lines: usize,
    pub allowed_extensions: BTreeSet<String>,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            chunk_lines: 80,
            chunk_overlap_lines: 12,
            allowed_extensions: [
                "c", "cc", "cpp", "css", "go", "h", "hpp", "html", "java", "js", "json", "jsx",
                "kt", "md", "mdx", "php", "py", "rb", "rs", "sh", "sql", "swift", "toml", "ts",
                "tsx", "txt", "xml", "yaml", "yml",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

impl IndexLimits {
    fn validate(&self) -> anyhow::Result<()> {
        if self.max_files == 0 {
            anyhow::bail!("max_files must be greater than zero");
        }
        if self.max_file_bytes == 0 || self.max_total_bytes == 0 {
            anyhow::bail!("file size limits must be greater than zero");
        }
        if self.chunk_lines == 0 {
            anyhow::bail!("chunk_lines must be greater than zero");
        }
        if self.chunk_overlap_lines >= self.chunk_lines {
            anyhow::bail!("chunk overlap must be smaller than chunk size");
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct IndexCancellation {
    cancelled: Arc<AtomicBool>,
}

impl IndexCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStage {
    Discovering,
    Indexing,
    Committing,
    Complete,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexProgress {
    pub stage: IndexStage,
    pub discovered: usize,
    pub indexed: usize,
    pub reused: usize,
    pub skipped: usize,
    pub removed: usize,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    pub discovered: usize,
    pub indexed: usize,
    pub reused: usize,
    pub skipped: usize,
    pub removed: usize,
    pub chunks: usize,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceProvenance {
    pub root: PathBuf,
    pub path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub score: f32,
    pub vector_score: Option<f32>,
    pub lexical_score: f32,
    pub content: String,
    pub source: SourceProvenance,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IndexSnapshot {
    pub files: usize,
    pub chunks: usize,
    pub content_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct IndexMetadata {
    #[serde(default)]
    embedding_fingerprint: String,
    #[serde(default)]
    files: BTreeMap<String, IndexedFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IndexedFile {
    root: PathBuf,
    relative_path: PathBuf,
    content_hash: String,
    size: u64,
    modified_nanos: u64,
    chunks: Vec<ChunkRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChunkRecord {
    id: String,
    start_line: usize,
    end_line: usize,
    content: String,
    content_hash: String,
    embedding: Vec<f32>,
}

pub struct KnowledgeIndex<E> {
    metadata_path: PathBuf,
    metadata: RwLock<IndexMetadata>,
    embedder: E,
    limits: IndexLimits,
    index_lock: Mutex<()>,
    corrupt_backup: Option<PathBuf>,
}

impl<E: Embedder> KnowledgeIndex<E> {
    pub fn open(
        metadata_path: impl Into<PathBuf>,
        embedder: E,
        limits: IndexLimits,
    ) -> anyhow::Result<Self> {
        limits.validate()?;
        let metadata_path = metadata_path.into();
        let (metadata, corrupt_backup) = load_metadata(&metadata_path)?;
        Ok(Self {
            metadata_path,
            metadata: RwLock::new(metadata),
            embedder,
            limits,
            index_lock: Mutex::new(()),
            corrupt_backup,
        })
    }

    pub fn recovered_corrupt_metadata(&self) -> Option<&Path> {
        self.corrupt_backup.as_deref()
    }

    pub fn snapshot(&self) -> IndexSnapshot {
        let metadata = self.metadata.read();
        IndexSnapshot {
            files: metadata.files.len(),
            chunks: metadata.files.values().map(|file| file.chunks.len()).sum(),
            content_bytes: metadata.files.values().map(|file| file.size).sum(),
        }
    }

    pub fn index_roots<F>(
        &self,
        roots: &[PathBuf],
        cancellation: &IndexCancellation,
        mut progress: F,
    ) -> anyhow::Result<IndexReport>
    where
        F: FnMut(&IndexProgress),
    {
        let _guard = self.index_lock.lock();
        let roots = canonical_roots(roots)?;
        let mut report = IndexReport::default();
        progress(&progress_record(IndexStage::Discovering, &report, None));

        let mut candidates = Vec::new();
        for root in &roots {
            discover_root(root, cancellation, &mut candidates)?;
            if cancellation.is_cancelled() {
                return Ok(cancelled_report(report, &mut progress));
            }
        }
        candidates.sort_by(|a, b| (&a.root, &a.relative_path).cmp(&(&b.root, &b.relative_path)));
        candidates.dedup_by(|a, b| a.absolute_path == b.absolute_path);
        report.discovered = candidates.len();

        let previous = self.metadata.read().clone();
        let embedding_fingerprint = self.embedder.fingerprint();
        let embeddings_match = previous.embedding_fingerprint == embedding_fingerprint;
        let mut next = IndexMetadata {
            embedding_fingerprint,
            ..IndexMetadata::default()
        };
        let mut admitted_bytes = 0_u64;
        for candidate in candidates {
            if cancellation.is_cancelled() {
                return Ok(cancelled_report(report, &mut progress));
            }
            progress(&progress_record(
                IndexStage::Indexing,
                &report,
                Some(candidate.relative_path.clone()),
            ));

            if self.is_metadata_file(&candidate.absolute_path)
                || next.files.len() >= self.limits.max_files
                || candidate.size > self.limits.max_file_bytes
                || admitted_bytes.saturating_add(candidate.size) > self.limits.max_total_bytes
                || !self.is_allowed_type(&candidate.relative_path)
            {
                report.skipped += 1;
                continue;
            }

            let key = source_key(&candidate.root, &candidate.relative_path);
            if let Some(existing) = previous.files.get(&key) {
                if embeddings_match
                    && existing.size == candidate.size
                    && existing.modified_nanos == candidate.modified_nanos
                {
                    admitted_bytes += candidate.size;
                    next.files.insert(key, existing.clone());
                    report.reused += 1;
                    continue;
                }
            }

            let bytes = match read_candidate(&candidate, self.limits.max_file_bytes) {
                Ok(Some(bytes)) => bytes,
                Ok(None) | Err(_) => {
                    report.skipped += 1;
                    continue;
                }
            };
            let actual_size = bytes.len() as u64;
            if admitted_bytes.saturating_add(actual_size) > self.limits.max_total_bytes {
                report.skipped += 1;
                continue;
            }
            if bytes.contains(&0) {
                report.skipped += 1;
                continue;
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    report.skipped += 1;
                    continue;
                }
            };
            let content_hash = sha256_hex(text.as_bytes());
            let chunks = if let Some(existing) = previous.files.get(&key) {
                if embeddings_match && existing.content_hash == content_hash {
                    existing.chunks.clone()
                } else {
                    self.chunk_file(&key, &text, cancellation)?
                }
            } else {
                self.chunk_file(&key, &text, cancellation)?
            };
            if cancellation.is_cancelled() {
                return Ok(cancelled_report(report, &mut progress));
            }

            admitted_bytes += actual_size;
            next.files.insert(
                key,
                IndexedFile {
                    root: candidate.root,
                    relative_path: candidate.relative_path,
                    content_hash,
                    size: actual_size,
                    modified_nanos: candidate.modified_nanos,
                    chunks,
                },
            );
            report.indexed += 1;
        }

        report.removed = previous
            .files
            .keys()
            .filter(|key| !next.files.contains_key(*key))
            .count();
        report.chunks = next.files.values().map(|file| file.chunks.len()).sum();
        progress(&progress_record(IndexStage::Committing, &report, None));
        storage::save_versioned(&self.metadata_path, KNOWLEDGE_SCHEMA_VERSION, &next)?;
        *self.metadata.write() = next;
        progress(&progress_record(IndexStage::Complete, &report, None));
        Ok(report)
    }

    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let query_tokens = token_counts(query);
        let metadata = self.metadata.read();
        let query_embedding = (metadata.embedding_fingerprint == self.embedder.fingerprint())
            .then(|| self.embedder.embed(query).ok())
            .flatten()
            .filter(|embedding| {
                !embedding.is_empty() && embedding.iter().all(|value| value.is_finite())
            });
        let mut hits = Vec::new();

        for file in metadata.files.values() {
            for chunk in &file.chunks {
                let lexical_score = lexical_score(&query_tokens, &chunk.content);
                let vector_score = query_embedding.as_ref().and_then(|query| {
                    if query.len() == chunk.embedding.len() && !chunk.embedding.is_empty() {
                        Some(cosine_similarity(query, &chunk.embedding).max(0.0))
                    } else {
                        None
                    }
                });
                let score = match vector_score {
                    Some(vector) => vector * 0.75 + lexical_score * 0.25,
                    None => lexical_score,
                };
                if score <= 0.0 {
                    continue;
                }
                hits.push(SearchHit {
                    score,
                    vector_score,
                    lexical_score,
                    content: chunk.content.clone(),
                    source: SourceProvenance {
                        root: file.root.clone(),
                        path: file.relative_path.clone(),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                    },
                });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.source.path.cmp(&b.source.path))
                .then_with(|| a.source.start_line.cmp(&b.source.start_line))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn is_allowed_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|extension| self.limits.allowed_extensions.contains(&extension))
    }

    fn is_metadata_file(&self, path: &Path) -> bool {
        fs::canonicalize(&self.metadata_path)
            .map(|metadata| metadata == path)
            .unwrap_or(false)
    }

    fn chunk_file(
        &self,
        source: &str,
        text: &str,
        cancellation: &IndexCancellation,
    ) -> anyhow::Result<Vec<ChunkRecord>> {
        let lines: Vec<_> = text.lines().collect();
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let mut chunks = Vec::new();
        let step = self.limits.chunk_lines - self.limits.chunk_overlap_lines;
        let mut start = 0;
        while start < lines.len() {
            if cancellation.is_cancelled() {
                break;
            }
            let end = (start + self.limits.chunk_lines).min(lines.len());
            let content = lines[start..end].join("\n");
            let content_hash = sha256_hex(content.as_bytes());
            let embedding = self
                .embedder
                .embed(&content)
                .ok()
                .filter(|values| values.iter().all(|value| value.is_finite()))
                .unwrap_or_default();
            let id = sha256_hex(format!("{source}:{}:{content_hash}", start + 1).as_bytes());
            chunks.push(ChunkRecord {
                id,
                start_line: start + 1,
                end_line: end,
                content,
                content_hash,
                embedding,
            });
            if end == lines.len() {
                break;
            }
            start += step;
        }
        Ok(chunks)
    }
}

#[derive(Debug)]
struct Candidate {
    root: PathBuf,
    relative_path: PathBuf,
    absolute_path: PathBuf,
    size: u64,
    modified_nanos: u64,
}

fn read_candidate(candidate: &Candidate, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
    if has_symlink_component(&candidate.root, &candidate.relative_path)? {
        return Ok(None);
    }
    let current_path = match fs::canonicalize(candidate.root.join(&candidate.relative_path)) {
        Ok(path) if path == candidate.absolute_path && path.starts_with(&candidate.root) => path,
        _ => return Ok(None),
    };
    let mut file = fs::File::open(current_path)?;
    let initial_capacity = candidate.size.min(max_bytes).min(64 * 1024) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn load_metadata(path: &Path) -> anyhow::Result<(IndexMetadata, Option<PathBuf>)> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((IndexMetadata::default(), None));
        }
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => {
            let backup = preserve_corrupt_metadata(path)?;
            return Ok((IndexMetadata::default(), Some(backup)));
        }
    };
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version > u64::from(KNOWLEDGE_SCHEMA_VERSION) {
        anyhow::bail!(
            "{} uses knowledge schema version {}, but this build supports up to {}",
            path.display(),
            version,
            KNOWLEDGE_SCHEMA_VERSION
        );
    }
    match storage::load_versioned(path, KNOWLEDGE_SCHEMA_VERSION, |_, value| Ok(value)) {
        Ok(Some(metadata)) => Ok((metadata, None)),
        Ok(None) => Ok((IndexMetadata::default(), None)),
        Err(_) => {
            let backup = preserve_corrupt_metadata(path)?;
            Ok((IndexMetadata::default(), Some(backup)))
        }
    }
}

fn preserve_corrupt_metadata(path: &Path) -> anyhow::Result<PathBuf> {
    let mut backup = path.with_extension("json.corrupt");
    if backup.exists() {
        backup = path.with_extension(format!("json.corrupt.{}", uuid::Uuid::new_v4()));
    }
    fs::rename(path, &backup).with_context(|| {
        format!(
            "failed to preserve corrupt knowledge metadata at {}",
            backup.display()
        )
    })?;
    Ok(backup)
}

fn canonical_roots(roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut canonical = Vec::new();
    for root in roots {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve knowledge root {}", root.display()))?;
        if !root.is_dir() {
            anyhow::bail!("knowledge root is not a directory: {}", root.display());
        }
        canonical.push(root);
    }
    canonical.sort();
    canonical.dedup();
    let mut minimal: Vec<PathBuf> = Vec::new();
    for root in canonical {
        if !minimal.iter().any(|parent| root.starts_with(parent)) {
            minimal.push(root);
        }
    }
    Ok(minimal)
}

fn discover_root(
    root: &Path,
    cancellation: &IndexCancellation,
    candidates: &mut Vec<Candidate>,
) -> anyhow::Result<()> {
    if discover_with_git(root, cancellation, candidates)? {
        return Ok(());
    }
    discover_directory(root, root, cancellation, candidates, &[])
}

fn discover_with_git(
    root: &Path,
    cancellation: &IndexCancellation,
    candidates: &mut Vec<Candidate>,
) -> anyhow::Result<bool> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Ok(false),
    };
    for raw_path in output.stdout.split(|byte| *byte == 0) {
        if cancellation.is_cancelled() {
            break;
        }
        if raw_path.is_empty() {
            continue;
        }
        let Ok(relative) = std::str::from_utf8(raw_path) else {
            continue;
        };
        add_candidate(root, Path::new(relative), candidates)?;
    }
    Ok(true)
}

fn discover_directory(
    root: &Path,
    directory: &Path,
    cancellation: &IndexCancellation,
    candidates: &mut Vec<Candidate>,
    inherited_rules: &[IgnoreRule],
) -> anyhow::Result<()> {
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let mut rules = inherited_rules.to_vec();
    rules.extend(load_ignore_rules(root, directory)?);
    let mut entries = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if cancellation.is_cancelled() {
            break;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if is_ignored(relative, file_type.is_dir(), &rules) {
            continue;
        }
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            discover_directory(root, &path, cancellation, candidates, &rules)?;
        } else if file_type.is_file() {
            add_candidate(root, relative, candidates)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    base: PathBuf,
    pattern: String,
    negated: bool,
    directory_only: bool,
    anchored: bool,
}

fn load_ignore_rules(root: &Path, directory: &Path) -> anyhow::Result<Vec<IgnoreRule>> {
    let ignore_path = directory.join(".gitignore");
    let contents = match fs::read_to_string(ignore_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let base = directory
        .strip_prefix(root)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let mut rules = Vec::new();
    for raw_line in contents.lines() {
        let mut line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let negated = line.starts_with('!');
        if negated {
            line = &line[1..];
        }
        let directory_only = line.ends_with('/');
        line = line.trim_end_matches('/');
        let anchored = line.starts_with('/');
        line = line.trim_start_matches('/');
        if line.is_empty() {
            continue;
        }
        rules.push(IgnoreRule {
            base: base.clone(),
            pattern: line.to_string(),
            negated,
            directory_only,
            anchored,
        });
    }
    Ok(rules)
}

fn is_ignored(relative: &Path, is_directory: bool, rules: &[IgnoreRule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        let Ok(local) = relative.strip_prefix(&rule.base) else {
            continue;
        };
        if rule.directory_only && !is_directory {
            continue;
        }
        let local = slash_path(local);
        let matched = if rule.anchored || rule.pattern.contains('/') {
            glob_matches(&rule.pattern, &local)
        } else {
            local
                .split('/')
                .any(|component| glob_matches(&rule.pattern, component))
        };
        if matched {
            ignored = !rule.negated;
        }
    }
    ignored
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    fn matches_from(
        pattern: &[u8],
        value: &[u8],
        pattern_index: usize,
        value_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            value_index == value.len()
        } else if pattern[pattern_index] == b'*' {
            let double = pattern.get(pattern_index + 1) == Some(&b'*');
            let next_pattern = pattern_index + if double { 2 } else { 1 };
            matches_from(pattern, value, next_pattern, value_index, memo)
                || (value_index < value.len()
                    && (double || value[value_index] != b'/')
                    && matches_from(pattern, value, pattern_index, value_index + 1, memo))
        } else if value_index < value.len()
            && (pattern[pattern_index] == value[value_index]
                || (pattern[pattern_index] == b'?' && value[value_index] != b'/'))
        {
            matches_from(pattern, value, pattern_index + 1, value_index + 1, memo)
        } else {
            false
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    matches_from(
        pattern.as_bytes(),
        value.as_bytes(),
        0,
        0,
        &mut HashMap::new(),
    )
}

fn add_candidate(
    root: &Path,
    relative_path: &Path,
    candidates: &mut Vec<Candidate>,
) -> anyhow::Result<()> {
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Ok(());
    }
    let absolute_path = root.join(relative_path);
    if has_symlink_component(root, relative_path)? {
        return Ok(());
    }
    let canonical = match fs::canonicalize(&absolute_path) {
        Ok(canonical) if canonical.starts_with(root) => canonical,
        _ => return Ok(()),
    };
    let metadata = match fs::metadata(&canonical) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(()),
    };
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    candidates.push(Candidate {
        root: root.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        absolute_path: canonical,
        size: metadata.len(),
        modified_nanos,
    });
    Ok(())
}

fn has_symlink_component(root: &Path, relative: &Path) -> anyhow::Result<bool> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn cancelled_report<F>(mut report: IndexReport, progress: &mut F) -> IndexReport
where
    F: FnMut(&IndexProgress),
{
    report.cancelled = true;
    progress(&progress_record(IndexStage::Cancelled, &report, None));
    report
}

fn progress_record(
    stage: IndexStage,
    report: &IndexReport,
    current_path: Option<PathBuf>,
) -> IndexProgress {
    IndexProgress {
        stage,
        discovered: report.discovered,
        indexed: report.indexed,
        reused: report.reused,
        skipped: report.skipped,
        removed: report.removed,
        current_path,
    }
}

fn source_key(root: &Path, relative: &Path) -> String {
    sha256_hex(format!("{}\0{}", root.display(), relative.display()).as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn token_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in tokenize(text) {
        *counts.entry(token).or_default() += 1;
    }
    counts
}

fn lexical_score(query: &HashMap<String, usize>, content: &str) -> f32 {
    if query.is_empty() {
        return 0.0;
    }
    let document = token_counts(content);
    let matched = query
        .iter()
        .map(|(token, query_count)| {
            document
                .get(token)
                .map(|count| (*count).min(*query_count))
                .unwrap_or_default()
        })
        .sum::<usize>();
    matched as f32 / query.values().sum::<usize>() as f32
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_directory(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("kiralm-knowledge-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_index<E: Embedder>(root: &Path, embedder: E) -> KnowledgeIndex<E> {
        KnowledgeIndex::open(
            root.join("metadata/index.json"),
            embedder,
            IndexLimits {
                chunk_lines: 2,
                chunk_overlap_lines: 0,
                ..IndexLimits::default()
            },
        )
        .unwrap()
    }

    #[derive(Clone)]
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
    }

    impl Embedder for CountingEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            DeterministicEmbedder::default().embed(text)
        }
    }

    struct FailingEmbedder;

    impl Embedder for FailingEmbedder {
        fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("embedding unavailable")
        }
    }

    struct KeywordEmbedder;

    impl Embedder for KeywordEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            if text.contains("apple") || text.contains("fruit") {
                Ok(vec![1.0, 0.0])
            } else {
                Ok(vec![0.0, 1.0])
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = test_directory("symlink");
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("safe.txt"), "safe local content").unwrap();
        fs::write(base.join("outside.txt"), "outside secret").unwrap();
        symlink(base.join("outside.txt"), root.join("escape.txt")).unwrap();
        let index = test_index(&base, FailingEmbedder);

        let report = index
            .index_roots(&[root], &IndexCancellation::default(), |_| {})
            .unwrap();

        assert_eq!(report.indexed, 1);
        assert!(index.search("secret", 5).unwrap().is_empty());
        assert_eq!(
            index.search("safe", 5).unwrap()[0].source.path,
            PathBuf::from("safe.txt")
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn incremental_no_op_reuses_embeddings() {
        let root = test_directory("noop");
        fs::write(root.join("notes.md"), "one\ntwo\nthree").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let index = test_index(
            &root,
            CountingEmbedder {
                calls: calls.clone(),
            },
        );

        let first = index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();
        let first_calls = calls.load(Ordering::Relaxed);
        let second = index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(first.indexed, 1);
        assert_eq!(first_calls, 2);
        assert_eq!(second.reused, 1);
        assert_eq!(calls.load(Ordering::Relaxed), first_calls);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_and_delete_reconcile_records() {
        let root = test_directory("reconcile");
        fs::write(root.join("keep.txt"), "old token").unwrap();
        fs::write(root.join("delete.txt"), "delete marker").unwrap();
        let index = test_index(&root, FailingEmbedder);
        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        fs::write(root.join("keep.txt"), "new replacement token").unwrap();
        fs::remove_file(root.join("delete.txt")).unwrap();
        let report = index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(report.indexed, 1);
        assert_eq!(report.removed, 1);
        assert!(index.search("old", 5).unwrap().is_empty());
        assert!(index.search("delete", 5).unwrap().is_empty());
        assert_eq!(index.search("replacement", 5).unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lexical_fallback_ranks_matching_content_first() {
        let root = test_directory("ranking");
        fs::write(root.join("fruit.txt"), "apple apple orchard").unwrap();
        fs::write(root.join("database.txt"), "database schema migration").unwrap();
        let index = test_index(&root, FailingEmbedder);
        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        let hits = index.search("apple", 5).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source.path, PathBuf::from("fruit.txt"));
        assert_eq!(hits[0].vector_score, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vector_search_ranks_a_semantic_match() {
        let root = test_directory("vector-ranking");
        fs::write(root.join("orchard.txt"), "apple orchard").unwrap();
        fs::write(root.join("storage.txt"), "database schema").unwrap();
        let index = test_index(&root, KeywordEmbedder);
        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        let hits = index.search("fruit", 5).unwrap();

        assert_eq!(hits[0].source.path, PathBuf::from("orchard.txt"));
        assert_eq!(hits[0].vector_score, Some(1.0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_returns_chunk_line_provenance() {
        let root = test_directory("provenance");
        fs::write(root.join("lines.txt"), "first\nsecond\ntarget line").unwrap();
        let index = test_index(&root, FailingEmbedder);
        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        let hit = index.search("target", 1).unwrap().remove(0);

        assert_eq!(hit.source.start_line, 3);
        assert_eq!(hit.source.end_line, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_metadata_is_preserved_and_recovered() {
        let root = test_directory("corrupt");
        let metadata = root.join("index.json");
        fs::write(&metadata, "{broken").unwrap();

        let index = KnowledgeIndex::open(
            &metadata,
            DeterministicEmbedder::default(),
            IndexLimits::default(),
        )
        .unwrap();

        let backup = index.recovered_corrupt_metadata().unwrap();
        assert!(backup.exists());
        assert_eq!(fs::read_to_string(backup).unwrap(), "{broken");
        assert_eq!(index.snapshot(), IndexSnapshot::default());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_discovery_honors_ignore_rules() {
        let root = test_directory("gitignore");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "hidden marker").unwrap();
        fs::write(root.join("visible.txt"), "visible marker").unwrap();
        let index = test_index(&root, FailingEmbedder);

        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(index.search("hidden", 5).unwrap().is_empty());
        assert_eq!(index.search("visible", 5).unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_git_discovery_honors_ignore_rules() {
        let root = test_directory("plain-ignore");
        fs::create_dir_all(root.join("generated")).unwrap();
        fs::write(
            root.join(".gitignore"),
            "ignored*.txt\ngenerated/\n!ignored-keep.txt\n",
        )
        .unwrap();
        fs::write(root.join("ignored-one.txt"), "hidden one").unwrap();
        fs::write(root.join("ignored-keep.txt"), "visible exception").unwrap();
        fs::write(root.join("generated/output.txt"), "hidden output").unwrap();
        let index = test_index(&root, FailingEmbedder);

        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(index.search("hidden", 5).unwrap().is_empty());
        assert_eq!(index.search("exception", 5).unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_keeps_the_previous_snapshot() {
        let root = test_directory("cancel");
        fs::write(root.join("first.txt"), "first version").unwrap();
        let index = test_index(&root, FailingEmbedder);
        index
            .index_roots(
                std::slice::from_ref(&root),
                &IndexCancellation::default(),
                |_| {},
            )
            .unwrap();
        let before = index.snapshot();
        fs::write(root.join("second.txt"), "second version").unwrap();
        let cancellation = IndexCancellation::default();
        cancellation.cancel();

        let report = index
            .index_roots(std::slice::from_ref(&root), &cancellation, |_| {})
            .unwrap();

        assert!(report.cancelled);
        assert_eq!(index.snapshot(), before);
        fs::remove_dir_all(root).unwrap();
    }
}
