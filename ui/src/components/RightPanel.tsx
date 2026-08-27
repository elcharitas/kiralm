import { useState } from "react";
import {
  Braces,
  ChevronRight,
  Columns2,
  FileCode,
  FileCode2,
  FileDiff,
  File as FileIcon,
  Image,
  List,
  MemoryStick,
  Palette,
  PanelRightClose,
  Pilcrow,
  Plus,
  Shapes,
  User,
  WrapText,
  X,
} from "lucide-react";
import type { StatusResponse } from "../api";
import type { ChatSession } from "../sessions";
import { turnFileDiffsOf, type DiffLine, type TurnFileDiff } from "../diff";
import { formatBytes } from "../format";
import { Select } from "./Select";

export type RightPanelMode = "split" | "collapsed";

// Colors mirror each type's real brand/file-kind tone, not a flat
// monochrome set, so tabs read at a glance like a real editor.
function fileIcon(name: string) {
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  if (["png", "jpg", "jpeg", "gif", "webp"].includes(ext))
    return <Image size={14} color="#e0a45a" />;
  if (ext === "svg") return <Shapes size={14} color="#ff8a4c" />;
  if (["ts", "tsx"].includes(ext)) return <FileCode2 size={14} color="#4ea1e8" />;
  if (["js", "jsx"].includes(ext)) return <FileCode size={14} color="#e8d44e" />;
  if (ext === "css") return <Palette size={14} color="#b06ee8" />;
  if (ext === "json") return <Braces size={14} color="#e0a45a" />;
  return <FileIcon size={14} />;
}

function DiffBody({ lines }: { lines: DiffLine[] }) {
  return (
    <pre className="changed-block-code mono file-row-detail diff-body">
      {lines.map((l, i) => (
        <div key={i} className={`diff-line diff-line-${l.type}`}>
          <span className="diff-line-marker">{l.type === "add" ? "+" : l.type === "del" ? "-" : " "}</span>
          {l.text}
        </div>
      ))}
    </pre>
  );
}

function FileRow({ f, open, onToggle }: { f: TurnFileDiff; open: boolean; onToggle: () => void }) {
  return (
    <li className="file-row">
      <button type="button" className="file-row-head" onClick={onToggle}>
        <span className={open ? "file-chevron file-chevron-open" : "file-chevron"}>
          <ChevronRight size={14} />
        </span>
        <span className="file-added-icon">
          <Plus size={12} />
        </span>
        <span className="file-name mono truncate">{f.filename}</span>
        <span className="file-stats">
          <span className="stat-del">-{f.removed}</span>
          <span className="stat-add">+{f.added}</span>
        </span>
      </button>
      {open && <DiffBody lines={f.lines} />}
    </li>
  );
}

// Tabs reflect the real files this turn's write_file calls actually
// touched (before/after content from the daemon), not the pulled-model
// list — that lives in Settings > Models now.
export function RightPanel({
  status,
  session,
  mode,
  onModeChange,
}: {
  status: StatusResponse | null;
  session: ChatSession | null;
  mode: RightPanelMode;
  onModeChange: (mode: RightPanelMode) => void;
}) {
  const turns = session?.messages.filter((m) => m.role === "assistant") ?? [];
  const [turnIdx, setTurnIdx] = useState(0);
  const activeTurn = turns[Math.min(turnIdx, turns.length - 1)];
  const files = turnFileDiffsOf(activeTurn?.steps);

  const [tab, setTab] = useState<string>("diff");
  const activeTab = files.some((f) => f.filename === tab) ? tab : "diff";
  const [open, setOpen] = useState<Record<string, boolean>>({});

  // Collapsed = a slim rail with just the reopen control, same pattern
  // as the sidebar's collapse, rather than squeezing tabs/diffs into
  // ~56px.
  if (mode === "collapsed") {
    return (
      <aside className="right-panel right-panel-collapsed">
        <button type="button" className="right-panel-icon-btn" title="Expand" onClick={() => onModeChange("split")}>
          <PanelRightClose size={16} style={{ transform: "rotate(180deg)" }} />
        </button>
        <div className="sidebar-collapsed-spacer" />
        {status && <RamDial status={status} />}
      </aside>
    );
  }

  return (
    <aside className="right-panel">
      <div className="right-panel-tabs">
        {files.map((f) => (
          <button
            type="button"
            key={f.filename}
            className={activeTab === f.filename ? "right-panel-tab right-panel-tab-active" : "right-panel-tab"}
            onClick={() => setTab(f.filename)}
            title={f.filename}
          >
            <span className="right-panel-tab-icon">{fileIcon(f.filename)}</span>
            <span className="mono truncate">{f.filename.split("/").pop()}</span>
          </button>
        ))}
        <button
          type="button"
          className={
            activeTab === "diff"
              ? "right-panel-tab right-panel-tab-diff right-panel-tab-active"
              : "right-panel-tab right-panel-tab-diff"
          }
          onClick={() => setTab("diff")}
        >
          <FileDiff size={14} /> Diff
        </button>
        <div className="right-panel-tab-actions">
          <button type="button" className="right-panel-icon-btn" title="Collapse" onClick={() => onModeChange("collapsed")}>
            <PanelRightClose size={14} />
          </button>
          <button
            type="button"
            className={mode === "split" ? "right-panel-icon-btn right-panel-icon-btn-active" : "right-panel-icon-btn"}
            title="Split view"
            onClick={() => onModeChange("split")}
          >
            <Columns2 size={14} />
          </button>
          <button type="button" className="right-panel-icon-btn" title="Account">
            <User size={14} />
          </button>
        </div>
      </div>

      {turns.length > 0 && (
        <div className="right-panel-selector">
          <Select
            triggerClassName="chip-select"
            value={String(turnIdx)}
            onChange={(v) => setTurnIdx(Number(v))}
            options={turns.map((_, i) => ({ value: String(i), label: `Turn ${i + 1}` }))}
          />
          <div className="right-panel-turn-stats">
            <span className="stat-add">
              +{files.reduce((n, f) => n + f.added, 0)}
            </span>
            <span className="stat-del">-{files.reduce((n, f) => n + f.removed, 0)}</span>
            <button type="button" className="right-panel-icon-btn" title="Selection">
              <X size={14} />
            </button>
            <button type="button" className="right-panel-icon-btn" title="List view">
              <List size={14} />
            </button>
            <button type="button" className="right-panel-icon-btn" title="Split view">
              <Columns2 size={14} />
            </button>
            <button type="button" className="right-panel-icon-btn" title="Wrap">
              <WrapText size={14} />
            </button>
            <button type="button" className="right-panel-icon-btn right-panel-icon-btn-active" title="Paragraph marks">
              <Pilcrow size={14} />
            </button>
          </div>
        </div>
      )}

      <div className="right-panel-body">
        {files.length === 0 ? (
          <p className="settings-hint">No changed files in this turn.</p>
        ) : activeTab === "diff" ? (
          <ul className="file-list">
            {files.map((f) => (
              <FileRow
                key={f.filename}
                f={f}
                open={!!open[f.filename]}
                onToggle={() => setOpen((o) => ({ ...o, [f.filename]: !o[f.filename] }))}
              />
            ))}
          </ul>
        ) : (
          (() => {
            const f = files.find((x) => x.filename === activeTab)!;
            return (
              <>
                <div className="file-stats file-stats-standalone">
                  <span className="stat-add">+{f.added}</span>
                  <span className="stat-del">-{f.removed}</span>
                </div>
                <DiffBody lines={f.lines} />
              </>
            );
          })()
        )}
      </div>

      {status && <RamFooter status={status} />}
    </aside>
  );
}

// Real RAM usage from the same /api/status the daemon computes
// residency decisions from, not a re-derived estimate. Pinned at the
// bottom like the primary sidebar's Settings icon, not scrolled away
// with the diff body above it.
function ramUsage(status: StatusResponse) {
  const { total_bytes, available_bytes } = status.ram;
  const usedBytes = Math.max(0, total_bytes - available_bytes);
  const percent = total_bytes > 0 ? Math.min(100, (usedBytes / total_bytes) * 100) : 0;
  return { usedBytes, totalBytes: total_bytes, availableBytes: available_bytes, percent };
}

function RamFooter({ status }: { status: StatusResponse }) {
  const { usedBytes, totalBytes, availableBytes, percent } = ramUsage(status);
  return (
    <div className="right-panel-footer" title={`${formatBytes(availableBytes)} available`}>
      <MemoryStick size={14} className="right-panel-footer-icon" />
      <div className="right-panel-ram">
        <div className="progress-track right-panel-ram-track">
          <div className="progress-fill" style={{ width: `${percent}%` }} />
        </div>
        <span className="right-panel-ram-label mono">
          {formatBytes(usedBytes)} / {formatBytes(totalBytes)}
        </span>
      </div>
    </div>
  );
}

// Collapsed-panel equivalent of RamFooter's bar -- a small radial
// gauge instead, same idea as the sidebar's own collapsed icon rail:
// one glanceable indicator, no room for the labeled bar.
function RamDial({ status }: { status: StatusResponse }) {
  const { usedBytes, totalBytes, availableBytes, percent } = ramUsage(status);
  const r = 8;
  const circumference = 2 * Math.PI * r;
  const offset = circumference * (1 - percent / 100);
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" className="right-panel-ram-dial">
      <title>{`${formatBytes(usedBytes)} / ${formatBytes(totalBytes)} (${formatBytes(availableBytes)} available)`}</title>
      <circle cx="10" cy="10" r={r} className="right-panel-ram-dial-track" strokeWidth="2.5" fill="none" />
      <circle
        cx="10"
        cy="10"
        r={r}
        className="right-panel-ram-dial-fill"
        strokeWidth="2.5"
        fill="none"
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform="rotate(-90 10 10)"
      />
    </svg>
  );
}
