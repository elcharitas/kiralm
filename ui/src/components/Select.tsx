import {
  useCallback,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
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
  ariaLabel,
}: {
  value: T;
  options?: SelectOption<T>[];
  groups?: SelectGroup<T>[];
  onChange: (value: T) => void;
  triggerClassName: string;
  icon?: ReactNode;
  disabled?: boolean;
  ariaLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const menuId = useId();
  const [pos, setPos] = useState<{ top: number; left: number; width: number } | null>(null);
  const flatOptions = groups ? groups.flatMap((group) => group.options) : (options ?? []);
  const current = flatOptions.find((option) => option.value === value);
  const selectedIndex = Math.max(
    0,
    flatOptions.findIndex((option) => option.value === value),
  );

  const focusOption = useCallback((index: number) => {
    if (flatOptions.length === 0) return;
    const next = (index + flatOptions.length) % flatOptions.length;
    optionRefs.current[next]?.focus();
  }, [flatOptions.length]);

  const positionMenu = useCallback(() => {
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
    setPos((current) => {
      const width = rect.width;
      if (current && current.top === top && current.left === left && current.width === width) {
        return current;
      }
      return { top, left, width };
    });
  }, []);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    positionMenu();
    focusOption(selectedIndex);
    const observer = new ResizeObserver(positionMenu);
    if (triggerRef.current) observer.observe(triggerRef.current);
    window.addEventListener("resize", positionMenu);
    document.addEventListener("scroll", positionMenu, true);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", positionMenu);
      document.removeEventListener("scroll", positionMenu, true);
    };
  }, [focusOption, open, positionMenu, selectedIndex]);

  useLayoutEffect(() => {
    if (!open) return;
    const onOutside = (e: MouseEvent) => {
      const target = e.target as Node;
      if (triggerRef.current?.contains(target)) return;
      if (menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
        triggerRef.current?.focus();
        return;
      }
      if (e.key === "Tab") {
        setOpen(false);
        return;
      }
      if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) {
        return;
      }
      e.preventDefault();
      const focused = optionRefs.current.findIndex(
        (option) => option === document.activeElement,
      );
      if (e.key === "Home") focusOption(0);
      else if (e.key === "End") focusOption(flatOptions.length - 1);
      else if (focused < 0) {
        focusOption(e.key === "ArrowDown" ? 0 : flatOptions.length - 1);
      } else focusOption(focused + (e.key === "ArrowDown" ? 1 : -1));
    };
    document.addEventListener("mousedown", onOutside);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onOutside);
      document.removeEventListener("keydown", onKey);
    };
  }, [flatOptions.length, focusOption, open]);

  const optionButton = (o: SelectOption<T>) => {
    const index = flatOptions.findIndex((option) => option.value === o.value);
    return (
    <button
      type="button"
      key={o.value}
      id={`${menuId}-option-${index}`}
      ref={(element) => {
        optionRefs.current[index] = element;
      }}
      role="option"
      aria-selected={o.value === value}
      tabIndex={-1}
      className={o.value === value ? "select-menu-active" : undefined}
      onClick={() => {
        setOpen(false);
        triggerRef.current?.focus();
        onChange(o.value);
      }}
    >
      <span className="select-menu-option-label truncate">
        {o.icon}
        <span className="truncate">{o.label}</span>
      </span>
      {o.value === value && <Check size={13} className="select-menu-check" />}
    </button>
    );
  };

  return (
    <div className="select-root">
      <button
        ref={triggerRef}
        type="button"
        className={triggerClassName}
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(event) => {
          if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
          event.preventDefault();
          if (!open) setOpen(true);
          else {
            focusOption(selectedIndex + (event.key === "ArrowDown" ? 1 : -1));
          }
        }}
        aria-haspopup="listbox"
        aria-label={ariaLabel}
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
      >
        {icon ?? current?.icon}
        <span className="select-value truncate">{current?.label ?? value}</span>
        <ChevronDown size={12} className="select-chevron" />
      </button>
      {open &&
        createPortal(
          <div
            className="select-menu"
            id={menuId}
            role="listbox"
            aria-label={ariaLabel ?? "Options"}
            ref={menuRef}
            style={pos ? { top: pos.top, left: pos.left, minWidth: pos.width } : { top: -9999, left: -9999 }}
          >
            {groups
              ? groups.map((g) => (
                  <div
                    key={g.section}
                    className="select-menu-group"
                    role="group"
                    aria-label={g.section}
                  >
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
