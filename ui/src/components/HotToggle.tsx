export function HotToggle({
  on,
  disabled,
  onChange,
}: {
  on: boolean;
  disabled?: boolean;
  onChange: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      className={`hot-toggle ${on ? "hot-toggle-on" : ""}`}
      disabled={disabled}
      aria-pressed={on}
      onClick={(e) => {
        e.stopPropagation();
        onChange(!on);
      }}
      title={on ? "Hot, pinned resident, mlocked" : "Cold, loaded on demand"}
    >
      <span className="hot-toggle-dot" />
      {on ? "Hot" : "Cold"}
    </button>
  );
}
