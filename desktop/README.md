# KiraLM desktop shell

Tauri v2 shell with the UI bundled into the application. On launch it checks
the configured Unix socket (`~/.kiralm/kiralm.sock` by default); if nothing
answers it spawns `kiralm serve` as a sidecar. UI requests cross Tauri IPC and
reach the daemon through that socket, so the installed desktop app does not
open a local HTTP URL. Closing the window hides it (menu bar icon:
Show/Hide/Quit); Quit gracefully stops the daemon only if this app spawned it.

Before building or running, copy the release daemon binary in as the
sidecar (target-triple-suffixed, per Tauri's externalBin convention):

```
cargo build --release --manifest-path ../Cargo.toml
cp ../target/release/kiralm src-tauri/binaries/kiralm-$(rustc -vV | sed -n 's/host: //p')
```

Then `cargo tauri dev` or `cargo tauri build` from `desktop/`.
