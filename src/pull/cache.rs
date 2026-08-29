use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const METADATA_VERSION: u32 = 1;

static CACHES: OnceLock<Mutex<HashMap<PathBuf, Weak<CacheShared>>>> = OnceLock::new();

#[derive(Clone)]
pub struct ArtifactCache {
    shared: Arc<CacheShared>,
}

struct CacheShared {
    root: PathBuf,
    metadata: Mutex<CacheMetadata>,
    flights: Mutex<HashMap<String, Arc<FlightState>>>,
    active_partials: Mutex<HashSet<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheMetadata {
    schema_version: u32,
    next_access_order: u64,
    artifacts: BTreeMap<String, ArtifactMetadata>,
}

impl Default for CacheMetadata {
    fn default() -> Self {
        Self {
            schema_version: METADATA_VERSION,
            next_access_order: 1,
            artifacts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub sha256: String,
    pub size_bytes: u64,
    pub source_url: Option<String>,
    pub etag: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_accessed_at: chrono::DateTime<chrono::Utc>,
    pub references: BTreeSet<String>,
    access_order: u64,
}

#[derive(Debug, Clone)]
pub struct CachedArtifact {
    pub sha256: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ArtifactSource {
    pub url: String,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArtifactFetch {
    pub artifact: CachedArtifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationMode {
    HardLink,
    Copy,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct PruneProtections {
    pub active: HashSet<String>,
    pub hot: HashSet<String>,
    pub downloading: HashSet<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct PruneReport {
    pub removed: Vec<String>,
    pub bytes_removed: u64,
    pub bytes_remaining: u64,
}

pub enum PullFlight {
    Start(PullLeader),
    Join(PullFollower),
}

pub struct PullLeader {
    shared: Weak<CacheShared>,
    key: String,
    state: Arc<FlightState>,
    finished: bool,
}

#[derive(Clone)]
pub struct PullFollower {
    state: Arc<FlightState>,
}

struct FlightState {
    result: Mutex<Option<Result<(), String>>>,
    notify: tokio::sync::Notify,
}

pub struct PartialArtifact {
    shared: Weak<CacheShared>,
    path: PathBuf,
}

impl ArtifactCache {
    pub fn from_home(home: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::new(home.as_ref().join("cache/models/sha256"))
    }

    pub fn new(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        let caches = CACHES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut caches = lock(caches)?;
        if let Some(shared) = caches.get(&root).and_then(Weak::upgrade) {
            return Ok(Self { shared });
        }

        std::fs::create_dir_all(root.join(".partial"))?;
        let metadata = load_or_recover_metadata(&root)?;
        let shared = Arc::new(CacheShared {
            root: root.clone(),
            metadata: Mutex::new(metadata),
            flights: Mutex::new(HashMap::new()),
            active_partials: Mutex::new(HashSet::new()),
        });
        caches.insert(root, Arc::downgrade(&shared));
        Ok(Self { shared })
    }

    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    pub fn begin_pull(&self, artifact_key: &str) -> anyhow::Result<PullFlight> {
        self.begin_flight(format!("pull:{artifact_key}"))
    }

    fn begin_artifact(&self, artifact_key: &str) -> anyhow::Result<PullFlight> {
        self.begin_flight(format!("artifact:{artifact_key}"))
    }

    fn begin_flight(&self, key: String) -> anyhow::Result<PullFlight> {
        let mut flights = lock(&self.shared.flights)?;
        if let Some(state) = flights.get(&key) {
            return Ok(PullFlight::Join(PullFollower {
                state: state.clone(),
            }));
        }
        let state = Arc::new(FlightState {
            result: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        });
        flights.insert(key.clone(), state.clone());
        Ok(PullFlight::Start(PullLeader {
            shared: Arc::downgrade(&self.shared),
            key,
            state,
            finished: false,
        }))
    }

    pub fn lookup(&self, sha256: &str) -> anyhow::Result<Option<CachedArtifact>> {
        let sha256 = normalize_sha256(sha256)?;
        let mut metadata = lock(&self.shared.metadata)?;
        let Some(entry) = metadata.artifacts.get(&sha256) else {
            return Ok(None);
        };
        let path = artifact_path(&self.shared.root, &sha256);
        let Ok(file) = std::fs::metadata(&path) else {
            metadata.artifacts.remove(&sha256);
            save_metadata(&self.shared.root, &metadata)?;
            return Ok(None);
        };
        if file.len() != entry.size_bytes {
            metadata.artifacts.remove(&sha256);
            save_metadata(&self.shared.root, &metadata)?;
            return Ok(None);
        }
        let size_bytes = entry.size_bytes;
        touch(&mut metadata, &sha256);
        save_metadata(&self.shared.root, &metadata)?;
        Ok(Some(CachedArtifact {
            sha256,
            path,
            size_bytes,
        }))
    }

    pub fn partial(&self, artifact_key: &str) -> anyhow::Result<PartialArtifact> {
        let filename = hex::encode(Sha256::digest(artifact_key.as_bytes()));
        let path = self
            .shared
            .root
            .join(".partial")
            .join(format!("{filename}.partial"));
        lock(&self.shared.active_partials)?.insert(path.clone());
        Ok(PartialArtifact {
            shared: Arc::downgrade(&self.shared),
            path,
        })
    }

    pub fn store_verified_partial(
        &self,
        partial_path: &Path,
        sha256: &str,
        source: Option<&ArtifactSource>,
    ) -> anyhow::Result<CachedArtifact> {
        let sha256 = normalize_sha256(sha256)?;
        let final_path = artifact_path(&self.shared.root, &sha256);
        let parent = final_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache artifact has no parent directory"))?;
        std::fs::create_dir_all(parent)?;
        if final_path.exists() && sha256_file(&final_path)? == sha256 {
            std::fs::remove_file(partial_path)?;
        } else {
            replace_file(partial_path, &final_path)?;
        }
        let size_bytes = std::fs::metadata(&final_path)?.len();
        let now = chrono::Utc::now();
        let mut metadata = lock(&self.shared.metadata)?;
        let access_order = next_access_order(&mut metadata);
        let entry = metadata
            .artifacts
            .entry(sha256.clone())
            .or_insert_with(|| ArtifactMetadata {
                sha256: sha256.clone(),
                size_bytes,
                source_url: source.map(|value| value.url.clone()),
                etag: source.and_then(|value| value.etag.clone()),
                created_at: now,
                last_accessed_at: now,
                references: BTreeSet::new(),
                access_order,
            });
        entry.size_bytes = size_bytes;
        entry.last_accessed_at = now;
        entry.access_order = access_order;
        if let Some(source) = source {
            entry.source_url = Some(source.url.clone());
            entry.etag = source.etag.clone();
        }
        save_metadata(&self.shared.root, &metadata)?;
        Ok(CachedArtifact {
            sha256,
            path: final_path,
            size_bytes,
        })
    }

    pub fn materialize(
        &self,
        artifact: &CachedArtifact,
        destination: &Path,
    ) -> anyhow::Result<MaterializationMode> {
        self.materialize_with(
            artifact,
            destination,
            |source, target| std::fs::hard_link(source, target),
            |source, target| std::fs::copy(source, target),
        )
    }

    fn materialize_with<L, C>(
        &self,
        artifact: &CachedArtifact,
        destination: &Path,
        linker: L,
        copier: C,
    ) -> anyhow::Result<MaterializationMode>
    where
        L: FnOnce(&Path, &Path) -> std::io::Result<()>,
        C: FnOnce(&Path, &Path) -> std::io::Result<u64>,
    {
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("model destination has no parent directory"))?;
        std::fs::create_dir_all(parent)?;
        let filename = destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("model.gguf");
        let temporary = parent.join(format!(
            ".{filename}.cache-{}.partial",
            uuid::Uuid::new_v4()
        ));
        let mode = match linker(&artifact.path, &temporary) {
            Ok(()) => MaterializationMode::HardLink,
            Err(_) => {
                copier(&artifact.path, &temporary)?;
                MaterializationMode::Copy
            }
        };
        replace_file(&temporary, destination)?;

        let reference = destination.to_string_lossy().into_owned();
        let mut metadata = lock(&self.shared.metadata)?;
        let access_order = next_access_order(&mut metadata);
        let entry = metadata
            .artifacts
            .get_mut(&artifact.sha256)
            .ok_or_else(|| anyhow::anyhow!("cache metadata is missing {}", artifact.sha256))?;
        entry.references.insert(reference);
        entry.last_accessed_at = chrono::Utc::now();
        entry.access_order = access_order;
        save_metadata(&self.shared.root, &metadata)?;
        Ok(mode)
    }

    pub fn remove_reference(&self, sha256: &str, path: &Path) -> anyhow::Result<bool> {
        let sha256 = normalize_sha256(sha256)?;
        let mut metadata = lock(&self.shared.metadata)?;
        let Some(entry) = metadata.artifacts.get_mut(&sha256) else {
            return Ok(false);
        };
        let removed = entry.references.remove(path.to_string_lossy().as_ref());
        if removed {
            save_metadata(&self.shared.root, &metadata)?;
        }
        Ok(removed)
    }

    pub fn prune_to_quota(
        &self,
        quota_bytes: u64,
        protections: &PruneProtections,
    ) -> anyhow::Result<PruneReport> {
        let mut metadata = lock(&self.shared.metadata)?;
        let mut total: u64 = metadata
            .artifacts
            .values()
            .map(|entry| entry.size_bytes)
            .sum();
        let mut candidates: Vec<_> = metadata
            .artifacts
            .values()
            .filter(|entry| !contains_hash(&protections.active, &entry.sha256))
            .filter(|entry| !contains_hash(&protections.hot, &entry.sha256))
            .filter(|entry| !contains_hash(&protections.downloading, &entry.sha256))
            .map(|entry| (entry.access_order, entry.sha256.clone(), entry.size_bytes))
            .collect();
        candidates.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

        let mut report = PruneReport::default();
        for (_, sha256, size_bytes) in candidates {
            if total <= quota_bytes {
                break;
            }
            let path = artifact_path(&self.shared.root, &sha256);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            metadata.artifacts.remove(&sha256);
            total = total.saturating_sub(size_bytes);
            report.bytes_removed += size_bytes;
            report.removed.push(sha256);
        }
        report.bytes_remaining = total;
        save_metadata(&self.shared.root, &metadata)?;
        Ok(report)
    }

    pub fn cleanup_stale_partials(&self, max_age: Duration) -> anyhow::Result<Vec<PathBuf>> {
        let partial_dir = self.shared.root.join(".partial");
        let active = lock(&self.shared.active_partials)?.clone();
        let now = std::time::SystemTime::now();
        let mut removed = Vec::new();
        for entry in std::fs::read_dir(&partial_dir)? {
            let entry = entry?;
            let path = entry.path();
            if active.contains(&path) || !entry.file_type()?.is_file() {
                continue;
            }
            let modified = entry.metadata()?.modified()?;
            if now.duration_since(modified).unwrap_or_default() < max_age {
                continue;
            }
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub fn metadata(&self, sha256: &str) -> anyhow::Result<Option<ArtifactMetadata>> {
        let sha256 = normalize_sha256(sha256)?;
        Ok(lock(&self.shared.metadata)?.artifacts.get(&sha256).cloned())
    }

    pub fn active_artifact_hashes(&self) -> anyhow::Result<HashSet<String>> {
        Ok(lock(&self.shared.flights)?
            .keys()
            .filter_map(|key| key.strip_prefix("artifact:"))
            .filter(|key| normalize_sha256(key).is_ok())
            .map(str::to_ascii_lowercase)
            .collect())
    }
}

impl PullLeader {
    pub fn finish(mut self, result: Result<(), String>) {
        complete_flight(&self.shared, &self.key, &self.state, result);
        self.finished = true;
    }
}

impl Drop for PullLeader {
    fn drop(&mut self) {
        if !self.finished {
            complete_flight(
                &self.shared,
                &self.key,
                &self.state,
                Err("artifact pull was cancelled".to_string()),
            );
        }
    }
}

impl PullFollower {
    pub async fn wait(self) -> Result<(), String> {
        loop {
            let notified = self.state.notify.notified();
            if let Some(result) = lock(&self.state.result)
                .map_err(|error| error.to_string())?
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

impl PartialArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PartialArtifact {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut active) = shared.active_partials.lock() {
                active.remove(&self.path);
            }
        }
    }
}

pub async fn obtain_artifact<F, Fut>(
    cache: &ArtifactCache,
    artifact_key: &str,
    expected_sha256: Option<&str>,
    source: &ArtifactSource,
    fetch: F,
) -> anyhow::Result<ArtifactFetch>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let cached = lookup_cached(cache, expected_sha256)?;
    if let Some(artifact) = cached {
        return Ok(ArtifactFetch { artifact });
    }

    match cache.begin_artifact(artifact_key)? {
        PullFlight::Join(follower) => {
            follower
                .wait()
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            let artifact = lookup_cached(cache, expected_sha256)?
                .ok_or_else(|| anyhow::anyhow!("joined artifact is missing from cache"))?;
            Ok(ArtifactFetch { artifact })
        }
        PullFlight::Start(leader) => {
            let result = async {
                if let Some(artifact) = lookup_cached(cache, expected_sha256)? {
                    return Ok(ArtifactFetch { artifact });
                }
                let partial = cache.partial(artifact_key)?;
                let actual_sha256 = normalize_sha256(&fetch(partial.path().to_path_buf()).await?)?;
                if let Some(expected) = expected_sha256 {
                    let expected = normalize_sha256(expected)?;
                    if expected != actual_sha256 {
                        let _ = std::fs::remove_file(partial.path());
                        anyhow::bail!("sha256 mismatch: expected {expected}, got {actual_sha256}");
                    }
                }
                let artifact =
                    cache.store_verified_partial(partial.path(), &actual_sha256, Some(source))?;
                Ok(ArtifactFetch { artifact })
            }
            .await;
            leader.finish(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            );
            result
        }
    }
}

fn lookup_cached(
    cache: &ArtifactCache,
    expected_sha256: Option<&str>,
) -> anyhow::Result<Option<CachedArtifact>> {
    match expected_sha256 {
        Some(sha256) => cache.lookup(sha256),
        None => Ok(None),
    }
}

fn complete_flight(
    shared: &Weak<CacheShared>,
    key: &str,
    state: &FlightState,
    result: Result<(), String>,
) {
    if let Ok(mut slot) = state.result.lock() {
        *slot = Some(result);
    }
    state.notify.notify_waiters();
    if let Some(shared) = shared.upgrade() {
        if let Ok(mut flights) = shared.flights.lock() {
            flights.remove(key);
        }
    }
}

fn contains_hash(set: &HashSet<String>, sha256: &str) -> bool {
    set.contains(sha256) || set.iter().any(|value| value.eq_ignore_ascii_case(sha256))
}

fn normalize_sha256(value: &str) -> anyhow::Result<String> {
    let normalized = value.to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid sha256 '{value}'");
    }
    Ok(normalized)
}

fn artifact_path(root: &Path, sha256: &str) -> PathBuf {
    root.join(&sha256[..2]).join(format!("{sha256}.gguf"))
}

fn next_access_order(metadata: &mut CacheMetadata) -> u64 {
    let order = metadata.next_access_order;
    metadata.next_access_order = metadata.next_access_order.saturating_add(1);
    order
}

fn touch(metadata: &mut CacheMetadata, sha256: &str) {
    let order = next_access_order(metadata);
    if let Some(entry) = metadata.artifacts.get_mut(sha256) {
        entry.last_accessed_at = chrono::Utc::now();
        entry.access_order = order;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> anyhow::Result<std::sync::MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow::anyhow!("artifact cache lock was poisoned"))
}

fn metadata_path(root: &Path) -> PathBuf {
    root.join("metadata.json")
}

fn save_metadata(root: &Path, metadata: &CacheMetadata) -> anyhow::Result<()> {
    let path = metadata_path(root);
    let temporary = root.join(format!(".metadata-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(serde_json::to_string_pretty(metadata)?.as_bytes())?;
    file.sync_all()?;
    replace_file(&temporary, &path)
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

fn load_or_recover_metadata(root: &Path) -> anyhow::Result<CacheMetadata> {
    let path = metadata_path(root);
    let (mut metadata, full_recovery) = match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<CacheMetadata>(&raw) {
            Ok(metadata) if metadata.schema_version == METADATA_VERSION => (metadata, false),
            Ok(metadata) => {
                anyhow::bail!(
                    "cache metadata version {} is unsupported",
                    metadata.schema_version
                )
            }
            Err(_) => {
                let corrupt = root.join(format!("metadata.json.corrupt-{}", uuid::Uuid::new_v4()));
                std::fs::rename(&path, corrupt)?;
                (CacheMetadata::default(), true)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (CacheMetadata::default(), true)
        }
        Err(error) => return Err(error.into()),
    };

    let mut recovered = BTreeMap::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name() == ".partial" {
            continue;
        }
        for artifact in std::fs::read_dir(entry.path())? {
            let artifact = artifact?;
            if !artifact.file_type()?.is_file() {
                continue;
            }
            let path = artifact.path();
            let Some(filename) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            let Some(sha256) = filename.strip_suffix(".gguf") else {
                continue;
            };
            let Ok(sha256) = normalize_sha256(sha256) else {
                continue;
            };
            let size_bytes = artifact.metadata()?.len();
            let order = next_access_order(&mut metadata);
            let existing = metadata.artifacts.remove(&sha256);
            let trusted = existing
                .as_ref()
                .map(|entry| entry.size_bytes == size_bytes)
                .unwrap_or(false);
            if (full_recovery || !trusted) && sha256_file(&path)? != sha256 {
                continue;
            }
            let now = chrono::Utc::now();
            let mut entry = existing.unwrap_or_else(|| ArtifactMetadata {
                sha256,
                size_bytes,
                source_url: None,
                etag: None,
                created_at: now,
                last_accessed_at: now,
                references: BTreeSet::new(),
                access_order: order,
            });
            entry.size_bytes = size_bytes;
            recovered.insert(entry.sha256.clone(), entry);
        }
    }
    metadata.artifacts = recovered;
    save_metadata(root, &metadata)?;
    Ok(metadata)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct TestHome(PathBuf);

    impl TestHome {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("kiralm-cache-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn store(cache: &ArtifactCache, name: &str, bytes: &[u8]) -> CachedArtifact {
        let partial = cache.partial(name).unwrap();
        std::fs::write(partial.path(), bytes).unwrap();
        cache
            .store_verified_partial(partial.path(), &digest(bytes), None)
            .unwrap()
    }

    #[tokio::test]
    async fn cache_hit_skips_fetch() {
        let home = TestHome::new("hit");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let artifact = store(&cache, "hit", b"cached model");
        let called = Arc::new(AtomicBool::new(false));
        let called_for_fetch = called.clone();
        let source = ArtifactSource {
            url: "https://example.invalid/model.gguf".to_string(),
            etag: None,
        };

        let fetched = obtain_artifact(
            &cache,
            "hit",
            Some(&artifact.sha256),
            &source,
            move |_| async move {
                called_for_fetch.store(true, Ordering::SeqCst);
                anyhow::bail!("network fetch should not run")
            },
        )
        .await
        .unwrap();

        assert_eq!(fetched.artifact.sha256, artifact.sha256);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn same_artifact_joins_one_flight() {
        let home = TestHome::new("flight");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let expected = digest(b"same");
        let source = ArtifactSource {
            url: "https://example.invalid/same.gguf".to_string(),
            etag: None,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_calls = calls.clone();
        let second_calls = calls.clone();
        let first = obtain_artifact(
            &cache,
            &expected,
            Some(&expected),
            &source,
            move |path| async move {
                first_calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                std::fs::write(path, b"same")?;
                Ok(digest(b"same"))
            },
        );
        let second = obtain_artifact(
            &cache,
            &expected,
            Some(&expected),
            &source,
            move |path| async move {
                second_calls.fetch_add(1, Ordering::SeqCst);
                std::fs::write(path, b"same")?;
                Ok(digest(b"same"))
            },
        );

        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().artifact.sha256, expected);
        assert_eq!(second.unwrap().artifact.sha256, expected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn materializes_with_hard_link_and_copy_fallback() {
        let home = TestHome::new("materialize");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let artifact = store(&cache, "materialize", b"model bytes");
        let hard_linked = home.0.join("models/hard.gguf");
        let copied = home.0.join("models/copy.gguf");

        assert_eq!(cache.root(), home.0.join("cache/models/sha256"));

        assert_eq!(
            cache.materialize(&artifact, &hard_linked).unwrap(),
            MaterializationMode::HardLink
        );
        assert_eq!(
            cache
                .materialize_with(
                    &artifact,
                    &copied,
                    |_, _| Err(std::io::Error::other("link disabled")),
                    |source, target| std::fs::copy(source, target),
                )
                .unwrap(),
            MaterializationMode::Copy
        );
        assert_eq!(std::fs::read(&hard_linked).unwrap(), b"model bytes");
        assert_eq!(std::fs::read(&copied).unwrap(), b"model bytes");
        assert_eq!(
            cache
                .metadata(&artifact.sha256)
                .unwrap()
                .unwrap()
                .references
                .len(),
            2
        );
        assert!(cache
            .remove_reference(&artifact.sha256, &hard_linked)
            .unwrap());
        assert!(cache.remove_reference(&artifact.sha256, &copied).unwrap());
    }

    #[test]
    fn recovers_metadata_from_verified_artifacts() {
        let home = TestHome::new("recovery");
        let root = home.0.join("cache/models/sha256");
        let sha256 = {
            let cache = ArtifactCache::new(&root).unwrap();
            store(&cache, "recovery", b"recover me").sha256
        };
        std::fs::write(root.join("metadata.json"), b"not json").unwrap();

        let recovered = ArtifactCache::new(&root).unwrap();
        assert!(recovered.lookup(&sha256).unwrap().is_some());
        assert!(std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("metadata.json.corrupt-")
        }));
    }

    #[test]
    fn quota_pruning_respects_exclusions_and_lru_order() {
        let home = TestHome::new("prune");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let old = store(&cache, "old", b"old");
        let active = store(&cache, "active", b"active");
        let hot = store(&cache, "hot", b"hot");
        let downloading = store(&cache, "downloading", b"downloading");
        let referenced = store(&cache, "referenced", b"referenced");
        let recent = store(&cache, "recent", b"recent");
        cache
            .materialize(&referenced, &home.0.join("models/referenced.gguf"))
            .unwrap();
        cache.lookup(&recent.sha256).unwrap();

        let protections = PruneProtections {
            active: HashSet::from([active.sha256.clone()]),
            hot: HashSet::from([hot.sha256.clone()]),
            downloading: HashSet::from([downloading.sha256.clone()]),
        };
        let total: u64 = [&old, &active, &hot, &downloading, &referenced, &recent]
            .iter()
            .map(|artifact| artifact.size_bytes)
            .sum();
        let report = cache
            .prune_to_quota(total - old.size_bytes, &protections)
            .unwrap();

        assert_eq!(report.removed, vec![old.sha256.clone()]);
        assert!(cache.lookup(&old.sha256).unwrap().is_none());
        for artifact in [active, hot, downloading, referenced, recent] {
            assert!(cache.lookup(&artifact.sha256).unwrap().is_some());
        }
    }

    #[test]
    fn quota_can_drop_a_cold_materialized_cache_link() {
        let home = TestHome::new("prune-reference");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let artifact = store(&cache, "referenced", b"model bytes");
        let model = home.0.join("models/model.gguf");
        cache.materialize(&artifact, &model).unwrap();

        let report = cache
            .prune_to_quota(0, &PruneProtections::default())
            .unwrap();

        assert_eq!(report.removed, vec![artifact.sha256]);
        assert_eq!(std::fs::read(model).unwrap(), b"model bytes");
    }

    #[test]
    fn stale_partial_cleanup_keeps_active_downloads() {
        let home = TestHome::new("partials");
        let cache = ArtifactCache::from_home(&home.0).unwrap();
        let active = cache.partial("active").unwrap();
        std::fs::write(active.path(), b"active").unwrap();
        let stale_path = {
            let stale = cache.partial("stale").unwrap();
            std::fs::write(stale.path(), b"stale").unwrap();
            stale.path().to_path_buf()
        };

        let removed = cache.cleanup_stale_partials(Duration::ZERO).unwrap();
        assert_eq!(removed, vec![stale_path]);
        assert!(active.path().exists());
        let active_path = active.path().to_path_buf();
        drop(active);
        assert_eq!(
            cache.cleanup_stale_partials(Duration::ZERO).unwrap(),
            vec![active_path]
        );
    }
}
