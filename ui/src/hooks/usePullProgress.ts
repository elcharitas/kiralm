import { useEffect, useState } from "react";
import { api, subscribeSse } from "../api";

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
    let close = () => {};
    close = subscribeSse(api.pullProgressUrl(id), (raw) => {
      try {
        const data = JSON.parse(raw) as PullProgress;
        setProgress(data);
        if (TERMINAL.includes(data.status)) close();
      } catch {
        // ignore malformed frame, next one carries fresh state
      }
    }, () => close());
    return close;
  }, [id]);

  return progress;
}
