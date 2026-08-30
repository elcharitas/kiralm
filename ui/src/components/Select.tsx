import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown } from "lucide-react";

export interface SelectOption<T extends string> {
  value: T;
  label: ReactNode;
  icon?: ReactNode;
}

export interface SelectGroup<T extends string> {
  section: string;
  options: SelectOption<T>[];
}

// App-styled dropdown replacing the browser's native <select> (which
// can't be themed -- wrong font, wrong radius, OS chrome) everywhere
// the app needs one: the turn selector, the composer's model/mode
// chips, Settings' pickers. Pass either `options` (flat) or `groups`
// (section headers, e.g. model ids grouped by publisher) -- not both.
// The menu is portaled to <body> and positioned in viewport (fixed)
// coordinates, clamped so it never runs off-screen, instead of being
// absolutely positioned inside whatever panel the trigger sits in
// (which clips or scrolls it away near an edge).
export function Select<T extends string>({
  value,
  options,
  groups,
  onChange,
  triggerClassName,
  icon,
  disabled,
}: {
  value: T;
  options?: SelectOption<T>[];
  groups?: SelectGroup<T>[];
  onChange: (value: T) => void;
  triggerClassName: string;
  icon?: ReactNode;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number; width: number } | null>(null);

  // Measures the trigger + menu after the menu's first (off-screen)
  // render, then places it -- runs before paint, so there's no flash.
  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    const trigger = triggerRef.current;
    const menu = menuRef.current;
    if (!trigger || !menu) return;
    const rect = trigger.getBoundingClientRect();
    const menuRect = menu.getBoundingClientRect();
    let left = Math.min(rect.left, window.innerWidth - menuRect.width - 8);
    left = Math.max(8, left);
    let top = rect.bottom + 4;
    if (top + menuRect.height > window.innerHeight - 8) {
      top = rect.top - menuRect.height - 4;
    }
    top = Math.max(8, top);
    setPos({ top, left, width: rect.width });
  }, [open]);

  useLayoutEffect(() => {
    if (!open) return;
    const onOutside = (e: MouseEvent) => {
      const target = e.target as Node;
      if (triggerRef.current?.contains(target)) return;
      if (menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onOutside);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onOutside);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const flatOptions = groups ? groups.flatMap((g) => g.options) : (options ?? []);
  const current = flatOptions.find((o) => o.value === value);

  const optionButton = (o: SelectOption<T>) => (
    <button
      type="button"
      key={o.value}
      className={o.value === value ? "select-menu-active" : undefined}
      onClick={() => {
        onChange(o.value);
        setOpen(false);
      }}
    >
      <span className="select-menu-option-label truncate">
        {o.icon}
        <span className="truncate">{o.label}</span>
      </span>
      {o.value === value && <Check size={13} className="select-menu-check" />}
    </button>
  );

  return (
    <div className="select-root">
      <button
        ref={triggerRef}
        type="button"
        className={triggerClassName}
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
      >
        {icon ?? current?.icon}
        <span className="select-value truncate">{current?.label ?? value}</span>
        <ChevronDown size={12} className="select-chevron" />
      </button>
      {open &&
        createPortal(
          <div
            className="select-menu"
            ref={menuRef}
            style={pos ? { top: pos.top, left: pos.left, minWidth: pos.width } : { top: -9999, left: -9999 }}
          >
            {groups
              ? groups.map((g) => (
                  <div key={g.section} className="select-menu-group">
                    <div className="select-menu-section">{g.section}</div>
                    {g.options.map(optionButton)}
                  </div>
                ))
              : (options ?? []).map(optionButton)}
          </div>,
          document.body,
        )}
    </div>
  );
}
