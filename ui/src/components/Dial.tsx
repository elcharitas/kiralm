// Small circular gauge, shared by the collapsed right panel's RAM
// indicator and the plugins list's install/uninstall progress. Omit
// `percent` for an indeterminate spinner (no real progress to report).
export function Dial({ percent, title }: { percent?: number; title?: string }) {
  const r = 8;
  const circumference = 2 * Math.PI * r;
  const indeterminate = percent === undefined;
  const offset = indeterminate ? circumference * 0.75 : circumference * (1 - percent / 100);
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" className={indeterminate ? "dial dial-spin" : "dial"}>
      {title && <title>{title}</title>}
      <circle cx="10" cy="10" r={r} className="dial-track" strokeWidth="2.5" fill="none" />
      <circle
        cx="10"
        cy="10"
        r={r}
        className="dial-fill"
        strokeWidth="2.5"
        fill="none"
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform="rotate(-90 10 10)"
      />
    </svg>
  );
}
