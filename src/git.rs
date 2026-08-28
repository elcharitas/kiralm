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

async fn clone_impl(url: &str, dest: &Path, extra_args: &[&str]) -> Result<(), String> {
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let dest_str = dest.to_string_lossy().into_owned();
    let mut args = vec!["clone"];
    args.extend_from_slice(extra_args);
    args.push(url);
    args.push(&dest_str);
    run(dest.parent().unwrap_or(dest), &args).await?;
    Ok(())
}

/// `git clone <url> <dest>`, real op -- what "Pull from git" on the
/// project-home screen drives. `dest`'s parent is created if missing;
/// `dest` itself must not already exist (git refuses a non-empty one).
pub async fn clone(url: &str, dest: &Path) -> Result<(), String> {
    clone_impl(url, dest, &[]).await
}

/// `git clone --depth 1 <url> <dest>` -- full history is never needed
/// for a marketplace checkout. Same dest-must-not-exist contract as
/// `clone` above.
pub async fn shallow_clone(url: &str, dest: &Path) -> Result<(), String> {
    clone_impl(url, dest, &["--depth", "1"]).await
}

/// Clones `url` pinned to a ref/sha for a marketplace plugin source.
/// A sha needs a full clone + checkout (shallow can't fetch an
/// arbitrary commit); a branch/tag ref can stay shallow.
pub async fn clone_at(url: &str, dest: &Path, git_ref: Option<&str>, sha: Option<&str>) -> Result<(), String> {
    if let Some(sha) = sha {
        clone(url, dest).await?;
        run(dest, &["checkout", sha]).await?;
        return Ok(());
    }
    match git_ref {
        Some(r) => clone_impl(url, dest, &["--depth", "1", "--branch", r]).await,
        None => shallow_clone(url, dest).await,
    }
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

