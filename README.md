# NodeLM

NodeLM is a local AI inference gateway: it downloads GGUF models from
Hugging Face, runs them as managed `llama-server` child processes, and
exposes OpenAI- and Anthropic-compatible HTTP APIs on your machine. A
single `nodelm` binary is both the CLI and the daemon.

## Build

```
cargo build --release
```

Produces a single binary at `target/release/nodelm` (no other artifacts
to install). Requires a Rust toolchain; developed and tested on macOS
Apple Silicon.

## Run

```
nodelm serve
```

Runs the daemon in the foreground, listening on `127.0.0.1:11535` (HTTP)
and `~/.nodelm/nodelm.sock` (CLI control traffic). Every other subcommand
below auto-spawns the daemon in the background if it isn't already
running.

## Pull and run a model

```
nodelm pull org/repo:quant
nodelm run org/repo:quant "your prompt here"
```

`nodelm pull` downloads and verifies a GGUF from Hugging Face. `nodelm run`
does a one-shot chat completion against it. Other subcommands: `list`
(downloaded models), `ps` (currently-resident models), `show <name>`,
`hot <name> on|off`, `rm <name>`.

## Files

NodeLM keeps all of its state under `~/.nodelm/`:

- `config.toml` — hand-edited config (model directory, RAM headroom, etc.)
- `models/` — downloaded `.gguf` files and their manifest sidecars
- `state.json` — daemon-owned runtime state
- `nodelm.sock`, `nodelm.pid`, `nodelm.log` — daemon process files
- `logs/` — per-model child process logs
