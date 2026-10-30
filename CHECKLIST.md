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
- [x] Dockerfile (multi-stage, static musl binary, distroless/static nonroot, read-only rootfs friendly, HEALTHCHECK); image 11.9 MB at M0, 17.1 MB at M1 (SQLite), 16.7 MB at M2 (HTTP client + size-optimised build); idle RSS < 2 MB
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
- [ ] `moonraker/ws.rs`: connect WebSocket (header or oneshot token), JSON-RPC request/response matching, reconnect with backoff
- [ ] `printer.objects.list` → build the subscription set (print_stats, virtual_sdcard, display_status, toolhead, extruder*, heater_bed, temperature_sensor/fan, fan, idle_timeout, webhooks)
- [ ] Merge `notify_status_update` partial deltas into the cached state
- [ ] `moonraker/hub.rs`: per-printer refcount, start on first subscriber, stop after `PRINTPIT_WS_IDLE_GRACE_SECS`, broadcast fan-out
- [ ] Throttle/coalesce updates to `PRINTPIT_UI_REFRESH_MS`
- [ ] HTTP fallback (`/printer/objects/query`) if the WS fails, still viewer-gated
- [ ] SSE endpoint(s) that render status partials (or JSON + small DOM patcher in app.js; pick one and record it in CLAUDE.md)
- [ ] `app.js`: close SSE on `visibilitychange` hidden, reopen when visible
- [ ] Auto-refresh checkbox in the dashboard header, persisted in `user_prefs.auto_refresh` (default on) via `POST /account/prefs`
- [ ] Auto-refresh on → SSE live updates; off → no SSE, per-card `[r] refresh` that calls `GET /p/{id}/status` (single `/printer/objects/query`)
- [ ] Toggling starts/stops the stream immediately without a reload
- [ ] Printer card: state badge, filename, text progress bar, elapsed/ETA, layer if available
- [ ] Sensors panel: extruder(s), bed, chamber/temperature sensors, fans; client-side sparklines
- [ ] Core controls: Start (file picker modal), Pause/Resume (state-aware), Cancel (confirm), E-STOP (confirm); role-gated; audit logged
- [ ] Camera: resolve the webcam via `server.webcams.list` (or override); snapshot proxy refreshed only while visible; MJPEG proxy on "live" click; upstream dropped on client disconnect
- [ ] Offline / Klippy-not-ready / unauthorized states rendered cleanly per card
- [ ] Integration test with a mock Moonraker: dashboard triggers only WS/status calls; WS closes after grace period with no viewers; with auto-refresh off, no WS is opened at all

## M4 — Files tab
- [ ] Printer selector component (remembered in `user_prefs`)
- [ ] Lazy-loaded `/ui/files` partial: directory listing (name, size, modified, estimated time, filament)
- [ ] Navigation into subdirectories, breadcrumb, `/` filter, `j/k` selection, sort by column
- [ ] Metadata panel + proxied thumbnail
- [ ] Upload: streaming passthrough (no full buffering), size limit, progress indicator, optional "print after upload"
- [ ] Delete file/directory (confirm), rename/move, copy, mkdir
- [ ] Download (streamed proxy)
- [ ] Start print from the file list (operator+)
- [ ] Short-TTL cache invalidated on mutation
- [ ] Path safety: reject `..`, absolute paths, encode segments
- [ ] Audit entries for upload/delete/move/start

## M5 — Logs tab
- [ ] Lazy-loaded `/ui/logs` partial: list files in the `logs` root
- [ ] Tail view: last N lines (Range request if supported, else a bounded streaming ring buffer)
- [ ] Line count selector (200/500/2000), client-side filter/highlight, wrap toggle
- [ ] Manual refresh + optional follow mode (stops when the tab is left)
- [ ] Download full log (streamed)

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
- [ ] TOTP 2FA
- [ ] Trusted-header SSO (behind an auth proxy)
- [ ] Job history view
- [ ] Temperature presets / set targets
- [ ] Notifications (print done/failed) via webhook
