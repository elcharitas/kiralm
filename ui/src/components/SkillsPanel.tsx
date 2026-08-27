import { useEffect, useState } from "react";
import { FolderOpen, Plus, Trash2, X } from "lucide-react";
import { api, type Skill } from "../api";
import { useDialogs } from "./Dialogs";

// Global skill management: list + enable toggle + add (paste or
// folder) + a read-only viewer + delete. No per-workspace scoping --
// skills live in ~/.kiralm/skills, same set regardless of which
// project's chat this panel is opened from.
export function SkillsPanel() {
  const [skills, setSkills] = useState<Skill[]>([]);
  const [loading, setLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [pasteValue, setPasteValue] = useState("");
  const [viewing, setViewing] = useState<Skill | null>(null);
  const { toast, confirm, browseFolder } = useDialogs();

  const load = () =>
    api.listSkills().then(
      (d) => setSkills(d.skills),
      () => {},
    );

  useEffect(() => {
    load().finally(() => setLoading(false));
  }, []);

  const addFromMarkdown = async () => {
    if (!pasteValue.trim()) return;
    try {
      await api.createSkillFromMarkdown(pasteValue);
      setPasteValue("");
      setAdding(false);
      await load();
      toast("Skill added", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const addFromFolder = async () => {
    const path = await browseFolder({ title: "Add skill from folder" });
    if (!path) return;
    try {
      await api.createSkillFromFolder(path);
      await load();
      toast("Skill added", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const toggle = async (skill: Skill) => {
    const next = !skill.enabled;
    setSkills((prev) => prev.map((s) => (s.name === skill.name ? { ...s, enabled: next } : s)));
    try {
      await api.setSkillEnabled(skill.name, next);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
      await load();
    }
  };

  const remove = async (skill: Skill) => {
    const ok = await confirm({
      title: "Delete skill",
      message: `Delete "${skill.name}"? This can't be undone.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteSkill(skill.name);
      if (viewing?.name === skill.name) setViewing(null);
      await load();
      toast("Skill deleted", "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  if (viewing) {
    return (
      <div className="skills-panel">
        <div className="skills-viewer-head">
          <button type="button" className="right-panel-icon-btn" title="Back" onClick={() => setViewing(null)}>
            <X size={14} />
          </button>
          <span className="mono truncate">{viewing.name}</span>
        </div>
        <p className="settings-hint">{viewing.description}</p>
        <pre className="changed-block-code mono skills-viewer-body">{viewing.body}</pre>
      </div>
    );
  }

  return (
    <div className="skills-panel">
      <div className="skills-panel-actions">
        <button type="button" className="btn" onClick={() => setAdding((v) => !v)}>
          <Plus size={14} /> Paste markdown
        </button>
        <button type="button" className="btn" onClick={addFromFolder}>
          <FolderOpen size={14} /> From folder
        </button>
      </div>

      {adding && (
        <div className="skills-add-form">
          <textarea
            autoFocus
            className="skills-paste-input mono"
            placeholder={"---\nname: my-skill\ndescription: What this skill does.\n---\nInstructions..."}
            value={pasteValue}
            onChange={(e) => setPasteValue(e.target.value)}
          />
          <div className="modal-actions">
            <button type="button" className="btn" onClick={() => setAdding(false)}>
              Cancel
            </button>
            <button type="button" className="btn btn-accent" disabled={!pasteValue.trim()} onClick={addFromMarkdown}>
              Add skill
            </button>
          </div>
        </div>
      )}

      {loading ? (
        <p className="settings-hint">Loading…</p>
      ) : skills.length === 0 ? (
        <p className="settings-hint">No skills installed yet.</p>
      ) : (
        <ul className="skills-list">
          {skills.map((s) => (
            <li key={s.name} className="skills-row">
              <button type="button" className="skills-row-main" onClick={() => setViewing(s)}>
                <span className="mono truncate">{s.name}</span>
                <span className="settings-hint skills-row-desc truncate">{s.description}</span>
              </button>
              <label className="switch-input-wrap" onClick={(e) => e.stopPropagation()}>
                <input type="checkbox" className="switch-input" checked={s.enabled} onChange={() => toggle(s)} />
              </label>
              <button type="button" className="right-panel-icon-btn" title="Delete" onClick={() => remove(s)}>
                <Trash2 size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
