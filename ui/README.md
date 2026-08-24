# nodelm UI

Vite + React + TypeScript dashboard for the NodeLM daemon (see repo-root
`SPEC.md` §9). Served embedded at `/` by the daemon in production; talks to
`/api/*` (internal status/control) and `/v1/*` (OpenAI-compatible chat) on
the same origin.

## Views

- **Models** — downloaded model list, hardware-verdict badges, hot/cold
  toggle, pull-new-model form with inline download progress, detail drawer.
- **Chat** — model picker + streaming tester against `/v1/chat/completions`.
- **Settings** — read-only daemon config (`~/.nodelm/config.toml` is
  hand-edited) plus a local default-hot-on-pull preference.

## Dev

```
npm install
npm run dev
```

`vite.config.ts` proxies `/api` and `/v1` to `127.0.0.1:11535`, so run
`nodelm serve` alongside `npm run dev` for a live backend. `npm run build`
outputs `dist/`, which the Rust binary embeds via `rust-embed`.
