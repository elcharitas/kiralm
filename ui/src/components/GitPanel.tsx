import { useCallback, useEffect, useState } from "react";
import { GitBranch, GitCommit, Plus, RefreshCw } from "lucide-react";
import {
  api,
  type GitBranchInfo,
  type GitDiffPreview,
  type GitLogEntry,
  type GitRepositoryState,
} from "../api";
import { formatRelativeTime } from "../format";
import { Select } from "./Select";
import { useDialogs } from "./Dialogs";

export function GitPanel({
  projectId,
  refreshVersion,
  gitBusy,
  onGitChanged,
}: {
  projectId: string | undefined;
  refreshVersion: number;
  gitBusy: boolean;
  onGitChanged: () => Promise<void>;
}) {
  const [state, setState] = useState<GitRepositoryState | null>(null);
  const [branches, setBranches] = useState<GitBranchInfo[]>([]);
  const [commits, setCommits] = useState<GitLogEntry[]>([]);
  const [diff, setDiff] = useState<GitDiffPreview | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [branchName, setBranchName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const { toast } = useDialogs();

  const load = useCallback(async () => {
    if (!projectId) {
      setState(null);
      setBranches([]);
      setCommits([]);
      setDiff(null);
      return;
    }
    try {
      const [nextState, branchResult, logResult, nextDiff] = await Promise.all([
        api.gitWorkflowState(projectId),
        api.gitWorkflowBranches(projectId),
        api.gitLog(projectId),
        api.gitWorkflowDiff(projectId),
      ]);
      setState(nextState);
      setBranches(branchResult.branches);
      setCommits(logResult.commits);
      setDiff(nextDiff);
      setError(null);
    } catch (caught) {
      setState(null);
      setBranches([]);
      setDiff(null);
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }, [projectId]);

  useEffect(() => {
    setLoading(true);
    void load().finally(() => setLoading(false));
  }, [load, refreshVersion]);

  const mutate = async (operation: () => Promise<unknown>, success: string) => {
    if (busy || gitBusy) return false;
    setBusy(true);
    try {
      await operation();
      await Promise.all([load(), onGitChanged()]);
      toast(success, "success");
      return true;
    } catch (caught) {
      toast(caught instanceof Error ? caught.message : String(caught), "error");
      return false;
    } finally {
      setBusy(false);
    }
  };

  const commit = async () => {
    if (!projectId || !message.trim()) return;
    const committed = await mutate(
      () => api.gitWorkflowCommit(projectId, message.trim()),
      "Commit created",
    );
    if (committed) setMessage("");
  };

  const createBranch = async () => {
    if (!projectId || !branchName.trim()) return;
    const created = await mutate(
      () => api.gitWorkflowCreateBranch(projectId, branchName.trim()),
      "Branch created",
    );
    if (created) setBranchName("");
  };

  if (!projectId) {
    return <p className="settings-hint">Open a workspace to use Git workflows.</p>;
  }
  if (loading) {
    return <p className="settings-hint">Loading Git state...</p>;
  }
  if (error || !state) {
    return <p className="settings-hint">{error ?? "Git state is unavailable."}</p>;
  }

  const hasStaged = state.files.some((file) => file.index_status !== null);

  return (
    <div className="git-panel">
      <div className="git-panel-toolbar">
        <Select
          triggerClassName="git-branch-select"
          value={state.branch ?? ""}
          onChange={(name) => {
            if (!projectId || !name || name === state.branch) return;
            void mutate(
              () => api.gitWorkflowSwitchBranch(projectId, name),
              `Switched to ${name}`,
            );
          }}
          disabled={busy || gitBusy}
          icon={<GitBranch size={13} />}
          options={branches.map((branch) => ({
            value: branch.name,
            label: branch.name,
          }))}
        />
        <button
          type="button"
          className="right-panel-icon-btn"
          title="Refresh Git state"
          disabled={busy || gitBusy}
          onClick={() => void load()}
        >
          <RefreshCw size={13} />
        </button>
      </div>

      <div className="git-panel-branch-form">
        <input
          className="mcp-field mono"
          placeholder="New branch name"
          value={branchName}
          disabled={busy || gitBusy}
          onChange={(event) => setBranchName(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") void createBranch();
          }}
        />
        <button
          type="button"
          className="btn"
          title="Create branch"
          disabled={!branchName.trim() || busy || gitBusy}
          onClick={() => void createBranch()}
        >
          <Plus size={13} /> Create
        </button>
      </div>

      {state.conflicted && (
        <p className="git-conflict-note">
          Resolve conflicts in the listed files before switching or committing.
        </p>
      )}

      <div className="git-change-list">
        {state.files.length === 0 ? (
          <p className="settings-hint">Working tree clean.</p>
        ) : (
          state.files.map((file) => (
            <div className="git-change-row" key={`${file.path}:${file.original_path ?? ""}`}>
              <div className="git-change-path">
                <span className="mono truncate" title={file.path}>{file.path}</span>
                <span className="settings-hint">
                  {file.conflicted
                    ? "conflict"
                    : [
                        file.index_status ? `staged ${file.index_status}` : null,
                        file.worktree_status ? `working ${file.worktree_status}` : null,
                      ].filter(Boolean).join(" · ")}
                </span>
              </div>
              <div className="git-change-actions">
                {file.worktree_status && (
                  <button
                    type="button"
                    className="btn"
                    disabled={busy || gitBusy || file.conflicted}
                    onClick={() =>
                      void mutate(
                        () => api.gitWorkflowStage(projectId, [file.path]),
                        `Staged ${file.path}`,
                      )
                    }
                  >
                    Stage
                  </button>
                )}
                {file.index_status && (
                  <button
                    type="button"
                    className="btn"
                    disabled={busy || gitBusy || file.conflicted}
                    onClick={() =>
                      void mutate(
                        () => api.gitWorkflowUnstage(projectId, [file.path]),
                        `Unstaged ${file.path}`,
                      )
                    }
                  >
                    Unstage
                  </button>
                )}
              </div>
            </div>
          ))
        )}
      </div>

      {diff?.text && (
        <details className="git-diff-preview">
          <summary>
            Working diff{diff.truncated ? " (truncated)" : ""}
          </summary>
          <pre>{diff.text}</pre>
        </details>
      )}

      <div className="git-panel-commit-form">
        <input
          className="mcp-field mono"
          placeholder="Commit staged changes"
          value={message}
          disabled={busy || gitBusy}
          onChange={(event) => setMessage(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") void commit();
          }}
        />
        <button
          type="button"
          className="btn btn-accent"
          disabled={
            !message.trim() ||
            !hasStaged ||
            busy ||
            gitBusy ||
            state.conflicted
          }
          onClick={() => void commit()}
        >
          <GitCommit size={14} /> Commit
        </button>
      </div>

      {commits.length > 0 && (
        <ul className="git-log-list">
          {commits.map((entry) => (
            <li key={entry.hash} className="git-log-row" title={entry.hash}>
              <span className="mono git-log-hash">{entry.short_hash}</span>
              <span className="git-log-message truncate">{entry.message}</span>
              <span className="settings-hint git-log-meta truncate">
                {entry.author} · {formatRelativeTime(entry.date)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
