import { useCallback, useEffect, useRef, useState } from "react";
import { api, type StatusResponse } from "../api";

// Polls GET /api/status every 2s, paused while the tab is hidden
// (Page Visibility API), per SPEC §9 sync rules. Exposes a manual
// `refresh` for immediate feedback after a mutating action.
export function useStatus() {
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const timer = useRef<number | undefined>(undefined);

  const tick = useCallback(async () => {
    if (document.hidden) return;
    try {
      const s = await api.status();
      setStatus(s);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    tick();
    timer.current = window.setInterval(tick, 2000);
    const onVisible = () => {
      if (!document.hidden) tick();
    };
    document.addEventListener("visibilitychange", onVisible);

    return () => {
      window.clearInterval(timer.current);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [tick]);

  return { status, error, refresh: tick };
}
