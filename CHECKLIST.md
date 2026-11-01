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
- [x] Dockerfile (multi-stage, static musl binary, distroless/static nonroot, read-only rootfs friendly, HEALTHCHECK); image 11.9 MB at M0, 17.1 MB at M1 (SQLite), 16.7 MB at M2 (HTTP client + size-optimised build), 17.3 MB at M3, 18.0 MB at M4, 18.1 MB at M5, 18.4 MB at M6, 19.0 MB at M8; RSS ~3.5 MB with three live printers
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
- [x] Migration `0004`: `temp_presets(id, name UNIQUE NOCASE, extruder REAL NULL, bed REAL NULL, …)`; NULL = leave that heater alone, at least one set. No manual reordering: listed coolest hotend first (PLA → PETG → ABS), bed-only last
- [x] `db::presets`: list/get/create/update/delete; "temperature presets" panel on the Printers page (inline edit rows + add row, plain-form fallback), audit `preset_create/update/delete`
- [x] G-code goes through the existing `run_action` (now `pub(crate)`) to `POST /printer/gcode/script?script=…`; no new client method needed
- [x] Heater set from Klipper objects: `extruder*`, `heater_bed`, `heater_generic X` → `SET_HEATER_TEMPERATURE HEATER=… TARGET=…`; `temperature_fan X` → `SET_TEMPERATURE_FAN_TARGET`. Validate the heater name against `printer.objects.list` and a strict charset (no G-code injection: `[A-Za-z0-9_ ]`, no newlines); target finite, 0–500 (Klipper enforces `max_temp`)
- [x] Card control `temps` (operator+, ready printers) opens a modal `GET /p/{id}/temps` (like the start picker; inputs aren't replaced by live updates): per-heater current/target, set-target form, `off`; preset buttons; `cooldown` (`TURN_OFF_HEATERS`). Heater status comes from `hub.peek()` when the dashboard already has a live connection, else one HTTP snapshot; a preset's hotend temperature goes to `extruder` only
- [x] `POST /p/{id}/temp` (heater, target), `POST /p/{id}/preset/{preset_id}`, `POST /p/{id}/cooldown`; audit `temp_set`, `temp_preset`, `temp_cooldown`
- [x] Checked in Chromium at 1280 px and 390 px: modal fits, set → flash + modal closes, no console errors
- [x] Mock: `/printer/gcode/script` records scripts (and refuses targets > 300 like Klipper's `max_temp`), mock gained `heater_generic chamber`; `tests/temps.rs` covers validation (bad heater, injection attempt, out of range), role gating, audit, CSRF

### 8.4 Job history tab
- [x] `Tab::History` (`[5]History`, key `5`), `/ui/history` renders the printer selector instantly (no printer calls)
- [x] `/p/{id}/history?start=&limit=50` → `server.history.list` (newest first): end time, file, status badge, duration, filament (m), with prev/next paging and `j/k` selection
- [x] Totals line from `server.history.totals` (jobs, print time, filament, longest print), fetched concurrently with the page (`tokio::join!`); a totals failure just hides the line
- [x] Job details `/p/{id}/history/job?uid=` → `server.history.job` (times, durations, filament, slicer facts; thumbnail via the existing files thumb proxy if the file still exists). `files::facts`/`files::thumb_url` are shared with the Files tab. Job ids must be alphanumeric (≤ 32) before they go upstream; deep links `/ui/history?printer=&job=` preload the details
- [x] Reprint button (operator+, only when `exists`) → existing `print/start` path; confirm dialog
- [x] History component not loaded (404) → a clean "history is disabled in Moonraker" box
- [x] Checked in Chromium at 1280 px and 390 px (`5`, `j/k`, Enter, paging, reload of a deep link); on phones the ended/filament columns hide
- [x] Mock history endpoints (60 generated jobs, `no_history` option); tests: tab lazy (no calls on `/ui/history`), paging, details, reprint role gating, disabled component

### 8.5 Print notifications (webhook)
- [x] Config: `PRINTPIT_NOTIFY_WEBHOOK_URL` (+ `_FILE`; it usually embeds a token), validated as http(s); never logged or rendered
- [x] Migration: `printers.notify INTEGER NOT NULL DEFAULT 0`; checkbox in the printer form (shown only when the webhook is configured; without one, saving keeps the stored value), `NOTIFY` badge in the list, audited in `printer_update` (`notify=on|off`)
- [x] `notify.rs`: a supervisor holds one hub `Subscription` per enabled `notify` printer, **the one documented exception to rule 2**. Resynced on startup and from `state.printer_changed`
- [x] Edge detection on `print_stats.state` (`notify::Tracker`, pure and unit-tested): `printing|paused → complete|error|cancelled` only. The first state after printpit starts is a baseline. The last state survives reconnects (a print that ended while disconnected is still reported; the same finished job isn't reported twice). A Klipper shutdown mid-print (`Link::NotReady` shutdown/error) counts as `error`
- [x] Payload: JSON `{printer, event, filename, duration, message}` plus `text`/`content` summary fields so Slack/Discord-style webhooks work. Printer name only, never its address
- [x] Separate webhook `reqwest::Client` (rustls, no redirects, 10 s timeout, 3 tries: 0/1/3 s), not the printer egress client; failures log the root cause without the URL. Checked in the real app: token absent from logs at debug level
- [x] Update CLAUDE.md rule 2, PLAN.md §6 (item 8), README (config table + "Print notifications" section) and both compose files (empty `PRINTPIT_NOTIFY_WEBHOOK_URL`)
- [x] Tests: transitions fire once; baseline doesn't fire; disabled printer/flag drops the subscription; webhook URL absent from logs and pages; local test webhook receiver

### 8.6 TOTP 2FA
- [x] Deps: `hmac` 0.13 + `sha1` 0.11 (already in the lock graph via other crates; RFC 6238 hand-rolled in `auth::totp`: 6 digits, 30 s, ±1 step), `qrcode` 0.14 with only the `svg` feature (the one new package); the inline SVG has no `style=` (unit-tested)
- [x] Migration: `users.totp_secret_enc BLOB NULL`, `users.totp_last_step INTEGER` (replay guard), `totp_recovery(user_id, code_hash)`
- [x] Secret sealed with `Cipher` (AAD `totp-secret:<user id>`, so blobs can't be swapped between users); during setup the unconfirmed secret round-trips through the form sealed with AAD `totp-enroll:<user id>` (nothing stored until confirmed); recovery codes (10 × 50 bits, `abcde-fghij`, shown once) stored as SHA-256
- [x] Account page: enrol (QR + manual key, confirm with a code before enabling; turning it on signs out other sessions), disable and regenerate recovery codes (both need password + code, failures count in the rate limiter); warns when < 3 recovery codes remain
- [x] Login step 2: after the password succeeds for a 2FA user, set a short-lived (5 min) sealed (AES-GCM) pending-login cookie carrying user id, expiry and `next` and show `/login/totp`. The session is created only after a valid code or recovery code. Failures count in the login rate limiter
- [x] Admin: reset a user's 2FA on the Users page (revokes their sessions)
- [x] Audit: `totp_enable`, `totp_disable`, `totp_failed`, `totp_recovery_used`, `totp_reset`
- [x] Replay guard is an atomic conditional `UPDATE … WHERE totp_last_step < ?` (`db::totp::claim_step`), so two concurrent requests can't both use one code
- [x] Checked in Chromium at 390 px: QR renders (no CSP errors), enrol → sign out → password → code (computed by Node's own HMAC) → dashboard
- [x] Tests: RFC 6238 vectors, replay rejected, window, recovery single-use, pending cookie expiry/tamper, no session before step 2, rate limit, admin reset, secret never rendered after enrolment

### 8.7 Trusted-header SSO
- [x] Config: `PRINTPIT_AUTH_HEADER` (e.g. `Remote-User`) requires `PRINTPIT_AUTH_PROXY_CIDRS` (config error otherwise); optional `PRINTPIT_AUTH_LOGOUT_URL`
- [x] Trust the header only when the **direct peer** IP is in `PRINTPIT_AUTH_PROXY_CIDRS` (new `client_ip::PeerIp` extractor, IPv4-mapped addresses canonicalised; never the XFF-derived client IP); ignore it otherwise, and refuse duplicate headers. Checked against the real server with curl (ConnectInfo), not only in tests
- [x] `require_auth`: no valid session + trusted header → look up an **existing, enabled** user (no auto-create; unknown → 403 page naming no internals; new `AppError::Denied(msg)`). Create a normal session (so CSRF etc. are unchanged), set the cookie, audit `login` with `detail=sso`
- [x] Session exists but the header names a different user → drop it and switch to the header's user; header missing behind the proxy → fall back to the normal login page
- [x] Header logins skip TOTP (the proxy does MFA); password login still works; logout redirects to `PRINTPIT_AUTH_LOGOUT_URL` when set (validated: http(s) or a `/path`, not `//host`); `/login` skips the form when the proxy vouches for someone
- [x] Docs: README "Single sign-on" section with Caddy + Authelia and Traefik examples (both strip client-supplied `Remote-User` first), config table rows, security model; PLAN §3.8
- [x] Test client can set a peer address (`Client::peer` → `ConnectInfo`)
- [x] Tests: untrusted peer ignored (incl. spoofed X-Forwarded-For and no peer), unknown user 403, disabled user refused, user switch, CSRF still enforced, audit entry

### Wrap-up
- [x] README/docs config reference for the new env vars (README table + sections, PLAN §9 routes and §11 config); CLAUDE.md decision log entries for 8.1–8.7
- [x] Re-check image size and the lazy-load contract test after M8: amd64 image 19.0 MB (was 18.3), runs read-only, healthcheck ok, DB files 0600, idle cgroup memory 1.5 MiB (RSS 6.9 MB of which 5.7 MB is file-backed binary pages). The contract test now opens every tab (incl. History, Account with the 2FA panel) as page and partial with zero printer calls

## M9 — Slicing with OrcaSlicer (planned 2026-11-01)
Design: PLAN.md §15. An optional `printpit-slicer` sidecar runs the OrcaSlicer CLI; printpit owns jobs, profiles, auth and the upload to the printer. Off unless `PRINTPIT_SLICER_URL` is set.
Same rules as M8: new protected routes go into `PROTECTED_GET`/`PROTECTED_POST`, admin routes into the authz matrix, mock endpoints into both `tests/common` and `examples/`.

### 9.0 Spike (do first; record findings in the CLAUDE.md decision log)
- [x] Pin an OrcaSlicer release; confirm the Linux build runs headless in Debian-slim (extracted AppImage, which shared libs are needed, whether it needs Xvfb/OSMesa at all). **v2.4.2 on `ubuntu:24.04`** (needs glibc 2.38), no Xvfb/OSMesa; GUI libs replaced by generated trap stubs (PLAN §15.10)
- [x] Confirm the CLI flags for this version: `--load-settings`, `--load-filaments`, `--slice`, `--outputdir`, `--arrange`, `--orient`, exit codes, where it writes the G-code and log. `out/plate_<n>.gcode` + `out/result.json` (trust it, not the exit status); `--debug 3 --logfile`
- [x] Do G-code thumbnails render without a GPU (needed for Moonraker/Files thumbnails)? If not: Xvfb+Mesa in the image, or ship without thumbnails and note it. **No, not even with Xvfb+Mesa** (CLI passes a null thumbnail callback; 3MF path has a GL-version bug). Ship without
- [x] arm64: is there an upstream aarch64 Linux build? If not, decide: amd64-only worker (documented: run it on another host) vs. building Orca from source in CI. **Yes**, same deps and glibc floor; build both arches
- [x] How `inherits` resolution must work for CLI use (does the CLI resolve against bundled `resources/profiles` itself, or must we flatten?), and the preset bundle zip layout. **Must flatten** (vendor index, then `OrcaFilamentLibrary`); bundle layouts from source
- [x] Measure: image size, RAM peak and time for a 20 mm cube and a ~50 MB STL (amd64; arm64 if available). amd64: cube 0.2 s/87 MB, 50 MB STL 15.5 s/637 MB (20.7 s/725 MB with arrange+orient); image est. ~280 MB + worker
- [ ] arm64: run the golden test and the 50 MB measurement on real arm64 (couldn't run arm64 in the sandbox)
- [ ] Verify the flattener with a real GUI-exported user preset and an `.orca_printer` bundle (only system profiles were tested)

### 9.1 Worker (`slicer-worker/` crate, `Dockerfile.slicer`)
- [x] Cargo workspace with `slicer-worker` as a member (shares `rustfmt.toml`, `deny.toml`, lockfile); printpit's build and image unchanged. CI runs fmt/clippy/test with `--workspace`
- [x] `Dockerfile.slicer`: `ubuntu:24.04`, pinned Orca download checked by SHA-256 per `TARGETARCH`, `unsquashfs` extraction on `$BUILDPLATFORM` (offset from the ELF header), stripped binary + `lib/orca-runtime` + profile JSON only, non-root user, read-only root FS, `HEALTHCHECK` via the worker's own subcommand. amd64 image 325 MB
- [x] GUI-library stubs generated at build time (`slicer-worker/docker/gui_stubs.py`): one SONAME stub per GUI library, every imported function traps (`__builtin_trap()`, no libc), data symbols are zeroed buffers, versioned symbols under their version node (stderr is clean); refuses BIND_NOW binaries and thread-local imports
- [x] `inherits` flattener in the worker (vendor index → `OrcaFilamentLibrary`, loop/depth guard), used by `/v1/profiles` and `/v1/profiles/resolve`; user presets keep their nearest system parent in `inherits` (the CLI's compatibility check needs it); unit-tested against fixture profiles
- [x] Slice outcome from `out/result.json` (`return_code`/`error_string` → 422 with a `kind`), never from the exit status; `--mstpp` below the hard timeout; `--arrange 1` whenever a machine is enforced on a 3MF; the chosen process is made to accept the machine
- [x] Bearer-token auth on every route incl. unknown ones (`PRINTPIT_SLICER_TOKEN`/`_FILE`, ≥24 chars, constant-time compare; refuses to start without one)
- [x] `GET /v1/health`, `GET /v1/info` (versions, limits, busy, vendors), `GET /v1/vendors/{vendor}` (selectable profiles with compatibility), `GET /v1/profiles/{kind}/{vendor}/{name}` (flattened), `POST /v1/profiles/resolve` (user presets)
- [x] `POST /v1/slice`: multipart to a 0700 temp dir with fixed file names and a cleared environment, one slice at a time (503 + `Retry-After`, decided before the body is read), hard timeout, process group killed on timeout or client disconnect, temp dir always removed
- [x] Success: stream G-code back, stats (time, filament mm/g, layers) from the footer in `X-Slice-*` headers; failure: 422 `{error, kind, code, log_tail}` with paths scrubbed
- [x] Unit tests with a fake `orca-slicer` script (`tests/fake-orca.sh`): args, cleared env, timeout and disconnect kill the group, busy → 503, cleanup, bad uploads; golden test with real Orca (`slicer-worker/golden/run.sh`: system profiles, renamed user presets with their overrides, arrange/orient, garbage model) in `.github/workflows/slicer.yml` (amd64 + arm64 build, golden on amd64)
- [ ] `copies` (PLAN §15.4): Orca's `--clone-objects`/`--repetitions` semantics untested; check with real Orca and add to `Options`, or drop copies from the form
- [ ] Verify the 3MF `project_settings` path in the golden test (checked by hand with a CLI-made 3MF; add a GUI-made multi-plate project as a fixture)

### 9.2 printpit plumbing
- [x] Config: `PRINTPIT_SLICER_URL` (http(s), redacted in `Debug`), `PRINTPIT_SLICER_TOKEN`(+`_FILE`, required with the URL → config error otherwise), `_TIMEOUT_SECS`, `_MAX_MODEL_MB`, `_RETENTION_HOURS`. Also: no credentials/query in the URL, token checked like the worker's (≥24 chars), a token without a URL is an error
- [x] `slicer.rs`: own `reqwest::Client` (no redirects, connect timeout, no printer egress), `SlicerError` with `label()`/`detail()` like `MoonrakerError` (offline, unauthorized, busy, failed, timeout), address-free messages. As `src/slicer/` (`client`, `error`, `storage`); `state.slicing: Option<Arc<Slicing>>` (`state.slicer` is the 8.1 ETA cache). Covers every worker route; `slice()` streams the model up and the G-code to disk. Contract-tested against the real worker (`tests/slicer_client.rs`)
- [x] Migration `0007_slicer.sql`: `slicer_profiles`, `printer_slicer`, `slice_jobs` (PLAN §10); `db::slicer` repository (guarded status transitions, atomic `claim_next`, delete guard, restart sweep, retention set; `tests/slicer_db.rs`)
- [x] `/data/slicer/{job}/` storage helpers; created 0700, files 0600 (verify in the Docker image, not just locally). The directory is verified in the image (700, uid 65532; DB still 600); files are 0600 in tests
- [ ] Verify job files are 0600 inside the Docker image once 9.4 creates them (end-to-end slice in the container)

### 9.3 Profile library (admin)
- [ ] `[s]Slicer` admin page (`/ui/slicer`, only when the slicer is configured): list profiles by kind, delete (blocked while a printer or queued job uses it), `test` button → `/v1/health` + `/v1/info`
- [ ] Import system profiles from the worker's index (vendor → printer/nozzle → matching processes and filaments), stored flat
- [ ] Import user preset JSON and Orca preset bundles (zip size/entry limits, JSON only, resolved by the worker)
- [ ] Printers page: per-printer machine profile + default process/filament (`printer_slicer`), audited as `printer_slicer_update`
- [ ] Audit `slicer_profile_create/update/delete`; tests: admin-only, bad JSON, wrong `type`, bundle with junk entries, delete guard

### 9.4 Slice tab and jobs (operator)
- [ ] `Tab::Slice` (`[6]Slice`, key `6`), shown only when the slicer is configured; `/ui/slice` renders from the DB only (printer selector via `select::select()`, form, recent jobs). Viewers see jobs but not the form
- [ ] Form (fields before the file input): process, filament (filtered by `compatible_printers` where simple), whitelisted overrides (layer height, infill %, walls, supports off/normal/tree, brim, copies, arrange, orient), output name, "start print when done" (only when the printer is idle); 3MF "use project settings"
- [ ] `slicer::options`: typed fields → fixed Orca config keys with range checks; unknown keys impossible by construction; unit-tested
- [ ] `POST /slice/jobs`: streamed upload to disk with its own `DefaultBodyLimit`, magic-byte check (STL ascii/binary, 3MF zip, STEP, OBJ), `clean_name`, per-user queued-job cap
- [ ] `slicer::Queue` in `AppState` (started at boot like the notifier; tests start it themselves): queued → slicing → uploading → done/failed/cancelled; restart marks `slicing`/`uploading` as `failed (interrupted)` and resumes `queued`
- [ ] Upload to `gcodes/printpit/<name>.gcode` with the existing streaming client upload; ask before overwriting; invalidate the Files cache
- [ ] Job panel `GET /slice/jobs/{id}`: status, stats, log tail on failure; refresh button with `data-every` only while running and auto-refresh is on
- [ ] Actions: cancel (any stage), reslice (same model, new options), upload again, print (confirm; reuses `print/start`; refused while the printer is busy), delete (removes files)
- [ ] Retention sweep for `/data/slicer` (on the queue's tick, not a new per-view timer)
- [ ] Optional webhook events `slice_done`/`slice_failed` when notifications are configured
- [ ] Audit `slice_job_create/cancel/upload/print/delete`
- [ ] Checked in Chromium at 390 px: phone upload from the file picker, progress, done → print; no horizontal scroll, no console/CSP errors

### 9.5 Tests, deploy, docs
- [ ] Mock worker (`tests/common/slicer.rs` + `examples/mock_slicer.rs`): canned G-code with footer stats, fail/stall/busy modes, records requests
- [ ] `tests/slice.rs`: lazy tab (zero printer and worker calls), feature off when unconfigured (routes 404, no tab), upload limits and magic checks, queue order, cancel per stage, interrupted on restart, output lands in `gcodes/printpit/`, print gating, role matrix, CSRF, token absent from pages and logs, retention
- [ ] Compose: optional `slicer` service (compose profile) on an `internal: true` network, token secret, CPU/memory limits, read-only + tmpfs; both compose files
- [ ] `release.yml`/CI: build and push `printpit-slicer` (arches per the spike); `cargo deny` covers the workspace
- [ ] Docs: README "Slicing" section (setup, importing your Orca profiles, phone workflow, running the worker on another host), config table, PLAN §9/§10/§11 updated from "planned", CLAUDE.md rule 2 note (background upload after a slice is user-initiated, not polling) + decision log
