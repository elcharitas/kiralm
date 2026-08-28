# kiralm UI

Vite + React + TypeScript dashboard for the KiraLM daemon (see repo-root
`SPEC.md` §9). The desktop bundles it and routes `/api/*` and `/v1/*` through
Tauri IPC. An explicit browser target serves the same build and routes on one
HTTP origin.

## Views

- **Models**: downloaded model list, hardware-verdict badges, hot/cold
  toggle, pull-new-model form with inline download progress, detail drawer.
- **Chat**: model picker + streaming tester against `/v1/chat/completions`.
- **Settings**: read-only daemon config (`~/.kiralm/config.toml` is
  hand-edited) plus a local default-hot-on-pull preference.

## Dev

```
npm install
npm run dev
```

`vite.config.ts` proxies `/api` and `/v1` to `127.0.0.1:11535`, so run
`kiralm serve --http 127.0.0.1:11535` alongside `npm run dev` for a live
browser backend. `npm run build` outputs `dist/`, which the daemon's explicit
browser target and the desktop bundle both consume.
