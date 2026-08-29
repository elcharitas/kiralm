#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ATTACHMENT_SCHEMA_VERSION: u32 = 1;
const READ_BUFFER_BYTES: usize = 64 * 1024;

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct AttachmentLimits {
    pub max_attachment_bytes: u64,
    pub max_attachments_per_reference: usize,
    pub quota_bytes: u64,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_attachment_bytes: 25 * 1024 * 1024,
            max_attachments_per_reference: 8,
            quota_bytes: 512 * 1024 * 1024,
        }
    }
}

impl AttachmentLimits {
    fn validate(&self) -> anyhow::Result<()> {
        if self.max_attachment_bytes == 0 {
            anyhow::bail!("attachment size limit must be greater than zero");
        }
        if self.max_attachments_per_reference == 0 {
            anyhow::bail!("attachment count limit must be greater than zero");
        }
        if self.quota_bytes == 0 {
            anyhow::bail!("attachment quota must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Document,
    Text,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AttachmentDescriptor {
    pub id: String,
    pub sha256: String,
    pub media_type: String,
    pub kind: AttachmentKind,
    pub size_bytes: u64,
    pub filename: String,
}

#[derive(Clone, Debug)]
pub enum AttachmentSource {
    Bytes(Vec<u8>),
    DataUrl(String),
    LocalPath(PathBuf),
}

#[derive(Clone, Debug)]
pub struct AttachmentInput {
    pub source: AttachmentSource,
    pub filename: Option<String>,
    pub declared_media_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachmentPruneReport {
    pub removed: Vec<String>,
    pub bytes_removed: u64,
    pub bytes_remaining: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AttachmentMetadata {
    #[serde(default = "attachment_schema_version")]
    schema_version: u32,
    next_access_order: u64,
    attachments: BTreeMap<String, AttachmentRecord>,
}

impl Default for AttachmentMetadata {
    fn default() -> Self {
        Self {
            schema_version: ATTACHMENT_SCHEMA_VERSION,
            next_access_order: 1,
            attachments: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AttachmentRecord {
    sha256: String,
    media_type: String,
    size_bytes: u64,
    created_at: DateTime<Utc>,
    last_accessed_at: DateTime<Utc>,
    access_order: u64,
    filenames: BTreeSet<String>,
    references: BTreeSet<String>,
}

pub struct AttachmentStore {
    root: PathBuf,
    limits: AttachmentLimits,
    metadata: Mutex<AttachmentMetadata>,
    active_partials: Mutex<HashSet<PathBuf>>,
    recovered_corrupt_metadata: Option<PathBuf>,
    clock: Clock,
}

impl AttachmentStore {
    pub fn from_home(home: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::open(
            home.as_ref().join("attachments"),
            AttachmentLimits::default(),
        )
    }

    pub fn open(root: impl Into<PathBuf>, limits: AttachmentLimits) -> anyhow::Result<Self> {
        Self::open_with_clock(root.into(), limits, Arc::new(Utc::now))
    }

    fn open_with_clock(
        root: PathBuf,
        limits: AttachmentLimits,
        clock: Clock,
    ) -> anyhow::Result<Self> {
        limits.validate()?;
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(root)?;
        ensure_store_directory(&root.join("blobs"))?;
        ensure_store_directory(&root.join(".partial"))?;
        let (mut metadata, recovered_corrupt_metadata) =
            load_metadata(&root, &clock, limits.max_attachment_bytes)?;
        let removed = plan_prune(&mut metadata, limits.quota_bytes, &HashSet::new())?;
        if !removed.is_empty() {
            save_metadata(&root, &metadata)?;
            remove_evicted_files(&root, &removed)?;
        }
        Ok(Self {
            root,
            limits,
            metadata: Mutex::new(metadata),
            active_partials: Mutex::new(HashSet::new()),
            recovered_corrupt_metadata,
            clock,
        })
    }

    pub fn recovered_corrupt_metadata(&self) -> Option<&Path> {
        self.recovered_corrupt_metadata.as_deref()
    }

    pub fn ingest_input(
        &self,
        input: AttachmentInput,
        reference: &str,
        allowed_roots: &[PathBuf],
    ) -> anyhow::Result<AttachmentDescriptor> {
        match input.source {
            AttachmentSource::Bytes(bytes) => self.ingest_reader(
                bytes.as_slice(),
                input.filename.as_deref(),
                input.declared_media_type.as_deref(),
                reference,
            ),
            AttachmentSource::DataUrl(data_url) => {
                if input.declared_media_type.is_some() {
                    anyhow::bail!("data URLs carry their own declared media type");
                }
                self.ingest_data_url(&data_url, input.filename.as_deref(), reference)
            }
            AttachmentSource::LocalPath(path) => self.ingest_local_path(
                &path,
                allowed_roots,
                input.filename.as_deref(),
                input.declared_media_type.as_deref(),
                reference,
            ),
        }
    }

    pub fn ingest_many(
        &self,
        inputs: Vec<AttachmentInput>,
        reference: &str,
        allowed_roots: &[PathBuf],
    ) -> anyhow::Result<Vec<AttachmentDescriptor>> {
        if inputs.len() > self.limits.max_attachments_per_reference {
            anyhow::bail!(
                "attachment count exceeds the limit of {}",
                self.limits.max_attachments_per_reference
            );
        }
        validate_reference(reference)?;
        let original = self
            .metadata
            .lock()
            .attachments
            .values()
            .filter(|entry| entry.references.contains(reference))
            .map(|entry| entry.sha256.clone())
            .collect::<HashSet<_>>();
        let mut descriptors = Vec::with_capacity(inputs.len());
        for input in inputs {
            match self.ingest_input(input, reference, allowed_roots) {
                Ok(descriptor) => descriptors.push(descriptor),
                Err(error) => {
                    let added = descriptors
                        .iter()
                        .map(|descriptor| descriptor.sha256.clone())
                        .filter(|sha256| !original.contains(sha256))
                        .collect::<HashSet<_>>();
                    let _ = self.release_hash_references(reference, &added);
                    return Err(error);
                }
            }
        }
        Ok(descriptors)
    }

    pub fn ingest_data_url(
        &self,
        data_url: &str,
        filename: Option<&str>,
        reference: &str,
    ) -> anyhow::Result<AttachmentDescriptor> {
        let (declared_media_type, encoded) = parse_data_url(data_url)?;
        let decoded = decode_base64_bounded(encoded, self.limits.max_attachment_bytes)?;
        self.ingest_reader(
            decoded.as_slice(),
            filename,
            Some(&declared_media_type),
            reference,
        )
    }

    pub fn ingest_local_path(
        &self,
        source: &Path,
        allowed_roots: &[PathBuf],
        filename: Option<&str>,
        declared_media_type: Option<&str>,
        reference: &str,
    ) -> anyhow::Result<AttachmentDescriptor> {
        if allowed_roots.is_empty() {
            anyhow::bail!("local attachment roots must be supplied explicitly");
        }
        let source = std::fs::canonicalize(source)?;
        let mut canonical_roots = Vec::with_capacity(allowed_roots.len());
        for root in allowed_roots {
            let root = std::fs::canonicalize(root)?;
            if !root.is_dir() {
                anyhow::bail!("attachment root is not a directory: {}", root.display());
            }
            canonical_roots.push(root);
        }
        if !canonical_roots.iter().any(|root| source.starts_with(root)) {
            anyhow::bail!("local attachment path escapes the supplied roots");
        }
        let file = open_local_file(&source)?;
        if !file.metadata()?.is_file() {
            anyhow::bail!("local attachment is not a regular file");
        }
        let current = std::fs::canonicalize(&source)?;
        if current != source || !canonical_roots.iter().any(|root| current.starts_with(root)) {
            anyhow::bail!("local attachment path changed outside the supplied roots");
        }
        let source_name = source.file_name().and_then(|value| value.to_str());
        self.ingest_reader(
            file,
            filename.or(source_name),
            declared_media_type,
            reference,
        )
    }

    pub fn ingest_reader<R: Read>(
        &self,
        mut reader: R,
        filename: Option<&str>,
        declared_media_type: Option<&str>,
        reference: &str,
    ) -> anyhow::Result<AttachmentDescriptor> {
        validate_reference(reference)?;
        let partial_path = self
            .root
            .join(".partial")
            .join(format!("{}.partial", uuid::Uuid::new_v4()));
        self.active_partials.lock().insert(partial_path.clone());
        let mut partial = PartialGuard::new(self, partial_path.clone());
        let mut output = create_private_file(&partial_path)?;
        let mut hasher = Sha256::new();
        let mut size_bytes = 0u64;
        let mut buffer = vec![0u8; READ_BUFFER_BYTES];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                break;
            }
            size_bytes = size_bytes
                .checked_add(read as u64)
                .ok_or_else(|| anyhow::anyhow!("attachment size overflow"))?;
            if size_bytes > self.limits.max_attachment_bytes {
                anyhow::bail!(
                    "attachment exceeds the {} byte limit",
                    self.limits.max_attachment_bytes
                );
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        if size_bytes == 0 {
            anyhow::bail!("empty attachments are not supported");
        }
        output.sync_all()?;
        drop(output);

        let media_type = sniff_media_type(&partial_path, self.limits.max_attachment_bytes)?;
        if let Some(declared) = declared_media_type {
            let declared = normalize_media_type(declared)?;
            if declared != media_type {
                anyhow::bail!(
                    "declared media type {declared} does not match detected {media_type}"
                );
            }
        }
        let sha256 = hex::encode(hasher.finalize());
        let safe_filename = safe_filename(filename, &sha256, media_type);
        let final_path = blob_path(&self.root, &sha256);

        let mut metadata = self.metadata.lock();
        let mut next = metadata.clone();
        let existing_for_reference = next
            .attachments
            .values()
            .filter(|entry| entry.references.contains(reference))
            .count();
        let already_referenced = next
            .attachments
            .get(&sha256)
            .is_some_and(|entry| entry.references.contains(reference));
        if !already_referenced
            && existing_for_reference >= self.limits.max_attachments_per_reference
        {
            anyhow::bail!(
                "attachment count exceeds the limit of {}",
                self.limits.max_attachments_per_reference
            );
        }

        let now = (self.clock)();
        let access_order = take_access_order(&mut next);
        let entry = next
            .attachments
            .entry(sha256.clone())
            .or_insert_with(|| AttachmentRecord {
                sha256: sha256.clone(),
                media_type: media_type.to_string(),
                size_bytes,
                created_at: now,
                last_accessed_at: now,
                access_order,
                filenames: BTreeSet::new(),
                references: BTreeSet::new(),
            });
        if entry.media_type != media_type || entry.size_bytes != size_bytes {
            anyhow::bail!("attachment metadata conflicts with its content hash");
        }
        entry.last_accessed_at = now;
        entry.access_order = access_order;
        entry.filenames.insert(safe_filename.clone());
        entry.references.insert(reference.to_string());

        let evicted = plan_prune(&mut next, self.limits.quota_bytes, &HashSet::new())?;
        let final_existed = blob_exists_regular(&final_path)?;
        if final_existed {
            if sha256_file(&final_path, self.limits.max_attachment_bytes)? != sha256 {
                anyhow::bail!("stored attachment content failed integrity verification");
            }
        } else {
            let parent = final_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("attachment blob has no parent directory"))?;
            ensure_store_directory(parent)?;
            std::fs::rename(&partial_path, &final_path)?;
            partial.keep();
        }

        if let Err(error) = save_metadata(&self.root, &next) {
            if !final_existed {
                let _ = std::fs::remove_file(&final_path);
            }
            return Err(error);
        }
        *metadata = next;
        if final_existed {
            partial.remove_now()?;
        }
        remove_evicted_files(&self.root, &evicted)?;

        Ok(AttachmentDescriptor {
            id: sha256.clone(),
            sha256,
            media_type: media_type.to_string(),
            kind: kind_for_media_type(media_type),
            size_bytes,
            filename: safe_filename,
        })
    }

    pub fn descriptor(&self, sha256: &str) -> anyhow::Result<Option<AttachmentDescriptor>> {
        let sha256 = normalize_sha256(sha256)?;
        let mut metadata = self.metadata.lock();
        let mut next = metadata.clone();
        let access_order = take_access_order(&mut next);
        let Some(entry) = next.attachments.get_mut(&sha256) else {
            return Ok(None);
        };
        let path = blob_path(&self.root, &sha256);
        if !blob_exists_regular(&path)? {
            anyhow::bail!("attachment blob is missing");
        }
        if std::fs::metadata(&path)?.len() != entry.size_bytes
            || sha256_file(&path, self.limits.max_attachment_bytes)? != sha256
            || sniff_media_type(&path, self.limits.max_attachment_bytes)? != entry.media_type
        {
            anyhow::bail!("attachment blob failed integrity verification");
        }
        entry.last_accessed_at = (self.clock)();
        entry.access_order = access_order;
        let descriptor = descriptor_from_record(entry);
        save_metadata(&self.root, &next)?;
        *metadata = next;
        Ok(Some(descriptor))
    }

    pub fn read(&self, sha256: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
        let sha256 = normalize_sha256(sha256)?;
        let expected_size = {
            let metadata = self.metadata.lock();
            let entry = metadata
                .attachments
                .get(&sha256)
                .ok_or_else(|| anyhow::anyhow!("attachment is not registered"))?;
            if entry.size_bytes > max_bytes {
                anyhow::bail!("attachment exceeds the requested read limit");
            }
            entry.size_bytes
        };
        let path = blob_path(&self.root, &sha256);
        let bytes = read_file_bounded(&path, max_bytes)?;
        if bytes.len() as u64 != expected_size || hex::encode(Sha256::digest(&bytes)) != sha256 {
            anyhow::bail!("attachment blob failed integrity verification");
        }
        let _ = self.descriptor(&sha256)?;
        Ok(bytes)
    }

    pub fn release_reference(&self, reference: &str) -> anyhow::Result<usize> {
        validate_reference(reference)?;
        let mut metadata = self.metadata.lock();
        let mut next = metadata.clone();
        let mut removed = 0;
        for entry in next.attachments.values_mut() {
            if entry.references.remove(reference) {
                removed += 1;
            }
        }
        if removed > 0 {
            save_metadata(&self.root, &next)?;
            *metadata = next;
        }
        Ok(removed)
    }

    fn release_hash_references(
        &self,
        reference: &str,
        sha256s: &HashSet<String>,
    ) -> anyhow::Result<usize> {
        let mut metadata = self.metadata.lock();
        let mut next = metadata.clone();
        let mut removed = 0;
        for sha256 in sha256s {
            if next
                .attachments
                .get_mut(sha256)
                .is_some_and(|entry| entry.references.remove(reference))
            {
                removed += 1;
            }
        }
        if removed > 0 {
            save_metadata(&self.root, &next)?;
            *metadata = next;
        }
        Ok(removed)
    }

    pub fn delete(&self, sha256: &str) -> anyhow::Result<bool> {
        let sha256 = normalize_sha256(sha256)?;
        let mut metadata = self.metadata.lock();
        let Some(entry) = metadata.attachments.get(&sha256) else {
            return Ok(false);
        };
        if !entry.references.is_empty() {
            anyhow::bail!("attachment is still referenced");
        }
        let mut next = metadata.clone();
        next.attachments.remove(&sha256);
        save_metadata(&self.root, &next)?;
        *metadata = next;
        remove_blob(&self.root, &sha256)?;
        Ok(true)
    }

    pub fn prune_to_quota(
        &self,
        quota_bytes: u64,
        protected_sha256: &HashSet<String>,
    ) -> anyhow::Result<AttachmentPruneReport> {
        let protected = protected_sha256
            .iter()
            .map(|value| normalize_sha256(value))
            .collect::<anyhow::Result<HashSet<_>>>()?;
        let mut metadata = self.metadata.lock();
        let mut next = metadata.clone();
        let removed = plan_prune(&mut next, quota_bytes, &protected)?;
        let bytes_removed = removed.iter().map(|(_, size)| size).sum();
        let bytes_remaining = next
            .attachments
            .values()
            .fold(0u64, |total, entry| total.saturating_add(entry.size_bytes));
        save_metadata(&self.root, &next)?;
        *metadata = next;
        remove_evicted_files(&self.root, &removed)?;
        Ok(AttachmentPruneReport {
            removed: removed.into_iter().map(|(sha256, _)| sha256).collect(),
            bytes_removed,
            bytes_remaining,
        })
    }

    pub fn cleanup_stale_partials(&self, max_age: Duration) -> anyhow::Result<Vec<PathBuf>> {
        let active = self.active_partials.lock().clone();
        let now = SystemTime::now();
        let mut paths = std::fs::read_dir(self.root.join(".partial"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        let mut removed = Vec::new();
        for path in paths {
            if active.contains(&path) || !path.is_file() {
                continue;
            }
            let modified = std::fs::metadata(&path)?.modified()?;
            if now.duration_since(modified).unwrap_or_default() < max_age {
                continue;
            }
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
        Ok(removed)
    }

    #[cfg(test)]
    fn metadata_json(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(metadata_path(&self.root))?)
    }
}

struct PartialGuard<'a> {
    store: &'a AttachmentStore,
    path: PathBuf,
    keep: bool,
}

impl<'a> PartialGuard<'a> {
    fn new(store: &'a AttachmentStore, path: PathBuf) -> Self {
        Self {
            store,
            path,
            keep: false,
        }
    }

    fn keep(&mut self) {
        self.keep = true;
    }

    fn remove_now(&mut self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.keep = true;
        Ok(())
    }
}

impl Drop for PartialGuard<'_> {
    fn drop(&mut self) {
        self.store.active_partials.lock().remove(&self.path);
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn descriptor_from_record(entry: &AttachmentRecord) -> AttachmentDescriptor {
    let filename = entry
        .filenames
        .iter()
        .next()
        .cloned()
        .unwrap_or_else(|| safe_filename(None, &entry.sha256, &entry.media_type));
    AttachmentDescriptor {
        id: entry.sha256.clone(),
        sha256: entry.sha256.clone(),
        media_type: entry.media_type.clone(),
        kind: kind_for_media_type(&entry.media_type),
        size_bytes: entry.size_bytes,
        filename,
    }
}

fn kind_for_media_type(media_type: &str) -> AttachmentKind {
    match media_type {
        "application/pdf" => AttachmentKind::Document,
        "text/plain" => AttachmentKind::Text,
        _ => AttachmentKind::Image,
    }
}

fn take_access_order(metadata: &mut AttachmentMetadata) -> u64 {
    let order = metadata.next_access_order;
    metadata.next_access_order = metadata.next_access_order.saturating_add(1);
    order
}

fn attachment_schema_version() -> u32 {
    ATTACHMENT_SCHEMA_VERSION
}

fn plan_prune(
    metadata: &mut AttachmentMetadata,
    quota_bytes: u64,
    protected: &HashSet<String>,
) -> anyhow::Result<Vec<(String, u64)>> {
    let mut total = metadata
        .attachments
        .values()
        .fold(0u64, |total, entry| total.saturating_add(entry.size_bytes));
    let mut candidates = metadata
        .attachments
        .values()
        .filter(|entry| entry.references.is_empty())
        .filter(|entry| !protected.contains(&entry.sha256))
        .map(|entry| (entry.access_order, entry.sha256.clone(), entry.size_bytes))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let mut removed = Vec::new();
    for (_, sha256, size_bytes) in candidates {
        if total <= quota_bytes {
            break;
        }
        metadata.attachments.remove(&sha256);
        total = total.saturating_sub(size_bytes);
        removed.push((sha256, size_bytes));
    }
    if total > quota_bytes {
        anyhow::bail!("attachment quota cannot be met without removing protected data");
    }
    Ok(removed)
}

fn remove_evicted_files(root: &Path, removed: &[(String, u64)]) -> anyhow::Result<()> {
    for (sha256, _) in removed {
        remove_blob(root, sha256)?;
    }
    Ok(())
}

fn remove_blob(root: &Path, sha256: &str) -> anyhow::Result<()> {
    let path = blob_path(root, sha256);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

fn metadata_path(root: &Path) -> PathBuf {
    root.join("metadata.json")
}

fn ensure_store_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("attachment store path is not a private directory");
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    options.open(path)
}

fn open_local_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn save_metadata(root: &Path, metadata: &AttachmentMetadata) -> anyhow::Result<()> {
    let temporary = root.join(format!(".metadata-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = create_private_file(&temporary)?;
        file.write_all(serde_json::to_string_pretty(metadata)?.as_bytes())?;
        file.sync_all()?;
        replace_file(&temporary, &metadata_path(root))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    if let Ok(directory) = File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn replace_file(temporary: &Path, destination: &Path) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(temporary, destination)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        match std::fs::rename(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) && destination.exists() =>
            {
                std::fs::remove_file(destination)?;
                std::fs::rename(temporary, destination)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn load_metadata(
    root: &Path,
    clock: &Clock,
    max_attachment_bytes: u64,
) -> anyhow::Result<(AttachmentMetadata, Option<PathBuf>)> {
    let path = metadata_path(root);
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!("attachment metadata cannot be a symbolic link");
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let metadata = recover_blobs(root, clock, max_attachment_bytes)?;
            save_metadata(root, &metadata)?;
            return Ok((metadata, None));
        }
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return recover_corrupt_metadata(root, &path, clock, max_attachment_bytes),
    };
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version > ATTACHMENT_SCHEMA_VERSION as u64 {
        anyhow::bail!("attachment metadata version {version} is newer than this build");
    }
    let mut metadata: AttachmentMetadata = match serde_json::from_value(value) {
        Ok(metadata) => metadata,
        Err(_) => return recover_corrupt_metadata(root, &path, clock, max_attachment_bytes),
    };
    if version < ATTACHMENT_SCHEMA_VERSION as u64 {
        let backup = root.join(format!("metadata.json.v{version}.bak"));
        if !backup.exists() {
            std::fs::write(&backup, raw)?;
        }
        metadata.schema_version = ATTACHMENT_SCHEMA_VERSION;
        save_metadata(root, &metadata)?;
    }
    Ok((metadata, None))
}

fn recover_corrupt_metadata(
    root: &Path,
    path: &Path,
    clock: &Clock,
    max_attachment_bytes: u64,
) -> anyhow::Result<(AttachmentMetadata, Option<PathBuf>)> {
    let backup = root.join(format!("metadata.json.corrupt-{}", uuid::Uuid::new_v4()));
    std::fs::rename(path, &backup)?;
    let metadata = recover_blobs(root, clock, max_attachment_bytes)?;
    save_metadata(root, &metadata)?;
    Ok((metadata, Some(backup)))
}

fn recover_blobs(
    root: &Path,
    clock: &Clock,
    max_attachment_bytes: u64,
) -> anyhow::Result<AttachmentMetadata> {
    let mut metadata = AttachmentMetadata::default();
    let blobs = root.join("blobs");
    let mut paths = Vec::new();
    for directory in std::fs::read_dir(&blobs)? {
        let directory = directory?;
        if !directory.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(directory.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    for path in paths {
        let Some(filename) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(sha256) = normalize_sha256(filename) else {
            continue;
        };
        let size_bytes = path.metadata()?.len();
        if size_bytes == 0 || size_bytes > max_attachment_bytes {
            continue;
        }
        if sha256_file(&path, max_attachment_bytes)? != sha256 {
            continue;
        }
        let Ok(media_type) = sniff_media_type(&path, max_attachment_bytes) else {
            continue;
        };
        let now = clock();
        let access_order = take_access_order(&mut metadata);
        metadata.attachments.insert(
            sha256.clone(),
            AttachmentRecord {
                sha256,
                media_type: media_type.to_string(),
                size_bytes,
                created_at: now,
                last_accessed_at: now,
                access_order,
                filenames: BTreeSet::new(),
                references: BTreeSet::new(),
            },
        );
    }
    Ok(metadata)
}

fn blob_path(root: &Path, sha256: &str) -> PathBuf {
    root.join("blobs").join(&sha256[..2]).join(sha256)
}

fn blob_exists_regular(path: &Path) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("attachment blob cannot be a symbolic link")
        }
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => anyhow::bail!("attachment blob path is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn normalize_sha256(value: &str) -> anyhow::Result<String> {
    let normalized = value.to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid attachment sha256");
    }
    Ok(normalized)
}

fn validate_reference(reference: &str) -> anyhow::Result<()> {
    if reference.is_empty() || reference.len() > 256 || reference.chars().any(char::is_control) {
        anyhow::bail!("attachment reference must contain 1 to 256 safe characters");
    }
    Ok(())
}

fn safe_filename(filename: Option<&str>, sha256: &str, media_type: &str) -> String {
    let extension = match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        _ => "txt",
    };
    let raw = filename
        .and_then(|value| value.rsplit(['/', '\\']).next())
        .unwrap_or(&sha256[..12]);
    let stem = raw.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(raw);
    let mut safe = String::new();
    for character in stem.chars() {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            character
        } else {
            '_'
        };
        if safe.len() + character.len_utf8() > 96 {
            break;
        }
        safe.push(character);
    }
    let safe = safe.trim_matches(['.', '_']);
    let mut stem = if safe.is_empty() {
        sha256[..12].to_string()
    } else {
        safe.to_string()
    };
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.to_ascii_uppercase().as_str())
    {
        stem.insert(0, '_');
    }
    format!("{stem}.{extension}")
}

fn normalize_media_type(value: &str) -> anyhow::Result<&'static str> {
    let value = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match value.as_str() {
        "image/png" => Ok("image/png"),
        "image/jpeg" => Ok("image/jpeg"),
        "image/webp" => Ok("image/webp"),
        "image/gif" => Ok("image/gif"),
        "application/pdf" => Ok("application/pdf"),
        "text/plain" => Ok("text/plain"),
        _ => anyhow::bail!("attachment media type is not permitted"),
    }
}

fn sniff_media_type(path: &Path, max_bytes: u64) -> anyhow::Result<&'static str> {
    let bytes = read_file_bounded(path, max_bytes)?;
    if bytes.is_empty() {
        anyhow::bail!("empty attachments are not supported");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            anyhow::bail!("invalid PNG attachment");
        }
        return Ok("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        if bytes.len() < 13 {
            anyhow::bail!("invalid GIF attachment");
        }
        return Ok("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        for marker in [
            b"/JavaScript".as_slice(),
            b"/JS".as_slice(),
            b"/Launch".as_slice(),
            b"/OpenAction".as_slice(),
            b"/AA".as_slice(),
            b"/EmbeddedFile".as_slice(),
            b"/RichMedia".as_slice(),
        ] {
            if contains_ascii_case_insensitive(&bytes, marker) {
                anyhow::bail!("active PDF attachments are not permitted");
            }
        }
        return Ok("application/pdf");
    }
    if is_executable_magic(&bytes) {
        anyhow::bail!("executable attachments are not permitted");
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("attachment type is unknown or not permitted"))?;
    let normalized = text.trim_start_matches('\u{feff}').trim_start();
    let lower = normalized
        .chars()
        .take(4096)
        .collect::<String>()
        .to_ascii_lowercase();
    if normalized.starts_with("#!")
        || lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || lower.starts_with("<svg")
        || lower.starts_with("<?xml")
        || ["<script", "<iframe", "<object", "<embed", "javascript:"]
            .iter()
            .any(|marker| lower.contains(marker))
    {
        anyhow::bail!("active text attachments are not permitted");
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("binary attachments are not permitted as text");
    }
    Ok("text/plain")
}

fn is_executable_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
        || bytes.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn read_file_bounded(path: &Path, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let size = std::fs::metadata(path)?.len();
    if size > max_bytes {
        anyhow::bail!("attachment exceeds the read limit");
    }
    let capacity = usize::try_from(size)
        .map_err(|_| anyhow::anyhow!("attachment is too large for this platform"))?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("attachment grew beyond the read limit");
    }
    Ok(bytes)
}

fn sha256_file(path: &Path, max_bytes: u64) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("attachment size overflow"))?;
        if total > max_bytes {
            anyhow::bail!("attachment exceeds the hash limit");
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn parse_data_url(data_url: &str) -> anyhow::Result<(String, &str)> {
    let body = data_url
        .strip_prefix("data:")
        .ok_or_else(|| anyhow::anyhow!("attachment is not a data URL"))?;
    let (header, encoded) = body
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("data URL has no payload"))?;
    if header.len() > 256 {
        anyhow::bail!("data URL header is too long");
    }
    let mut parts = header.split(';');
    let media_type = normalize_media_type(parts.next().unwrap_or_default())?;
    let parameters = parts.collect::<Vec<_>>();
    if parameters.is_empty()
        || !parameters
            .last()
            .is_some_and(|value| value.eq_ignore_ascii_case("base64"))
        || parameters[..parameters.len() - 1]
            .iter()
            .any(|value| !value.eq_ignore_ascii_case("charset=utf-8"))
    {
        anyhow::bail!("only base64 data URLs are supported");
    }
    Ok((media_type.to_string(), encoded))
}

fn decode_base64_bounded(encoded: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    if encoded.bytes().any(|byte| byte.is_ascii_whitespace()) || !encoded.len().is_multiple_of(4) {
        anyhow::bail!("data URL contains invalid base64");
    }
    let max_encoded = max_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("data URL size limit overflow"))?;
    if encoded.len() as u64 > max_encoded {
        anyhow::bail!("decoded data URL would exceed the attachment limit");
    }
    let capacity = encoded
        .len()
        .checked_div(4)
        .and_then(|value| value.checked_mul(3))
        .ok_or_else(|| anyhow::anyhow!("data URL capacity overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    let chunks = encoded.as_bytes().chunks_exact(4);
    let chunk_count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let final_chunk = index + 1 == chunk_count;
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c_padding = chunk[2] == b'=';
        let d_padding = chunk[3] == b'=';
        if c_padding && !d_padding || d_padding && !final_chunk {
            anyhow::bail!("data URL contains invalid base64 padding");
        }
        let c = if c_padding {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if d_padding {
            0
        } else {
            base64_value(chunk[3])?
        };
        if c_padding && b & 0x0f != 0 || d_padding && !c_padding && c & 0x03 != 0 {
            anyhow::bail!("data URL contains invalid base64 padding bits");
        }
        output.push((a << 2) | (b >> 4));
        if !c_padding {
            output.push((b << 4) | (c >> 2));
        }
        if !d_padding {
            output.push((c << 6) | d);
        }
        if output.len() as u64 > max_bytes {
            anyhow::bail!("decoded data URL exceeds the attachment limit");
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => anyhow::bail!("data URL contains invalid base64"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestStore {
        root: PathBuf,
    }

    impl TestStore {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "kiralm-attachments-{name}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn store(&self, limits: AttachmentLimits) -> AttachmentStore {
            AttachmentStore::open_with_clock(
                self.root.clone(),
                limits,
                Arc::new(|| {
                    DateTime::parse_from_rfc3339("2026-08-29T12:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                }),
            )
            .unwrap()
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn limits(max_bytes: u64, quota_bytes: u64) -> AttachmentLimits {
        AttachmentLimits {
            max_attachment_bytes: max_bytes,
            max_attachments_per_reference: 3,
            quota_bytes,
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    #[test]
    fn rejects_spoofed_mime_and_active_content() {
        let home = TestStore::new("mime");
        let store = home.store(limits(1024, 4096));
        let mismatch = store
            .ingest_reader(
                png().as_slice(),
                Some("photo.jpg"),
                Some("image/jpeg"),
                "chat",
            )
            .unwrap_err()
            .to_string();
        assert!(mismatch.contains("does not match"));
        let html = store
            .ingest_reader(
                b"<!doctype html><script>alert(1)</script>".as_slice(),
                None,
                Some("text/plain"),
                "chat",
            )
            .unwrap_err()
            .to_string();
        assert!(html.contains("active text"));
        let executable = store
            .ingest_reader(b"\x7fELFpayload".as_slice(), None, None, "chat")
            .unwrap_err()
            .to_string();
        assert!(executable.contains("executable"));
        assert!(store
            .ingest_reader(b"<svg><script/></svg>".as_slice(), None, None, "chat")
            .is_err());
        assert!(store
            .ingest_reader(b"\0\xffunknown".as_slice(), None, None, "chat")
            .is_err());
        assert!(store
            .ingest_reader(
                b"%PDF-1.7\n/OpenAction 1 0 R".as_slice(),
                None,
                None,
                "chat"
            )
            .is_err());
    }

    #[test]
    fn stops_oversized_streams_and_removes_partials() {
        let home = TestStore::new("oversize");
        let store = home.store(limits(8, 64));
        let error = store
            .ingest_reader(std::io::repeat(b'x').take(9), None, None, "chat")
            .unwrap_err()
            .to_string();
        assert!(error.contains("8 byte limit"));
        assert_eq!(
            std::fs::read_dir(home.root.join(".partial"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_local_path_and_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let home = TestStore::new("paths");
        let allowed = home.root.join("allowed");
        let outside = home.root.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(allowed.join("safe.txt"), "safe").unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(outside.join("secret.txt"), allowed.join("escape.txt")).unwrap();
        let store = home.store(limits(1024, 4096));
        let error = store
            .ingest_local_path(
                &allowed.join("escape.txt"),
                std::slice::from_ref(&allowed),
                None,
                None,
                "chat",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes"));
        assert!(store
            .ingest_local_path(
                &outside.join("secret.txt"),
                std::slice::from_ref(&allowed),
                None,
                None,
                "chat",
            )
            .is_err());
        assert!(store
            .ingest_local_path(
                &allowed.join("safe.txt"),
                std::slice::from_ref(&allowed),
                None,
                None,
                "chat",
            )
            .is_ok());
    }

    #[test]
    fn deduplicates_content_and_tracks_references() {
        let home = TestStore::new("dedupe");
        let store = home.store(limits(1024, 4096));
        let first = store
            .ingest_reader(b"same text".as_slice(), Some("one.md"), None, "chat-a")
            .unwrap();
        let second = store
            .ingest_reader(b"same text".as_slice(), Some("two.txt"), None, "chat-b")
            .unwrap();
        assert_eq!(first.sha256, second.sha256);
        let blob_files = std::fs::read_dir(home.root.join("blobs").join(&first.sha256[..2]))
            .unwrap()
            .count();
        assert_eq!(blob_files, 1);
        assert_eq!(store.release_reference("chat-a").unwrap(), 1);
        assert!(store.delete(&first.sha256).is_err());
        assert_eq!(store.release_reference("chat-b").unwrap(), 1);
        assert!(store.delete(&first.sha256).unwrap());
    }

    #[test]
    fn recovers_atomically_from_corrupt_metadata() {
        let home = TestStore::new("recovery");
        let descriptor = {
            let store = home.store(limits(1024, 4096));
            store
                .ingest_reader(b"recover me".as_slice(), None, None, "chat")
                .unwrap()
        };
        std::fs::write(home.root.join("metadata.json"), "{broken").unwrap();
        let store = home.store(limits(1024, 4096));
        assert!(store
            .recovered_corrupt_metadata()
            .is_some_and(Path::is_file));
        assert!(store.descriptor(&descriptor.sha256).unwrap().is_some());
        assert!(serde_json::from_str::<serde_json::Value>(&store.metadata_json().unwrap()).is_ok());
    }

    #[test]
    fn quota_never_removes_referenced_or_explicitly_protected_data() {
        let home = TestStore::new("quota");
        let store = home.store(limits(1024, 4096));
        let referenced = store
            .ingest_reader(b"referenced".as_slice(), None, None, "chat")
            .unwrap();
        let removable = store
            .ingest_reader(b"removable".as_slice(), None, None, "other")
            .unwrap();
        store.release_reference("other").unwrap();
        let report = store
            .prune_to_quota(referenced.size_bytes, &HashSet::new())
            .unwrap();
        assert_eq!(report.removed, vec![removable.sha256.clone()]);
        assert!(store.descriptor(&referenced.sha256).unwrap().is_some());

        let protected = store
            .ingest_reader(b"protected".as_slice(), None, None, "protect")
            .unwrap();
        store.release_reference("protect").unwrap();
        let protections = HashSet::from([protected.sha256.clone()]);
        let error = store
            .prune_to_quota(0, &protections)
            .unwrap_err()
            .to_string();
        assert!(error.contains("protected data"));
        assert!(store.descriptor(&protected.sha256).unwrap().is_some());
    }

    #[test]
    fn decodes_bounded_data_urls_and_uses_detected_extensions() {
        let home = TestStore::new("data-url");
        let store = home.store(limits(64, 4096));
        let descriptor = store
            .ingest_data_url(
                "data:text/plain;charset=utf-8;base64,aGVsbG8=",
                Some("../../unsafe.exe"),
                "chat",
            )
            .unwrap();
        assert_eq!(descriptor.filename, "unsafe.txt");
        assert_eq!(store.read(&descriptor.sha256, 64).unwrap(), b"hello");
        assert!(store
            .ingest_data_url("data:text/plain;base64,%%%%", None, "chat")
            .is_err());
        let oversized = format!("data:text/plain;base64,{}", "eHh4".repeat(22));
        assert!(store.ingest_data_url(&oversized, None, "chat").is_err());
    }

    #[test]
    fn metadata_is_deterministic_and_covers_supported_types() {
        let first_home = TestStore::new("deterministic-a");
        let second_home = TestStore::new("deterministic-b");
        let samples = vec![
            (png(), "image/png"),
            (b"\xff\xd8\xffjpeg".to_vec(), "image/jpeg"),
            (b"GIF89a\x01\0\x01\0\0\0\0".to_vec(), "image/gif"),
            (b"RIFF\x04\0\0\0WEBP".to_vec(), "image/webp"),
            (
                b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n%%EOF".to_vec(),
                "application/pdf",
            ),
            (b"plain text".to_vec(), "text/plain"),
        ];
        for home in [&first_home, &second_home] {
            let store = home.store(AttachmentLimits {
                max_attachment_bytes: 4096,
                max_attachments_per_reference: 8,
                quota_bytes: 8192,
            });
            for (index, (bytes, media_type)) in samples.iter().enumerate() {
                let descriptor = store
                    .ingest_reader(
                        bytes.as_slice(),
                        Some(&format!("sample-{index}.wrong")),
                        Some(media_type),
                        "chat",
                    )
                    .unwrap();
                assert_eq!(&descriptor.media_type, media_type);
            }
        }
        assert_eq!(
            std::fs::read_to_string(first_home.root.join("metadata.json")).unwrap(),
            std::fs::read_to_string(second_home.root.join("metadata.json")).unwrap()
        );
    }

    #[test]
    fn removes_only_stale_inactive_partials() {
        let home = TestStore::new("partials");
        let store = home.store(limits(1024, 4096));
        let stale = store.root.join(".partial/stale.partial");
        let active = store.root.join(".partial/active.partial");
        std::fs::write(&stale, "stale").unwrap();
        std::fs::write(&active, "active").unwrap();
        store.active_partials.lock().insert(active.clone());
        let removed = store.cleanup_stale_partials(Duration::ZERO).unwrap();
        assert_eq!(removed, vec![stale]);
        assert!(active.is_file());
    }

    #[test]
    fn enforces_reference_count_and_preserves_prior_batch_state() {
        let home = TestStore::new("count");
        let store = home.store(limits(1024, 4096));
        let prior = store
            .ingest_reader(b"prior".as_slice(), None, None, "chat")
            .unwrap();
        let inputs = vec![
            AttachmentInput {
                source: AttachmentSource::Bytes(b"second".to_vec()),
                filename: None,
                declared_media_type: None,
            },
            AttachmentInput {
                source: AttachmentSource::Bytes(b"third".to_vec()),
                filename: None,
                declared_media_type: None,
            },
            AttachmentInput {
                source: AttachmentSource::Bytes(b"fourth".to_vec()),
                filename: None,
                declared_media_type: None,
            },
        ];
        assert!(store.ingest_many(inputs, "chat", &[]).is_err());
        assert!(store.delete(&prior.sha256).is_err());
        assert_eq!(store.release_reference("chat").unwrap(), 1);
    }

    #[test]
    fn migrates_legacy_metadata_and_rejects_future_versions() {
        let home = TestStore::new("versions");
        let store = home.store(limits(1024, 4096));
        let path = home.root.join("metadata.json");
        let mut legacy: serde_json::Value =
            serde_json::from_str(&store.metadata_json().unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("schema_version");
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
        drop(store);

        let migrated = home.store(limits(1024, 4096));
        assert!(home.root.join("metadata.json.v0.bak").is_file());
        let saved: serde_json::Value =
            serde_json::from_str(&migrated.metadata_json().unwrap()).unwrap();
        assert_eq!(saved["schema_version"], ATTACHMENT_SCHEMA_VERSION);
        drop(migrated);

        let mut future = saved;
        future["schema_version"] = serde_json::Value::from(2);
        std::fs::write(&path, serde_json::to_string_pretty(&future).unwrap()).unwrap();
        let error = match AttachmentStore::open(&home.root, limits(1024, 4096)) {
            Ok(_) => panic!("future metadata version was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("newer than this build"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(path).unwrap())
                .unwrap()["schema_version"],
            2
        );
    }
}
