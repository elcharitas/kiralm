import { useEffect, useRef, useState } from "react";
import { api, type CatalogPlugin } from "../api";
import { useDialogs } from "./Dialogs";
import { Select } from "./Select";

const sourceKindLabel: Record<CatalogPlugin["source_kind"], string> = {
  claude_marketplace: "Claude Marketplace",
  agent_plugin: "Agent Plugin",
  dsh: "dsh",
};

const sourceKindBadgeClass: Record<CatalogPlugin["source_kind"], string> = {
  claude_marketplace: "badge-good",
  agent_plugin: "badge-warn",
  dsh: "badge-bad",
};

const PAGE_SIZE = 50;

type InstalledFilter = "all" | "installed" | "uninstalled";

const installedFilterOptions = [
  { value: "all" as const, label: "All" },
  { value: "installed" as const, label: "Installed" },
  { value: "uninstalled" as const, label: "Not installed" },
];

// Module-scope so remounting the panel (every sidebar tab switch)
// restores instantly instead of refetching 2000+ rows every time.
let catalogCache: {
  query: string;
  installedFilter: InstalledFilter;
  items: CatalogPlugin[];
  total: number;
} | null = null;

// Single browsable catalog, sourced from kiralm-web (GET /api/plugins),
// covering all three install backends (marketplace.rs, agent_plugin.rs,
// dsh.rs) behind one Install/Uninstall button each -- no URL/path/
// package text input anywhere here, the source is baked into the
// catalog row server-side.
export function PluginsPanel() {
  const [searchInput, setSearchInput] = useState(catalogCache?.query ?? "");
  const [query, setQuery] = useState(catalogCache?.query ?? "");
  const [installedFilter, setInstalledFilter] = useState<InstalledFilter>(catalogCache?.installedFilter ?? "all");
  const [plugins, setPlugins] = useState<CatalogPlugin[]>(catalogCache?.items ?? []);
  const [total, setTotal] = useState(catalogCache?.total ?? 0);
  const [loading, setLoading] = useState(catalogCache === null);
  const [revalidating, setRevalidating] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [pendingId, setPendingId] = useState<string | null>(null);
  const [pendingAction, setPendingAction] = useState<"install" | "uninstall" | null>(null);
  const sentinelRef = useRef<HTMLLIElement | null>(null);
  const { toast } = useDialogs();

  // Debounce typing before it becomes the query that actually re-fetches.
  useEffect(() => {
    const id = setTimeout(() => setQuery(searchInput), 250);
    return () => clearTimeout(id);
  }, [searchInput]);

  useEffect(() => {
    if (catalogCache !== null && catalogCache.query === query && catalogCache.installedFilter === installedFilter) return;
    let cancelled = false;
    // Blank-slate loading only on the very first load ever -- a search
    // or filter change keeps the current rows on screen (see
    // `revalidating`) instead of flashing to empty and back.
    if (catalogCache === null) setLoading(true);
    else setRevalidating(true);
    api
      .listPluginCatalog({
        q: query || undefined,
        offset: 0,
        limit: PAGE_SIZE,
        installed: installedFilter === "all" ? undefined : installedFilter,
      })
      .then((res) => {
        if (cancelled) return;
        setPlugins(res.data);
        setTotal(res.total);
        catalogCache = { query, installedFilter, items: res.data, total: res.total };
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
          setRevalidating(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [query, installedFilter]);

  const loadMore = async () => {
    if (loadingMore || plugins.length >= total) return;
    setLoadingMore(true);
    try {
      const res = await api.listPluginCatalog({
        q: query || undefined,
        offset: plugins.length,
        limit: PAGE_SIZE,
        installed: installedFilter === "all" ? undefined : installedFilter,
      });
      setPlugins((prev) => {
        const next = [...prev, ...res.data];
        catalogCache = { query, installedFilter, items: next, total: res.total };
        return next;
      });
      setTotal(res.total);
    } catch {
      // sentinel stays visible, next scroll/intersection retries
    } finally {
      setLoadingMore(false);
    }
  };

  useEffect(() => {
    const el = sentinelRef.current;
    if (!el || loading || loadingMore || plugins.length >= total) return;
    const observer = new IntersectionObserver((entries) => {
      if (entries[0]?.isIntersecting) loadMore();
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [plugins.length, total, loading, loadingMore, query, installedFilter]);

  const patchInstalled = (id: string, installed: boolean) => {
    setPlugins((prev) => {
      const next = prev.map((p) => (p.id === id ? { ...p, installed } : p));
      if (catalogCache) catalogCache = { ...catalogCache, items: next };
      return next;
    });
  };

  const install = async (p: CatalogPlugin) => {
    setPendingId(p.id);
    setPendingAction("install");
    try {
      await api.installCatalogPlugin(p.id);
      patchInstalled(p.id, true);
      toast(`${p.name} installed`, "success");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setPendingId(null);
      setPendingAction(null);
    }
  };

  const uninstall = async (p: CatalogPlugin) => {
    setPendingId(p.id);
    setPendingAction("uninstall");
    try {
      await api.uninstallCatalogPlugin(p.id);
      patchInstalled(p.id, false);
      toast(`${p.name} uninstalled`, "info");
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setPendingId(null);
      setPendingAction(null);
    }
  };

  return (
    <div className="plugins-panel">
      <div className="plugins-search-row">
        <input
          className="mcp-field plugins-search"
          placeholder="Search plugins…"
          value={searchInput}
          onChange={(e) => setSearchInput(e.target.value)}
        />
        <Select
          triggerClassName="settings-select plugins-filter"
          value={installedFilter}
          onChange={setInstalledFilter}
          options={installedFilterOptions}
        />
      </div>

      <div className="plugins-list-scroll">
        {loading ? (
          <p className="settings-hint">Loading…</p>
        ) : plugins.length === 0 ? (
          <p className="settings-hint">
            {revalidating ? "Loading…" : query ? "No plugins match your search." : "No plugins available yet."}
          </p>
        ) : (
          <ul className={revalidating ? "skills-list plugins-list-revalidating" : "skills-list"}>
            {plugins.map((p) => (
              <li key={p.id} className="skills-row plugins-row">
                <div className="skills-row-main">
                  <span className="mono truncate">{p.name}</span>
                  <span className={`badge ${sourceKindBadgeClass[p.source_kind]}`}>
                    {sourceKindLabel[p.source_kind]}
                  </span>
                  <span className="settings-hint skills-row-desc truncate">{p.description ?? p.source}</span>
                </div>
                {pendingId === p.id ? (
                  <div className="plugins-progress" title={pendingAction === "install" ? "Installing…" : "Uninstalling…"} />
                ) : p.installed ? (
                  <button type="button" className="btn plugins-btn" onClick={() => uninstall(p)}>
                    Uninstall
                  </button>
                ) : (
                  <button type="button" className="btn btn-accent plugins-btn" onClick={() => install(p)}>
                    Install
                  </button>
                )}
              </li>
            ))}
            <li ref={sentinelRef} className="plugins-sentinel" aria-hidden="true" />
          </ul>
        )}
      </div>
    </div>
  );
}
