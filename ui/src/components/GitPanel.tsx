import { useEffect, useState } from "react";
import { GitCommit } from "lucide-react";
import { api, type GitLogEntry } from "../api";
import { formatRelativeTime } from "../format";
import { useDialogs } from "./Dialogs";

// Real commit history + real commit creation for the active project's
// directory -- no branch switching/push/pull/per-file staging, this is
// scoped to the two capabilities the panel actually offers.
export function GitPanel({ projectId }: { projectId: string | undefined }) {
  const [commits, setCommits] = useState<GitLogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [message, setMessage] = useState("");
  const [committing, setCommitting] = useState(false);
  const { toast } = useDialogs();

  const load = () => {
    if (!projectId) {
      setCommits([]);
      return Promise.resolve();
    }
    return api.gitLog(projectId).then(
      (d) => setCommits(d.commits),
      () => setCommits([]),
    );
  };

  useEffect(() => {
    setLoading(true);
    load().finally(() => setLoading(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId]);

  const commit = async () => {
    if (!projectId || !message.trim() || committing) return;
    setCommitting(true);
    try {
      await api.gitCommit(projectId, message.trim());
      setMessage("");
      await load();
      toast("Commit created", "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setCommitting(false);
    }
  };

  if (!projectId) {
    return <p className="settings-hint">Open a workspace to see its git history.</p>;
  }

  return (
    <div className="git-panel">
      <div className="git-panel-commit-form">
        <input
          className="mcp-field mono"
          placeholder="Commit message"
          value={message}
          disabled={committing}
          onChange={(e) => setMessage(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
          }}
        />
        <button
          type="button"
          className="btn btn-accent"
          disabled={!message.trim() || committing}
          onClick={commit}
        >
          <GitCommit size={14} />Commit
        </button>
      </div>

      {loading ? (
        <p className="settings-hint">Loading…</p>
      ) : commits.length === 0 ? (
        <p className="settings-hint">No commits yet.</p>
      ) : (
        <ul className="git-log-list">
          {commits.map((c) => (
            <li key={c.hash} className="git-log-row" title={c.hash}>
              <span className="mono git-log-hash">{c.short_hash}</span>
              <span className="git-log-message truncate">{c.message}</span>
              <span className="settings-hint git-log-meta truncate">
                {c.author} · {formatRelativeTime(c.date)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
