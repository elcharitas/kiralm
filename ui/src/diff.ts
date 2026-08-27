// Parses fenced code blocks out of an assistant reply so a real
// per-turn "changed files" list can be shown. A block counts as a
// changed file when it names one, via ```lang:path/to/file.ext or a
// leading "// path/to/file.ext" comment line — otherwise it's just
// an anonymous code block. There's no prior version to diff against
// (this is a chat reply, not a repo mutation), so every line in a
// named block counts as an addition.
export interface ParsedBlock {
  filename: string | null;
  body: string;
}

function parseBlocks(text: string): ParsedBlock[] {
  const blocks: ParsedBlock[] = [];
  const fenceRe = /```([^\n`]*)\n([\s\S]*?)```/g;
  let match: RegExpExecArray | null;
  while ((match = fenceRe.exec(text))) {
    const info = match[1].trim();
    let body = match[2];
    let filename: string | null = null;
    if (info.includes(":")) {
      const path = info.split(":", 2)[1]?.trim();
      filename = path || null;
    } else {
      const firstLine = body.split("\n", 1)[0];
      const m = firstLine.match(/^(?:\/\/|#|<!--)\s*([\w./-]+\.\w{1,8})/);
      if (m) {
        filename = m[1];
        body = body.slice(firstLine.length + 1);
      }
    }
    blocks.push({ filename, body });
  }
  return blocks;
}

export interface ChangedFile {
  filename: string;
  added: number;
  body: string;
}

export function changedFilesOf(text: string): ChangedFile[] {
  const files = new Map<string, ChangedFile>();
  for (const b of parseBlocks(text)) {
    if (!b.filename) continue;
    const added = b.body.split("\n").filter((l) => l.trim().length > 0).length;
    const existing = files.get(b.filename);
    if (existing) {
      existing.added += added;
      existing.body += `\n${b.body}`;
    } else {
      files.set(b.filename, { filename: b.filename, added, body: b.body });
    }
  }
  return [...files.values()];
}

export function codeBlockCountOf(text: string): number {
  return parseBlocks(text).filter((b) => !b.filename).length;
}

// Prose-only view of a reply: every fenced block (named or not) is
// rendered by its own collapsible component, so raw fences and
// language:filename tags shouldn't also leak into the plain-text turn.
export function stripCodeBlocks(text: string): string {
  return text
    .replace(/```[^\n`]*\n[\s\S]*?```/g, "")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

// Real per-turn diffs, built from the daemon's actual write_file
// before/after content (src/tools.rs's FileEdit), not parsed chat text.
import { diffLines as jsDiffLines } from "diff";
import type { ToolStep } from "./api";

export interface DiffLine {
  type: "add" | "del" | "ctx";
  text: string;
}

// Line diff via jsdiff -- each hunk's value is exploded back into the
// per-line shape the UI renders (one row per source/dest line).
export function diffLines(before: string, after: string): DiffLine[] {
  const lines: DiffLine[] = [];
  for (const part of jsDiffLines(before, after)) {
    const type = part.added ? "add" : part.removed ? "del" : "ctx";
    const partLines = part.value.split("\n");
    if (partLines[partLines.length - 1] === "") partLines.pop();
    for (const text of partLines) lines.push({ type, text });
  }
  return lines;
}

export interface TurnFileDiff {
  filename: string;
  lines: DiffLine[];
  added: number;
  removed: number;
}

// One entry per path actually touched by write_file this turn. Several
// writes to the same path within one turn collapse into a single
// before(first)->after(last) diff, the real net change for the turn.
export function turnFileDiffsOf(steps: ToolStep[] | undefined): TurnFileDiff[] {
  const edits = new Map<string, { before: string | null; after: string }>();
  for (const s of steps ?? []) {
    if (!s.file_edit) continue;
    const existing = edits.get(s.file_edit.path);
    edits.set(s.file_edit.path, {
      before: existing ? existing.before : s.file_edit.before,
      after: s.file_edit.after,
    });
  }
  return [...edits.entries()].map(([filename, { before, after }]) => {
    const lines = diffLines(before ?? "", after);
    let added = 0;
    let removed = 0;
    for (const l of lines) {
      if (l.type === "add") added++;
      if (l.type === "del") removed++;
    }
    return { filename, lines, added, removed };
  });
}
