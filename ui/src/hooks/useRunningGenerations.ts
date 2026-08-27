import { useEffect, useState } from "react";
import { api } from "../api";

// Polls which sessions have generation actually running server-side, so
// the sidebar's status dots reflect real background state for every
// session (started from this tab or not), not just whichever one this
// tab happens to be actively watching.
export function useRunningGenerations() {
  const [running, setRunning] = useState<Set<string>>(new Set());

  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      const res = await api.runningGenerations().catch(() => null);
      if (!cancelled && res) setRunning(new Set(res.running));
    };
    tick();
    const id = window.setInterval(tick, 1500);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  return running;
}
