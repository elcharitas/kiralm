use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{Disks, System};

pub const DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_MIN_FREE_SPACE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformKind {
    Macos,
    Linux,
    Windows,
    Other,
}

impl PlatformKind {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSource {
    ProductOverride,
    LegacyHome,
    PlatformDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPath {
    pub path: PathBuf,
    pub source: PathSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub data: ManagedPath,
    pub cache: ManagedPath,
    pub runtime: ManagedPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    MissingPathBase(&'static str),
    InvalidPath { path: PathBuf, reason: String },
    InvalidExecutableName(String),
    ProcessSpawn(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPathBase(name) => write!(formatter, "missing path base: {name}"),
            Self::InvalidPath { path, reason } => {
                write!(
                    formatter,
                    "invalid managed path {}: {reason}",
                    path.display()
                )
            }
            Self::InvalidExecutableName(name) => {
                write!(formatter, "invalid executable name: {name}")
            }
            Self::ProcessSpawn(message) => write!(formatter, "probe could not start: {message}"),
        }
    }
}

impl std::error::Error for PlatformError {}

pub fn resolve_app_paths() -> Result<AppPaths, PlatformError> {
    let legacy = std::env::var_os("KIRALM_HOME").or_else(|| {
        let path = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?
            .join(".kiralm");
        path.exists().then(|| path.into_os_string())
    });
    resolve_app_paths_with(PlatformKind::current(), |key| {
        if key == "KIRALM_HOME" {
            legacy.clone()
        } else {
            std::env::var_os(key)
        }
    })
}

fn resolve_app_paths_with<F>(platform: PlatformKind, get: F) -> Result<AppPaths, PlatformError>
where
    F: Fn(&str) -> Option<OsString>,
{
    let legacy_home = get("KIRALM_HOME").map(PathBuf::from);
    let home = match platform {
        PlatformKind::Windows => get("USERPROFILE").or_else(|| get("HOME")),
        _ => get("HOME").or_else(|| get("USERPROFILE")),
    }
    .map(PathBuf::from);

    let data = choose_path(
        get("KIRALM_DATA_DIR"),
        legacy_home.clone(),
        || platform_data_path(platform, &get, home.as_deref()),
        "data",
    )?;
    let cache = choose_path(
        get("KIRALM_CACHE_DIR"),
        legacy_home.as_ref().map(|path| path.join("cache")),
        || platform_cache_path(platform, &get, home.as_deref()),
        "cache",
    )?;
    let runtime = choose_path(
        get("KIRALM_RUNTIME_DIR"),
        legacy_home.as_ref().map(|path| path.join("run")),
        || Ok(platform_runtime_path(platform, &get, &cache.path)),
        "runtime",
    )?;

    let protected = home
        .into_iter()
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    for managed in [&data, &cache, &runtime] {
        validate_managed_path(&managed.path, &protected)?;
    }

    Ok(AppPaths {
        data,
        cache,
        runtime,
    })
}

fn choose_path<F>(
    product_override: Option<OsString>,
    legacy: Option<PathBuf>,
    platform_default: F,
    label: &'static str,
) -> Result<ManagedPath, PlatformError>
where
    F: FnOnce() -> Result<PathBuf, PlatformError>,
{
    let (path, source) = if let Some(path) = product_override {
        (PathBuf::from(path), PathSource::ProductOverride)
    } else if let Some(path) = legacy {
        (path, PathSource::LegacyHome)
    } else {
        let platform_default = platform_default()?;
        if platform_default.as_os_str().is_empty() {
            return Err(PlatformError::MissingPathBase(label));
        }
        (platform_default, PathSource::PlatformDefault)
    };
    Ok(ManagedPath {
        path: normalize_absolute(&path)?,
        source,
    })
}

fn platform_data_path<F>(
    platform: PlatformKind,
    get: &F,
    home: Option<&Path>,
) -> Result<PathBuf, PlatformError>
where
    F: Fn(&str) -> Option<OsString>,
{
    match platform {
        PlatformKind::Macos => required_home(home).map(|path| {
            path.join("Library")
                .join("Application Support")
                .join("KiraLM")
        }),
        PlatformKind::Linux => Ok(get("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or(required_home(home)?.join(".local").join("share"))
            .join("kiralm")),
        PlatformKind::Windows => get("APPDATA")
            .map(PathBuf::from)
            .or_else(|| get("LOCALAPPDATA").map(PathBuf::from))
            .map(|path| path.join("KiraLM"))
            .ok_or(PlatformError::MissingPathBase("APPDATA")),
        PlatformKind::Other => Ok(std::env::temp_dir().join("kiralm").join("data")),
    }
}

fn platform_cache_path<F>(
    platform: PlatformKind,
    get: &F,
    home: Option<&Path>,
) -> Result<PathBuf, PlatformError>
where
    F: Fn(&str) -> Option<OsString>,
{
    match platform {
        PlatformKind::Macos => {
            required_home(home).map(|path| path.join("Library").join("Caches").join("KiraLM"))
        }
        PlatformKind::Linux => Ok(get("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or(required_home(home)?.join(".cache"))
            .join("kiralm")),
        PlatformKind::Windows => get("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("KiraLM").join("Cache"))
            .ok_or(PlatformError::MissingPathBase("LOCALAPPDATA")),
        PlatformKind::Other => Ok(std::env::temp_dir().join("kiralm").join("cache")),
    }
}

fn platform_runtime_path<F>(platform: PlatformKind, get: &F, cache: &Path) -> PathBuf
where
    F: Fn(&str) -> Option<OsString>,
{
    match platform {
        PlatformKind::Linux => get("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .map(|path| path.join("kiralm"))
            .unwrap_or_else(|| cache.join("run")),
        PlatformKind::Windows => get("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("KiraLM").join("Runtime"))
            .unwrap_or_else(|| cache.join("run")),
        _ => cache.join("run"),
    }
}

fn required_home(home: Option<&Path>) -> Result<PathBuf, PlatformError> {
    home.map(Path::to_path_buf)
        .ok_or(PlatformError::MissingPathBase("HOME"))
}

pub fn normalize_absolute(path: &Path) -> Result<PathBuf, PlatformError> {
    if !path.is_absolute() {
        return Err(PlatformError::InvalidPath {
            path: path.to_path_buf(),
            reason: "path must be absolute".to_string(),
        });
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PlatformError::InvalidPath {
                        path: path.to_path_buf(),
                        reason: "path escapes its filesystem root".to_string(),
                    });
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

pub fn validate_managed_path(
    path: &Path,
    protected_roots: &[PathBuf],
) -> Result<PathBuf, PlatformError> {
    let normalized = normalize_absolute(path)?;
    let resolved = resolve_existing_prefix(&normalized)?;
    if is_broad_root(&normalized) || is_broad_root(&resolved) {
        return Err(PlatformError::InvalidPath {
            path: normalized,
            reason: "path is too broad for managed writes".to_string(),
        });
    }

    for protected in protected_roots {
        let protected = normalize_absolute(protected)?;
        let protected = resolve_existing_prefix(&protected)?;
        if resolved == protected {
            return Err(PlatformError::InvalidPath {
                path: normalized,
                reason: "path matches a protected directory".to_string(),
            });
        }
    }
    Ok(resolved)
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, PlatformError> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| PlatformError::InvalidPath {
                path: path.to_path_buf(),
                reason: "path has no existing filesystem root".to_string(),
            })?;
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| PlatformError::InvalidPath {
            path: path.to_path_buf(),
            reason: format!("cannot resolve path: {error}"),
        })?;
    for part in missing.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn is_broad_root(path: &Path) -> bool {
    let normal_components = path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    normal_components < 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    InvalidManagedPath,
    PathNotWritable,
    FreeSpaceUnavailable,
    LowFreeSpace,
    MissingExecutable,
    ProbeFailed,
    ProbeTimedOut,
    InvalidListener,
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: FindingCode,
    pub summary: String,
    pub action: String,
}

impl Finding {
    fn new(
        severity: Severity,
        code: FindingCode,
        summary: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            code,
            summary: summary.into(),
            action: action.into(),
        }
    }
}

pub fn validate_local_listener(value: &str) -> Result<SocketAddr, Finding> {
    let address = value.parse::<SocketAddr>().map_err(|_| {
        Finding::new(
            Severity::Error,
            FindingCode::InvalidListener,
            "listener must be an IP address and port",
            "Use 127.0.0.1:PORT or [::1]:PORT.",
        )
    })?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(Finding::new(
            Severity::Error,
            FindingCode::InvalidListener,
            "listener is not a usable loopback address",
            "Bind to 127.0.0.1 or ::1 with a nonzero port.",
        ));
    }
    Ok(address)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareCapabilities {
    pub platform: PlatformKind,
    pub architecture: String,
    pub logical_cpus: usize,
    pub physical_cpus: Option<usize>,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub supports_unix_sockets: bool,
    pub supports_page_locking: bool,
    pub accelerator_hint: AcceleratorHint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorHint {
    AppleSilicon,
    PlatformDependent,
}

pub fn hardware_capabilities() -> HardwareCapabilities {
    let mut system = System::new_all();
    system.refresh_memory();
    let platform = PlatformKind::current();
    HardwareCapabilities {
        platform,
        architecture: std::env::consts::ARCH.to_string(),
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        physical_cpus: system.physical_core_count(),
        total_memory_bytes: system.total_memory(),
        available_memory_bytes: system.available_memory(),
        supports_unix_sockets: cfg!(unix),
        supports_page_locking: cfg!(unix),
        accelerator_hint: if platform == PlatformKind::Macos && std::env::consts::ARCH == "aarch64"
        {
            AcceleratorHint::AppleSilicon
        } else {
            AcceleratorHint::PlatformDependent
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemDiagnostic {
    pub kind: ManagedPathKind,
    pub path: String,
    pub source: PathSource,
    pub writable: bool,
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedPathKind {
    Data,
    Cache,
    Runtime,
}

fn probe_filesystem(
    kind: ManagedPathKind,
    path: &ManagedPath,
    redactor: &Redactor,
    min_free_bytes: u64,
) -> (FilesystemDiagnostic, Vec<Finding>) {
    let mut findings = Vec::new();
    let writable = check_writable(&path.path).is_ok();
    if !writable {
        findings.push(Finding::new(
            Severity::Error,
            FindingCode::PathNotWritable,
            format!("{} directory is not writable", path_label(kind)),
            "Choose a writable application directory and retry.",
        ));
    }
    let available_bytes = available_space(&path.path);
    match available_bytes {
        Some(available) if available < min_free_bytes => findings.push(Finding::new(
            Severity::Warning,
            FindingCode::LowFreeSpace,
            format!("{} directory has low free space", path_label(kind)),
            "Free disk space or select a different directory.",
        )),
        None => findings.push(Finding::new(
            Severity::Warning,
            FindingCode::FreeSpaceUnavailable,
            format!("{} directory free space is unavailable", path_label(kind)),
            "Check the filesystem mount and permissions.",
        )),
        _ => {}
    }
    (
        FilesystemDiagnostic {
            kind,
            path: redactor.redact(&path.path.to_string_lossy()),
            source: path.source,
            writable,
            available_bytes,
        },
        findings,
    )
}

fn path_label(kind: ManagedPathKind) -> &'static str {
    match kind {
        ManagedPathKind::Data => "data",
        ManagedPathKind::Cache => "cache",
        ManagedPathKind::Runtime => "runtime",
    }
}

fn check_writable(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let probe = path.join(format!(
        ".kiralm-write-check-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&probe)?;
        file.write_all(b"ok")?;
        file.sync_all()
    })();
    let cleanup = fs::remove_file(&probe);
    result.and(cleanup)
}

pub fn available_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|candidate| candidate.exists())?;
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| existing.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
}

#[derive(Debug, Clone)]
pub struct ExecutableSearch {
    pub directories: Vec<PathBuf>,
    pub extensions: Vec<OsString>,
}

impl ExecutableSearch {
    pub fn from_process() -> Self {
        let directories = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        #[cfg(windows)]
        let extensions = std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|part| !part.is_empty())
                    .map(OsString::from)
                    .collect()
            })
            .unwrap_or_else(|| vec![OsString::from(".EXE")]);
        #[cfg(not(windows))]
        let extensions = Vec::new();
        Self {
            directories,
            extensions,
        }
    }
}

pub fn discover_executable(name: &str) -> Result<Option<PathBuf>, PlatformError> {
    discover_executable_with(name, &ExecutableSearch::from_process())
}

pub fn discover_executable_with(
    name: &str,
    search: &ExecutableSearch,
) -> Result<Option<PathBuf>, PlatformError> {
    validate_executable_name(name)?;
    let direct = Path::new(name);
    if direct.is_absolute() {
        return Ok(is_executable(direct).then(|| direct.to_path_buf()));
    }
    for directory in &search.directories {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Ok(Some(candidate));
        }
        if candidate.extension().is_none() {
            for extension in &search.extensions {
                let mut filename = OsString::from(name);
                filename.push(extension);
                let candidate = directory.join(filename);
                if is_executable(&candidate) {
                    return Ok(Some(candidate));
                }
            }
        }
    }
    Ok(None)
}

fn validate_executable_name(name: &str) -> Result<(), PlatformError> {
    let path = Path::new(name);
    if name.is_empty()
        || (!path.is_absolute()
            && !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte)))
    {
        return Err(PlatformError::InvalidExecutableName(name.to_string()));
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(not(any(unix, windows)))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[derive(Debug, Clone, Copy)]
pub struct ProbeLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for ProbeLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
            max_output_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub elapsed: Duration,
}

pub fn probe_command(
    executable: &Path,
    args: &[String],
    limits: ProbeLimits,
) -> Result<ProbeResult, PlatformError> {
    let started = Instant::now();
    let timeout = limits.timeout.min(MAX_PROBE_TIMEOUT);
    let max_output_bytes = limits.max_output_bytes.min(MAX_PROBE_OUTPUT_BYTES);
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let mut child = command
        .spawn()
        .map_err(|error| PlatformError::ProcessSpawn(error.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let budget = Arc::new(Mutex::new(max_output_bytes));
    let truncated = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    spawn_reader(stdout, OutputStream::Stdout, &budget, &truncated, &sender);
    spawn_reader(stderr, OutputStream::Stderr, &budget, &truncated, &sender);
    drop(sender);

    let deadline = started + timeout;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|error| PlatformError::ProcessSpawn(error.to_string()))?;
                break (status, true);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PlatformError::ProcessSpawn(error.to_string()));
            }
        }
    };

    let read_deadline = Instant::now() + Duration::from_millis(100);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while stdout.is_empty() || stderr.is_empty() {
        let Some(remaining) = read_deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(remaining) {
            Ok((OutputStream::Stdout, bytes)) => stdout = bytes,
            Ok((OutputStream::Stderr, bytes)) => stderr = bytes,
            Err(_) => break,
        }
    }

    let (stdout, stderr, decoding_truncated) =
        decode_bounded_output(&stdout, &stderr, max_output_bytes);
    Ok(ProbeResult {
        exit_code: status.code(),
        timed_out,
        stdout,
        stderr,
        output_truncated: truncated.load(Ordering::Relaxed) || decoding_truncated,
        elapsed: started.elapsed(),
    })
}

fn decode_bounded_output(stdout: &[u8], stderr: &[u8], max_bytes: usize) -> (String, String, bool) {
    let mut remaining = max_bytes;
    let (stdout, stdout_truncated) = decode_bounded_stream(stdout, &mut remaining);
    let (stderr, stderr_truncated) = decode_bounded_stream(stderr, &mut remaining);
    (stdout, stderr, stdout_truncated || stderr_truncated)
}

fn decode_bounded_stream(bytes: &[u8], remaining: &mut usize) -> (String, bool) {
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    if value.len() <= *remaining {
        *remaining -= value.len();
        return (value, false);
    }
    let mut boundary = *remaining;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    *remaining = 0;
    (value, true)
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream: OutputStream,
    budget: &Arc<Mutex<usize>>,
    truncated: &Arc<AtomicBool>,
    sender: &mpsc::Sender<(OutputStream, Vec<u8>)>,
) {
    let budget = Arc::clone(budget);
    let truncated = Arc::clone(truncated);
    let sender = sender.clone();
    std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            let keep = {
                let mut remaining = budget.lock().unwrap_or_else(|error| error.into_inner());
                let keep = count.min(*remaining);
                *remaining -= keep;
                keep
            };
            captured.extend_from_slice(&buffer[..keep]);
            if keep < count {
                truncated.store(true, Ordering::Relaxed);
            }
        }
        let _ = sender.send((stream, captured));
    });
}

#[derive(Debug, Clone)]
pub struct Redactor {
    replacements: Vec<(String, String)>,
}

impl Redactor {
    pub fn new(home: Option<&Path>, sensitive_values: &[String]) -> Self {
        let mut replacements = sensitive_values
            .iter()
            .filter(|value| !value.is_empty())
            .map(|value| (value.clone(), "<redacted>".to_string()))
            .collect::<Vec<_>>();
        if let Some(home) = home {
            let home = home.to_string_lossy();
            if !home.is_empty() {
                replacements.push((home.into_owned(), "~".to_string()));
            }
        }
        replacements.sort_by(|left, right| right.0.len().cmp(&left.0.len()));
        replacements.dedup_by(|left, right| left.0 == right.0);
        Self { replacements }
    }

    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        Self::new(home.as_deref(), &[])
    }

    pub fn redact(&self, value: &str) -> String {
        let mut redacted = value.to_string();
        for (needle, replacement) in &self.replacements {
            redacted = redacted.replace(needle, replacement);
        }
        redacted = redact_bearer_tokens(&redacted);
        redacted = redact_sensitive_assignments(&redacted);
        redact_url_userinfo(&redacted)
    }
}

fn redact_bearer_tokens(input: &str) -> String {
    redact_after_marker(input, "bearer ", |byte| {
        byte.is_ascii_whitespace() || byte == b',' || byte == b'"'
    })
}

fn redact_sensitive_assignments(input: &str) -> String {
    const KEYS: [&str; 7] = [
        "authorization",
        "password",
        "api_key",
        "apikey",
        "credential",
        "secret",
        "token",
    ];
    let mut value = input.to_string();
    for key in KEYS {
        value = redact_assignment_for_key(&value, key);
    }
    value
}

fn redact_assignment_for_key(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(key) {
        let key_start = cursor + relative;
        let key_end = key_start + key.len();
        if key_start > 0 && lower.as_bytes()[key_start - 1].is_ascii_alphanumeric() {
            output.push_str(&input[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let mut value_start = key_end;
        while value_start < input.len()
            && matches!(
                input.as_bytes()[value_start],
                b' ' | b'\t' | b'"' | b'\'' | b':' | b'='
            )
        {
            value_start += 1;
        }
        if value_start == key_end {
            output.push_str(&input[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let quote = input.as_bytes().get(value_start.wrapping_sub(1)).copied();
        let value_end = input.as_bytes()[value_start..]
            .iter()
            .position(|byte| match quote {
                Some(b'"') => *byte == b'"',
                Some(b'\'') => *byte == b'\'',
                _ => byte.is_ascii_whitespace() || matches!(*byte, b',' | b'&' | b'}'),
            })
            .map(|offset| value_start + offset)
            .unwrap_or(input.len());
        output.push_str(&input[cursor..value_start]);
        output.push_str("<redacted>");
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn redact_after_marker<F>(input: &str, marker: &str, stop: F) -> String
where
    F: Fn(u8) -> bool,
{
    let lower = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(marker) {
        let start = cursor + relative;
        let value_start = start + marker.len();
        let value_end = input.as_bytes()[value_start..]
            .iter()
            .position(|byte| stop(*byte))
            .map(|offset| value_start + offset)
            .unwrap_or(input.len());
        output.push_str(&input[cursor..value_start]);
        output.push_str("<redacted>");
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn redact_url_userinfo(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = input[cursor..].find("://") {
        let authority_start = cursor + relative + 3;
        let authority_end = input[authority_start..]
            .find(['/', ' ', '\n'])
            .map(|offset| authority_start + offset)
            .unwrap_or(input.len());
        let authority = &input[authority_start..authority_end];
        let Some(at) = authority.rfind('@') else {
            output.push_str(&input[cursor..authority_start]);
            cursor = authority_start;
            continue;
        };
        if !authority[..at].contains(':') {
            output.push_str(&input[cursor..authority_start]);
            cursor = authority_start;
            continue;
        }
        output.push_str(&input[cursor..authority_start]);
        output.push_str("<redacted>@");
        output.push_str(&authority[at + 1..]);
        cursor = authority_end;
    }
    output.push_str(&input[cursor..]);
    output
}

#[derive(Debug, Clone)]
pub struct ExecutableRequirement {
    pub name: String,
    pub required: bool,
    pub probe_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PreflightOptions {
    pub listener: String,
    pub required_executables: Vec<ExecutableRequirement>,
    pub min_free_space_bytes: u64,
    pub probe_limits: ProbeLimits,
}

impl Default for PreflightOptions {
    fn default() -> Self {
        Self {
            listener: "127.0.0.1:11535".to_string(),
            required_executables: Vec::new(),
            min_free_space_bytes: DEFAULT_MIN_FREE_SPACE_BYTES,
            probe_limits: ProbeLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableDiagnostic {
    pub name: String,
    pub required: bool,
    pub found: bool,
    pub path: Option<String>,
    pub version_excerpt: Option<String>,
    pub probe_exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub hardware: HardwareCapabilities,
    pub listener: Option<String>,
    pub paths: Vec<FilesystemDiagnostic>,
    pub executables: Vec<ExecutableDiagnostic>,
    pub findings: Vec<Finding>,
}

pub fn run_preflight(options: &PreflightOptions) -> DiagnosticReport {
    let redactor = Redactor::from_process();
    let mut findings = Vec::new();
    let hardware = hardware_capabilities();
    if hardware.platform == PlatformKind::Other {
        findings.push(Finding::new(
            Severity::Warning,
            FindingCode::UnsupportedPlatform,
            "platform support is not verified",
            "Use macOS, Linux, or Windows for production workloads.",
        ));
    }

    let listener = match validate_local_listener(&options.listener) {
        Ok(address) => Some(address.to_string()),
        Err(finding) => {
            findings.push(finding);
            None
        }
    };

    let mut paths = Vec::new();
    match resolve_app_paths() {
        Ok(resolved) => {
            for (kind, managed) in [
                (ManagedPathKind::Data, resolved.data),
                (ManagedPathKind::Cache, resolved.cache),
                (ManagedPathKind::Runtime, resolved.runtime),
            ] {
                let (diagnostic, path_findings) =
                    probe_filesystem(kind, &managed, &redactor, options.min_free_space_bytes);
                paths.push(diagnostic);
                findings.extend(path_findings);
            }
        }
        Err(error) => findings.push(Finding::new(
            Severity::Error,
            FindingCode::InvalidManagedPath,
            redactor.redact(&error.to_string()),
            "Set KIRALM path overrides to safe absolute directories.",
        )),
    }

    let search = ExecutableSearch::from_process();
    let mut executables = Vec::new();
    for requirement in &options.required_executables {
        let found = discover_executable_with(&requirement.name, &search)
            .ok()
            .flatten();
        let Some(path) = found else {
            findings.push(Finding::new(
                if requirement.required {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                FindingCode::MissingExecutable,
                format!("{} executable was not found", requirement.name),
                format!("Install {} or configure its full path.", requirement.name),
            ));
            executables.push(ExecutableDiagnostic {
                name: requirement.name.clone(),
                required: requirement.required,
                found: false,
                path: None,
                version_excerpt: None,
                probe_exit_code: None,
            });
            continue;
        };

        let probe = probe_command(&path, &requirement.probe_args, options.probe_limits);
        let (version_excerpt, probe_exit_code) = match probe {
            Ok(result) => {
                if result.timed_out {
                    findings.push(Finding::new(
                        Severity::Warning,
                        FindingCode::ProbeTimedOut,
                        format!("{} probe timed out", requirement.name),
                        "Check the executable or increase the probe timeout.",
                    ));
                } else if result.exit_code != Some(0) {
                    findings.push(Finding::new(
                        Severity::Warning,
                        FindingCode::ProbeFailed,
                        format!("{} probe returned an error", requirement.name),
                        "Run the executable manually and verify its installation.",
                    ));
                }
                let excerpt = first_nonempty_line(&result.stdout)
                    .or_else(|| first_nonempty_line(&result.stderr))
                    .map(|line| redactor.redact(line));
                (excerpt, result.exit_code)
            }
            Err(_) => {
                findings.push(Finding::new(
                    Severity::Warning,
                    FindingCode::ProbeFailed,
                    format!("{} probe could not start", requirement.name),
                    "Verify the executable permissions and installation.",
                ));
                (None, None)
            }
        };
        executables.push(ExecutableDiagnostic {
            name: requirement.name.clone(),
            required: requirement.required,
            found: true,
            path: Some(redactor.redact(&path.to_string_lossy())),
            version_excerpt,
            probe_exit_code,
        });
    }

    DiagnosticReport {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        hardware,
        listener,
        paths,
        executables,
        findings,
    }
}

fn first_nonempty_line(value: &str) -> Option<&str> {
    value
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(value)))
            .collect::<BTreeMap<_, _>>();
        move |key| values.get(key).cloned()
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiralm-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn path_overrides_take_precedence_and_normalize() {
        let paths = resolve_app_paths_with(
            PlatformKind::Linux,
            env(&[
                ("KIRALM_DATA_DIR", "/srv/app/./data"),
                ("KIRALM_CACHE_DIR", "/srv/app/tmp/../cache"),
                ("KIRALM_RUNTIME_DIR", "/srv/app/run"),
                ("XDG_DATA_HOME", "/xdg/data"),
            ]),
        )
        .unwrap();

        assert_eq!(paths.data.path, Path::new("/srv/app/data"));
        assert_eq!(paths.cache.path, Path::new("/srv/app/cache"));
        assert_eq!(paths.runtime.path, Path::new("/srv/app/run"));
        assert_eq!(paths.data.source, PathSource::ProductOverride);
        assert_eq!(paths.cache.source, PathSource::ProductOverride);
    }

    #[test]
    fn legacy_home_precedes_platform_defaults() {
        let paths = resolve_app_paths_with(
            PlatformKind::Linux,
            env(&[
                ("HOME", "/home/tester"),
                ("KIRALM_HOME", "/srv/kiralm/home"),
                ("XDG_DATA_HOME", "/xdg/data"),
                ("XDG_CACHE_HOME", "/xdg/cache"),
            ]),
        )
        .unwrap();

        assert_eq!(paths.data.path, Path::new("/srv/kiralm/home"));
        assert_eq!(paths.cache.path, Path::new("/srv/kiralm/home/cache"));
        assert_eq!(paths.runtime.path, Path::new("/srv/kiralm/home/run"));
        assert_eq!(paths.runtime.source, PathSource::LegacyHome);
    }

    #[test]
    fn linux_defaults_follow_xdg_then_home() {
        let paths = resolve_app_paths_with(
            PlatformKind::Linux,
            env(&[
                ("HOME", "/home/tester"),
                ("XDG_DATA_HOME", "/var/lib/tester"),
                ("XDG_CACHE_HOME", "/var/cache/tester"),
                ("XDG_RUNTIME_DIR", "/var/run/tester"),
            ]),
        )
        .unwrap();

        assert_eq!(paths.data.path, Path::new("/var/lib/tester/kiralm"));
        assert_eq!(paths.cache.path, Path::new("/var/cache/tester/kiralm"));
        assert_eq!(paths.runtime.path, Path::new("/var/run/tester/kiralm"));
        assert_eq!(paths.data.source, PathSource::PlatformDefault);
    }

    #[test]
    fn macos_defaults_use_standard_application_directories() {
        let paths =
            resolve_app_paths_with(PlatformKind::Macos, env(&[("HOME", "/Users/tester")])).unwrap();

        assert_eq!(
            paths.data.path,
            Path::new("/Users/tester/Library/Application Support/KiraLM")
        );
        assert_eq!(
            paths.cache.path,
            Path::new("/Users/tester/Library/Caches/KiraLM")
        );
        assert_eq!(
            paths.runtime.path,
            Path::new("/Users/tester/Library/Caches/KiraLM/run")
        );
    }

    #[test]
    fn broad_and_protected_paths_are_refused() {
        let root = Path::new("/");
        assert!(validate_managed_path(root, &[]).is_err());
        assert!(validate_managed_path(Path::new("/tmp"), &[]).is_err());
        assert!(
            validate_managed_path(Path::new("/home/tester"), &[PathBuf::from("/home/tester")])
                .is_err()
        );
        assert!(validate_managed_path(Path::new("relative/path"), &[]).is_err());
    }

    #[test]
    fn symlink_to_broad_path_is_refused() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = temp_path("symlink");
            fs::create_dir_all(&root).unwrap();
            let link = root.join("target");
            symlink("/", &link).unwrap();
            assert!(validate_managed_path(&link, &[]).is_err());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn listener_accepts_only_loopback_addresses() {
        assert_eq!(
            validate_local_listener("127.0.0.1:11535").unwrap(),
            "127.0.0.1:11535".parse().unwrap()
        );
        assert!(validate_local_listener("0.0.0.0:11535").is_err());
        assert!(validate_local_listener("localhost:11535").is_err());
        assert!(validate_local_listener("[::1]:0").is_err());
    }

    #[test]
    fn executable_discovery_reports_missing_and_rejects_fragments() {
        let search = ExecutableSearch {
            directories: vec![temp_path("empty-path")],
            extensions: Vec::new(),
        };
        assert_eq!(
            discover_executable_with("definitely-missing-kiralm", &search).unwrap(),
            None
        );
        assert!(discover_executable_with("git; echo leaked", &search).is_err());
        assert!(discover_executable_with("../git", &search).is_err());
    }

    #[test]
    fn redaction_removes_credentials_and_user_paths() {
        let redactor = Redactor::new(
            Some(Path::new("/Users/jonathan")),
            &["exact-secret".to_string()],
        );
        let input = concat!(
            "/Users/jonathan/.kiralm token=abc123 ",
            "Authorization: Bearer xyz password='open-sesame' ",
            "https://name:pass@example.com exact-secret"
        );
        let output = redactor.redact(input);

        for secret in [
            "/Users/jonathan",
            "abc123",
            "xyz",
            "open-sesame",
            "name:pass",
            "exact-secret",
        ] {
            assert!(!output.contains(secret), "secret remained: {secret}");
        }
        assert!(output.contains("~/.kiralm"));
        assert!(output.contains("<redacted>"));
    }

    #[test]
    fn writable_probe_cleans_up_after_itself() {
        let path = temp_path("writable");
        check_writable(&path).unwrap();
        assert!(fs::read_dir(&path).unwrap().next().is_none());
        fs::remove_dir(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_probe_bounds_time_and_combined_output() {
        let executable = discover_executable("yes").unwrap().unwrap();
        let started = Instant::now();
        let result = probe_command(
            &executable,
            &[],
            ProbeLimits {
                timeout: Duration::from_millis(50),
                max_output_bytes: 64,
            },
        )
        .unwrap();

        assert!(result.timed_out);
        assert!(result.output_truncated);
        assert!(result.stdout.len() + result.stderr.len() <= 64);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn subprocess_probe_reports_missing_binary_without_waiting() {
        let started = Instant::now();
        let error = probe_command(
            Path::new("/path/that/does/not/exist/kiralm-probe"),
            &[],
            ProbeLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, PlatformError::ProcessSpawn(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn output_decoding_preserves_the_combined_byte_ceiling() {
        let (stdout, stderr, truncated) = decode_bounded_output(&[0xff; 32], &[0xfe; 32], 64);

        assert!(truncated);
        assert!(stdout.len() + stderr.len() <= 64);
        assert!(stdout.is_char_boundary(stdout.len()));
        assert!(stderr.is_char_boundary(stderr.len()));
    }

    #[test]
    fn report_serialization_is_stable_and_redacted() {
        let report = DiagnosticReport {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            hardware: HardwareCapabilities {
                platform: PlatformKind::Linux,
                architecture: "x86_64".to_string(),
                logical_cpus: 8,
                physical_cpus: Some(4),
                total_memory_bytes: 16,
                available_memory_bytes: 8,
                supports_unix_sockets: true,
                supports_page_locking: true,
                accelerator_hint: AcceleratorHint::PlatformDependent,
            },
            listener: Some("127.0.0.1:11535".to_string()),
            paths: vec![FilesystemDiagnostic {
                kind: ManagedPathKind::Data,
                path: "~/.kiralm".to_string(),
                source: PathSource::LegacyHome,
                writable: true,
                available_bytes: Some(42),
            }],
            executables: vec![],
            findings: vec![Finding::new(
                Severity::Info,
                FindingCode::UnsupportedPlatform,
                "sample",
                "sample action",
            )],
        };
        let value = serde_json::to_string(&report).unwrap();

        assert_eq!(
            value,
            "{\"schema_version\":1,\"hardware\":{\"platform\":\"linux\",\
             \"architecture\":\"x86_64\",\"logical_cpus\":8,\
             \"physical_cpus\":4,\"total_memory_bytes\":16,\
             \"available_memory_bytes\":8,\"supports_unix_sockets\":true,\
             \"supports_page_locking\":true,\
             \"accelerator_hint\":\"platform_dependent\"},\
             \"listener\":\"127.0.0.1:11535\",\"paths\":[{\"kind\":\"data\",\
             \"path\":\"~/.kiralm\",\"source\":\"legacy_home\",\
             \"writable\":true,\"available_bytes\":42}],\"executables\":[],\
             \"findings\":[{\"severity\":\"info\",\
             \"code\":\"unsupported_platform\",\"summary\":\"sample\",\
             \"action\":\"sample action\"}]}"
        );
    }
}
