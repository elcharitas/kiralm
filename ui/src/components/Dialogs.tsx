import { createContext, useCallback, useContext, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { CheckCircle2, CornerLeftUp, Folder, Info, XCircle } from "lucide-react";
import { api } from "../api";

// Real modal/confirm/prompt/toast primitives, replacing window.alert /
// window.confirm / window.prompt so dialogs match the app's own look
// instead of the browser chrome's.

type ConfirmOpts = { title: string; message?: string; confirmLabel?: string; cancelLabel?: string; danger?: boolean };
type PromptOpts = {
  title: string;
  message?: string;
  placeholder?: string;
  defaultValue?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  multiline?: boolean;
  maxLength?: number;
};
type BrowseOpts = { title: string; startPath?: string };
type ToastKind = "info" | "success" | "error";
type Toast = { id: string; message: string; kind: ToastKind };

interface DialogsApi {
  confirm: (opts: ConfirmOpts) => Promise<boolean>;
  prompt: (opts: PromptOpts) => Promise<string | null>;
  // Real in-app directory explorer (see FolderBrowserBody) -- not a
  // native OS dialog, so it looks and behaves like the rest of the
  // app and works identically in a dev browser tab or the real shell.
  browseFolder: (opts: BrowseOpts) => Promise<string | null>;
  toast: (message: string, kind?: ToastKind) => void;
}

const DialogsContext = createContext<DialogsApi | null>(null);

export function useDialogs(): DialogsApi {
  const ctx = useContext(DialogsContext);
  if (!ctx) throw new Error("useDialogs must be used within DialogsProvider");
  return ctx;
}

type ConfirmState = ConfirmOpts & { kind: "confirm"; resolve: (v: boolean) => void };
type PromptState = PromptOpts & { kind: "prompt"; resolve: (v: string | null) => void };
type BrowseState = BrowseOpts & { resolve: (v: string | null) => void };

type DirListing = { path: string; parent: string | null; entries: { name: string; path: string }[] };

// The actual explorer: lists real subdirectories of the current path
// (daemon-side, via /api/fs/list), lets you drill in or go up, and
// confirms whatever directory you're currently looking at. Most
// people picking a workspace don't think in filesystem paths, so this
// is the default entry point (see Sidebar/HomeView) -- typing one
// directly is a fallback, not the primary flow, hence "manual" mode
// starts off.
function FolderBrowserBody({
  startPath,
  onSelect,
  onCancel,
}: {
  startPath?: string;
  onSelect: (path: string) => void;
  onCancel: () => void;
}) {
  const [path, setPath] = useState(startPath);
  const [listing, setListing] = useState<DirListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [manual, setManual] = useState(false);
  const [manualValue, setManualValue] = useState("");

  useEffect(() => {
    let cancelled = false;
    api.listDir(path).then(
      (d) => {
        if (cancelled) return;
        setListing(d);
        setError(null);
      },
      (e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [path]);

  if (manual) {
    return (
      <>
        <div className="modal-input-row">
          <input
            autoFocus
            className="modal-input"
            placeholder="/Users/you/code/my-repo"
            value={manualValue}
            onChange={(e) => setManualValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && manualValue.trim()) onSelect(manualValue.trim());
              if (e.key === "Escape") onCancel();
            }}
          />
        </div>
        <button type="button" className="folder-browser-mode-toggle" onClick={() => setManual(false)}>
          Browse folders instead
        </button>
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="btn btn-accent"
            disabled={!manualValue.trim()}
            onClick={() => onSelect(manualValue.trim())}
          >
            Select this folder
          </button>
        </div>
      </>
    );
  }

  return (
    <>
      <ul className="folder-browser-list">
        {listing?.parent && (
          <li>
            <button type="button" className="folder-browser-row" onClick={() => setPath(listing.parent!)}>
              <CornerLeftUp size={14} /> ..
            </button>
          </li>
        )}
        {listing?.entries.map((e) => (
          <li key={e.path}>
            <button type="button" className="folder-browser-row" onClick={() => setPath(e.path)}>
              <Folder size={14} /> {e.name}
            </button>
          </li>
        ))}
        {listing && listing.entries.length === 0 && !listing.parent && (
          <li className="settings-hint">No subfolders here.</li>
        )}
      </ul>
      {error && <p className="settings-hint">{error}</p>}
      <button
        type="button"
        className="folder-browser-mode-toggle"
        onClick={() => {
          setManualValue(listing?.path ?? "");
          setManual(true);
        }}
      >
        Type a path instead
      </button>
      <div className="modal-actions">
        <button type="button" className="btn" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="btn btn-accent" disabled={!listing} onClick={() => listing && onSelect(listing.path)}>
          Select this folder
        </button>
      </div>
    </>
  );
}

export function DialogsProvider({ children }: { children: ReactNode }) {
  const [dialog, setDialog] = useState<ConfirmState | PromptState | null>(null);
  const [browse, setBrowse] = useState<BrowseState | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [promptValue, setPromptValue] = useState("");
  const toastTimer = useRef<Record<string, number>>({});
  const modalRef = useRef<HTMLDivElement>(null);
  const returnFocusRef = useRef<HTMLElement | null>(null);
  const dialogTitleId = useId();
  const browseTitleId = useId();

  const toast = useCallback((message: string, kind: ToastKind = "info") => {
    const id = crypto.randomUUID();
    setToasts((prev) => [...prev, { id, message, kind }]);
    toastTimer.current[id] = window.setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
      delete toastTimer.current[id];
    }, 4500);
  }, []);

  const confirm = useCallback((opts: ConfirmOpts) => {
    returnFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    return new Promise<boolean>((resolve) => {
      setDialog({ kind: "confirm", ...opts, resolve });
    });
  }, []);

  const prompt = useCallback((opts: PromptOpts) => {
    returnFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setPromptValue(opts.defaultValue ?? "");
    return new Promise<string | null>((resolve) => {
      setDialog({ kind: "prompt", ...opts, resolve });
    });
  }, []);

  const browseFolder = useCallback((opts: BrowseOpts) => {
    returnFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    return new Promise<string | null>((resolve) => {
      setBrowse({ ...opts, resolve });
    });
  }, []);

  const closeWith = (value: boolean | string | null) => {
    if (!dialog) return;
    if (dialog.kind === "confirm") dialog.resolve(value as boolean);
    else dialog.resolve(value as string | null);
    setDialog(null);
    window.setTimeout(() => returnFocusRef.current?.focus(), 0);
  };

  const closeBrowseWith = (value: string | null) => {
    if (!browse) return;
    browse.resolve(value);
    setBrowse(null);
    window.setTimeout(() => returnFocusRef.current?.focus(), 0);
  };

  useEffect(() => {
    if (!dialog && !browse) return;
    const frame = window.requestAnimationFrame(() => {
      modalRef.current
        ?.querySelector<HTMLElement>("textarea, input, button, [tabindex]:not([tabindex='-1'])")
        ?.focus();
    });
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        if (dialog) {
          if (dialog.kind === "confirm") dialog.resolve(false);
          else dialog.resolve(null);
          setDialog(null);
          window.setTimeout(() => returnFocusRef.current?.focus(), 0);
        } else if (browse) {
          browse.resolve(null);
          setBrowse(null);
          window.setTimeout(() => returnFocusRef.current?.focus(), 0);
        }
        return;
      }
      if (event.key !== "Tab" || !modalRef.current) return;
      const focusable = Array.from(
        modalRef.current.querySelectorAll<HTMLElement>(
          "button:not(:disabled), input:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex='-1'])",
        ),
      );
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      window.cancelAnimationFrame(frame);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [browse, dialog]);

  return (
    <DialogsContext.Provider value={{ confirm, prompt, browseFolder, toast }}>
      {children}

      {dialog && (
        <div className="modal-overlay" onMouseDown={() => closeWith(dialog.kind === "confirm" ? false : null)}>
          <div ref={modalRef} className="modal" role="dialog" aria-modal="true" aria-labelledby={dialogTitleId} onMouseDown={(e) => e.stopPropagation()}>
            <h2 id={dialogTitleId} className="modal-title">{dialog.title}</h2>
            {dialog.message && <p className="modal-message">{dialog.message}</p>}
            {dialog.kind === "prompt" && (dialog.multiline ? (
              <textarea
                autoFocus
                className="modal-input modal-textarea"
                placeholder={dialog.placeholder}
                value={promptValue}
                maxLength={dialog.maxLength}
                onChange={(e) => setPromptValue(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) closeWith(promptValue);
                  if (e.key === "Escape") closeWith(null);
                }}
              />
            ) : (
              <input
                autoFocus
                className="modal-input"
                placeholder={dialog.placeholder}
                value={promptValue}
                maxLength={dialog.maxLength}
                onChange={(e) => setPromptValue(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") closeWith(promptValue);
                  if (e.key === "Escape") closeWith(null);
                }}
              />
            ))}
            <div className="modal-actions">
              <button type="button" className="btn" onClick={() => closeWith(dialog.kind === "confirm" ? false : null)}>
                {dialog.cancelLabel ?? "Cancel"}
              </button>
              <button
                type="button"
                autoFocus={dialog.kind === "confirm"}
                className={dialog.kind === "confirm" && dialog.danger ? "btn btn-danger-solid" : "btn btn-accent"}
                onClick={() => closeWith(dialog.kind === "confirm" ? true : promptValue)}
              >
                {dialog.confirmLabel ?? (dialog.kind === "confirm" ? "Confirm" : "OK")}
              </button>
            </div>
          </div>
        </div>
      )}

      {browse && (
        <div className="modal-overlay" onMouseDown={() => closeBrowseWith(null)}>
          <div ref={modalRef} className="modal modal-browse" role="dialog" aria-modal="true" aria-labelledby={browseTitleId} onMouseDown={(e) => e.stopPropagation()}>
            <h2 id={browseTitleId} className="modal-title">{browse.title}</h2>
            <FolderBrowserBody startPath={browse.startPath} onSelect={(p) => closeBrowseWith(p)} onCancel={() => closeBrowseWith(null)} />
          </div>
        </div>
      )}

      <div className="toast-stack" role="status" aria-live="polite" aria-atomic="false">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.kind}`}>
            {t.kind === "success" ? <CheckCircle2 size={14} /> : t.kind === "error" ? <XCircle size={14} /> : <Info size={14} />}
            {t.message}
          </div>
        ))}
      </div>
    </DialogsContext.Provider>
  );
}
