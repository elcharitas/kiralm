export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null) return "-";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = bytes;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

// Both numbers in one unit (the total's natural one), not each
// independently -- otherwise e.g. used=900MB/total=16GB would each
// pick their own unit and read as the nonsensical "900/16 GB".
export function formatBytesPair(used: number, total: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = total;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  const scaled = used / 1024 ** i;
  const digits = i === 0 ? 0 : 1;
  return `${scaled.toFixed(digits)}/${n.toFixed(digits)} ${units[i]}`;
}

export function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 1) return "<1s";
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}

export function formatContextLength(tokens: number | null | undefined): string {
  if (!tokens) return "ctx unknown";
  if (tokens % 1024 === 0) return `${tokens / 1024}k ctx`;
  return `${(tokens / 1024).toFixed(1)}k ctx`;
}

export function slugify(text: string): string {
  const slug = text
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/(^-|-$)/g, "");
  return slug.slice(0, 40) || "session";
}

export function formatRelativeTime(iso: string | null | undefined): string {
  if (!iso) return "never";
  const diffMs = Date.now() - new Date(iso).getTime();
  const mins = Math.round(diffMs / 60000);
  if (mins < 1) return "now";
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

// A model id like "unsloth/Qwen3-0.6B-GGUF:Q8_0" splits into section
// "unsloth" (the org) and name "Qwen3-0.6B-GGUF:Q8_0" -- used to group
// the model selector and the Models settings table by publisher
// instead of one long flat alphabetical list.
export function modelSection(id: string): string {
  return id.includes("/") ? id.slice(0, id.indexOf("/")) : "other";
}

export function modelName(id: string): string {
  return id.includes("/") ? id.slice(id.indexOf("/") + 1) : id;
}

export interface Grouped<T> {
  section: string;
  items: T[];
}

export function groupBySection<T>(items: T[], idOf: (item: T) => string): Grouped<T>[] {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const section = modelSection(idOf(item));
    const list = groups.get(section);
    if (list) list.push(item);
    else groups.set(section, [item]);
  }
  return [...groups.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([section, sectionItems]) => ({
      section,
      items: sectionItems.slice().sort((a, b) => idOf(a).localeCompare(idOf(b))),
    }));
}
