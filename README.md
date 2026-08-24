# KiraLM

KiraLM is a local AI inference gateway: it downloads GGUF models from
Hugging Face, runs them as managed `llama-server` child processes, and
exposes OpenAI- and Anthropic-compatible HTTP APIs on your machine. A
single `kiralm` binary is both the CLI and the daemon.

## Prerequisites

KiraLM spawns `llama-server` to actually run models. It doesn't bundle
it. Install llama.cpp first:

```
brew install llama.cpp
```

By default KiraLM looks for the binary at `/opt/homebrew/bin/llama-server`
(brew's default path); override via `llama_server_bin` in `config.toml`
if yours lives elsewhere.

## Build

```
cargo build --release
```

Produces a single binary at `target/release/kiralm` (no other artifacts
to install). Requires a Rust toolchain; developed and tested on macOS
Apple Silicon.

## Run

```
kiralm serve
```

Runs the daemon in the foreground, listening on `127.0.0.1:11535` (HTTP)
and `~/.kiralm/kiralm.sock` (CLI control traffic). Every other subcommand
below auto-spawns the daemon in the background if it isn't already
running.

## Pull and run a model

```
kiralm pull org/repo:quant
kiralm run org/repo:quant "your prompt here"
```

`kiralm pull` downloads and verifies a GGUF from Hugging Face. `kiralm run`
does a one-shot chat completion against it. Other subcommands: `list`
(downloaded models), `ps` (currently-resident models), `show <name>`,
`hot <name> on|off`, `rm <name>`.

## Files

KiraLM keeps all of its state under `~/.kiralm/`:

- `config.toml`: hand-edited config (model directory, RAM headroom, etc.)
- `models/`: downloaded `.gguf` files and their manifest sidecars
- `state.json`: daemon-owned runtime state
- `kiralm.sock`, `kiralm.pid`, `kiralm.log`: daemon process files
- `logs/`: per-model child process logs
