# NodeLM desktop shell

Tauri v2 native chrome around the daemon's own served UI -- no duplicate
frontend. On launch it health-checks the configured `http_listen`
address (`~/.nodelm/config.toml`, default `127.0.0.1:11535`); if nothing
answers it spawns `nodelm serve` as a sidecar, then points the window at
it. Closing the window hides it (menu bar icon: Show/Hide/Quit); Quit
gracefully stops the daemon only if this app spawned it.

Before building or running, copy the release daemon binary in as the
sidecar (target-triple-suffixed, per Tauri's externalBin convention):

```
cargo build --release --manifest-path ../Cargo.toml
cp ../target/release/nodelm src-tauri/binaries/nodelm-$(rustc -vV | sed -n 's/host: //p')
```

Then `cargo tauri dev` or `cargo tauri build` from `desktop/`.
