# CLAUDE.md

Project memory for printpit. Keep this short and current. Update it when a decision changes or a gotcha is found.

## What this is
printpit is a Rust web app, shipped as a Docker image, for controlling several Moonraker/Klipper 3D printers. It has built-in auth and a minimal TUI-style UI.
- `PLAN.md`: architecture and design (the source of truth for *why*).
- `CHECKLIST.md`: the implementation checklist. **Tick items off (`[x]`) as you finish them and add new items as you discover work.** Read it before you start a session's work.

## Non-negotiable rules
1. **The browser never talks to a printer.** All Moonraker HTTP and WebSocket traffic comes from the backend. Printer URLs, hosts and API keys must never appear in any response body, HTML, JS or log line. The browser only knows `/p/{printer_id}/...`.
2. **Lazy loading.** A tab or view makes Moonraker calls only when the user opens it. There is no prefetching and no background polling without an active viewer. The dashboard's live data goes through `PrinterHub` (refcounted WebSocket, one per printer, closed after an idle grace period). Don't add a Moonraker call to a shared layout or shell. The user's `auto_refresh` pref (a checkbox on the dashboard, stored in `user_prefs`) gates all automatic refreshing: when it is off, there is no SSE and no timers, only manual refresh.
3. **Auth on everything** except `/login`, `/setup` (while no users exist), `/healthz` and `/static/*`. State-changing routes require a CSRF token and the right role (`admin` > `operator` > `viewer`).
4. **Egress is segmented.** Outbound printer requests use the egress client from `moonraker/net.rs`, which applies `PRINTPIT_EGRESS_BIND` and the `PRINTPIT_PRINTER_CIDRS` allowlist, checked *after* DNS resolution. Never build an ad-hoc `reqwest::Client` for printer traffic.
5. **Secrets.** Printer API keys are encrypted with AES-GCM (`crypto.rs`) using `PRINTPIT_SECRET_KEY`. Passwords use Argon2id. Session IDs are stored as SHA-256 hashes only.
6. **No CORS.** Don't add CORS layers. Everything is same-origin.

## Stack and conventions
- axum + tokio, askama templates, htmx (vendored in `static/js/`, pinned, embedded with rust-embed), SSE for live updates, sqlx + SQLite, reqwest (rustls) + tokio-tungstenite for Moonraker.
- No JS framework and no frontend build step. Keep `static/js/app.js` small. CSS custom properties for theming live in `static/css/tui.css`.
- `/ui/*` routes return an htmx partial when `HX-Request` is set, or the full shell otherwise, so deep links work.
- Errors: `AppError` maps to an HTML error panel. `MoonrakerError` variants (offline, unauthorized, klippy not ready, …) render as an inline TUI error box and never panic.
- Config comes only from `PRINTPIT_*` env vars (plus `*_FILE` variants for Docker secrets).
- Before committing: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Moonraker notes
- Auth header: `X-Api-Key: <key>`. For the WebSocket, use the header on the upgrade request, or fetch `GET /access/oneshot_token` and pass `?token=`.
- JSON-RPC over the WebSocket: `printer.objects.subscribe`. Updates arrive as `notify_status_update` with *partial* objects, so merge them into the cached state. Don't replace it.
- File roots: `gcodes` (manage), `logs` (read). Paths are relative to the root. URL-encode each path segment.
- `/printer/*` endpoints fail with an error when Klippy isn't ready. Show that state instead of treating it as offline.

## Commands
```
cargo run                                   # needs PRINTPIT_SECRET_KEY, PRINTPIT_SECURE_COOKIES=false for local http
cargo test
docker build -t printpit .
docker compose up
```
(Update this section as tooling is added.)

## Gotchas / decisions log
- *(add entries as `YYYY-MM-DD: note`)*
- 2026-10-30: The project started. Decided on server-rendered HTML + htmx + SSE instead of an SPA, to meet the speed and minimal-UI goals and so that tab partials are lazy by construction.
- 2026-10-30: The auto-refresh checkbox lives on the dashboard and is remembered per user (`user_prefs.auto_refresh`, default on). It controls dashboard SSE and System tab auto-refresh. Logs follow mode is separate. The compose file ships both the simple and the segmented variants.
