# printpit — Implementation Checklist

Work through this top to bottom. Tick `[x]` when an item is done **and tested**. Add sub-items as you find new work.
See `PLAN.md` for design details and `CLAUDE.md` for rules.

## M0 — Skeleton
- [ ] `cargo init`, set edition 2021, add the core dependencies (axum, tokio, tower-http, serde, askama, rust-embed, tracing)
- [ ] `config.rs`: load `PRINTPIT_*` env vars, `*_FILE` secret variants, validation with clear errors
- [ ] `tracing` setup (pretty locally, JSON via `PRINTPIT_LOG_FORMAT=json`)
- [ ] `AppState` + `AppError` (HTML error panel rendering)
- [ ] axum server with graceful shutdown (SIGTERM/SIGINT)
- [ ] Embedded static assets with content-hash URLs and long-cache headers
- [ ] Vendor a pinned htmx into `static/js/` (record the version in CLAUDE.md)
- [ ] `tui.css`: palette variables (dark + light), monospace grid, box panels with inset titles, tab strip, buttons, badges, text progress bar, modal
- [ ] Base askama layout: top bar, tab strip with key hints, content region, footer/status line
- [ ] `GET /healthz` and a `--healthcheck` CLI subcommand
- [ ] Dockerfile (multi-stage, static musl binary, distroless/scratch, non-root, read-only rootfs friendly, HEALTHCHECK)
- [ ] `docker-compose.yml` (simple single-network variant first)
- [ ] CI workflow: fmt, clippy, test, docker build
- [ ] README quickstart

## M1 — Authentication (before any printer features)
- [ ] sqlx + SQLite pool, migrations folder, run migrations on startup
- [ ] Migrations: `users`, `sessions`, `user_prefs` (incl. `auto_refresh` default 1), `audit_log`
- [ ] `password.rs`: Argon2id hash/verify (+ tests)
- [ ] First-run setup: detect no users, generate a one-time setup token and log it, `/setup` form creates the admin
- [ ] Login page (TUI styled) + `POST /login`
- [ ] Server-side sessions: random 256-bit ID, SHA-256 stored, cookie flags (HttpOnly, Secure, SameSite=Strict), idle + absolute expiry, periodic cleanup task
- [ ] `POST /logout`
- [ ] Auth extractors: `CurrentUser`, `RequireRole<Operator>`, `RequireRole<Admin>`; unauthenticated → redirect to `/login` (or `HX-Redirect` for htmx)
- [ ] CSRF: per-session token, `hx-headers` injection in the layout, hidden field for forms, middleware check on non-GET
- [ ] Login rate limiting (per IP + per username), trusted-proxy aware client IP
- [ ] Security headers middleware (CSP, nosniff, referrer policy, frame-ancestors none, permissions policy)
- [ ] Account page: change your own password (revokes other sessions)
- [ ] Tests: unauthenticated access rejected on every route, CSRF enforced, session expiry, rate limit

## M2 — Printer management
- [ ] Migration: `printers` table
- [ ] `crypto.rs`: AES-256-GCM seal/open with random nonce, key from config (+ round-trip test)
- [ ] `moonraker/net.rs`: egress `reqwest::Client` builder (rustls, timeouts, optional `local_address` = `PRINTPIT_EGRESS_BIND`)
- [ ] CIDR allowlist check with a custom DNS resolver that rejects disallowed IPs after resolution (+ tests incl. rebinding case)
- [ ] `MoonrakerClient::new(printer)`: base URL, decrypted key → `X-Api-Key` header
- [ ] `server.info` / `printer.info` calls + typed models
- [ ] Printers page (admin): list, add, edit, delete, reorder, enable/disable
- [ ] API key never rendered back (show "set"/"not set", replace/clear controls)
- [ ] "Test connection" button showing Klippy state + versions, or a typed error
- [ ] Audit entries for printer create/update/delete
- [ ] Test: API key never appears in any HTTP response body

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
