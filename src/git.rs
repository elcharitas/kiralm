//! Real git operations, shelled out to the system `git` binary with a
//! project's directory as cwd -- not reimplemented, just scoped to
//! whichever project is active.

use std::path::Path;

use serde::Serialize;
use tokio::process::Command;

async fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git clone <url> <dest>`, real op -- what "Pull from git" on the
/// project-home screen drives. `dest`'s parent is created if missing;
/// `dest` itself must not already exist (git refuses a non-empty one).
pub async fn clone(url: &str, dest: &Path) -> Result<(), String> {
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    run(dest.parent().unwrap_or(dest), &["clone", url, &dest.to_string_lossy()]).await?;
    Ok(())
}

/// `git clone --depth 1 <url> <dest>` -- full history is never needed
/// for a marketplace checkout. Same dest-must-not-exist contract as
/// `clone` above.
pub async fn shallow_clone(url: &str, dest: &Path) -> Result<(), String> {
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    run(dest.parent().unwrap_or(dest), &["clone", "--depth", "1", url, &dest.to_string_lossy()]).await?;
    Ok(())
}

/// `git pull --quiet` inside an existing clone -- the "fetch if already
/// known" half of adding a marketplace, and what a refresh re-runs.
pub async fn pull(root: &Path) -> Result<(), String> {
    run(root, &["pull", "--quiet"]).await.map(|_| ())
}

pub async fn is_repo(root: &Path) -> bool {
    run(root, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|s| s == "true")
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStatus {
    pub path: String,
    /// Raw two-char `git status --porcelain` code, e.g. "M", "??", "A".
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    pub is_repo: bool,
    /// `None` for a repo with no commits yet or a detached HEAD.
    pub branch: Option<String>,
    pub dirty: bool,
    pub files: Vec<FileStatus>,
}

/// Real status for `root`: branch + working-tree changes. Never errors --
/// a non-repo or a git failure just comes back as `is_repo: false` (or an
/// empty-but-real status), so callers can show "not a git repo" instead
/// of guessing from a thrown error.
pub async fn status(root: &Path) -> GitStatus {
    if !is_repo(root).await {
        return GitStatus { is_repo: false, branch: None, dirty: false, files: vec![] };
    }
    let branch = run(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()
        .filter(|s| !s.is_empty() && s != "HEAD");
    let porcelain = run(root, &["status", "--porcelain=v1"]).await.unwrap_or_default();
    let files: Vec<FileStatus> = porcelain
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let split = line.len().min(2);
            let (code, path) = line.split_at(split);
            FileStatus { path: path.trim().to_string(), status: code.trim().to_string() }
        })
        .collect();
    let dirty = !files.is_empty();
    GitStatus { is_repo: true, branch, dirty, files }
}

