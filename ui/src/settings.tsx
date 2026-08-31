import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { api, type AppSettings } from "./api";

// Server-persisted UI preferences (~/.kiralm/settings.json) -- fetched
// once before the app renders (see main.tsx) so every consumer can
// read `settings` synchronously, the same way localStorage reads used
// to work, without a loading state threaded through every call site.
export const DEFAULT_SETTINGS: AppSettings = {
  default_permission: "read-only",
  custom_instructions: "",
  send_on_enter: true,
  notify_on_finish: false,
  default_hot_on_pull: false,
  line_height: "normal",
  font: "default",
  sidebar_collapsed: false,
  collapsed_workspaces: [],
  auto_compact: true,
  runtime_dock_height: 280,
  runtime_dock_tab: "terminal",
};

const LINE_HEIGHTS: Record<AppSettings["line_height"], string> = {
  compact: "1.2",
  normal: "1.4",
  relaxed: "1.6",
};

// "default" = the bundled Open Sans build (index.css's
// --font-sans-base). Anything else is a real system font name from
// queryLocalFonts(), quoted and still falling back to that same stack.
function fontStackFor(font: string): string {
  return font === "default" ? "var(--font-sans-base)" : `"${font}", var(--font-sans-base)`;
}

interface SettingsApi {
  settings: AppSettings;
  // Persists the given settings to the daemon and updates every
  // consumer once it's confirmed saved.
  save: (next: AppSettings) => Promise<void>;
}

const SettingsContext = createContext<SettingsApi | null>(null);

export function useSettings(): SettingsApi {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings must be used within SettingsProvider");
  return ctx;
}

export function SettingsProvider({ initial, children }: { initial: AppSettings; children: ReactNode }) {
  const [settings, setSettings] = useState(initial);

  // Line height/font are real CSS custom properties, not baked into
  // index.css, so a change here applies instantly app-wide.
  useEffect(() => {
    const root = document.documentElement.style;
    root.setProperty("--line-height", LINE_HEIGHTS[settings.line_height]);
    root.setProperty("--font-sans", fontStackFor(settings.font));
  }, [settings.line_height, settings.font]);

  const save = async (next: AppSettings) => {
    const saved = await api.putSettings(next);
    setSettings(saved);
  };

  return <SettingsContext.Provider value={{ settings, save }}>{children}</SettingsContext.Provider>;
}
