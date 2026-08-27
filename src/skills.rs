//! `~/.kiralm/skills/<name>/SKILL.md` -- reusable, model-loadable
//! instruction bundles, file-format-compatible with Claude Code's own
//! SKILL.md (YAML frontmatter + markdown body, optional sibling files
//! like `references/*.md` copied in verbatim, never parsed).
//! `~/.kiralm/skills.json` is KiraLM's own sidecar for the one bit of
//! state the format itself doesn't carry: whether a skill is enabled.
//! Global only -- no per-workspace scoping.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;
use crate::marketplace::PluginRef;

pub fn skills_dir() -> PathBuf {
    kiralm_home().join("skills")
}

pub fn skills_state_path() -> PathBuf {
    kiralm_home().join("skills.json")
}

/// The on-disk YAML frontmatter shape, field names matching Claude
/// Code's own SKILL.md convention exactly (`allowed-tools`, kebab-case).
#[derive(Debug, Clone, Deserialize, Default)]
struct Frontmatter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "allowed-tools", default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// A skill as KiraLM sees it: frontmatter + body content, plus the
/// enabled bit from skills.json. This is both the API wire shape and
/// what `load_skill` hands back to the model (via `body`).
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub body: String,
    pub enabled: bool,
    /// Which marketplace plugin installed this skill, if any -- `None`
    /// for one added by hand (paste/folder). What uninstall filters on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<PluginRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SkillsState {
    /// name -> enabled. A name absent here (freshly added skill)
    /// defaults to enabled, see `enabled_for`.
    #[serde(default)]
    enabled: HashMap<String, bool>,
    /// name -> installing plugin, for skills a marketplace install put
    /// here -- absent for a hand-added skill.
    #[serde(default)]
    installed_by: HashMap<String, PluginRef>,
}

impl SkillsState {
    fn load() -> SkillsState {
        Self::load_from(&skills_state_path())
    }

    fn load_from(path: &Path) -> SkillsState {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => SkillsState::default(),
        }
    }

    /// Atomic rewrite, same posture as settings.rs/chats.rs.
    fn save(&self) -> anyhow::Result<()> {
        self.save_to(&skills_state_path())
    }

    fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn enabled_for(&self, name: &str) -> bool {
        self.enabled.get(name).copied().unwrap_or(true)
    }
}

/// Splits a SKILL.md's leading `---`-delimited YAML block from its
/// markdown body and parses the frontmatter. Never panics: a malformed
/// or missing frontmatter block comes back as `Err`, for the caller to
/// skip (listing) or report (create), not crash the daemon over.
fn parse_skill_md(content: &str) -> Result<(Frontmatter, String), String> {
    let rest = content
        .strip_prefix("---")
        .ok_or_else(|| "SKILL.md must start with a '---' YAML frontmatter block".to_string())?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| "SKILL.md frontmatter is missing its closing '---'".to_string())?;
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);

    let fm: Frontmatter =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid frontmatter YAML: {e}"))?;
    if fm.name.trim().is_empty() {
        return Err("frontmatter 'name' is required".to_string());
    }
    if fm.description.trim().is_empty() {
        return Err("frontmatter 'description' is required".to_string());
    }
    Ok((fm, body.to_string()))
}

/// A skill name becomes a directory name under `skills_dir()`, so it's
/// checked the same way a path component would be, not just "non-empty".
fn sanitize_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(format!("invalid skill name '{name}'"));
    }
    Ok(name.to_string())
}

fn skill_from_dir(dir: &Path, state: &SkillsState) -> Option<Skill> {
    let content = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (fm, body) = parse_skill_md(&content).ok()?;
    let enabled = state.enabled_for(&fm.name);
    let installed_by = state.installed_by.get(&fm.name).cloned();
    Some(Skill {
        name: fm.name,
        description: fm.description,
        allowed_tools: fm.allowed_tools,
        license: fm.license,
        metadata: fm.metadata,
        body,
        enabled,
        installed_by,
    })
}

/// Every installed skill, sorted by name. Malformed skills (bad or
/// missing frontmatter) are silently skipped -- a broken sibling
/// directory degrades to "not listed", not a daemon crash.
pub fn list() -> Vec<Skill> {
    let state = SkillsState::load();
    let mut skills: Vec<Skill> = match std::fs::read_dir(skills_dir()) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| skill_from_dir(&e.path(), &state))
            .collect(),
        Err(_) => Vec::new(),
    };
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Just the enabled ones -- what `load_skill`'s tool description is
/// built from, and all `load_skill` will actually serve.
pub fn enabled() -> Vec<Skill> {
    list().into_iter().filter(|s| s.enabled).collect()
}

pub fn get(name: &str) -> Option<Skill> {
    list().into_iter().find(|s| s.name == name)
}

fn write_skill_md(dir: &Path, raw: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let target = dir.join("SKILL.md");
    let tmp = dir.join("SKILL.md.tmp");
    std::fs::write(&tmp, raw).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())
}

/// Registers a skill from raw SKILL.md text (the "paste markdown" path).
pub fn create_from_markdown(raw: &str) -> Result<Skill, String> {
    let (fm, body) = parse_skill_md(raw)?;
    let name = sanitize_name(&fm.name)?;
    let dir = skills_dir().join(&name);
    if dir.exists() {
        return Err(format!("skill '{name}' already exists"));
    }
    write_skill_md(&dir, raw)?;
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        allowed_tools: fm.allowed_tools,
        license: fm.license,
        metadata: fm.metadata,
        body,
        enabled: true,
        installed_by: None,
    })
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dest_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Registers a skill from a local folder that already contains a
/// SKILL.md (the "browse a folder" path) -- sibling files (e.g.
/// `references/*.md`) are copied in verbatim, never parsed.
pub fn create_from_folder(src: &Path) -> Result<Skill, String> {
    if !src.is_dir() {
        return Err(format!("'{}' is not a directory", src.display()));
    }
    let raw = std::fs::read_to_string(src.join("SKILL.md"))
        .map_err(|e| format!("no SKILL.md in '{}': {e}", src.display()))?;
    let (fm, body) = parse_skill_md(&raw)?;
    let name = sanitize_name(&fm.name)?;
    let dest = skills_dir().join(&name);
    if dest.exists() {
        return Err(format!("skill '{name}' already exists"));
    }
    copy_dir_all(src, &dest).map_err(|e| e.to_string())?;
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        allowed_tools: fm.allowed_tools,
        license: fm.license,
        metadata: fm.metadata,
        body,
        enabled: true,
        installed_by: None,
    })
}

/// Tags an already-created skill with the plugin that installed it, so
/// `delete_by_plugin` can later find it again.
pub fn mark_installed(name: &str, plugin: PluginRef) -> Result<(), String> {
    if get(name).is_none() {
        return Err(format!("skill '{name}' not found"));
    }
    let mut state = SkillsState::load();
    state.installed_by.insert(name.to_string(), plugin);
    state.save().map_err(|e| e.to_string())
}

/// Deletes every skill tagged as installed by this plugin, and nothing
/// else. Returns the names actually removed.
pub fn delete_by_plugin(marketplace: &str, plugin: &str) -> Vec<String> {
    let names: Vec<String> = list()
        .into_iter()
        .filter(|s| {
            s.installed_by
                .as_ref()
                .is_some_and(|p| p.marketplace == marketplace && p.plugin == plugin)
        })
        .map(|s| s.name)
        .collect();
    for name in &names {
        let _ = delete(name);
    }
    names
}

pub fn set_enabled(name: &str, on: bool) -> Result<(), String> {
    if get(name).is_none() {
        return Err(format!("skill '{name}' not found"));
    }
    let mut state = SkillsState::load();
    state.enabled.insert(name.to_string(), on);
    state.save().map_err(|e| e.to_string())
}

pub fn delete(name: &str) -> Result<(), String> {
    let name = sanitize_name(name)?;
    let dir = skills_dir().join(&name);
    if !dir.is_dir() {
        return Err(format!("skill '{name}' not found"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut state = SkillsState::load();
    state.enabled.remove(&name);
    state.installed_by.remove(&name);
    let _ = state.save();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `kiralm_home()` at a fresh temp dir for the duration of
    /// one test, so these never touch a real `~/.kiralm`. Serializes on
    /// the crate-wide `HOME_ENV_LOCK` -- `tunnel.rs`'s tests swap the
    /// same process-global env var, so a module-local lock here isn't
    /// enough to avoid racing them.
    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            let guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("kiralm-skills-test-{}", uuid::Uuid::new_v4()));
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

    const VALID_MD: &str = "---\nname: test-skill\ndescription: A skill for testing.\n---\nBody content here.\n";

    #[test]
    fn parses_valid_frontmatter() {
        let (fm, body) = parse_skill_md(VALID_MD).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A skill for testing.");
        assert_eq!(body, "Body content here.\n");
    }

    #[test]
    fn rejects_missing_closing_delimiter() {
        assert!(parse_skill_md("---\nname: x\ndescription: y\nno closing here").is_err());
    }

    #[test]
    fn rejects_missing_leading_delimiter() {
        assert!(parse_skill_md("name: x\ndescription: y\n---\nbody").is_err());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse_skill_md("---\nname: x\n---\nbody").is_err());
        assert!(parse_skill_md("---\ndescription: y\n---\nbody").is_err());
    }

    #[test]
    fn malformed_yaml_does_not_panic() {
        let result = parse_skill_md("---\nname: [unterminated\n---\nbody");
        assert!(result.is_err());
    }

    #[test]
    fn create_list_get_round_trip() {
        let _home = TempHome::new();
        let skill = create_from_markdown(VALID_MD).unwrap();
        assert_eq!(skill.name, "test-skill");
        assert!(skill.enabled);

        let listed = list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "test-skill");

        let fetched = get("test-skill").unwrap();
        assert_eq!(fetched.body, "Body content here.\n");
    }

    #[test]
    fn a_malformed_sibling_skill_is_skipped_not_fatal() {
        let _home = TempHome::new();
        create_from_markdown(VALID_MD).unwrap();
        std::fs::create_dir_all(skills_dir().join("broken")).unwrap();
        std::fs::write(skills_dir().join("broken").join("SKILL.md"), "not frontmatter at all").unwrap();

        let listed = list();
        assert_eq!(listed.len(), 1, "the broken skill should be skipped, not crash listing");
        assert_eq!(listed[0].name, "test-skill");
    }

    #[test]
    fn enabled_state_survives_a_reload() {
        let _home = TempHome::new();
        create_from_markdown(VALID_MD).unwrap();
        set_enabled("test-skill", false).unwrap();

        // Simulates a daemon restart: nothing in-memory carries over,
        // only what's on disk.
        let reloaded = get("test-skill").unwrap();
        assert!(!reloaded.enabled);
        assert!(enabled().is_empty());

        set_enabled("test-skill", true).unwrap();
        assert!(get("test-skill").unwrap().enabled);
    }

    #[test]
    fn create_from_folder_copies_sibling_files_verbatim() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-skill-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(src.join("references")).unwrap();
        std::fs::write(src.join("SKILL.md"), VALID_MD).unwrap();
        std::fs::write(src.join("references").join("extra.md"), "reference content").unwrap();

        let skill = create_from_folder(&src).unwrap();
        assert_eq!(skill.name, "test-skill");
        let copied = std::fs::read_to_string(skills_dir().join("test-skill").join("references").join("extra.md")).unwrap();
        assert_eq!(copied, "reference content");

        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn delete_removes_dir_and_state() {
        let _home = TempHome::new();
        create_from_markdown(VALID_MD).unwrap();
        delete("test-skill").unwrap();
        assert!(get("test-skill").is_none());
        assert!(!skills_dir().join("test-skill").exists());
    }

    #[test]
    fn rejects_path_traversal_in_name() {
        let bad = "---\nname: ../../evil\ndescription: x\n---\nbody";
        assert!(create_from_markdown(bad).is_err());
    }

    #[tokio::test]
    async fn load_skill_tool_returns_the_real_body() {
        let _home = TempHome::new();
        create_from_markdown(VALID_MD).unwrap();

        let (content, step) = crate::tools::execute(
            "load_skill",
            r#"{"name":"test-skill"}"#,
            &std::env::temp_dir(),
            crate::tools::Permission::ReadOnly,
            &crate::mcp::McpRegistry::new(),
            "dsh",
        )
        .await;
        assert!(step.ok, "{content}");
        assert_eq!(content, "Body content here.\n");
    }

    #[tokio::test]
    async fn load_skill_tool_rejects_a_disabled_skill() {
        let _home = TempHome::new();
        create_from_markdown(VALID_MD).unwrap();
        set_enabled("test-skill", false).unwrap();

        let (content, step) = crate::tools::execute(
            "load_skill",
            r#"{"name":"test-skill"}"#,
            &std::env::temp_dir(),
            crate::tools::Permission::ReadOnly,
            &crate::mcp::McpRegistry::new(),
            "dsh",
        )
        .await;
        assert!(!step.ok);
        assert!(content.contains("disabled"));
    }
}
