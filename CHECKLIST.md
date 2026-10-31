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
- [x] Dockerfile (multi-stage, static musl binary, distroless/static nonroot, read-only rootfs friendly, HEALTHCHECK); image 11.9 MB at M0, 17.1 MB at M1 (SQLite), 16.7 MB at M2 (HTTP client + size-optimised build), 17.3 MB at M3, 18.0 MB at M4, 18.1 MB at M5, 18.4 MB at M6; RSS ~3.5 MB with three live printers
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
- [x] Lazy-loaded `/ui/system`: two halves load on open (`/p/{id}/system/info`, `/p/{id}/system/stats`)
- [x] `machine.system_info`: model, CPU, distribution, memory, storage, python, services (state badges), virtualization; interface addresses + hostname admin-only, serial numbers never shown
- [x] `machine.proc_stats`: CPU usage (overall + per core), memory, Moonraker process stats, throttled flags (Pi), network throughput, CPU temp, uptime, websocket count; disk from the gcodes root
- [x] Moonraker/Klipper versions (`server.info`, `printer.info`) and read-only `machine.update.status?refresh=false`
- [x] Manual `[r] refresh` always available; only the stats half auto-refreshes every `PRINTPIT_SYSTEM_REFRESH_SECS`, only when the user's `auto_refresh` pref is on; the timer dies with the tab
- [x] TUI meters (`cpu [████░░░░] 41%`), amber ≥ 75 %, red ≥ 90 %

## M7 — Admin, hardening and polish
- [x] User management (admin): create, disable, change role, reset password, delete; self-protection + last-admin guard; revokes sessions
- [x] Audit log view (admin) with filtering (action/user/printer) and paging
- [x] Keyboard shortcuts + `?` help overlay
- [x] Responsive/mobile layout pass (no horizontal scroll at 390 px; tab strip row; optional columns hide)
- [x] Theme toggle (auto/dark/light) persisted in `user_prefs`
- [x] Segmented `docker-compose.segmented.yml` (Caddy on a frontend bridge, printer macvlan, `PRINTPIT_BIND` / `PRINTPIT_EGRESS_BIND`) — verified with a bridge stand-in
- [x] Multi-arch image build (amd64, arm64) — cross-compiled with cargo-zigbuild; `release.yml` publishes to GHCR (arm64 built and inspected here, not executed: no QEMU in the sandbox)
- [x] `cargo deny` (advisories, licences, bans, sources) in CI
- [x] Performance check: image 18.3 MB (amd64) / 17.5 MB (arm64); RSS 1.6 MB idle, 2.4 MB after load; median response 0.5–2.6 ms, p95 < 4 ms
- [x] Security review pass: no secrets/addresses in logs, SSRF via allowlist + egress, CSRF everywhere, authz matrix test for admin routes, HSTS with TLS
- [x] Docs: configuration reference, reverse proxy examples (Caddy/Traefik/nginx), backup of `/data`, security model

## M8 — Later items (planned 2026-10-31)
Order: smallest and most self-contained first; the auth changes (TOTP, SSO) last, because they touch the login path every test uses.
Every new protected route goes into `PROTECTED_GET`/`PROTECTED_POST` in `tests/auth.rs`; every new admin route goes into the authz matrix.
New mock endpoints go in both `tests/common/moonraker.rs` and `examples/mock_moonraker.rs`.

### 8.1 Slicer ETA
- [x] Hub: when `print_stats.filename` changes to a non-empty name, fetch `server.files.metadata` once (over the open RPC; in `poll_once` through a small per-printer `filename → estimated_time` cache) and keep it in the `Publisher`
- [x] `Snapshot` gains `estimated_time: Option<f64>` (cleared when the filename changes or empties; a metadata error just leaves it `None`)
- [x] Card: `eta 30:00 · slicer 28:12`, where slicer = `estimated_time - print_duration` (clamped ≥ 0; hidden when complete or unknown)
- [x] The hub's lookup goes over the open WebSocket (`server.files.metadata`), merging status updates that arrive during the call; the HTTP poller and manual refresh use `GET /server/files/metadata`. All share `state.slicer` (`SlicerTimes`, latest file per printer, cleared by `printer_changed`)
- [x] Tests: card unit tests (with/without slicer time, overrun clamps to 0); hub integration test: one metadata call per file, none when idle

### 8.2 Camera flip/rotation
- [x] `Webcam` carries `flip_h`, `flip_v`, `rotation` (0/90/180/270; anything else → 0) from `server.webcams.list`
- [x] Lazy camera tile: `.cam` loads `GET /p/{id}/camera/tile` on `hx-trigger="intersect once"`. It uses `WebcamCache` (the snapshot needs the same lookup, so no extra Moonraker calls) and renders the `<img>` with transform classes, or `[ no camera ]` server-side
- [x] CSS classes `cam-flip-h`, `cam-flip-v`, `cam-rot-{90,180,270}` (no inline styles, CSP); rotated 90/270 tiles keep the card's aspect box; check at 390 px
- [x] `app.js`: snapshot refresh and live toggle work on swapped-in tiles (existing `htmx:afterSettle` hook); stream-only cameras (override without `action=stream`) show `[ no snapshots · press live ]` and never load a snapshot
- [x] Checked in Chromium at 1280 px and 390 px: rotated test card fills the 16:9 frame, live/stop keeps the rotation, no horizontal scroll, no console errors
- [x] Orientation select in the printer form (migration `0003`, column `webcam_transform`): "auto" (NULL) uses Moonraker's settings, any other choice overrides them, for discovered and override cameras alike
- [x] Tests: tile HTML has the classes; dashboard page load still makes zero printer calls; mock webcam with flip/rotation

### 8.3 Temperature targets and presets
- [ ] Migration: `temp_presets(id, name UNIQUE NOCASE, extruder REAL, bed REAL, sort_order, created_at, updated_at)`
- [ ] `db::presets`: list/create/update/delete/reorder; admin UI section on the Printers page (`/ui/presets…` or inline panel), audit `preset_create/update/delete`
- [ ] `MoonrakerClient::gcode_script()` → `POST /printer/gcode/script?script=…`
- [ ] Heater set from Klipper objects: `extruder*`, `heater_bed`, `heater_generic X` → `SET_HEATER_TEMPERATURE HEATER=… TARGET=…`; `temperature_fan X` → `SET_TEMPERATURE_FAN_TARGET`. Validate the heater name against `printer.objects.list` and a strict charset (no G-code injection: `[A-Za-z0-9_ ]`, no newlines); target finite, 0–500 (Klipper enforces `max_temp`)
- [ ] Card control `temps` (operator+, ready printers) opens a modal `GET /p/{id}/temps` (like the start picker; inputs aren't replaced by live updates): per-heater current/target, set-target form, `off`; preset buttons; `cooldown` (all heaters 0)
- [ ] `POST /p/{id}/temp` (heater, target), `POST /p/{id}/preset/{preset_id}`, `POST /p/{id}/cooldown`; audit `temp_set`, `temp_preset`, `temp_cooldown`
- [ ] Mock: `/printer/gcode/script` records scripts; tests for validation (bad heater, injection attempt, out of range), role gating, audit, CSRF

### 8.4 Job history tab
- [ ] `Tab::History` (`[5]History`, key `5`), `/ui/history` renders the printer selector instantly (no printer calls)
- [ ] `/p/{id}/history?start=&limit=50` → `server.history.list` (newest first): end time, file, status badge, duration, filament (m), with prev/next paging and `j/k` selection
- [ ] Totals line from `server.history.totals` (jobs, print time, filament, longest job) loaded with the list
- [ ] Job details `/p/{id}/history/job?uid=` → `server.history.job` (metadata, thumbnail via the existing files thumb proxy if the file still exists)
- [ ] Reprint button (operator+, only when `exists`) → existing `print/start` path; confirm dialog
- [ ] History component not loaded (404) → a clean "history is disabled in Moonraker" box
- [ ] Mock history endpoints; tests: tab lazy (no calls on `/ui/history`), paging, details, reprint role gating, disabled component

### 8.5 Print notifications (webhook)
- [ ] Config: `PRINTPIT_NOTIFY_WEBHOOK_URL` (+ `_FILE`; it usually embeds a token), validated as http(s); never logged or rendered
- [ ] Migration: `printers.notify INTEGER NOT NULL DEFAULT 0`; checkbox in the printer form (shown only when the webhook is configured), audited with the printer update
- [ ] `notify.rs`: a supervisor holds one hub `Subscription` per enabled `notify` printer, **the one documented exception to rule 2**. Resynced on startup and from `state.printer_changed`
- [ ] Edge detection on `print_stats.state`: `printing|paused → complete|error|cancelled` only. The first snapshot after (re)connect sets the baseline and never fires, so reconnects don't double-notify
- [ ] Payload: JSON `{printer, event, filename, duration, message}` plus `text`/`content` summary fields so Slack/Discord-style webhooks work. Printer name only, never its address
- [ ] Separate webhook `reqwest::Client` (rustls, no redirects, 10 s timeout, 3 tries with backoff), not the printer egress client; document why in CLAUDE.md
- [ ] Update CLAUDE.md rule 2 and PLAN.md §6 with the exception
- [ ] Tests: transitions fire once; baseline doesn't fire; disabled printer/flag drops the subscription; webhook URL absent from logs and pages; local test webhook receiver

### 8.6 TOTP 2FA
- [ ] Deps: `hmac` + `sha1` (RustCrypto, RFC 6238 hand-rolled: 6 digits, 30 s, ±1 step), `qrcode` (SVG, no image deps) for the enrolment QR, checked against CSP (no `style=` in the SVG)
- [ ] Migration: `users.totp_secret_enc BLOB NULL`, `users.totp_last_step INTEGER` (replay guard), `totp_recovery(user_id, code_hash)`
- [ ] Secret sealed with `Cipher` (AAD `totp-secret`); recovery codes (10, random, shown once) stored as SHA-256
- [ ] Account page: enrol (QR + manual key, confirm with a code before enabling), disable (needs password + code), regenerate recovery codes
- [ ] Login step 2: after the password succeeds for a 2FA user, set a short-lived (5 min) HMAC-signed pending-login cookie and show `/login/totp`. The session is created only after a valid code or recovery code. Failures count in the login rate limiter
- [ ] Admin: reset a user's 2FA on the Users page (revokes their sessions)
- [ ] Audit: `totp_enable`, `totp_disable`, `totp_failed`, `totp_recovery_used`, `totp_reset`
- [ ] Tests: RFC 6238 vectors, replay rejected, window, recovery single-use, pending cookie expiry/tamper, no session before step 2, rate limit, admin reset, secret never rendered after enrolment

### 8.7 Trusted-header SSO
- [ ] Config: `PRINTPIT_AUTH_HEADER` (e.g. `Remote-User`) requires `PRINTPIT_AUTH_PROXY_CIDRS` (config error otherwise); optional `PRINTPIT_AUTH_LOGOUT_URL`
- [ ] Trust the header only when the **direct peer** IP is in `PRINTPIT_AUTH_PROXY_CIDRS` (not the XFF-derived client IP); ignore it otherwise
- [ ] `require_auth`: no valid session + trusted header → look up an **existing, enabled** user (no auto-create; unknown → 403 page naming no internals). Create a normal session (so CSRF etc. are unchanged), set the cookie, audit `login` with `detail=sso`
- [ ] Session exists but the header names a different user → drop it and switch to the header's user; header missing behind the proxy → fall back to the normal login page
- [ ] Header logins skip TOTP (the proxy does MFA); password login still works; logout redirects to `PRINTPIT_AUTH_LOGOUT_URL` when set
- [ ] Docs: Authelia/authentik + Caddy/Traefik examples, the proxy must strip client-supplied copies of the header
- [ ] Tests: untrusted peer ignored, unknown user 403, disabled user refused, user switch, CSRF still enforced, audit entry

### Wrap-up
- [ ] README/docs config reference for the new env vars; CLAUDE.md decision log entries
- [ ] Re-check image size and the lazy-load contract test after M8
