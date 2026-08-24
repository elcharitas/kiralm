import { useEffect, useState } from "react";
import { api } from "../api";

export interface PullProgress {
  status: "downloading" | "verifying" | "loading" | "done" | "error";
  bytes_downloaded: number;
  total_bytes: number;
  message?: string;
}

const TERMINAL: PullProgress["status"][] = ["done", "error"];

// Subscribes to the SSE download-progress stream for one model while it's
// active; closes itself once the stream reports a terminal status.
export function usePullProgress(id: string | null) {
  const [progress, setProgress] = useState<PullProgress | null>(null);

  useEffect(() => {
    if (!id) {
      setProgress(null);
      return;
    }
    const source = new EventSource(api.pullProgressUrl(id));
    source.onmessage = (ev) => {
      try {
        const data = JSON.parse(ev.data) as PullProgress;
        setProgress(data);
        if (TERMINAL.includes(data.status)) source.close();
      } catch {
        // ignore malformed frame, next one carries fresh state
      }
    };
    source.onerror = () => source.close();
    return () => source.close();
  }, [id]);

  return progress;
}
