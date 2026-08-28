import { Folder, X } from "lucide-react";

export function TopBar({
  itemName,
  projectName,
  onClose,
}: {
  itemName: string;
  // Real project name for the active session's project -- null on
  // pages with no project context (Settings), which then show just
  // their own label instead of a fake "project / page" pair.
  projectName: string | null;
  // Home renders no TopBar at all, so there's always something to close here.
  onClose: () => void;
}) {
  return (
    <header className="topbar" data-tauri-drag-region>
      <div className="topbar-breadcrumb">
        {projectName && (
          <>
            <span className="topbar-icon">
              <Folder size={14} />
            </span>
            <span>{projectName}</span>
            <span className="topbar-sep">/</span>
          </>
        )}
        <span className="topbar-current truncate">{itemName}</span>
        <button type="button" className="topbar-close-btn" title="Close" onClick={onClose}>
          <X size={13} />
        </button>
      </div>
    </header>
  );
}
