//! DeepSeek Harness (`dsh`) bridge: binary detection, profile
//! create/list/delete, plugin install/list/remove (all shelled out to
//! the real `dsh`/pnpm), and the one-shot `run_dsh_profile` execution.
//! `dsh` itself is real Node/Cordis code, not a manifest -- there's no
//! documented streaming or IPC contract, only the headless
//! `dsh --profile <name> "<prompt>"` invocation, so this module treats
//! stdout as the answer and nonzero exit + stderr as the error, nothing
//! fancier.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::kiralm_home;

/// A dsh session can run arbitrarily long tool-using work; this caps it
/// so one runaway session can't hang a chat turn forever. 3 minutes:
/// generous for a real multi-step agent loop, still bounded.
pub const RUN_PROFILE_TIMEOUT: Duration = Duration::from_secs(180);

pub fn profiles_dir() -> PathBuf {
    #[cfg(unix)]
    {
        kiralm_home().join("dsh_profiles")
    }
    #[cfg(not(unix))]
    {
        dsh_home_dir().join("profiles")
    }
}

/// Real `dsh` resolves a profile as `$DSH_HOME/profiles/<name>`
/// (`~/.dsh` if unset), a fixed layout it owns -- so every real `dsh`
/// invocation below points `$DSH_HOME` at this directory, which holds
/// nothing but a `profiles` symlink back to `profiles_dir()`. Keeps our
/// mandated storage layout as the one real directory on disk.
fn dsh_home_dir() -> PathBuf {
    kiralm_home().join("dsh_home")
}

fn ensure_dsh_home_linked() -> Result<(), String> {
    std::fs::create_dir_all(profiles_dir()).map_err(|e| e.to_string())?;
    let home = dsh_home_dir();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let link = home.join("profiles");
    if link.is_symlink() || link.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(profiles_dir(), &link).map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&link).map_err(|e| e.to_string())?;
    Ok(())
}

/// A profile name becomes a directory name, checked the same way
/// skills.rs checks skill names.
fn sanitize_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(format!("invalid profile name '{name}'"));
    }
    Ok(name.to_string())
}

fn profile_dir(name: &str) -> Result<PathBuf, String> {
    Ok(profiles_dir().join(sanitize_name(name)?))
}

/// `dsh --version` presence check, same posture as `tunnel::detect`:
/// only a failed spawn (missing binary) is an error, a nonzero exit is
/// still "present".
pub fn detect(dsh_bin: &str) -> anyhow::Result<()> {
    std::process::Command::new(dsh_bin)
        .arg("--version")
        .output()
        .map_err(|_| {
            anyhow::anyhow!(
                "dsh not found at {dsh_bin}. Install DeepSeek Harness (developer preview) and \
                 ensure it's on PATH, or set dsh.bin"
            )
        })?;
    Ok(())
}

/// Best-effort version string for the status endpoint. `None` if the
/// binary is missing or the call fails, never an error.
pub fn version(dsh_bin: &str) -> Option<String> {
    std::process::Command::new(dsh_bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DshProfileManifest {
    #[serde(default)]
    bundles: Vec<String>,
    // Defaults off: an unattended patch-reload on a profile the user
    // hasn't reviewed yet is more surprising than useful.
    #[serde(default, rename = "patchReload")]
    patch_reload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DshSection {
    #[serde(default)]
    profile: DshProfileManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfilePackageJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    dependencies: HashMap<String, String>,
    #[serde(default)]
    dsh: DshSection,
}

/// Creates `~/.kiralm/dsh_profiles/<name>/` with a minimal
/// `package.json` (empty `dsh.profile.bundles`, `patchReload` off) and
/// an empty `cordis.patch.yml` -- the user's own patch layer, deliberately
/// blank until they edit it.
pub fn create_profile(name: &str) -> Result<(), String> {
    let dir = profile_dir(name)?;
    if dir.exists() {
        return Err(format!("profile '{name}' already exists"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let pkg = ProfilePackageJson {
        name: sanitize_name(name)?,
        dependencies: HashMap::new(),
        dsh: DshSection::default(),
    };
    let body = serde_json::to_string_pretty(&pkg).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("package.json"), body).map_err(|e| e.to_string())?;
    // Real dsh requires this to be a top-level YAML array of patch
    // entries, not a mapping -- an empty array is the valid "no
    // overlay yet" state.
    std::fs::write(dir.join("cordis.patch.yml"), "[]\n").map_err(|e| e.to_string())?;
    Ok(())
}

/// Every configured profile name, sorted. Empty (not an error) if the
/// profiles dir doesn't exist yet. Requires a `package.json` sibling,
/// not just any directory -- pnpm hoists a shared `node_modules` right
/// next to profile dirs here, which isn't itself a profile.
pub fn list_profiles() -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(profiles_dir()) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir() && e.path().join("package.json").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

pub fn delete_profile(name: &str) -> Result<(), String> {
    let dir = profile_dir(name)?;
    if !dir.is_dir() {
        return Err(format!("profile '{name}' not found"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

/// The real installed-plugin list: parsed straight from the profile's
/// own `package.json` dependencies, not a parallel record KiraLM keeps
/// in sync itself.
pub fn list_plugins(name: &str) -> Result<Vec<String>, String> {
    let dir = profile_dir(name)?;
    let raw = std::fs::read_to_string(dir.join("package.json"))
        .map_err(|e| format!("profile '{name}' package.json unreadable: {e}"))?;
    let pkg: ProfilePackageJson =
        serde_json::from_str(&raw).map_err(|e| format!("profile '{name}' package.json malformed: {e}"))?;
    let mut names: Vec<String> = pkg.dependencies.into_keys().collect();
    names.sort();
    Ok(names)
}

/// Runs one `dsh` subcommand to completion, cwd'd into `dir`, and
/// returns stdout on a zero exit or stderr+exit code as the error.
async fn run_dsh(dsh_bin: &str, dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    ensure_dsh_home_linked()?;
    let output = Command::new(dsh_bin)
        .args(args)
        .current_dir(dir)
        .env("DSH_HOME", dsh_home_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to spawn dsh: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(format!(
            "dsh exited {code}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// `dsh plugin --profile <name> add <package>`, forwarded straight to
/// pnpm inside the profile directory -- no npm client of our own.
pub async fn install_plugin(dsh_bin: &str, name: &str, package: &str) -> Result<String, String> {
    let dir = profile_dir(name)?;
    if !dir.is_dir() {
        return Err(format!("profile '{name}' not found"));
    }
    run_dsh(dsh_bin, &dir, &["plugin", "--profile", name, "add", package]).await
}

pub async fn remove_plugin(dsh_bin: &str, name: &str, package: &str) -> Result<String, String> {
    let dir = profile_dir(name)?;
    if !dir.is_dir() {
        return Err(format!("profile '{name}' not found"));
    }
    run_dsh(dsh_bin, &dir, &["plugin", "--profile", name, "remove", package]).await
}

/// `dsh --profile <name> "<prompt>"`: one fresh persisted session,
/// fire-and-wait (not a supervised long-running process like the model
/// server). Bounded by `timeout`; production callers use
/// `run_profile`, this split-out form is what the timeout test drives
/// with a short duration instead of waiting out the real 3 minutes.
async fn run_profile_with_timeout(
    dsh_bin: &str,
    name: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    let dir = profile_dir(name)?;
    if !dir.is_dir() {
        return Err(format!("profile '{name}' not found"));
    }
    ensure_dsh_home_linked()?;
    let child = Command::new(dsh_bin)
        .arg("--profile")
        .arg(name)
        .arg(prompt)
        .current_dir(&dir)
        .env("DSH_HOME", dsh_home_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn dsh: {e}"))?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Ok(out)) => Err(format!(
            "dsh exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        )),
        Ok(Err(e)) => Err(format!("dsh run failed: {e}")),
        Err(_) => Err(format!("dsh session timed out after {}s", timeout.as_secs())),
    }
}

pub async fn run_profile(dsh_bin: &str, name: &str, prompt: &str) -> Result<String, String> {
    run_profile_with_timeout(dsh_bin, name, prompt, RUN_PROFILE_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            let guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("kiralm-dsh-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", &dir);
            TempHome { _guard: guard, prev, dir }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn detect_reports_missing_binary_cleanly() {
        let err = detect("/definitely/not/a/real/dsh/binary").unwrap_err();
        assert!(err.to_string().contains("dsh not found"));
    }

    #[test]
    fn create_list_delete_round_trip() {
        let _home = TempHome::new();
        assert!(list_profiles().is_empty());

        create_profile("work").unwrap();
        assert_eq!(list_profiles(), vec!["work".to_string()]);
        assert!(profiles_dir().join("work").join("package.json").exists());
        assert!(profiles_dir().join("work").join("cordis.patch.yml").exists());
        assert!(list_plugins("work").unwrap().is_empty());

        assert!(create_profile("work").is_err(), "duplicate create should fail");

        delete_profile("work").unwrap();
        assert!(list_profiles().is_empty());
        assert!(delete_profile("work").is_err(), "deleting a missing profile should fail");
    }

    #[test]
    fn rejects_path_traversal_in_name() {
        let _home = TempHome::new();
        assert!(create_profile("../escape").is_err());
        assert!(create_profile("a/b").is_err());
    }

    #[tokio::test]
    async fn run_profile_timeout_fires_against_a_real_slow_child() {
        let _home = TempHome::new();
        create_profile("slow").unwrap();

        // A real script that ignores every arg dsh would normally take
        // and just sleeps, so the timeout below is exercised against a
        // genuinely slow child process, not a mocked one.
        let script = std::env::temp_dir().join(format!("kiralm-dsh-slow-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = run_profile_with_timeout(
            script.to_str().unwrap(),
            "slow",
            "hello",
            Duration::from_millis(200),
        )
        .await;
        let err = result.unwrap_err();
        assert!(err.contains("timed out"), "{err}");

        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn run_profile_rejects_a_missing_profile() {
        let _home = TempHome::new();
        let err = run_profile("dsh", "does-not-exist", "hi").await.unwrap_err();
        assert!(err.contains("not found"));
    }
}
