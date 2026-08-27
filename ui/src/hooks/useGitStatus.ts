import { useCallback, useEffect, useState } from "react";
import { api, type GitStatus } from "../api";

// Real git status (branch + dirty files) for whichever project is
// active, re-fetched whenever the project changes or `refresh` is
// called (e.g. right after a commit). `null` for no active project.
export function useGitStatus(projectId: string | undefined) {
  const [status, setStatus] = useState<GitStatus | null>(null);

  const refresh = useCallback(async () => {
    if (!projectId) {
      setStatus(null);
      return;
    }
    const s = await api.gitStatus(projectId).catch(() => null);
    setStatus(s);
  }, [projectId]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return { status, refresh };
}
