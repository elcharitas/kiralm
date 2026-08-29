#![allow(dead_code)]

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_DIFF_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_PATHS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_AUDIT_RECORDS: usize = 1000;
const MAX_CONCURRENCY: usize = 16;

#[derive(Debug, Clone)]
pub struct GitWorkflowLimits {
    pub command_timeout: Duration,
    pub max_command_output_bytes: usize,
    pub max_diff_output_bytes: usize,
    pub max_paths_per_operation: usize,
    pub max_path_bytes: usize,
    pub max_branch_bytes: usize,
    pub max_commit_message_bytes: usize,
    pub max_concurrent_operations: usize,
    pub audit_capacity: usize,
}

impl Default for GitWorkflowLimits {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(20),
            max_command_output_bytes: 256 * 1024,
            max_diff_output_bytes: 512 * 1024,
            max_paths_per_operation: 64,
            max_path_bytes: 1024,
            max_branch_bytes: MAX_BRANCH_BYTES,
            max_commit_message_bytes: MAX_MESSAGE_BYTES,
            max_concurrent_operations: 4,
            audit_capacity: 250,
        }
    }
}

impl GitWorkflowLimits {
    fn validate(&self) -> Result<(), GitWorkflowError> {
        let valid = !self.command_timeout.is_zero()
            && self.command_timeout <= MAX_TIMEOUT
            && (1..=MAX_COMMAND_OUTPUT).contains(&self.max_command_output_bytes)
            && (1..=MAX_DIFF_OUTPUT).contains(&self.max_diff_output_bytes)
            && (1..=MAX_PATHS).contains(&self.max_paths_per_operation)
            && (1..=MAX_PATH_BYTES).contains(&self.max_path_bytes)
            && (1..=MAX_BRANCH_BYTES).contains(&self.max_branch_bytes)
            && (1..=MAX_MESSAGE_BYTES).contains(&self.max_commit_message_bytes)
            && (1..=MAX_CONCURRENCY).contains(&self.max_concurrent_operations)
            && (1..=MAX_AUDIT_RECORDS).contains(&self.audit_capacity);
        if valid {
            Ok(())
        } else {
            Err(GitWorkflowError::InvalidConfiguration)
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum GitWorkflowError {
    InvalidConfiguration,
    NotRepository,
    RepositoryChanged,
    InvalidPath {
        message: String,
    },
    PathEscape {
        path: String,
    },
    SymlinkEscape {
        path: String,
    },
    InvalidBranch {
        message: String,
    },
    BranchExists {
        branch: String,
    },
    BranchNotFound {
        branch: String,
    },
    InvalidCommitMessage {
        message: String,
    },
    DirtyWorktree,
    Conflicts {
        paths: Vec<String>,
    },
    NothingToCommit,
    Busy,
    TimedOut,
    OutputLimitExceeded,
    Io {
        operation: String,
        message: String,
    },
    Git {
        operation: String,
        exit_code: Option<i32>,
        message: String,
    },
}

impl GitWorkflowError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::NotRepository => "not_repository",
            Self::RepositoryChanged => "repository_changed",
            Self::InvalidPath { .. } => "invalid_path",
            Self::PathEscape { .. } => "path_escape",
            Self::SymlinkEscape { .. } => "symlink_escape",
            Self::InvalidBranch { .. } => "invalid_branch",
            Self::BranchExists { .. } => "branch_exists",
            Self::BranchNotFound { .. } => "branch_not_found",
            Self::InvalidCommitMessage { .. } => "invalid_commit_message",
            Self::DirtyWorktree => "dirty_worktree",
            Self::Conflicts { .. } => "conflicts",
            Self::NothingToCommit => "nothing_to_commit",
            Self::Busy => "busy",
            Self::TimedOut => "timed_out",
            Self::OutputLimitExceeded => "output_limit_exceeded",
            Self::Io { .. } => "io",
            Self::Git { .. } => "git",
        }
    }
}

impl fmt::Display for GitWorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => write!(formatter, "invalid git workflow limits"),
            Self::NotRepository => write!(formatter, "path is not a git repository root"),
            Self::RepositoryChanged => write!(formatter, "repository identity changed"),
            Self::InvalidPath { message } => write!(formatter, "invalid path: {message}"),
            Self::PathEscape { path } => write!(formatter, "path escapes repository: {path}"),
            Self::SymlinkEscape { path } => {
                write!(formatter, "symlink escapes repository: {path}")
            }
            Self::InvalidBranch { message } => write!(formatter, "invalid branch: {message}"),
            Self::BranchExists { branch } => write!(formatter, "branch already exists: {branch}"),
            Self::BranchNotFound { branch } => write!(formatter, "branch not found: {branch}"),
            Self::InvalidCommitMessage { message } => {
                write!(formatter, "invalid commit message: {message}")
            }
            Self::DirtyWorktree => write!(formatter, "working tree has uncommitted changes"),
            Self::Conflicts { paths } => {
                write!(
                    formatter,
                    "repository has conflicts in {} path(s)",
                    paths.len()
                )
            }
            Self::NothingToCommit => write!(formatter, "nothing staged to commit"),
            Self::Busy => write!(formatter, "git workflow concurrency limit reached"),
            Self::TimedOut => write!(formatter, "git operation timed out"),
            Self::OutputLimitExceeded => write!(formatter, "git output limit exceeded"),
            Self::Io { operation, message } => write!(formatter, "{operation}: {message}"),
            Self::Git {
                operation,
                exit_code,
                message,
            } => write!(
                formatter,
                "{operation} failed with status {}: {message}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
        }
    }
}

impl std::error::Error for GitWorkflowError {}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    RepositoryState,
    DiffPreview,
    ListBranches,
    CreateBranch,
    SwitchBranch,
    StagePaths,
    UnstagePaths,
    Commit,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Succeeded,
    Truncated,
    Rejected,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitAuditRecord {
    pub sequence: u64,
    pub id: String,
    pub operation: GitOperation,
    pub repository: String,
    pub subject: Option<String>,
    pub paths: Vec<String>,
    pub started_at: String,
    pub finished_at: String,
    pub elapsed_ms: u64,
    pub status: AuditStatus,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkingTreeEntry {
    pub path: String,
    pub original_path: Option<String>,
    pub index_status: Option<String>,
    pub worktree_status: Option<String>,
    pub conflicted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepositoryState {
    pub repository: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: bool,
    pub conflicted: bool,
    pub files: Vec<WorkingTreeEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub current: bool,
    pub commit: String,
    pub upstream: Option<String>,
    pub tracking: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    All,
    Staged,
    Unstaged,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiffPreview {
    pub scope: DiffScope,
    pub text: String,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CommitResult {
    pub hash: String,
    pub short_hash: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn read(path: &Path) -> Result<Self, GitWorkflowError> {
        let metadata = fs::metadata(path).map_err(|error| GitWorkflowError::Io {
            operation: "inspect repository".to_string(),
            message: sanitize_message(&error.to_string(), 512),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(Self {})
        }
    }
}

#[derive(Debug)]
pub struct GitWorkflowService {
    root: PathBuf,
    git_dir: PathBuf,
    root_identity: FileIdentity,
    git_dir_identity: FileIdentity,
    limits: GitWorkflowLimits,
    permits: Arc<Semaphore>,
    audit_sequence: AtomicU64,
    audit: Mutex<VecDeque<GitAuditRecord>>,
}

impl GitWorkflowService {
    pub async fn open(
        root: impl AsRef<Path>,
        limits: GitWorkflowLimits,
    ) -> Result<Self, GitWorkflowError> {
        limits.validate()?;
        let root = fs::canonicalize(root.as_ref()).map_err(|error| GitWorkflowError::Io {
            operation: "open repository".to_string(),
            message: sanitize_message(&error.to_string(), 512),
        })?;
        if !root.is_dir() {
            return Err(GitWorkflowError::NotRepository);
        }

        let top_level = run_unscoped(&root, &["rev-parse", "--show-toplevel"], &limits).await?;
        if !top_level.status.success() {
            return Err(GitWorkflowError::NotRepository);
        }
        let discovered_root = canonical_output_path(&top_level.stdout)?;
        if discovered_root != root {
            return Err(GitWorkflowError::NotRepository);
        }

        let git_dir = run_unscoped(&root, &["rev-parse", "--absolute-git-dir"], &limits).await?;
        if !git_dir.status.success() {
            return Err(GitWorkflowError::NotRepository);
        }
        let git_dir = canonical_output_path(&git_dir.stdout)?;
        let root_identity = FileIdentity::read(&root)?;
        let git_dir_identity = FileIdentity::read(&git_dir)?;

        Ok(Self {
            root,
            git_dir,
            root_identity,
            git_dir_identity,
            permits: Arc::new(Semaphore::new(limits.max_concurrent_operations)),
            limits,
            audit_sequence: AtomicU64::new(0),
            audit: Mutex::new(VecDeque::new()),
        })
    }

    pub fn repository_root(&self) -> &Path {
        &self.root
    }

    pub fn audit_records(&self, after_sequence: u64, limit: usize) -> Vec<GitAuditRecord> {
        let limit = limit.clamp(1, 100);
        self.audit
            .lock()
            .iter()
            .filter(|record| record.sequence > after_sequence)
            .take(limit)
            .cloned()
            .collect()
    }

    pub async fn repository_state(&self) -> Result<RepositoryState, GitWorkflowError> {
        self.audited(
            GitOperation::RepositoryState,
            None,
            Vec::new(),
            self.repository_state_inner(),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn diff_preview(
        &self,
        scope: DiffScope,
        paths: &[String],
    ) -> Result<DiffPreview, GitWorkflowError> {
        let audit_paths = bounded_audit_paths(paths);
        self.audited(
            GitOperation::DiffPreview,
            Some(format!("{scope:?}").to_lowercase()),
            audit_paths,
            self.diff_preview_inner(scope, paths),
            |preview| {
                if preview.truncated {
                    AuditStatus::Truncated
                } else {
                    AuditStatus::Succeeded
                }
            },
        )
        .await
    }

    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>, GitWorkflowError> {
        self.audited(
            GitOperation::ListBranches,
            None,
            Vec::new(),
            self.list_branches_inner(),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn create_branch(&self, name: &str) -> Result<BranchInfo, GitWorkflowError> {
        let subject = Some(sanitize_message(name, 256));
        self.audited(
            GitOperation::CreateBranch,
            subject,
            Vec::new(),
            self.create_branch_inner(name),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn switch_branch(&self, name: &str) -> Result<BranchInfo, GitWorkflowError> {
        let subject = Some(sanitize_message(name, 256));
        self.audited(
            GitOperation::SwitchBranch,
            subject,
            Vec::new(),
            self.switch_branch_inner(name),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn stage_paths(&self, paths: &[String]) -> Result<RepositoryState, GitWorkflowError> {
        self.audited(
            GitOperation::StagePaths,
            None,
            bounded_audit_paths(paths),
            self.stage_paths_inner(paths),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn unstage_paths(
        &self,
        paths: &[String],
    ) -> Result<RepositoryState, GitWorkflowError> {
        self.audited(
            GitOperation::UnstagePaths,
            None,
            bounded_audit_paths(paths),
            self.unstage_paths_inner(paths),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    pub async fn commit(&self, message: &str) -> Result<CommitResult, GitWorkflowError> {
        let subject = message
            .lines()
            .next()
            .map(|line| sanitize_message(line, 160));
        self.audited(
            GitOperation::Commit,
            subject,
            Vec::new(),
            self.commit_inner(message),
            |_| AuditStatus::Succeeded,
        )
        .await
    }

    async fn audited<T, F, C>(
        &self,
        operation: GitOperation,
        subject: Option<String>,
        paths: Vec<String>,
        future: F,
        classify: C,
    ) -> Result<T, GitWorkflowError>
    where
        F: Future<Output = Result<T, GitWorkflowError>>,
        C: FnOnce(&T) -> AuditStatus,
    {
        let started_at = Utc::now();
        let started = Instant::now();
        let permit = match self.acquire_permit() {
            Ok(permit) => permit,
            Err(error) => {
                self.record_audit(
                    operation,
                    subject,
                    paths,
                    started_at.to_rfc3339(),
                    started.elapsed(),
                    AuditStatus::Rejected,
                    Some(error.code().to_string()),
                );
                return Err(error);
            }
        };
        let result = future.await;
        drop(permit);
        let status = match &result {
            Ok(value) => classify(value),
            Err(GitWorkflowError::TimedOut) => AuditStatus::TimedOut,
            Err(error) if is_rejection(error) => AuditStatus::Rejected,
            Err(_) => AuditStatus::Failed,
        };
        let error_code = result.as_ref().err().map(|error| error.code().to_string());
        self.record_audit(
            operation,
            subject,
            paths,
            started_at.to_rfc3339(),
            started.elapsed(),
            status,
            error_code,
        );
        result
    }

    fn acquire_permit(&self) -> Result<OwnedSemaphorePermit, GitWorkflowError> {
        self.permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| GitWorkflowError::Busy)
    }

    fn record_audit(
        &self,
        operation: GitOperation,
        subject: Option<String>,
        paths: Vec<String>,
        started_at: String,
        elapsed: Duration,
        status: AuditStatus,
        error_code: Option<String>,
    ) {
        let sequence = self.audit_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let record = GitAuditRecord {
            sequence,
            id: uuid::Uuid::new_v4().to_string(),
            operation,
            repository: self.root.to_string_lossy().into_owned(),
            subject,
            paths,
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            elapsed_ms: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            status,
            error_code,
        };
        let mut audit = self.audit.lock();
        audit.push_back(record);
        while audit.len() > self.limits.audit_capacity {
            audit.pop_front();
        }
    }

    async fn repository_state_inner(&self) -> Result<RepositoryState, GitWorkflowError> {
        self.verify_repository_identity()?;
        let branch = self
            .read_optional_ref(
                "read branch",
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
            )
            .await?;
        let head = self
            .read_optional_ref("read head", &["rev-parse", "--verify", "--quiet", "HEAD"])
            .await?;
        let status = self
            .run_success(
                "read status",
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            )
            .await?;
        let files = parse_porcelain_status(&status.stdout);
        Ok(RepositoryState {
            repository: self.root.to_string_lossy().into_owned(),
            branch,
            head,
            dirty: !files.is_empty(),
            conflicted: files.iter().any(|file| file.conflicted),
            files,
        })
    }

    async fn diff_preview_inner(
        &self,
        scope: DiffScope,
        paths: &[String],
    ) -> Result<DiffPreview, GitWorkflowError> {
        self.verify_repository_identity()?;
        let paths = self.validate_diff_paths(paths)?;
        let mut text = String::new();
        let mut truncated: bool;
        match scope {
            DiffScope::Staged => {
                let output = self
                    .run_diff(true, &paths, self.limits.max_diff_output_bytes)
                    .await?;
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                truncated = output.truncated;
            }
            DiffScope::Unstaged => {
                let output = self
                    .run_diff(false, &paths, self.limits.max_diff_output_bytes)
                    .await?;
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                truncated = output.truncated;
            }
            DiffScope::All => {
                let has_head = self.has_head().await?;
                if has_head {
                    let output = self
                        .run_diff_from_head(&paths, self.limits.max_diff_output_bytes)
                        .await?;
                    text.push_str(&String::from_utf8_lossy(&output.stdout));
                    truncated = output.truncated;
                } else {
                    let staged = self
                        .run_diff(true, &paths, self.limits.max_diff_output_bytes)
                        .await?;
                    text.push_str(&String::from_utf8_lossy(&staged.stdout));
                    truncated = staged.truncated;
                    if !truncated && text.len() < self.limits.max_diff_output_bytes {
                        let remaining = self.limits.max_diff_output_bytes - text.len();
                        let unstaged = self.run_diff(false, &paths, remaining).await?;
                        text.push_str(&String::from_utf8_lossy(&unstaged.stdout));
                        truncated = unstaged.truncated;
                    }
                }
            }
        }
        if text.len() > self.limits.max_diff_output_bytes {
            text.truncate(self.limits.max_diff_output_bytes);
            truncated = true;
        }
        Ok(DiffPreview {
            scope,
            bytes: text.len(),
            text,
            truncated,
        })
    }

    async fn list_branches_inner(&self) -> Result<Vec<BranchInfo>, GitWorkflowError> {
        self.verify_repository_identity()?;
        let output = self
            .run_success(
                "list branches",
                &[
                    "for-each-ref",
                    "--sort=refname",
                    "--format=%(refname:short)%00%(HEAD)%00%(objectname)%00%(upstream:short)%00%(upstream:track)",
                    "refs/heads",
                ],
            )
            .await?;
        Ok(parse_branches(&output.stdout))
    }

    async fn create_branch_inner(&self, name: &str) -> Result<BranchInfo, GitWorkflowError> {
        self.verify_repository_identity()?;
        let name = self.validate_branch(name).await?;
        if self.branch_exists(&name).await? {
            return Err(GitWorkflowError::BranchExists { branch: name });
        }
        let output = self
            .run_owned(
                vec!["branch".to_string(), "--".to_string(), name.clone()],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(map_git_failure("create branch", output, Some(&name)));
        }
        self.branch_info(&name).await
    }

    async fn switch_branch_inner(&self, name: &str) -> Result<BranchInfo, GitWorkflowError> {
        self.verify_repository_identity()?;
        let name = self.validate_branch(name).await?;
        if !self.branch_exists(&name).await? {
            return Err(GitWorkflowError::BranchNotFound { branch: name });
        }
        let state = self.repository_state_inner().await?;
        if state.conflicted {
            return Err(GitWorkflowError::Conflicts {
                paths: conflict_paths(&state),
            });
        }
        if state.branch.as_deref() == Some(name.as_str()) {
            return self.branch_info(&name).await;
        }
        if state.dirty {
            return Err(GitWorkflowError::DirtyWorktree);
        }
        let output = self
            .run_owned(
                vec!["switch".to_string(), "--".to_string(), name.clone()],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(map_git_failure("switch branch", output, Some(&name)));
        }
        self.branch_info(&name).await
    }

    async fn stage_paths_inner(
        &self,
        paths: &[String],
    ) -> Result<RepositoryState, GitWorkflowError> {
        self.verify_repository_identity()?;
        let paths = self.validate_paths(paths)?;
        let mut args = vec!["add".to_string(), "--".to_string()];
        args.extend(paths);
        let output = self
            .run_owned(
                args,
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(map_git_failure("stage paths", output, None));
        }
        self.repository_state_inner().await
    }

    async fn unstage_paths_inner(
        &self,
        paths: &[String],
    ) -> Result<RepositoryState, GitWorkflowError> {
        self.verify_repository_identity()?;
        let paths = self.validate_paths(paths)?;
        let mut args = if self.has_head().await? {
            vec![
                "restore".to_string(),
                "--staged".to_string(),
                "--".to_string(),
            ]
        } else {
            vec![
                "rm".to_string(),
                "--cached".to_string(),
                "--ignore-unmatch".to_string(),
                "--".to_string(),
            ]
        };
        args.extend(paths);
        let output = self
            .run_owned(
                args,
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(map_git_failure("unstage paths", output, None));
        }
        self.repository_state_inner().await
    }

    async fn commit_inner(&self, message: &str) -> Result<CommitResult, GitWorkflowError> {
        self.verify_repository_identity()?;
        let message = self.validate_commit_message(message)?;
        let state = self.repository_state_inner().await?;
        if state.conflicted {
            return Err(GitWorkflowError::Conflicts {
                paths: conflict_paths(&state),
            });
        }
        let staged = self
            .run_owned(
                vec![
                    "diff".to_string(),
                    "--cached".to_string(),
                    "--quiet".to_string(),
                    "--exit-code".to_string(),
                ],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        match staged.status.code() {
            Some(0) => return Err(GitWorkflowError::NothingToCommit),
            Some(1) => {}
            _ => return Err(map_git_failure("inspect staged changes", staged, None)),
        }

        let output = self
            .run_owned(
                vec!["commit".to_string(), "--message".to_string(), message],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(map_git_failure("commit", output, None));
        }
        let entry = self
            .run_success("read commit", &["log", "-1", "--format=%H%x00%h%x00%s"])
            .await?;
        parse_commit_result(&entry.stdout).ok_or_else(|| GitWorkflowError::Git {
            operation: "read commit".to_string(),
            exit_code: entry.status.code(),
            message: "git returned an invalid commit record".to_string(),
        })
    }

    async fn validate_branch(&self, name: &str) -> Result<String, GitWorkflowError> {
        if name.is_empty()
            || name.len() > self.limits.max_branch_bytes
            || name.trim() != name
            || name.starts_with('-')
            || name.eq_ignore_ascii_case("head")
            || name.chars().any(|character| character.is_control())
        {
            return Err(GitWorkflowError::InvalidBranch {
                message: "branch name is empty, unsafe, or too long".to_string(),
            });
        }
        let output = self
            .run_owned(
                vec![
                    "check-ref-format".to_string(),
                    "--branch".to_string(),
                    name.to_string(),
                ],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        if !output.status.success() {
            return Err(GitWorkflowError::InvalidBranch {
                message: "branch name does not satisfy git reference rules".to_string(),
            });
        }
        Ok(name.to_string())
    }

    fn validate_commit_message(&self, message: &str) -> Result<String, GitWorkflowError> {
        let message = message.trim();
        if message.is_empty() || message.len() > self.limits.max_commit_message_bytes {
            return Err(GitWorkflowError::InvalidCommitMessage {
                message: "message is empty or too long".to_string(),
            });
        }
        if message
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
        {
            return Err(GitWorkflowError::InvalidCommitMessage {
                message: "message contains unsupported control characters".to_string(),
            });
        }
        Ok(message.to_string())
    }

    fn validate_paths(&self, paths: &[String]) -> Result<Vec<String>, GitWorkflowError> {
        if paths.is_empty() || paths.len() > self.limits.max_paths_per_operation {
            return Err(GitWorkflowError::InvalidPath {
                message: "path list is empty or too large".to_string(),
            });
        }
        paths.iter().map(|path| self.validate_path(path)).collect()
    }

    fn validate_diff_paths(&self, paths: &[String]) -> Result<Vec<String>, GitWorkflowError> {
        if paths.len() > self.limits.max_paths_per_operation {
            return Err(GitWorkflowError::InvalidPath {
                message: "path list is too large".to_string(),
            });
        }
        paths.iter().map(|path| self.validate_path(path)).collect()
    }

    fn validate_path(&self, relative: &str) -> Result<String, GitWorkflowError> {
        if relative.is_empty()
            || relative.len() > self.limits.max_path_bytes
            || relative.chars().any(char::is_control)
        {
            return Err(GitWorkflowError::InvalidPath {
                message: "path is empty, unsafe, or too long".to_string(),
            });
        }
        let path = Path::new(relative);
        if path.is_absolute() {
            return Err(GitWorkflowError::PathEscape {
                path: sanitize_message(relative, 256),
            });
        }
        let mut component_count = 0usize;
        for component in path.components() {
            component_count += 1;
            match component {
                Component::Normal(value) if !is_git_metadata_component(value) => {}
                _ => {
                    return Err(GitWorkflowError::PathEscape {
                        path: sanitize_message(relative, 256),
                    });
                }
            }
        }
        if component_count == 0 || component_count > 128 {
            return Err(GitWorkflowError::InvalidPath {
                message: "path has an invalid component count".to_string(),
            });
        }

        let joined = self.root.join(path);
        let mut probe = joined.as_path();
        loop {
            match fs::symlink_metadata(probe) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        let target = fs::canonicalize(probe).map_err(|_| {
                            GitWorkflowError::SymlinkEscape {
                                path: sanitize_message(relative, 256),
                            }
                        })?;
                        if !target.starts_with(&self.root) {
                            return Err(GitWorkflowError::SymlinkEscape {
                                path: sanitize_message(relative, 256),
                            });
                        }
                    }
                    let canonical =
                        fs::canonicalize(probe).map_err(|error| GitWorkflowError::Io {
                            operation: "validate path".to_string(),
                            message: sanitize_message(&error.to_string(), 512),
                        })?;
                    if !canonical.starts_with(&self.root) {
                        return Err(GitWorkflowError::PathEscape {
                            path: sanitize_message(relative, 256),
                        });
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    probe = probe.parent().ok_or_else(|| GitWorkflowError::PathEscape {
                        path: sanitize_message(relative, 256),
                    })?;
                }
                Err(error) => {
                    return Err(GitWorkflowError::Io {
                        operation: "validate path".to_string(),
                        message: sanitize_message(&error.to_string(), 512),
                    });
                }
            }
        }
        if fs::metadata(&joined).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(GitWorkflowError::InvalidPath {
                message: "directories are not accepted as exact paths".to_string(),
            });
        }
        Ok(relative.to_string())
    }

    fn verify_repository_identity(&self) -> Result<(), GitWorkflowError> {
        let root = fs::canonicalize(&self.root).map_err(|_| GitWorkflowError::RepositoryChanged)?;
        let git_dir =
            fs::canonicalize(&self.git_dir).map_err(|_| GitWorkflowError::RepositoryChanged)?;
        if root != self.root
            || git_dir != self.git_dir
            || FileIdentity::read(&root)? != self.root_identity
            || FileIdentity::read(&git_dir)? != self.git_dir_identity
        {
            return Err(GitWorkflowError::RepositoryChanged);
        }
        Ok(())
    }

    async fn has_head(&self) -> Result<bool, GitWorkflowError> {
        let output = self
            .run(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(map_git_failure("inspect head", output, None)),
        }
    }

    async fn branch_exists(&self, name: &str) -> Result<bool, GitWorkflowError> {
        let reference = format!("refs/heads/{name}");
        let output = self
            .run_owned(
                vec![
                    "show-ref".to_string(),
                    "--verify".to_string(),
                    "--quiet".to_string(),
                    reference,
                ],
                self.limits.max_command_output_bytes,
                OverflowPolicy::Reject,
            )
            .await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(map_git_failure("inspect branch", output, Some(name))),
        }
    }

    async fn branch_info(&self, name: &str) -> Result<BranchInfo, GitWorkflowError> {
        self.list_branches_inner()
            .await?
            .into_iter()
            .find(|branch| branch.name == name)
            .ok_or_else(|| GitWorkflowError::BranchNotFound {
                branch: name.to_string(),
            })
    }

    async fn run_diff(
        &self,
        staged: bool,
        paths: &[String],
        cap: usize,
    ) -> Result<CommandOutput, GitWorkflowError> {
        let mut args = vec![
            "diff".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            "--unified=3".to_string(),
        ];
        if staged {
            args.push("--cached".to_string());
        }
        args.push("--".to_string());
        args.extend(paths.iter().cloned());
        let output = self.run_owned(args, cap, OverflowPolicy::Truncate).await?;
        if output.status.success() || output.truncated {
            Ok(output)
        } else {
            Err(map_git_failure("preview diff", output, None))
        }
    }

    async fn run_diff_from_head(
        &self,
        paths: &[String],
        cap: usize,
    ) -> Result<CommandOutput, GitWorkflowError> {
        let mut args = vec![
            "diff".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            "--unified=3".to_string(),
            "HEAD".to_string(),
            "--".to_string(),
        ];
        args.extend(paths.iter().cloned());
        let output = self.run_owned(args, cap, OverflowPolicy::Truncate).await?;
        if output.status.success() || output.truncated {
            Ok(output)
        } else {
            Err(map_git_failure("preview diff", output, None))
        }
    }

    async fn run(&self, args: &[&str]) -> Result<CommandOutput, GitWorkflowError> {
        self.run_owned(
            args.iter().map(|arg| (*arg).to_string()).collect(),
            self.limits.max_command_output_bytes,
            OverflowPolicy::Reject,
        )
        .await
    }

    async fn run_success(
        &self,
        operation: &str,
        args: &[&str],
    ) -> Result<CommandOutput, GitWorkflowError> {
        let output = self.run(args).await?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(map_git_failure(operation, output, None))
        }
    }

    async fn read_optional_ref(
        &self,
        operation: &str,
        args: &[&str],
    ) -> Result<Option<String>, GitWorkflowError> {
        let output = self.run(args).await?;
        if output.status.success() {
            Ok(nonempty_output(&output.stdout))
        } else if matches!(output.status.code(), Some(1) | Some(128)) {
            Ok(None)
        } else {
            Err(map_git_failure(operation, output, None))
        }
    }

    async fn run_owned(
        &self,
        args: Vec<String>,
        output_cap: usize,
        overflow_policy: OverflowPolicy,
    ) -> Result<CommandOutput, GitWorkflowError> {
        execute_git(
            &self.root,
            Some((&self.git_dir, &self.root)),
            &args,
            self.limits.command_timeout,
            output_cap,
            overflow_policy,
        )
        .await
    }
}

fn is_rejection(error: &GitWorkflowError) -> bool {
    matches!(
        error,
        GitWorkflowError::InvalidConfiguration
            | GitWorkflowError::NotRepository
            | GitWorkflowError::RepositoryChanged
            | GitWorkflowError::InvalidPath { .. }
            | GitWorkflowError::PathEscape { .. }
            | GitWorkflowError::SymlinkEscape { .. }
            | GitWorkflowError::InvalidBranch { .. }
            | GitWorkflowError::BranchExists { .. }
            | GitWorkflowError::BranchNotFound { .. }
            | GitWorkflowError::InvalidCommitMessage { .. }
            | GitWorkflowError::DirtyWorktree
            | GitWorkflowError::Conflicts { .. }
            | GitWorkflowError::NothingToCommit
            | GitWorkflowError::Busy
    )
}

fn conflict_paths(state: &RepositoryState) -> Vec<String> {
    state
        .files
        .iter()
        .filter(|file| file.conflicted)
        .map(|file| file.path.clone())
        .collect()
}

fn bounded_audit_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .take(64)
        .map(|path| sanitize_message(path, 256))
        .collect()
}

fn is_git_metadata_component(component: &OsStr) -> bool {
    component
        .to_str()
        .is_some_and(|value| value.eq_ignore_ascii_case(".git"))
}

fn canonical_output_path(output: &[u8]) -> Result<PathBuf, GitWorkflowError> {
    let end = output
        .iter()
        .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
        .map_or(0, |index| index + 1);
    #[cfg(unix)]
    let value = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(output[..end].to_vec()))
    };
    #[cfg(not(unix))]
    let value = PathBuf::from(
        String::from_utf8(output[..end].to_vec()).map_err(|_| GitWorkflowError::NotRepository)?,
    );
    fs::canonicalize(value).map_err(|_| GitWorkflowError::NotRepository)
}

fn nonempty_output(output: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(output).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn parse_porcelain_status(output: &[u8]) -> Vec<WorkingTreeEntry> {
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 3 || record[2] != b' ' {
            continue;
        }
        let index = record[0] as char;
        let worktree = record[1] as char;
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        let original_path = if matches!(index, 'R' | 'C') {
            records
                .next()
                .map(|value| String::from_utf8_lossy(value).into_owned())
        } else {
            None
        };
        files.push(WorkingTreeEntry {
            path,
            original_path,
            index_status: status_character(index),
            worktree_status: status_character(worktree),
            conflicted: is_conflict_code(index, worktree),
        });
    }
    files
}

fn status_character(character: char) -> Option<String> {
    (character != ' ').then(|| character.to_string())
}

fn is_conflict_code(index: char, worktree: char) -> bool {
    matches!(
        (index, worktree),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

fn parse_branches(output: &[u8]) -> Vec<BranchInfo> {
    output
        .split(|byte| *byte == b'\n')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let fields: Vec<&[u8]> = record.split(|byte| *byte == 0).collect();
            if fields.len() != 5 {
                return None;
            }
            let field = |index: usize| String::from_utf8_lossy(fields[index]).to_string();
            let upstream = field(3);
            let tracking = field(4);
            Some(BranchInfo {
                name: field(0),
                current: fields[1] == b"*",
                commit: field(2),
                upstream: (!upstream.is_empty()).then_some(upstream),
                tracking: (!tracking.is_empty()).then_some(tracking),
            })
        })
        .collect()
}

fn parse_commit_result(output: &[u8]) -> Option<CommitResult> {
    let fields: Vec<&[u8]> = output
        .strip_suffix(b"\n")
        .unwrap_or(output)
        .split(|byte| *byte == 0)
        .collect();
    if fields.len() != 3 {
        return None;
    }
    Some(CommitResult {
        hash: String::from_utf8_lossy(fields[0]).into_owned(),
        short_hash: String::from_utf8_lossy(fields[1]).into_owned(),
        summary: String::from_utf8_lossy(fields[2]).into_owned(),
    })
}

fn map_git_failure(
    operation: &str,
    output: CommandOutput,
    branch: Option<&str>,
) -> GitWorkflowError {
    let message = sanitize_message(&String::from_utf8_lossy(&output.stderr), 1024);
    let lowercase = message.to_ascii_lowercase();
    if lowercase.contains("not a git repository") {
        GitWorkflowError::NotRepository
    } else if lowercase.contains("nothing to commit") {
        GitWorkflowError::NothingToCommit
    } else if lowercase.contains("already exists") {
        GitWorkflowError::BranchExists {
            branch: branch.unwrap_or("unknown").to_string(),
        }
    } else if lowercase.contains("local changes") || lowercase.contains("would be overwritten") {
        GitWorkflowError::DirtyWorktree
    } else {
        GitWorkflowError::Git {
            operation: operation.to_string(),
            exit_code: output.status.code(),
            message: if message.is_empty() {
                "git command failed without an error message".to_string()
            } else {
                message
            },
        }
    }
}

fn sanitize_message(value: &str, limit: usize) -> String {
    let mut output = String::with_capacity(value.len().min(limit));
    for character in value.chars() {
        if output.len() >= limit {
            break;
        }
        if character == '\n' || character == '\t' || !character.is_control() {
            let remaining = limit - output.len();
            if character.len_utf8() <= remaining {
                output.push(character);
            }
        }
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowPolicy {
    Reject,
    Truncate,
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

struct ManagedChild {
    child: Child,
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        kill_process_group(&self.child);
        let _ = self.child.start_kill();
    }
}

async fn run_unscoped(
    root: &Path,
    args: &[&str],
    limits: &GitWorkflowLimits,
) -> Result<CommandOutput, GitWorkflowError> {
    let args = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    execute_git(
        root,
        None,
        &args,
        limits.command_timeout,
        limits.max_command_output_bytes,
        OverflowPolicy::Reject,
    )
    .await
}

async fn execute_git(
    current_dir: &Path,
    scope: Option<(&Path, &Path)>,
    args: &[String],
    timeout: Duration,
    output_cap: usize,
    overflow_policy: OverflowPolicy,
) -> Result<CommandOutput, GitWorkflowError> {
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some((git_dir, work_tree)) = scope {
        command.arg("--git-dir").arg(git_dir);
        command.arg("--work-tree").arg(work_tree);
    } else {
        command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
    }
    command.arg("--literal-pathspecs").args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let child = command.spawn().map_err(|error| GitWorkflowError::Io {
        operation: "start git".to_string(),
        message: sanitize_message(&error.to_string(), 512),
    })?;
    let mut child = ManagedChild { child };
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| GitWorkflowError::Io {
            operation: "capture git stdout".to_string(),
            message: "stdout pipe was unavailable".to_string(),
        })?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| GitWorkflowError::Io {
            operation: "capture git stderr".to_string(),
            message: "stderr pipe was unavailable".to_string(),
        })?;
    let used = Arc::new(AtomicUsize::new(0));
    let overflowed = Arc::new(AtomicBool::new(false));
    let overflow = Arc::new(Notify::new());
    let stdout_task = tokio::spawn(read_bounded(
        stdout,
        output_cap,
        used.clone(),
        overflowed.clone(),
        overflow.clone(),
    ));
    let stderr_task = tokio::spawn(read_bounded(
        stderr,
        output_cap,
        used,
        overflowed.clone(),
        overflow.clone(),
    ));

    enum Completion {
        Exited(std::io::Result<ExitStatus>),
        Overflow,
        TimedOut,
    }
    let completion = tokio::select! {
        biased;
        _ = overflow.notified() => Completion::Overflow,
        _ = tokio::time::sleep(timeout) => Completion::TimedOut,
        status = child.child.wait() => Completion::Exited(status),
    };
    let (status, truncated) = match completion {
        Completion::Exited(status) => (
            status.map_err(|error| GitWorkflowError::Io {
                operation: "wait for git".to_string(),
                message: sanitize_message(&error.to_string(), 512),
            })?,
            overflowed.load(Ordering::Relaxed),
        ),
        Completion::Overflow => {
            terminate_child(&mut child.child).await;
            if overflow_policy == OverflowPolicy::Reject {
                stdout_task.abort();
                stderr_task.abort();
                return Err(GitWorkflowError::OutputLimitExceeded);
            }
            let status = child
                .child
                .wait()
                .await
                .unwrap_or_else(|_| synthetic_failure_status());
            (status, true)
        }
        Completion::TimedOut => {
            terminate_child(&mut child.child).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(GitWorkflowError::TimedOut);
        }
    };
    let stdout = join_reader(stdout_task, "read git stdout").await?;
    let stderr = join_reader(stderr_task, "read git stderr").await?;
    let truncated = truncated || overflowed.load(Ordering::Relaxed);
    if truncated && overflow_policy == OverflowPolicy::Reject {
        return Err(GitWorkflowError::OutputLimitExceeded);
    }
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
        truncated,
    })
}

async fn read_bounded<R>(
    mut reader: R,
    cap: usize,
    used: Arc<AtomicUsize>,
    overflowed: Arc<AtomicBool>,
    overflow: Arc<Notify>,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if overflowed.load(Ordering::Relaxed) {
            continue;
        }
        let start = used.fetch_add(read, Ordering::Relaxed);
        let available = cap.saturating_sub(start);
        captured.extend_from_slice(&buffer[..read.min(available)]);
        if read > available && !overflowed.swap(true, Ordering::Relaxed) {
            overflow.notify_one();
        }
    }
    Ok(captured)
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    operation: &str,
) -> Result<Vec<u8>, GitWorkflowError> {
    task.await
        .map_err(|error| GitWorkflowError::Io {
            operation: operation.to_string(),
            message: sanitize_message(&error.to_string(), 512),
        })?
        .map_err(|error| GitWorkflowError::Io {
            operation: operation.to_string(),
            message: sanitize_message(&error.to_string(), 512),
        })
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(child);
    let _ = child.kill().await;
}

#[cfg(unix)]
fn kill_process_group(child: &Child) {
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        // Kill hooks through the child's process group.
        unsafe {
            libc::kill(-id, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn synthetic_failure_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(1 << 8)
}

#[cfg(windows)]
fn synthetic_failure_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kiralm-git-workflow-{name}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            let repository = Self { path };
            repository.git(&["init", "--quiet", "--initial-branch=main"]);
            repository.git(&["config", "user.email", "test@example.com"]);
            repository.git(&["config", "user.name", "Test User"]);
            repository
        }

        fn git(&self, args: &[&str]) -> std::process::Output {
            let output = StdCommand::new("git")
                .args(args)
                .current_dir(&self.path)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        fn write(&self, relative: &str, contents: &str) {
            fs::write(self.path.join(relative), contents).unwrap();
        }

        fn commit_file(&self, relative: &str, contents: &str, message: &str) {
            self.write(relative, contents);
            self.git(&["add", "--", relative]);
            self.git(&["commit", "--quiet", "--message", message]);
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    async fn service(repository: &TestRepository) -> GitWorkflowService {
        GitWorkflowService::open(&repository.path, GitWorkflowLimits::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_non_repository_and_nested_roots() {
        let directory = std::env::temp_dir().join(format!(
            "kiralm-git-workflow-non-repo-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let error = GitWorkflowService::open(&directory, GitWorkflowLimits::default())
            .await
            .unwrap_err();
        assert_eq!(error, GitWorkflowError::NotRepository);
        fs::remove_dir_all(&directory).unwrap();

        let repository = TestRepository::new("nested-root");
        fs::create_dir(repository.path.join("nested")).unwrap();
        let error =
            GitWorkflowService::open(repository.path.join("nested"), GitWorkflowLimits::default())
                .await
                .unwrap_err();
        assert_eq!(error, GitWorkflowError::NotRepository);
    }

    #[tokio::test]
    async fn stages_commits_and_records_audits() {
        let repository = TestRepository::new("commit");
        let service = service(&repository).await;
        repository.write("notes.txt", "first\n");

        let state = service
            .stage_paths(&["notes.txt".to_string()])
            .await
            .unwrap();
        assert_eq!(state.files[0].index_status.as_deref(), Some("A"));
        let result = service.commit("Add notes").await.unwrap();
        assert_eq!(result.summary, "Add notes");
        assert_eq!(result.hash.len(), 40);
        assert!(!service.repository_state().await.unwrap().dirty);

        let audit = service.audit_records(0, 20);
        assert_eq!(audit.len(), 3);
        assert_eq!(audit[0].operation, GitOperation::StagePaths);
        assert_eq!(audit[1].operation, GitOperation::Commit);
        assert_eq!(audit[2].status, AuditStatus::Succeeded);
    }

    #[tokio::test]
    async fn unstages_exact_paths_only() {
        let repository = TestRepository::new("unstage");
        repository.commit_file("one.txt", "one\n", "initial");
        repository.write("one.txt", "changed\n");
        repository.write("two.txt", "two\n");
        let service = service(&repository).await;
        service
            .stage_paths(&["one.txt".to_string(), "two.txt".to_string()])
            .await
            .unwrap();

        let state = service
            .unstage_paths(&["one.txt".to_string()])
            .await
            .unwrap();
        let one = state
            .files
            .iter()
            .find(|file| file.path == "one.txt")
            .unwrap();
        let two = state
            .files
            .iter()
            .find(|file| file.path == "two.txt")
            .unwrap();
        assert_eq!(one.index_status, None);
        assert_eq!(one.worktree_status.as_deref(), Some("M"));
        assert_eq!(two.index_status.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn creates_lists_and_switches_clean_branches() {
        let repository = TestRepository::new("branches");
        repository.commit_file("base.txt", "base\n", "initial");
        let service = service(&repository).await;

        let created = service.create_branch("feature/safe").await.unwrap();
        assert_eq!(created.name, "feature/safe");
        assert!(!created.current);
        let switched = service.switch_branch("feature/safe").await.unwrap();
        assert!(switched.current);
        let branches = service.list_branches().await.unwrap();
        assert_eq!(branches.len(), 2);
        assert!(branches.iter().any(|branch| branch.name == "main"));
    }

    #[tokio::test]
    async fn refuses_switch_with_dirty_worktree() {
        let repository = TestRepository::new("dirty-switch");
        repository.commit_file("base.txt", "base\n", "initial");
        let service = service(&repository).await;
        service.create_branch("other").await.unwrap();
        repository.write("base.txt", "dirty\n");

        let error = service.switch_branch("other").await.unwrap_err();
        assert_eq!(error, GitWorkflowError::DirtyWorktree);
        assert_eq!(
            service.audit_records(0, 20).last().unwrap().status,
            AuditStatus::Rejected
        );
    }

    #[tokio::test]
    async fn rejects_path_and_git_metadata_escapes() {
        let repository = TestRepository::new("path-escape");
        let service = service(&repository).await;
        for path in ["../outside.txt", "/tmp/outside.txt", ".git/config"] {
            assert!(matches!(
                service.stage_paths(&[path.to_string()]).await,
                Err(GitWorkflowError::PathEscape { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_that_escape_the_repository() {
        use std::os::unix::fs::symlink;

        let repository = TestRepository::new("symlink-escape");
        let outside =
            std::env::temp_dir().join(format!("kiralm-git-outside-{}", uuid::Uuid::new_v4()));
        fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, repository.path.join("link.txt")).unwrap();
        let service = service(&repository).await;

        let error = service
            .stage_paths(&["link.txt".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(error, GitWorkflowError::SymlinkEscape { .. }));
        fs::remove_file(outside).unwrap();
    }

    #[tokio::test]
    async fn detects_merge_conflicts_before_commit() {
        let repository = TestRepository::new("conflict");
        repository.commit_file("shared.txt", "base\n", "initial");
        repository.git(&["switch", "--quiet", "-c", "feature"]);
        repository.commit_file("shared.txt", "feature\n", "feature change");
        repository.git(&["switch", "--quiet", "main"]);
        repository.commit_file("shared.txt", "main\n", "main change");
        let merge = StdCommand::new("git")
            .args(["merge", "feature"])
            .current_dir(&repository.path)
            .output()
            .unwrap();
        assert!(!merge.status.success());
        let service = service(&repository).await;

        let state = service.repository_state().await.unwrap();
        assert!(state.conflicted);
        assert_eq!(state.files[0].path, "shared.txt");
        let error = service.commit("Resolve conflict").await.unwrap_err();
        assert_eq!(
            error,
            GitWorkflowError::Conflicts {
                paths: vec!["shared.txt".to_string()]
            }
        );
    }

    #[tokio::test]
    async fn truncates_large_diff_previews() {
        let repository = TestRepository::new("diff-cap");
        repository.commit_file("large.txt", "small\n", "initial");
        repository.write("large.txt", &"changed line\n".repeat(1000));
        let limits = GitWorkflowLimits {
            max_diff_output_bytes: 512,
            ..GitWorkflowLimits::default()
        };
        let service = GitWorkflowService::open(&repository.path, limits)
            .await
            .unwrap();

        let preview = service
            .diff_preview(DiffScope::Unstaged, &["large.txt".to_string()])
            .await
            .unwrap();
        assert!(preview.truncated);
        assert!(preview.bytes <= 512);
        assert_eq!(
            service.audit_records(0, 10)[0].status,
            AuditStatus::Truncated
        );
    }

    #[tokio::test]
    async fn previews_the_full_repository_without_path_filters() {
        let repository = TestRepository::new("diff-all");
        repository.commit_file("one.txt", "one\n", "initial");
        repository.write("one.txt", "changed\n");
        let service = service(&repository).await;

        let preview = service.diff_preview(DiffScope::All, &[]).await.unwrap();
        assert!(preview.text.contains("one.txt"));
        assert!(!preview.truncated);
    }

    #[tokio::test]
    async fn rejects_work_when_the_concurrency_limit_is_reached() {
        let repository = TestRepository::new("concurrency");
        let limits = GitWorkflowLimits {
            max_concurrent_operations: 1,
            ..GitWorkflowLimits::default()
        };
        let service = GitWorkflowService::open(&repository.path, limits)
            .await
            .unwrap();
        let permit = service.acquire_permit().unwrap();

        let error = service.repository_state().await.unwrap_err();
        assert_eq!(error, GitWorkflowError::Busy);
        assert_eq!(
            service.audit_records(0, 10)[0].status,
            AuditStatus::Rejected
        );
        drop(permit);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounds_hook_output_and_execution_time() {
        use std::os::unix::fs::PermissionsExt;

        let repository = TestRepository::new("hook-bounds");
        repository.write("file.txt", "content\n");
        let hook = repository.path.join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\nyes x | head -c 4096\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let limits = GitWorkflowLimits {
            max_command_output_bytes: 512,
            ..GitWorkflowLimits::default()
        };
        let service = GitWorkflowService::open(&repository.path, limits)
            .await
            .unwrap();
        service
            .stage_paths(&["file.txt".to_string()])
            .await
            .unwrap();
        assert_eq!(
            service.commit("bounded output").await.unwrap_err(),
            GitWorkflowError::OutputLimitExceeded
        );

        fs::write(&hook, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let limits = GitWorkflowLimits {
            command_timeout: Duration::from_millis(100),
            ..GitWorkflowLimits::default()
        };
        let service = GitWorkflowService::open(&repository.path, limits)
            .await
            .unwrap();
        assert_eq!(
            service.commit("bounded time").await.unwrap_err(),
            GitWorkflowError::TimedOut
        );
    }

    #[tokio::test]
    async fn validates_branch_and_commit_inputs() {
        let repository = TestRepository::new("inputs");
        repository.commit_file("base.txt", "base\n", "initial");
        let service = service(&repository).await;

        assert!(matches!(
            service.create_branch("-unsafe").await,
            Err(GitWorkflowError::InvalidBranch { .. })
        ));
        assert!(matches!(
            service.commit(" \n ").await,
            Err(GitWorkflowError::InvalidCommitMessage { .. })
        ));
        assert_eq!(
            service.commit("No staged changes").await.unwrap_err(),
            GitWorkflowError::NothingToCommit
        );
    }
}
