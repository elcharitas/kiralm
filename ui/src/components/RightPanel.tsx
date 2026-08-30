import { useRef, useState } from "react";
import type { PointerEvent, ReactNode } from "react";
import {
  Braces,
  ChevronRight,
  FileCode,
  FileCode2,
  GitBranch,
  File as FileIcon,
  Image,
  MemoryStick,
  Palette,
  PanelRightClose,
  Plug,
  Plus,
  Puzzle,
  Shapes,
  Sparkles,
  User,
} from "lucide-react";
import type { StatusResponse } from "../api";
import type { ChatSession } from "../sessions";
import { turnFileDiffsOf, type DiffLine, type TurnFileDiff } from "../diff";
import { formatBytes, formatBytesPair } from "../format";
import { Dial } from "./Dial";
import { GitPanel } from "./GitPanel";
import { McpPanel } from "./McpPanel";
import { PluginsPanel } from "./PluginsPanel";
import { Select } from "./Select";
import { SkillsPanel } from "./SkillsPanel";

// Min height (px) either half of the Source tab's split can be dragged
// to, so the handle can't collapse a section to zero/negative.
const SPLIT_MIN_SECTION = 96;
const SPLIT_HANDLE_HEIGHT = 22;

// Pointer-driven vertical split for the Source tab: git panel on top,
// diff view below, resized by dragging the handle between them.
function SourceSplit({ top, bottom }: { top: ReactNode; bottom: ReactNode }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [topRatio, setTopRatio] = useState(0.5);
  const dragging = useRef(false);

  const onDown = (e: PointerEvent<HTMLDivElement>) => {
    dragging.current = true;
    document.body.classList.add("source-split-resizing");
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onMove = (e: PointerEvent<HTMLDivElement>) => {
    if (!dragging.current || !containerRef.current) return;
    const rect = containerRef.current.getBoundingClientRect();
    const available = rect.height - SPLIT_HANDLE_HEIGHT;
    const max = available - SPLIT_MIN_SECTION;
    const next = Math.min(
      max,
      Math.max(SPLIT_MIN_SECTION, e.clientY - rect.top),
    );
    setTopRatio(next / available);
  };
  const onUp = (e: PointerEvent<HTMLDivElement>) => {
    dragging.current = false;
    document.body.classList.remove("source-split-resizing");
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
  };

  return (
    <div className="git-source-split" ref={containerRef}>
      <div
        className="git-source-split-top"
        style={{
          height: `calc(${topRatio * 100}% - ${SPLIT_HANDLE_HEIGHT / 2}px)`,
        }}
      >
        {top}
      </div>
      <div
        className="git-source-split-handle"
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
      />
      <div className="git-source-split-bottom">{bottom}</div>
    </div>
  );
}

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

  const [tab, setTab] = useState<string>("plugins");
  const activeTab =
    tab === "skills" || tab === "mcp" || tab === "plugins" || files.some((f) => f.filename === tab)
      ? tab
      : "source";
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
            activeTab === "plugins"
              ? "right-panel-tab right-panel-tab-diff right-panel-tab-active"
              : "right-panel-tab right-panel-tab-diff"
          }
          onClick={() => setTab("plugins")}
          title="Plugins"
        >
          <Puzzle size={14} /> <span className="right-panel-tab-label">Plugins</span>
        </button>
        <button
          type="button"
          className={
            activeTab === "skills"
              ? "right-panel-tab right-panel-tab-diff right-panel-tab-active"
              : "right-panel-tab right-panel-tab-diff"
          }
          onClick={() => setTab("skills")}
          title="Skills"
        >
          <Sparkles size={14} /> <span className="right-panel-tab-label">Skills</span>
        </button>
        <button
          type="button"
          className={
            activeTab === "mcp"
              ? "right-panel-tab right-panel-tab-diff right-panel-tab-active"
              : "right-panel-tab right-panel-tab-diff"
          }
          onClick={() => setTab("mcp")}
          title="MCP"
        >
          <Plug size={14} /> <span className="right-panel-tab-label">MCP</span>
        </button>
        <button
          type="button"
          className={
            activeTab === "source"
              ? "right-panel-tab right-panel-tab-diff right-panel-tab-active"
              : "right-panel-tab right-panel-tab-diff"
          }
          onClick={() => setTab("source")}
          title="Source"
        >
          <GitBranch size={14} /> <span className="right-panel-tab-label">Source</span>
        </button>
        <div className="right-panel-tab-actions">
          <button type="button" className="right-panel-icon-btn" title="Collapse" onClick={() => onModeChange("collapsed")}>
            <PanelRightClose size={14} />
          </button>
          <button type="button" className="right-panel-icon-btn" title="Account">
            <User size={14} />
          </button>
        </div>
      </div>

      {activeTab !== "skills" && activeTab !== "mcp" && activeTab !== "plugins" && turns.length > 0 && (
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
          </div>
        </div>
      )}

      <div className="right-panel-body">
        {activeTab === "skills" ? (
          <SkillsPanel />
        ) : activeTab === "mcp" ? (
          <McpPanel />
        ) : activeTab === "plugins" ? (
          <PluginsPanel />
        ) : (
          <SourceSplit
            top={<GitPanel projectId={session?.projectId} />}
            bottom={
              files.length === 0 ? (
                <p className="settings-hint">No changed files in this turn.</p>
              ) : activeTab === "source" ? (
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
              )
            }
          />
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
        <span className="right-panel-ram-label mono">{formatBytesPair(usedBytes, totalBytes)}</span>
      </div>
    </div>
  );
}

// Collapsed-panel equivalent of RamFooter's bar -- a small radial
// gauge instead, same idea as the sidebar's own collapsed icon rail:
// one glanceable indicator, no room for the labeled bar.
function RamDial({ status }: { status: StatusResponse }) {
  const { usedBytes, totalBytes, availableBytes, percent } = ramUsage(status);
  return (
    <Dial percent={percent} title={`${formatBytes(usedBytes)} / ${formatBytes(totalBytes)} (${formatBytes(availableBytes)} available)`} />
  );
}
