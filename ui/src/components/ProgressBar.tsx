export function ProgressBar({ percent, label }: { percent: number; label?: string }) {
  const pct = Math.max(0, Math.min(100, percent));
  return (
    <div className="progress">
      <div className="progress-track">
        <div className="progress-fill" style={{ width: `${pct}%` }} />
      </div>
      {label && <span className="progress-label">{label}</span>}
    </div>
  );
}
