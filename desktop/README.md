# KiraLM desktop shell

Tauri v2 shell with the UI bundled into the application. On launch it checks
the configured Unix socket (`~/.kiralm/kiralm.sock` by default); if nothing
answers it spawns `kiralm serve` as a sidecar. UI requests cross Tauri IPC and
reach the daemon through that socket, so the installed desktop app does not
open a local HTTP URL. Closing the window hides it (menu bar icon:
Show/Hide/Quit); Quit gracefully stops the daemon only if this app spawned it.

Run `cargo tauri dev` or `cargo tauri build` from `desktop/`. The Tauri
pre-build hook rebuilds the UI and the matching daemon, then atomically
places the target-triple-suffixed sidecar under `src-tauri/binaries/`.
This is intentionally automatic so a release cannot silently package a
daemon left over from an earlier source revision.
