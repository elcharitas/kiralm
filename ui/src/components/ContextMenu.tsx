import { useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

export interface ContextMenuItem {
  label: string;
  onClick: () => void;
  danger?: boolean;
}

// Real right-click menu, positioned at the cursor and clamped to stay
// on-screen -- not the browser's own context menu.
export function ContextMenu({
  x,
  y,
  items,
  onClose,
}: {
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ top: y, left: x });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const left = Math.min(x, window.innerWidth - rect.width - 8);
    const top = Math.min(y, window.innerHeight - rect.height - 8);
    setPos({ top: Math.max(8, top), left: Math.max(8, left) });
  }, [x, y]);

  useLayoutEffect(() => {
    const onOutside = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", onOutside);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onOutside);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  // Portaled to <body>: several panels use backdrop-filter, which (like
  // transform) creates a new containing block that would otherwise
  // pin this fixed-position menu to the panel instead of the viewport.
  return createPortal(
    <div className="context-menu" style={pos} ref={ref}>
      {items.map((item) => (
        <button
          type="button"
          key={item.label}
          className={item.danger ? "context-menu-danger" : undefined}
          onClick={() => {
            item.onClick();
            onClose();
          }}
        >
          {item.label}
        </button>
      ))}
    </div>,
    document.body,
  );
}
