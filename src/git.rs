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

#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    pub commit: String,
    pub branch: String,
    pub pushed: bool,
}

/// `git add -A && git commit -m <message> && git push`, real ops against
/// `root`. Sets upstream on first push of a new branch. A commit that
/// lands but fails to push is still reported truthfully (not silently
/// dropped), via `Err` naming the real commit hash.
pub async fn commit_and_push(root: &Path, message: &str) -> Result<CommitResult, String> {
    if !is_repo(root).await {
        return Err("not a git repository".to_string());
    }
    run(root, &["add", "-A"]).await?;
    let porcelain = run(root, &["status", "--porcelain=v1"]).await.unwrap_or_default();
    if porcelain.trim().is_empty() {
        return Err("nothing to commit".to_string());
    }
    run(root, &["commit", "-m", message]).await?;
    let commit = run(root, &["rev-parse", "--short", "HEAD"]).await?;
    let branch = run(root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;

    let has_upstream = run(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .await
        .is_ok();
    let push = if has_upstream {
        run(root, &["push"]).await
    } else {
        run(root, &["push", "--set-upstream", "origin", &branch]).await
    };
    match push {
        Ok(_) => Ok(CommitResult { commit, branch, pushed: true }),
        Err(e) => Err(format!("committed {commit} but push failed: {e}")),
    }
}
