export type View = "models" | "chat" | "settings";

const ITEMS: { id: View; label: string }[] = [
  { id: "models", label: "Models" },
  { id: "chat", label: "Chat" },
  { id: "settings", label: "Settings" },
];

export function Sidebar({
  active,
  onSelect,
  ramLabel,
}: {
  active: View;
  onSelect: (v: View) => void;
  ramLabel?: string;
}) {
  return (
    <nav className="sidebar">
      <div className="sidebar-brand">
        node<span className="sidebar-brand-accent">lm</span>
      </div>
      <ul className="sidebar-nav">
        {ITEMS.map((item) => (
          <li key={item.id}>
            <button
              type="button"
              className={item.id === active ? "sidebar-link sidebar-link-active" : "sidebar-link"}
              onClick={() => onSelect(item.id)}
            >
              {item.label}
            </button>
          </li>
        ))}
      </ul>
      {ramLabel && <div className="sidebar-footer mono">{ramLabel}</div>}
    </nav>
  );
}
