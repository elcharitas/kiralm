import { useState } from "react";
import { Sidebar, type View } from "./components/Sidebar";
import { ModelsView } from "./views/ModelsView";
import { ChatView } from "./views/ChatView";
import { SettingsView } from "./views/SettingsView";
import { useStatus } from "./hooks/useStatus";
import { formatBytes } from "./format";

export default function App() {
  const [view, setView] = useState<View>("models");
  const { status, error, refresh } = useStatus();

  const ramLabel = status
    ? `${formatBytes(status.ram.available_bytes)} free / ${formatBytes(status.ram.total_bytes)}`
    : undefined;

  return (
    <div className="app">
      <Sidebar active={view} onSelect={setView} ramLabel={ramLabel} />
      <main className="app-content">
        {error && <div className="banner banner-bad app-conn-error">{error}</div>}
        {view === "models" && <ModelsView status={status} refresh={refresh} />}
        {view === "chat" && <ChatView status={status} />}
        {view === "settings" && <SettingsView status={status} />}
      </main>
    </div>
  );
}
