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

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author: String,
    /// Strict ISO 8601 (`git log --pretty=%aI`), so the UI can just
    /// `new Date()` it without a custom parser.
    pub date: String,
}

/// `\x1f` (unit separator) between fields -- never appears in a commit
/// message/author, so no escaping is needed to split the log back apart.
const LOG_SEP: &str = "\x1f";

/// Most recent `limit` commits on the current branch, newest first.
/// Never errors -- a non-repo or a repo with no commits yet just comes
/// back empty.
pub async fn log(root: &Path, limit: usize) -> Vec<LogEntry> {
    if !is_repo(root).await {
        return vec![];
    }
    let format = format!("--pretty=format:%H{LOG_SEP}%h{LOG_SEP}%s{LOG_SEP}%an{LOG_SEP}%aI");
    let n = format!("-{limit}");
    let out = run(root, &["log", &n, &format]).await.unwrap_or_default();
    out.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(LOG_SEP).collect();
            let [hash, short_hash, message, author, date] = parts.as_slice() else { return None };
            Some(LogEntry {
                hash: hash.to_string(),
                short_hash: short_hash.to_string(),
                message: message.to_string(),
                author: author.to_string(),
                date: date.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    pub hash: String,
    pub short_hash: String,
    pub summary: String,
}

/// `git add -A` then `git commit -m <message>` against `root`. Rejects a
/// blank message and a clean working tree with a clear error rather than
/// letting either hit git and pass its raw failure through.
pub async fn commit(root: &Path, message: &str) -> Result<CommitResult, String> {
    let message = message.trim();
    if message.is_empty() {
        return Err("commit message is required".to_string());
    }
    if !is_repo(root).await {
        return Err(format!("{} is not a git repository", root.display()));
    }
    run(root, &["add", "-A"]).await?;
    let porcelain = run(root, &["status", "--porcelain=v1"]).await.unwrap_or_default();
    if porcelain.trim().is_empty() {
        return Err("nothing to commit".to_string());
    }
    run(root, &["commit", "-m", message]).await?;
    let hash = run(root, &["rev-parse", "HEAD"]).await?;
    let short_hash = run(root, &["rev-parse", "--short", "HEAD"]).await?;
    Ok(CommitResult { hash, short_hash, summary: message.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kiralm-git-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn init_repo(dir: &Path) {
        run(dir, &["init", "--quiet"]).await.unwrap();
        run(dir, &["config", "user.email", "test@example.com"]).await.unwrap();
        run(dir, &["config", "user.name", "Test"]).await.unwrap();
    }

    #[tokio::test]
    async fn log_is_empty_for_a_non_repo() {
        let dir = temp_dir("log-non-repo");
        assert!(log(&dir, 50).await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn log_returns_real_commits_newest_first() {
        let dir = temp_dir("log-real");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        run(&dir, &["add", "-A"]).await.unwrap();
        run(&dir, &["commit", "-m", "first commit"]).await.unwrap();
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        run(&dir, &["add", "-A"]).await.unwrap();
        run(&dir, &["commit", "-m", "second commit"]).await.unwrap();

        let entries = log(&dir, 50).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "second commit");
        assert_eq!(entries[1].message, "first commit");
        assert!(!entries[0].hash.is_empty());
        assert!(!entries[0].short_hash.is_empty());
        assert_eq!(entries[0].author, "Test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_rejects_empty_message() {
        let dir = temp_dir("commit-empty-msg");
        init_repo(&dir).await;
        let err = commit(&dir, "   ").await.unwrap_err();
        assert!(err.contains("message is required"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_rejects_clean_tree() {
        let dir = temp_dir("commit-clean");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        run(&dir, &["add", "-A"]).await.unwrap();
        run(&dir, &["commit", "-m", "seed"]).await.unwrap();

        let err = commit(&dir, "no changes").await.unwrap_err();
        assert!(err.contains("nothing to commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_creates_a_real_commit_visible_in_log() {
        let dir = temp_dir("commit-real");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        run(&dir, &["add", "-A"]).await.unwrap();
        run(&dir, &["commit", "-m", "seed"]).await.unwrap();

        std::fs::write(dir.join("b.txt"), "two").unwrap();
        let result = commit(&dir, "add b.txt").await.unwrap();
        assert_eq!(result.summary, "add b.txt");
        assert!(!result.hash.is_empty());
        assert!(result.hash.starts_with(&result.short_hash));

        let entries = log(&dir, 50).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "add b.txt");
        assert_eq!(entries[0].hash, result.hash);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

