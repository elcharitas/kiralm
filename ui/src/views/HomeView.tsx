import { useState } from "react";
import { FolderPlus, GitBranch, Loader2 } from "lucide-react";
import type { Project } from "../api";
import { AppLogo } from "../components/AppLogo";
import { useDialogs } from "../components/Dialogs";

// Default landing page when no chat is selected -- register an
// existing local workspace or clone one from git. The chat view only
// shows once there's a real active session (see App.tsx's `view`
// routing).
export function HomeView({
  onCreateProject,
  onCloneProject,
  onOpenProject,
}: {
  onCreateProject: (path: string, name?: string) => Promise<Project>;
  onCloneProject: (url: string, path: string, name?: string) => Promise<Project>;
  onOpenProject: (project: Project) => void;
}) {
  const { toast, browseFolder } = useDialogs();
  const [cloneOpen, setCloneOpen] = useState(false);
  const [cloneUrl, setCloneUrl] = useState("");
  const [clonePath, setClonePath] = useState("");
  const [cloning, setCloning] = useState(false);

  const newProject = async () => {
    const path = await browseFolder({ title: "New workspace" });
    if (!path || !path.trim()) return;
    try {
      const project = await onCreateProject(path.trim());
      onOpenProject(project);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  const pullFromGit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!cloneUrl.trim() || !clonePath.trim()) return;
    setCloning(true);
    try {
      const project = await onCloneProject(cloneUrl.trim(), clonePath.trim());
      toast(`Cloned into ${project.path}`, "success");
      setCloneUrl("");
      setClonePath("");
      setCloneOpen(false);
      onOpenProject(project);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setCloning(false);
    }
  };

  return (
    <div className="home-view">
      <AppLogo className="home-logo" />
      <div className="home-cards">
        <button type="button" className="home-card" onClick={newProject}>
          <FolderPlus size={20} className="home-card-icon" />
          <span className="home-card-title">New workspace</span>
          <span className="home-card-hint">Register a local folder</span>
        </button>

        <button type="button" className="home-card" onClick={() => setCloneOpen(true)}>
          <GitBranch size={20} className="home-card-icon" />
          <span className="home-card-title">Pull from git</span>
          <span className="home-card-hint">Clone a repo by URL</span>
        </button>
      </div>

      {cloneOpen && (
        <div className="modal-overlay" onMouseDown={() => !cloning && setCloneOpen(false)}>
          <form className="modal" onMouseDown={(e) => e.stopPropagation()} onSubmit={pullFromGit}>
            <h2 className="modal-title">Pull from git</h2>
            <input
              autoFocus
              className="modal-input"
              placeholder="https://github.com/org/repo.git"
              value={cloneUrl}
              onChange={(e) => setCloneUrl(e.target.value)}
            />
            <input
              className="modal-input"
              placeholder="/Users/you/code/repo"
              value={clonePath}
              onChange={(e) => setClonePath(e.target.value)}
            />
            <div className="modal-actions">
              <button type="button" className="btn" disabled={cloning} onClick={() => setCloneOpen(false)}>
                Cancel
              </button>
              <button type="submit" className="btn btn-accent" disabled={cloning || !cloneUrl.trim() || !clonePath.trim()}>
                {cloning ? <Loader2 size={14} className="spin-icon" /> : "Clone"}
              </button>
            </div>
          </form>
        </div>
      )}
    </div>
  );
}
