# printpit — Implementation Checklist

Work through this top to bottom. Tick `[x]` when an item is done **and tested**. Add sub-items as you find new work.
See `PLAN.md` for design details and `CLAUDE.md` for rules.

## M0 — Skeleton
- [x] `cargo init` (edition 2024), core dependencies (axum, tokio, tower-http, serde, askama, rust-embed, tracing)
- [x] `config.rs`: load `PRINTPIT_*` env vars, `*_FILE` secret variants, validation with clear errors
- [x] `tracing` setup (pretty locally, JSON via `PRINTPIT_LOG_FORMAT=json`; ANSI only on a TTY)
- [x] `AppState` + `AppError` (HTML error panel rendering; full-page wrap for non-htmx requests)
- [x] axum server with graceful shutdown (SIGTERM/SIGINT)
- [x] Embedded static assets with content-hash URLs and long-cache headers
- [x] Vendor a pinned htmx into `static/js/` (record the version in CLAUDE.md)
- [x] `tui.css`: palette variables (dark + light), monospace grid, box panels with inset titles, tab strip, buttons, badges, text progress bar, modal
- [x] Base askama layout: top bar, tab strip with key hints, content region, footer/status line
- [x] Placeholder tab partials (`/ui/{dashboard,files,logs,system,printers}`), partial vs full page + OOB tab strip
- [x] Keyboard tab shortcuts (`1`–`4`, `p`) in `app.js`
- [x] `GET /healthz` and a `healthcheck` CLI subcommand
- [x] Dockerfile (multi-stage, static musl binary, distroless/static nonroot, read-only rootfs friendly, HEALTHCHECK); image 11.9 MB at M0, 17.1 MB at M1 (SQLite), 16.7 MB at M2 (HTTP client + size-optimised build), 17.3 MB at M3, 18.0 MB at M4; RSS ~3.5 MB with three live printers
- [x] `docker-compose.yml` (simple single-network variant first)
- [x] CI workflow: fmt, clippy, test, docker build + container smoke test
- [x] README quickstart

## M1 — Authentication (before any printer features)
- [x] sqlx + SQLite pool, migrations folder, run migrations on startup (DB file created 0600)
- [x] Migrations: `users`, `sessions`, `user_prefs` (incl. `auto_refresh` default 1), `audit_log`
- [x] `password.rs`: Argon2id hash/verify (+ tests), dummy-hash verify for unknown users (timing)
- [x] First-run setup: detect no users, generate a one-time setup token and log it, `/setup` form creates the admin (atomic: `INSERT … WHERE NOT EXISTS`)
- [x] Login page (TUI styled) + `POST /login`
- [x] Server-side sessions: random 256-bit ID, SHA-256 stored, cookie flags (HttpOnly, Secure, SameSite=Lax, `__Host-` prefix when secure), idle + absolute expiry, periodic cleanup task
- [x] `POST /logout`
- [x] Auth extractors: `CurrentUser`, `RequireRole<Operator>`, `RequireRole<Admin>`; unauthenticated → redirect to `/login?next=` (or 401 + `HX-Redirect` for htmx)
- [x] CSRF: per-session token, `hx-headers` injection in the layout, hidden field for forms, middleware check on non-GET; double-submit cookie for login/setup; `Sec-Fetch-Site: cross-site` refused
- [x] Login rate limiting (per IP + per username, failures only), trusted-proxy aware client IP
- [x] Security headers middleware (CSP, nosniff, referrer policy, frame-ancestors none, permissions policy) — done in M0 (`web/security.rs`)
- [x] Account page: change your own password (revokes other sessions)
- [x] Audit entries: setup, login, login_failed, logout, password_change
- [x] Tests: unauthenticated access rejected on every route, CSRF enforced, session expiry, rate limit, roles, disabled users, open-redirect guard, session fixation

## M2 — Printer management
- [x] Migration: `printers` table
- [x] `crypto.rs`: AES-256-GCM seal/open with random nonce, HKDF subkey from config, AAD context (+ round-trip/tamper tests)
- [x] `moonraker/net.rs`: egress `reqwest::Client` builder (rustls + ring, connect timeout, `no_proxy`, no redirects, optional `local_address` = `PRINTPIT_EGRESS_BIND`)
- [x] CIDR allowlist check with a custom DNS resolver that rejects disallowed IPs after resolution; IP literals checked separately (+ tests incl. rebinding case)
- [x] `MoonrakerClient::new(printer)`: base URL, decrypted key → `X-Api-Key` header
- [x] `server.info` / `printer.info` calls + typed models; typed `MoonrakerError` (address-free messages)
- [x] Printers page (admin): list, add, edit, delete, reorder, enable/disable
- [x] API key never rendered back (show "set"/"none"/"unreadable", replace/clear controls)
- [x] "Test connection" button showing Klippy state + versions, or a typed error
- [x] Audit entries for printer create/update/delete/enable/disable
- [x] Test: API key never appears in any HTTP response body (and is sent upstream)
- [x] TUI confirm dialog for `hx-confirm` (y/n keys) — reused by M3 controls
- [x] Dashboard placeholder lists enabled printers (DB only, no Moonraker calls)

## M3 — Dashboard (main page)
- [x] `moonraker/client.rs` + `rpc.rs`: WebSocket via HTTP upgrade on the egress client (X-Api-Key header), JSON-RPC request/response matching; hub reconnects with backoff
- [x] `printer.objects.list` → build the subscription set (print_stats, virtual_sdcard, display_status, extruder*, heater_bed, heater_generic, temperature_sensor/fan, fans, idle_timeout, webhooks)
- [x] Merge `notify_status_update` partial deltas into the cached state (deep merge)
- [x] `moonraker/hub.rs`: per-printer refcount, start on first subscriber, stop after `PRINTPIT_WS_IDLE_GRACE_SECS`, `watch` fan-out, reset on printer edit
- [x] Throttle/coalesce updates to `PRINTPIT_UI_REFRESH_MS`
- [x] HTTP fallback (`/printer/objects/query`) if the WS fails, still viewer-gated
- [x] SSE: one multiplexed `/ui/dashboard/stream`, server-rendered HTML per card (`<id>\n<html>`), swapped with `htmx.swap`; controls sent only on change; session re-checked every 60 s
- [x] `app.js`: close SSE on `visibilitychange` hidden / when the dashboard leaves the DOM, reopen when visible
- [x] Auto-refresh checkbox in the dashboard header, persisted in `user_prefs.auto_refresh` (default on) via `POST /account/prefs`
- [x] Auto-refresh on → SSE live updates; off → no SSE, per-card `[r] refresh` that calls `GET /p/{id}/status` (one snapshot on open, then manual only)
- [x] Toggling starts/stops the stream immediately without a reload
- [x] Printer card: state badge, filename, text progress bar, elapsed/ETA, layer if available
- [x] Sensors: extruder(s), bed, chamber/temperature sensors, fans; sparklines (rendered server-side from hub history)
- [x] Core controls: Start (file picker modal), Pause/Resume (state-aware), Cancel (confirm), E-STOP (confirm); role-gated; audit logged
- [x] Camera: webcam via `server.webcams.list` (or override, cached 60 s); snapshot proxy refreshed only while visible + auto-refresh on; MJPEG proxy on "live" click; upstream dropped on client disconnect
- [x] Offline / Klippy-not-ready / unauthorized states rendered cleanly per card
- [x] Integration tests with a mock Moonraker: page load makes no printer calls; viewers share one WS; WS closes after grace; quick return reuses it; auto-refresh off opens no WS; controls; camera; polling fallback
- [x] `cargo run --example mock_moonraker`: fake printers for UI work

## M4 — Files tab
- [x] Printer selector component (remembered in `user_prefs.selected_printer_id`; shared via `web/select.rs`)
- [x] Lazy-loaded listing: `/ui/files` renders instantly, `/p/{id}/files` loads the directory (name, size, modified, estimated time, filament, disk usage)
- [x] Navigation into subdirectories with URL/history, breadcrumb, `/` filter, `j/k` selection + Enter, sort by column
- [x] Metadata panel + proxied thumbnail (largest ≤ 400 px)
- [x] Upload: streaming passthrough (multipart piped through a channel, no buffering), size limit, text progress bar, optional "print after upload"
- [x] Delete file/directory (confirm), rename/move, copy, mkdir
- [x] Download (streamed proxy, Content-Disposition)
- [x] Start print from the file details (operator+)
- [x] Short-TTL (5 s) listing cache invalidated on mutation and printer edits
- [x] Path safety: reject `..`, absolute paths, empty segments, backslashes; encode segments
- [x] Audit entries for upload/delete/move/copy/mkdir/start

## M5 — Logs tab
- [x] Lazy-loaded `/ui/logs`: list files in the `logs` root (newest first) when the tab opens; viewer only on pick; deep links preload the viewer
- [x] Tail view: last N lines via a suffix `Range` request sized for N (widened once if short), else a bounded streaming ring buffer
- [x] Line count selector (200/500/2000), server-side highlighting (errors/warnings/dim Stats), client-side filter, CSS-only wrap toggle
- [x] Manual refresh (`r`) + optional follow mode (every 3 s; stops when unticked, the tab is left or the page is hidden)
- [x] Download full log (streamed)
- [x] Keyboard shortcuts work while a checkbox/button has focus (only text inputs swallow keys)

## M6 — System tab
- [ ] Lazy-loaded `/ui/system` partial
- [ ] `machine.system_info`: CPU, distribution, memory, network interfaces, SD card / disk, services, virtualization
- [ ] `machine.proc_stats`: CPU usage, memory, Moonraker process stats, system load, throttled state (Pi), network throughput, CPU temp
- [ ] Moonraker/Klipper versions (`server.info`, `machine.update.status` read-only)
- [ ] Manual `[r] refresh` always available; auto-refresh every `PRINTPIT_SYSTEM_REFRESH_SECS` only when the user's `auto_refresh` pref is on; timer stops when the tab is left
- [ ] TUI meters (`CPU [████░░░░] 41%`)

## M7 — Admin, hardening and polish
- [ ] User management (admin): create, disable, change role, reset password, delete
- [ ] Audit log view (admin) with filtering
- [ ] Keyboard shortcuts + `?` help overlay
- [ ] Responsive/mobile layout pass
- [ ] Theme toggle persisted in `user_prefs`
- [ ] Segmented `docker-compose.yml` example (frontend bridge + macvlan printer network) + docs for `PRINTPIT_BIND` / `PRINTPIT_EGRESS_BIND`
- [ ] Multi-arch image build (amd64, arm64)
- [ ] `cargo deny` / `cargo audit` in CI
- [ ] Performance check: image < 20 MB, idle RSS < 20 MB, page TTFB measured
- [ ] Security review pass (secrets in logs, SSRF, CSRF, authz on every route)
- [ ] Docs: configuration reference, reverse proxy examples (Caddy/Traefik/nginx), backup of `/data`

## Later / nice-to-have
- [ ] Camera flip/rotation from `server.webcams.list` (needs webcam info at render time without a page-load call)
- [ ] Use slicer `estimated_time` (file metadata) for ETA alongside the progress-based estimate
- [ ] TOTP 2FA
- [ ] Trusted-header SSO (behind an auth proxy)
- [ ] Job history view
- [ ] Temperature presets / set targets
- [ ] Notifications (print done/failed) via webhook
