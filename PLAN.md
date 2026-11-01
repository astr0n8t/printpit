# printpit — Plan

A self-hosted web interface for controlling several Klipper/Moonraker 3D printers from one place.
It is written in Rust, ships as one Docker container, and has a minimal, fast UI styled like a terminal UI (TUI).

---

## 1. Goals

| # | Goal | What it means in practice |
|---|------|---------------------------|
| G1 | **Authentication first** | Nothing works until you log in. On first run there is a setup flow that creates the admin account. |
| G2 | **Multiple printers** | Add, edit and remove printers, each with a Moonraker URL and an optional API key. Keys are encrypted at rest. |
| G3 | **TUI look** | Monospace text, box-drawing panels, text progress bars, keyboard navigation, dark by default. No UI framework. |
| G4 | **Fast and responsive** | Server-rendered HTML with small partial updates, no SPA bundle, and assets embedded in the binary. |
| G5 | **Rust + Docker** | One static binary in a small, non-root container image. |
| G6 | **Dashboard** | Print status, camera and sensors for each printer, plus only the core controls: start, pause/resume, cancel/stop. |
| G7 | **Files tab** | List, upload, delete, rename/move and start G-code files on a printer. |
| G8 | **Logs tab** | List and view Moonraker log files (klippy.log, moonraker.log, …), tail first, download in full. |
| G9 | **System tab** | Host CPU, memory, disk, network, temperatures, services and versions of the printer host. |
| G10 | **Lazy loading** | A tab sends requests to Moonraker only when it is opened. Nothing is prefetched. |
| G11 | **A real server-side client** | Every Moonraker call is made by the printpit backend. The browser never talks to a printer, so Moonraker needs no CORS setup. |
| G12 | **Network segmentation** | Printer traffic leaves the container on its own network, separate from the network that serves the web app. |

### Non-goals (for v1)
- Replacing Mainsail/Fluidd for deep tuning (macros editor, config file editing, bed mesh visualisation, console).
- Slicing *inside the printpit binary or image*. Slicing is planned as an optional sidecar worker (§15, M9).
- OctoPrint or other non-Moonraker backends. The client sits behind a trait so they could be added later.
- Multiple tenants or organisations.

---

## 2. Architecture

```
            ┌──────────────────────── docker container ────────────────────────┐
 Browser ──►│  [web listener :8080]                                            │
 (HTTPS via │     axum router ── auth middleware ── handlers ── askama HTML    │
 reverse    │          │                                   ▲                   │
 proxy)     │          ▼                                   │                   │
            │     SQLite (users, sessions, printers, audit) │                   │
            │          │                                   │                   │
            │          ▼                                   │                   │
            │     PrinterHub ── per-printer MoonrakerClient (reqwest + WS)     │
            │                     │ egress bound to printer network            │
            └─────────────────────┼────────────────────────────────────────────┘
                                  ▼
                       printer network (VLAN / macvlan)
                       ├── Moonraker #1  (:7125, webcam :8080)
                       ├── Moonraker #2
                       └── …
```

### 2.1 Tech stack

| Concern | Choice | Why |
|---|---|---|
| HTTP server | `axum` + `tokio` + `tower-http` | Fast, ergonomic, works well with tower middleware (compression, timeouts, limits). |
| Templates | `askama` | Templates are checked at compile time, add no runtime cost, and give type-safe HTML. |
| Interactivity | **htmx** (vendored and embedded) + a small `app.js` | Partial HTML swaps make lazy-loaded tabs trivial and keep JS small. No build step. |
| Live updates | Server-Sent Events (SSE) from printpit to the browser | One-way push from server to browser is exactly what the dashboard needs. Works through proxies. |
| Moonraker HTTP | `reqwest` (rustls, streaming) | Talks JSON-RPC over HTTP, uploads files and proxies the camera stream. |
| Moonraker live | `tokio-tungstenite` WebSocket + `printer.objects.subscribe` | Moonraker pushes changes, so the dashboard does not poll the printer. |
| Database | SQLite via `sqlx` (with compile-time-checked queries if practical) | No extra service; a single volume holds the state. |
| Passwords | `argon2` (Argon2id) | Current best practice for password hashing. |
| Secrets at rest | `aes-gcm` (key from env var or Docker secret file) | Encrypts printer API keys. |
| Static assets | `rust-embed` | The binary is the whole app. |
| Logging | `tracing` + `tracing-subscriber` (JSON or pretty) | Structured logs. |
| Config | Environment variables (`PRINTPIT_*`), optional TOML file | Fits Docker. |
| Rate limiting | Hand-rolled in-memory limiter that counts failed logins (`auth/ratelimit.rs`) | Brute-force protection without penalising normal use. |

### 2.2 Proposed layout

```
Cargo.toml
build.rs                  (optional: embed git version)
migrations/               sqlx migrations
src/
  main.rs                 bootstrap, config, listener, graceful shutdown
  config.rs               env/TOML config
  error.rs                AppError -> HTTP response (HTML or htmx partial)
  state.rs                AppState (db pool, hub, crypto, config)
  crypto.rs               AES-GCM seal/open for secrets
  db/                     repositories: users, sessions, printers, audit
  auth/
    password.rs           argon2 hash/verify
    session.rs            server-side sessions, cookie handling
    csrf.rs               per-session CSRF token checks
    middleware.rs         require_user / require_role extractors
    setup.rs              first-run admin creation
  moonraker/
    client.rs             MoonrakerClient (HTTP JSON-RPC, files, logs, system)
    types.rs              serde models for Moonraker responses
    ws.rs                 WebSocket connection + object subscription
    hub.rs                PrinterHub: subscriber refcounts, cached state, fan-out
    net.rs                egress HTTP client builder (bind addr, CIDR allowlist)
  web/
    mod.rs                router assembly
    layout.rs             shared page chrome, tab bar
    dashboard.rs          overview grid + per-printer card + SSE
    controls.rs           start / pause / resume / cancel / e-stop
    camera.rs             snapshot + MJPEG proxy
    files.rs              G-code file management
    logs.rs               log list / tail / download
    system.rs             host monitoring
    printers.rs           add/edit/remove printers, test connection
    users.rs              user management (admin)
    auth.rs               login / logout / setup pages
templates/                askama templates (*.html), partials under templates/partials/
static/
  css/tui.css             the TUI theme
  js/htmx.min.js          vendored, pinned version
  js/app.js               keyboard shortcuts, confirm dialogs, SSE glue
Dockerfile
docker-compose.yml
```

---

## 3. Authentication and authorization

1. **First run.** If the `users` table is empty, every route redirects to `/setup`, where you create the first admin. A one-time setup token, printed to the container log, guards it so that whoever reaches the port first cannot claim the instance.
2. **Login.** Username and password are checked against an Argon2id hash. Failed attempts are rate-limited per IP and per username, with a constant-time response path.
3. **Sessions.** Sessions live on the server in SQLite. The cookie holds a random 256-bit ID, stored only as a SHA-256 hash, and is `HttpOnly; Secure; SameSite=Lax; Path=/`, named `__Host-printpit_session` when secure. (`Lax` instead of `Strict` keeps you signed in when you follow a link to printpit from elsewhere. Cross-site POSTs still carry no cookie.) Sessions have an idle timeout and an absolute timeout. Logging out deletes the row.
4. **CSRF.** Each session has a token. htmx sends it in an `X-CSRF-Token` header (set globally through `hx-headers`), and plain forms carry it in a hidden field. Every state-changing method is checked. `SameSite=Lax` and a `Sec-Fetch-Site: cross-site` rejection are extra layers. Login and setup, which happen before any session exists, use a double-submit cookie.
5. **Roles.**
   - `admin`: everything, including users and printers.
   - `operator`: can control printers, manage files and view logs/system, but cannot manage users or printers.
   - `viewer`: read-only (dashboard, camera, file list, logs, system).
6. **Password changes** for yourself, and **admin resets** for others. Changing a password revokes that user's other sessions.
7. **Audit log.** Control actions (start, pause, cancel, e-stop, file delete/upload, printer and user changes) are recorded with user, printer, action and timestamp, and can be viewed by admins.
8. Optional TOTP second factor (account tab; see `auth::totp`), and optional trusted-header authentication for setups behind an SSO proxy (`auth::sso`: `PRINTPIT_AUTH_HEADER`, believed only from `PRINTPIT_AUTH_PROXY_CIDRS` as the direct peer, existing enabled users only, skips TOTP).

---

## 4. Printer management

- Fields: `name`, `base_url` (e.g. `http://10.20.0.11:7125`), `api_key` (optional, encrypted), `webcam_override` (optional), `enabled`, `sort_order`.
- **Test connection** on save calls `server.info` and `printer.info` and shows Klippy state and versions.
- API keys are **never sent back to the browser**. The edit form shows `••••••• (set)` with a replace/clear option.
- The key is sent as an `X-Api-Key` header on HTTP and WebSocket requests. If Moonraker issues a oneshot token for the WebSocket, use it (`access.oneshot_token`).
- A URL check limits printer `base_url` hosts to the configured `PRINTPIT_PRINTER_CIDRS` allowlist (an SSRF guard), so that a user with printer-edit rights cannot make the container reach arbitrary internal services.

---

## 5. Moonraker client (server side)

All calls go through `MoonrakerClient`, one per printer, sharing one egress `reqwest::Client` per network profile.

| Area | Moonraker API used |
|---|---|
| Identity / health | `GET /server/info`, `GET /printer/info` |
| Live status | WebSocket `/websocket` → `printer.objects.subscribe` for `print_stats`, `virtual_sdcard`, `display_status`, `toolhead`, `extruder*`, `heater_bed`, `temperature_sensor *`, `temperature_fan *`, `fan`, `idle_timeout`, `webhooks`. The object list comes from `printer.objects.list` on connect. |
| Status fallback | `GET /printer/objects/query?...` (used only if the WebSocket is unavailable) |
| Controls | `POST /printer/print/start?filename=`, `/printer/print/pause`, `/printer/print/resume`, `/printer/print/cancel`, `/printer/emergency_stop` |
| Camera | `GET /server/webcams/list` → snapshot/stream URLs (proxied, never handed to the browser) |
| Files | `GET /server/files/list?root=gcodes`, `GET /server/files/directory`, `GET /server/files/metadata?filename=`, `POST /server/files/upload` (streamed multipart), `DELETE /server/files/gcodes/{path}`, `POST /server/files/move`, `POST /server/files/copy`, `POST/DELETE /server/files/directory`, `GET /server/files/gcodes/{path}` (download), thumbnails from metadata |
| History | `GET /server/history/list` (last job and job info on the files tab, optional) |
| Logs | `GET /server/files/list?root=logs`, `GET /server/files/logs/{name}` (streamed; tail by reading the end, using `Range` if supported, otherwise a bounded ring buffer) |
| System | `GET /machine/system_info`, `GET /machine/proc_stats`, `GET /machine/update/status` (read-only), `GET /server/info` |

Timeouts: connect 3 s, request 10 s (uploads and streams excepted). Errors are mapped to a typed `MoonrakerError` (offline, unauthorized, klippy not ready, HTTP error, decode error) and shown as a TUI-style error panel, not a crash.

---

## 6. Lazy loading and request minimization

This is the core rule: **printpit talks to a printer only because a logged-in user is looking at something that needs it.**

1. **Tabs are htmx partials.** The page shell loads with no printer data. Each tab (`Dashboard`, `Files`, `Logs`, `System`) is fetched with `hx-get` when clicked. Before it is opened, a tab costs zero Moonraker calls.
2. **No background polling when nobody is watching.** `PrinterHub` keeps a reference count of SSE subscribers per printer:
   - First subscriber: open the Moonraker WebSocket and subscribe to objects.
   - More subscribers: they share the same cached state and fan-out (a `tokio::sync::broadcast` channel). No extra printer traffic.
   - Last subscriber leaves: the WebSocket closes after a short grace period (default 30 s, so tab switching doesn't thrash).
3. **Throttled push.** Moonraker can send many updates per second. The hub merges deltas and sends the browser at most one update per `PRINTPIT_UI_REFRESH_MS` (default 1000 ms) per printer.
4. **Hidden tabs pause.** `app.js` closes the SSE stream when `document.visibilityState === 'hidden'` and reopens it when the page is visible again.
5. **The camera loads on demand.** Snapshots refresh only while the card is visible. MJPEG streams only after the user clicks "live", and the upstream connection drops when the client disconnects.
6. **Short-TTL caches** (e.g. a 5 s file list and a 2 s system info) absorb double-clicks and multiple users without refetching. Files are invalidated on upload, delete and move.
7. **Auto-refresh checkbox.** The main page (dashboard) has an `[x] auto-refresh` checkbox in its header. It is saved per user in `user_prefs.auto_refresh`, so it follows you across devices and reloads. It is on by default.
   - **On:** dashboard cards get live updates over SSE, and the System tab refreshes itself every `PRINTPIT_SYSTEM_REFRESH_SECS` (default 5 s) while it is open.
   - **Off:** no SSE stream and no timers. Each card and the System tab show a `[r] refresh` action that fetches a single snapshot (`/printer/objects/query` for cards). This is the lowest-traffic mode.
   - Toggling the box saves the setting (`POST /account/prefs`, CSRF-protected) and starts or stops the live stream right away, without a page reload.
   - The Logs tab's follow mode stays a separate toggle on that tab and is not remembered.
8. **The one exception: print notifications.** A printer an admin has opted in to notifications (`printers.notify`, only offered when `PRINTPIT_NOTIFY_WEBHOOK_URL` is set) is watched permanently: `notify.rs` holds a hub subscription for it, so its WebSocket stays open with or without viewers, and posts to the webhook when a print leaves `printing`/`paused` for `complete`, `error` or `cancelled`. All other printers stay lazy.

---

## 7. UI and UX (TUI aesthetic)

- **Typography:** a system monospace stack (`ui-monospace, "JetBrains Mono", "Cascadia Mono", Menlo, Consolas, monospace`), one font size scale, line height tuned so it reads like a character grid.
- **Palette:** dark terminal background with a phosphor-ish foreground, plus accent colours for states (printing = green, paused = yellow, error/offline = red, standby = dim). There is a light theme through `prefers-color-scheme` and a toggle. All colours are CSS custom properties.
- **Chrome:** panels drawn with CSS borders styled like box-drawing characters, with titles inset in the top border (`┌─ Voron 2.4 ─────┐`). The top bar is a tab strip with key hints: `[1]Dash [2]Files [3]Logs [4]System [p]Printers`.
- **Live updates:** the server renders each card's HTML and pushes it over one SSE stream per dashboard. See CLAUDE.md for the event format.
- **Widgets:**
  - Progress: `[████████░░░░░░░░] 52%  ETA 01:12:03`
  - Temperatures: `extruder  214.8 / 215.0 °C  ▁▂▃▅▆▇` (sparkline from the last N samples, kept in the browser)
  - State badge: `[PRINTING]`, `[PAUSED]`, `[READY]`, `[OFFLINE]`
- **Keyboard:** number keys switch tabs, `j/k` move the selection in lists, `Enter` opens, `/` filters, `?` shows help, `Esc` closes dialogs. Every action is also clickable, and the layout works on touch screens and phones.
- **Destructive actions** (cancel, e-stop, delete) need a TUI-style confirmation modal (`Cancel print on Voron? [y/N]`).
- **Dashboard header:** the printer count summary (`3 printers · 1 printing · 1 offline`) and the remembered `[x] auto-refresh` checkbox (see §6.7).
- **Dashboard layout:** a responsive grid with one card per printer. Each card shows the state, file, progress, temperatures, sensors, a camera tile and core controls (`Start`, `Pause/Resume`, `Cancel`, `E-STOP`). "Start" opens a small picker of recent or uploaded files.
- **Printer scope for tabs:** Files, Logs and System act on one printer at a time, with a printer selector at the top of the tab (remembered per user).

---

## 8. Network segmentation

There are two layers.

### 8.1 Container networking (docker-compose)
- `frontend` network: printpit's web listener. Only the reverse proxy (or the published port) can reach it.
- `printers` network: a `macvlan`/`ipvlan` network attached to the printer VLAN, or a bridge with routing to it. printpit's outbound Moonraker traffic leaves here.
- The web listener binds only to the frontend interface address (`PRINTPIT_BIND`, default `0.0.0.0:8080`, recommended: the frontend IP). This keeps the UI off the printer network.
- The egress HTTP client can bind to the printer-network interface (`PRINTPIT_EGRESS_BIND=<ip>`) so printer traffic always leaves through that interface.
- A sample `docker-compose.yml` ships with both topologies documented: simple (single bridge) and segmented (frontend bridge + macvlan).

### 8.2 Application level
- The printer URL allowlist (`PRINTPIT_PRINTER_CIDRS`) is enforced when you save a printer **and** when connecting, after DNS resolution. This blocks DNS rebinding.
- The camera proxy only fetches URLs that come from the printer's own Moonraker webcam config (or the admin override) and that pass the same allowlist.
- Printer hosts, URLs and API keys never appear in HTML or JS sent to the browser. The browser only sees `/p/{printer_id}/...` routes.
- A strict CSP (`default-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'`), plus `X-Content-Type-Options`, `Referrer-Policy` and `Permissions-Policy`.

---

## 9. HTTP routes (draft)

```
GET  /setup, POST /setup                first-run admin
GET  /login, POST /login, POST /logout
GET  /                                  shell, defaults to dashboard tab
GET  /ui/dashboard                      partial: printer grid (no live data yet)
GET  /ui/dashboard/stream               SSE: multiplexed card updates for all enabled printers (204 if auto-refresh off)
GET  /p/{id}/status                     one-shot HTTP snapshot (manual refresh)
GET  /p/{id}/start                      start-print file picker (modal)
POST /p/{id}/print/{start|pause|resume|cancel}
POST /p/{id}/estop
GET  /p/{id}/camera/snapshot            proxied JPEG
GET  /p/{id}/camera/stream              proxied MJPEG (multipart/x-mixed-replace)
GET  /ui/files?printer={id}&path=       partial: file browser
POST /p/{id}/files/upload               streamed upload passthrough
POST /p/{id}/files/move | /copy | /mkdir
DELETE /p/{id}/files?path=
GET  /p/{id}/files/download?path=
GET  /p/{id}/files/thumb?path=          proxied thumbnail
GET  /ui/logs?printer={id}              partial: log list
GET  /ui/logs/view?printer={id}&name=&lines=500
GET  /p/{id}/logs/download?name=
GET  /ui/system?printer={id}            partial: host info
GET  /ui/printers, POST /printers, POST /printers/{id}, DELETE /printers/{id}, POST /printers/{id}/test
GET  /ui/users, ... (admin)
GET  /ui/account, POST /account/password
POST /account/prefs                     save prefs (auto_refresh, theme, selected printer)
GET  /p/{id}/status                     one-shot status snapshot (manual refresh when auto-refresh is off)
GET  /ui/audit (admin)
GET  /healthz                           liveness (no auth, no printer calls)
GET  /static/*                          embedded assets, long cache + content hash
-- added in M8 --
GET  /p/{id}/camera/tile                camera tile with orientation (loaded when scrolled into view)
GET  /p/{id}/temps                      temperatures modal (operator)
POST /p/{id}/temp | /preset/{pid} | /cooldown
POST /presets, POST|DELETE /presets/{id}   temperature presets (admin)
GET  /ui/history, /p/{id}/history?start=, /p/{id}/history/job?uid=
GET  /login/totp, POST /login/totp      second sign-in step (public, needs the pending cookie)
GET  /account/totp/setup, POST /account/totp/{enable|disable|recovery}
POST /users/{id}/totp/reset             (admin)
-- planned in M9 (slicing, §15) --
GET  /ui/slice                          Slice tab: printer selector + upload form + recent jobs (no printer or worker calls)
POST /slice/jobs                        streamed model upload + options -> queued job (operator)
GET  /slice/jobs/{id}                   job panel (status, log tail, stats); refresh button with data-every while running
POST /slice/jobs/{id}/{cancel|reslice|upload|print}
DELETE /slice/jobs/{id}
GET  /ui/slicer, POST /slicer/profiles, POST|DELETE /slicer/profiles/{id}, POST /slicer/test   (admin)
```

Every `/ui/*` route returns a partial when `HX-Request` is present, or the full shell with that tab pre-rendered otherwise, so deep links and reloads work.

---

## 10. Data model (SQLite)

```sql
users(id, username UNIQUE, password_hash, role, created_at, updated_at, disabled)
sessions(id_hash PK, user_id FK, csrf_token, created_at, last_seen_at, expires_at, user_agent, ip)
printers(id, name UNIQUE NOCASE, base_url, api_key_enc BLOB NULL /* version||nonce||ciphertext */,
         webcam_override NULL, enabled, sort_order, created_at, updated_at)
user_prefs(user_id PK, selected_printer_id, theme, auto_refresh BOOL DEFAULT 1)
audit_log(id, user_id, printer_id NULL, action, detail_json, created_at)
-- failed-login counters are kept in memory (auth/ratelimit.rs), not in the DB
-- planned in M9 (§15.3, §15.5):
slicer_profiles(id, kind /* machine|filament|process */, name, source /* system|user */, settings_json, created_at, updated_at)
printer_slicer(printer_id PK FK, machine_profile_id FK, default_process_id NULL, default_filament_id NULL)
slice_jobs(id, user_id, printer_id, model_name, model_size, status, process_id, filament_id, options_json,
           output_name NULL, est_seconds NULL, filament_mm NULL, error NULL, log_tail NULL,
           created_at, started_at NULL, finished_at NULL)
```

---

## 11. Configuration

| Variable | Default | Purpose |
|---|---|---|
| `PRINTPIT_BIND` | `0.0.0.0:8080` | Web listener address |
| `PRINTPIT_DATA_DIR` | `/data` | SQLite DB location |
| `PRINTPIT_SECRET_KEY` / `PRINTPIT_SECRET_KEY_FILE` | *(required)* | 32-byte key (base64) for encrypting API keys |
| `PRINTPIT_SECURE_COOKIES` | `true` | Set to `false` only for plain-HTTP local testing |
| `PRINTPIT_SESSION_IDLE_MINS` / `_ABSOLUTE_HOURS` | `720` / `168` | Session lifetimes |
| `PRINTPIT_PRINTER_CIDRS` | `10.0.0.0/8,172.16.0.0/12,192.168.0.0/16` | Allowed printer address ranges |
| `PRINTPIT_EGRESS_BIND` | *(unset)* | Local IP for outbound printer traffic |
| `PRINTPIT_UI_REFRESH_MS` | `1000` | Max push rate to the browser per printer |
| `PRINTPIT_SYSTEM_REFRESH_SECS` | `5` | System tab interval when auto-refresh is on |
| `PRINTPIT_WS_IDLE_GRACE_SECS` | `30` | Keep the Moonraker WebSocket open after the last viewer leaves |
| `PRINTPIT_MAX_UPLOAD_MB` | `1024` | Upload size limit |
| `PRINTPIT_TRUSTED_PROXIES` | *(unset)* | CIDRs whose `X-Forwarded-For` is trusted (for rate limiting and audit IPs) |
| `PRINTPIT_NOTIFY_WEBHOOK_URL` (+`_FILE`) | *(unset)* | Webhook for print notifications (§6.8) |
| `PRINTPIT_AUTH_HEADER` | *(unset)* | Trusted-header SSO header, e.g. `Remote-User` (§3.8) |
| `PRINTPIT_AUTH_PROXY_CIDRS` | *(unset)* | Required with the header: peers whose header is believed |
| `PRINTPIT_AUTH_LOGOUT_URL` | *(unset)* | Logout destination behind an auth proxy |
| `PRINTPIT_SLICER_URL` | *(unset)* | Slicer worker base URL, e.g. `http://slicer:8090`. Unset = slicing off (§15, planned) |
| `PRINTPIT_SLICER_TOKEN` (+`_FILE`) | *(required with the URL)* | Shared bearer token for the worker |
| `PRINTPIT_SLICER_TIMEOUT_SECS` | `1800` | Kill a slice that runs longer than this |
| `PRINTPIT_SLICER_MAX_MODEL_MB` | `200` | Model upload limit |
| `PRINTPIT_SLICER_RETENTION_HOURS` | `72` | Delete models and output from `/data/slicer` after this |
| `RUST_LOG` | `info` | Log filter |

---

## 12. Docker

- Multi-stage build: `rust:<pinned>-alpine` (or `cargo-chef` + `musl`) produces a static binary. The final stage is `gcr.io/distroless/static` or `scratch` with CA certs.
- Runs as a non-root UID, with a read-only root filesystem and `/data` as the only writable volume.
- `HEALTHCHECK` uses the binary's own `--healthcheck` subcommand (no curl in the image).
- Images are multi-arch (`linux/amd64`, `linux/arm64`), since many people run this on a Pi.
- Target image size is under 20 MB, and idle RAM under 20 MB.

---

## 13. Testing strategy

- **Unit:** crypto round-trip, password hashing, CSRF checks, CIDR allowlist, Moonraker response parsing (fixtures captured from a real Moonraker), hub refcount and throttle logic.
- **Integration:** a mock Moonraker server (axum in the test, HTTP + WebSocket) checks that:
  - opening only the dashboard triggers no file, log or system calls (the lazy-load contract),
  - the WebSocket closes after the grace period once there are no viewers,
  - API keys are sent upstream and never appear in any response body,
  - all mutating routes reject missing or invalid CSRF tokens, and all routes reject unauthenticated requests.
- **E2E (optional):** Playwright smoke test for login → add printer → dashboard → tabs.
- **CI:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo audit`/`cargo deny`, Docker build.

---

## 14. Milestones

1. **M0 – Skeleton:** cargo project, config, logging, axum server, embedded static assets, TUI CSS shell, healthz, Dockerfile.
2. **M1 – Auth:** DB and migrations, setup flow, login/logout, sessions, CSRF, roles, rate limiting, security headers.
3. **M2 – Printers:** CRUD, encrypted API keys, CIDR allowlist, egress client, test connection.
4. **M3 – Dashboard:** Moonraker WS, PrinterHub, SSE, status cards, sensors, core controls, camera proxy.
5. **M4 – Files tab:** browse, metadata and thumbnails, upload, delete, move/rename, mkdir, start print.
6. **M5 – Logs tab:** list, tail view, download, auto-follow toggle.
7. **M6 – System tab:** system info, proc stats, services, versions.
8. **M7 – Hardening and polish:** audit log UI, user management, keyboard help, mobile layout, compose examples, multi-arch image, docs.

9. **M8 – Later items:** slicer ETA, camera orientation, temperatures, history, notifications, TOTP, SSO.
10. **M9 – Slicing (planned):** OrcaSlicer sidecar worker, profile library, Slice tab, slice → upload → print (§15).

See `CHECKLIST.md` for the task-level breakdown.

---

## 15. Slicing (M9, planned 2026-11-01)

**Goal:** upload an STL/3MF/STEP/OBJ from a phone, pick a printer, process and filament, and have printpit slice it with **OrcaSlicer**, put the G-code on that printer and (optionally) start it. No desktop needed.

### 15.1 Where the slicer runs: a sidecar worker
OrcaSlicer is a large C++/wxWidgets application (hundreds of MB with its GTK/Mesa dependencies). Putting it in the printpit image would break G5 (a ~19 MB distroless image) and would put a big C++ parser of untrusted model files in the same process space as the auth database and the secret key. So:

- A separate image, **`printpit-slicer`**, built from this repo (`slicer-worker/`, a small Rust crate in a cargo workspace plus `Dockerfile.slicer`). It is an `ubuntu:24.04` image (the release Orca's Linux AppImage is built for; see §15.10) with a pinned OrcaSlicer Linux release (AppImage extracted, checked by SHA-256, like the Zig wheel) and a tiny axum HTTP API wrapping the OrcaSlicer CLI.
- printpit reaches it at `PRINTPIT_SLICER_URL` with a shared bearer token (`PRINTPIT_SLICER_TOKEN`). Unset means the feature is off: no Slice tab, no slicer routes, same as the webhook.
- Because the boundary is HTTP, the worker can run on a beefier machine than a Pi running printpit. Slicing a big model on a Pi 4 can take minutes, and Orca may not ship an arm64 Linux build (see spike).
- The worker is **stateless**: each request carries the model and the fully resolved profiles. printpit owns the jobs, profiles, auth and audit.
- The worker gets its own `reqwest::Client` in printpit (`slicer.rs`, like the webhook client), not the printer egress client. Rule 4 is about printer traffic. Its URL is config only, never rendered, and errors are written address-free.

### 15.2 Worker API (internal, versioned)
Every route, unknown ones included, needs `Authorization: Bearer <PRINTPIT_SLICER_TOKEN>` (401 otherwise).
```
GET  /v1/health                            200 {status, orca_version}; 503 if Orca couldn't run at startup
GET  /v1/info                              {worker_version, orca_version, busy, timeout_secs, max_model_mb, vendors: [{name, version}]}
GET  /v1/vendors/{vendor}                  that vendor's selectable machines {name, printer_model, nozzle},
                                           processes and filaments {name, ..., compatible_printers(_condition)}
GET  /v1/profiles/{kind}/{vendor}/{name}   one system profile, flattened (kind: machine | process | filament)
POST /v1/profiles/resolve                  {presets: [...]} → {results: [{name, kind, profile} | {error}]}
                                           user presets flattened against the system library and each other
POST /v1/slice                             multipart: machine, process, filament (JSON), options (JSON), model (file, last)
                                           200: G-code (streamed); X-Slice-Print-Time-S, -Filament-Mm, -Filament-G, -Layers
                                           4xx/5xx: {error, kind, code?, log_tail?}
```
`options` are only the CLI's own switches: `{arrange?: bool, orient: bool, plate: 1.., project_settings: bool}`. Settings overrides (layer height, infill, …) are applied by printpit to the process JSON before it is sent (§15.4). With `project_settings` (3MF only), only the machine is sent and loaded.

Error `kind`s: `unauthorized`, `not_found`, `bad_request`, `too_large` (413), `busy` (503 + `Retry-After`), and for 422 slicer outcomes `model`, `profile`, `timeout`, `resources`, `slicer`, `crashed`. `code` is Orca's `CLI_*` return code and `log_tail` the last 40 lines of its log with the job's paths removed.

One slice at a time per worker: busy is decided before the body is read, so printpit doesn't upload a big model for nothing, and keeps the job queued. Each slice gets a 0700 temp dir (tmpfs) with fixed file names (`machine.json`, `model.stl`, …), a cleared environment (the token never reaches Orca) and its own process group, which is killed on the hard timeout or when the client disconnects. The command line, for 2.4.2:
`orca-slicer --datadir data --load-settings "machine.json;process.json" --load-filaments filament.json [--arrange 0|1] [--orient 1] --mstpp <timeout> --slice <plate> --outputdir out --debug 3 --logfile orca.log model.stl`
A 3MF is always `--arrange 1` unless told otherwise, since our machine replaces its own. The worker also makes the chosen process accept the machine (it adds the machine's system name to `compatible_printers`), since printpit's choice is deliberate and Orca would otherwise refuse it (−17). Flags are re-checked on every Orca bump (§15.10).

### 15.3 Profiles
OrcaSlicer's CLI wants complete machine/process/filament JSON. Presets made in the GUI only store the differences and point at a parent with `inherits`. printpit keeps a **profile library** (admin-managed, `slicer_profiles`) with three ways in:
1. **System profiles** from the worker's bundled Orca library (`/v1/info`): pick vendor → printer → nozzle, then the matching processes and filaments. Easiest from a phone.
2. **Import a user preset** JSON exported from OrcaSlicer (or the files from Orca's user preset folder). The worker resolves the `inherits` chain against its system library and the other imported presets, so what printpit stores is flat and self-contained.
3. **Import an Orca preset bundle** (*File → Export → Export preset bundle*, a zip) to bring a tuned printer with its filaments and processes in one go.

Each printer gets one **machine profile** (and optional default process/filament) on the Printers page (`printer_slicer`). At slice time the process and filament lists are filtered by Orca's `compatible_printers` / `compatible_printers_condition` where it can be evaluated simply; otherwise everything is shown.

Profiles hold G-code (`machine_start_gcode`, filament start G-code, …) that runs on the printer, so editing them is admin-only. (Operators can already upload arbitrary G-code, so this isn't a new privilege, but the library is shared by everyone.)

### 15.4 Slice options (the phone form)
A short, whitelisted set of overrides applied to a copy of the process profile. Never arbitrary keys:
- layer height (from the process's allowed range), infill density %, wall loops, supports (off / normal / tree, build plate only), brim (auto/off/width), copies, auto-arrange, auto-orient.
- Each one is a typed field with a range check, written as an Orca config key by a fixed table in `slicer::options`. Out-of-range values are form errors.
- **3MF projects:** an Orca `.3mf` can be sliced "with project settings" (its plate layout, painted supports, modifiers), with the printer's machine profile still enforced so G-code for the wrong printer can't be produced by accident.

### 15.5 Job flow
1. **Upload:** `POST /slice/jobs` streams the multipart file to `/data/slicer/{job}/model.<ext>` (size-limited, extension + magic check: binary/ASCII STL, zip for 3MF, `ISO-10303` for STEP), then creates a `slice_jobs` row (`queued`). The name goes through `clean_name`.
2. **Slice:** a single `slicer::Queue` task (in `AppState`, started at boot like the notifier) runs jobs in order: `slicing` → stream the model + profiles to the worker → stream the G-code back to `/data/slicer/{job}/out.gcode`. Stats (estimated time, filament length/weight, layer count) come from the response headers or the G-code footer.
3. **Upload to the printer:** `uploading` → the existing streaming `MoonrakerClient` upload to `gcodes/<folder>/<name>.gcode` (default folder `printpit/`, name `<model>_<process>_<filament>.gcode`, no overwrite without a confirm). Moonraker extracts metadata like any upload, so the Files tab and slicer ETA (8.1) just work. (No thumbnails: Orca 2.4.2's CLI doesn't write them, §15.10.)
4. **Done:** `done` with buttons `print` (operator, confirm dialog, reuses `print/start`; disabled while the printer is busy), `files` (link to the Files tab), `reslice` (same model, new options). Optional "start print when done" checkbox on the form, only offered if the printer is idle at submit time and re-checked before starting.
5. **Failure:** `failed` with the error and the last ~40 lines of the slicer log (worker log, never containing addresses or tokens). **Cancel** at any stage drops the worker request (which kills Orca) or aborts the upload.

On a restart, jobs left in `slicing`/`uploading` are marked `failed (interrupted)`; `queued` ones are resumed. Files under `/data/slicer` are deleted after `PRINTPIT_SLICER_RETENTION_HOURS`, and right away when the job is deleted. A small cap on queued jobs per user keeps a phone retry loop from filling the disk.

### 15.6 Lazy loading (rule 2)
- The Slice tab (`[6]Slice`, key `6`) renders from the DB only: printer selector, upload form, recent jobs. Opening it makes **no printer calls and no worker calls**. The profile picker comes from the DB.
- Job progress is shown by the job panel's refresh button with `data-every="2000"` while a job is `queued`/`slicing`/`uploading`, and only when the user's auto-refresh pref is on (otherwise a manual `[r] refresh`). No new timers, no new SSE stream.
- The upload to Moonraker after a slice and the optional print start are **user-initiated actions that finish in the background**, like a long file upload. They are not polling, so they don't break rule 2, but they're the reason the queue exists. CLAUDE.md rule 2 gets a sentence saying so.
- If the webhook is configured, `slice done` / `slice failed` can be posted too (reuses the `notify.rs` client), so you can walk away from the phone.

### 15.7 Security
- **Untrusted model files** are only ever parsed by the worker: non-root, read-only root FS, tmpfs work dir, CPU/memory limits in compose, a hard timeout, process-group kill, and **no network** (an `internal: true` compose network shared only with printpit). The worker never talks to printers.
- printpit itself only checks magic bytes and size. It never parses meshes.
- Token auth on every worker route (constant-time compare). The worker refuses to start without a token.
- Overrides are whitelisted (§15.4). Model and output names go through `clean_name`. The output path is always under `gcodes/`.
- Roles: slicing and uploading are `operator` (same as file upload). Starting a print is `operator` with a confirm. Viewers see jobs but no form. Profile library and per-printer mapping are `admin`.
- Audit: `slice_job_create`, `slice_job_cancel`, `slice_job_upload`, `slice_job_print`, `slice_job_delete`, `slicer_profile_create/update/delete`, `printer_slicer_update`.

### 15.8 Deployment
- Both compose files gain an optional `slicer` service (`ghcr.io/<owner>/printpit-slicer`) behind a commented-out block or a compose profile (`docker compose --profile slicer up`), on an internal network with printpit, plus a `slicer-token` secret.
- `release.yml` builds and pushes `printpit-slicer` for amd64 (and arm64 if an upstream arm64 build exists; see spike). CI builds the worker image on PRs that touch `slicer-worker/` or `Dockerfile.slicer`.
- `printpit-slicer` is pinned to one Orca version. Bumping it is a PR that updates the URL + hash and re-runs the golden test.

### 15.9 Testing
- A **mock worker** (`tests/common/slicer.rs` + `examples/mock_slicer.rs`) that answers `/v1/slice` with canned G-code (with Orca-style footer stats), can be told to fail, stall or 503, and records requests. All printpit tests use it; no test needs Orca.
- Worker unit tests: `inherits` resolution, option → config key mapping, arg building, timeout/kill, disconnect-kills-child (with a fake `orca-slicer` shell script).
- One **golden test** in the worker image's CI job: slice a 20 mm calibration cube with a bundled system profile and check that G-code comes out with plausible stats. That is the only test that runs real Orca, and it is also what guards the stubbed GUI libraries (§15.10): a GUI call traps, so the test fails.
- printpit integration tests: tab lazy (zero printer *and* worker calls on `/ui/slice`), upload limits and magic checks, queue order, cancel at each stage, interrupted jobs on restart, upload lands in `gcodes/printpit/`, print start gated by role and printer state, CSRF/auth matrix, token never in a page or log, retention cleanup.

### 15.10 Spike findings (9.0, 2026-11-01, OrcaSlicer 2.4.2)
Measured in the Claude cloud sandbox (4 vCPU amd64). arm64 was inspected but could not be run there (no QEMU).

- **Release:** pinned to **v2.4.2**, the latest stable (2.5.0 is at rc). Assets: `OrcaSlicer_Linux_AppImage_Ubuntu2404_V2.4.2.AppImage` (sha256 `d12fb8c8eac1aecd2dfb6377acd48f994f8fa439ed5292fa532dd82880f029fd`) and `OrcaSlicer_Linux_AppImage_Ubuntu2404_aarch64_V2.4.2.AppImage` (sha256 `e1a07275a25f176626c55a5df39e91bc4476d8c28ee4a3192ff758e29dd5c3ba`). Upstream publishes no checksums we could reach, so these are pinned on first download, like the Zig wheel.
- **arm64:** there is an upstream aarch64 AppImage with the same `NEEDED` set and glibc floor, so one Dockerfile covers both arches (`TARGETARCH` picks URL and hash). Running it and measuring it on arm64 is still to do on real hardware or in CI.
- **Base image: `ubuntu:24.04`, not Debian-slim.** The binary needs glibc 2.38 (bookworm has 2.36) and is built for Ubuntu 24.04. Trixie would probably work, but 24.04 is an exact ABI match.
- **Extraction:** `unsquashfs -o <offset>` on the AppImage. The offset is the end of the AppImage's ELF runtime (section header offset + count × size), checked for the `hsqs` magic. Searching for the first `hsqs` is wrong: the x86_64 runtime contains that string earlier. It needs no FUSE and doesn't run the AppImage, so it works when cross-building. The CLI needs only `bin/orca-slicer` and `lib/orca-runtime/` (libz, lzma, expat, mspack). `strip` cuts the binary from 124 to 98 MB. `resources/` is **not needed** when profiles are passed flat. Keep `resources/profiles/**/*.json` (53 MB) only for the worker's `/v1/info` index.
- **Headless:** no display, Xvfb, network or GPU is needed. It runs as a non-root user with a read-only root FS, `HOME=/tmp`, `--datadir /tmp/...` and `--network none`.
- **GUI libraries are stubbed.** The binary links GTK3, WebKitGTK 4.1, GStreamer, Soup, libsecret, X11/Wayland and GL, but a CLI slice calls none of them. Installing them costs ~710 MB. Instead, the worker image keeps the real libc/libstdc++/glib and replaces each other `NEEDED` library with an empty stub carrying the right SONAME. One `libprintpit-gui-stub.so` defines every remaining imported function as a trap (`__builtin_trap()`), so a call into GUI code crashes loudly instead of doing nothing. It also defines the imported data symbols (`_gst_debug_min`, nine `wl_*_interface`). Versioned imports (dbus, xkbcommon) go into their own library's stub under their version node. The generator is `slicer-worker/docker/gui_stubs.py`. The spike stubbed glib too: with it, the runtime image needs no apt packages at all. This works because the binary is lazily bound and imports no versioned symbols from those libraries. The result is a 125 MB base instead of 833 MB, and the G-code is **byte-identical** to a full install (cube and 1M-triangle torus, md5 of everything except the `generated by` line). The stubs are generated at build time from `readelf --dyn-syms`, so a version bump regenerates them, and the golden test is the guard. Measured image (9.1): 325 MB for amd64 (78 MB Ubuntu + a 154 MB Orca tree + the worker).
- **CLI (2.4.2):** `orca-slicer --datadir D --load-settings "machine.json;process.json" --load-filaments "filament.json" [--arrange 1] [--orient 1] --slice 0 --outputdir out model.stl`. The output is `out/plate_<n>.gcode`. Logs: stdout/stderr carry almost nothing. `--debug 3 --logfile F` gives a useful trace.
- **Result and exit codes:** Orca always writes `out/result.json` with `return_code` (the negative `CLI_*` codes from `libslic3r/Utils.hpp`) and `error_string` (readable English, no paths seen). The process exit status is that code mod 256 (−17 → 239). **The worker trusts `result.json`, not the exit status.** Observed: −3 file not found, −6 unparsable model (random bytes, a single degenerate facet), −17 process not compatible with printer, −50 no object fully on the plate (model bigger than the bed). Also useful: `--mstpp <secs>` (Orca's own per-plate time limit, −58) as a second line behind our hard timeout.
- **`inherits`: the CLI does not resolve it.** Unflattened system profiles fail with −17, because the compatibility lists live in the parents. **The worker must flatten:** walk `inherits` to the root and merge parent then child. For filaments, look in the vendor's own index first, then `OrcaFilamentLibrary` (Voron, for example, ships no filaments). Vendor indexes are `profiles/<Vendor>.json` with `machine_model_list`/`machine_list`/`process_list`/`filament_list` (`name`, `sub_path`). Entries with `"instantiation": "true"` are selectable; `fdm_*_common` are abstract bases. Flattened profiles slice correctly (a 50-line Python prototype was enough). User presets exported from the GUI use the same `inherits` scheme, but no real GUI export was tested. Verify with one in 9.3.
- **Preset bundles** (from the 2.4.2 source): `.orca_printer` is a zip with `bundle_structure.json` (`bundle_type: "printer config bundle"`, `printer_preset_name`, `printer_config`, `filament_config[]`, `process_config[]`) and files under `printer/`, `filament/`, `process/`. `.orca_filament` is `bundle_type: "filament config bundle"` with files under `<vendor>/`. The plain "presets (.zip)" exports have no `bundle_structure.json`, just JSON files. The importer accepts all three: if `bundle_structure.json` is present, use it, otherwise take every `*.json`.
- **3MF projects:** `--slice 0` on an Orca 3MF with no `--load-*` uses the project settings. Adding `--load-settings`/`--load-filaments` enforces our profiles. A different machine works, and Orca re-centres the objects for the new bed. A smaller bed fails with −50, so **always pass `--arrange 1` when enforcing a machine on a 3MF.**
- **Thumbnails: none in 2.4.2.** The CLI exports G-code with a `nullptr` thumbnail callback (`OrcaSlicer.cpp`), so no thumbnails are written with or without a GPU, even though the profile sets `thumbnails = 48x48/PNG,300x300/PNG`. Thumbnails are rendered only for `--export-3mf`, and that path is broken too: it requests an OpenGL context using the *GLFW library* version (3.4) and fails even under Xvfb + Mesa. Decision: **ship without thumbnails and without Xvfb/Mesa.** Moonraker and the Files tab work without them. A possible later improvement is a worker-side thumbnail (rasterise the mesh, inject a `; thumbnail begin` block).
- **G-code:** stats are in the footer: `; filament used [mm] =`, `[cm3]`, `[g]`, `; total filament used [g] =`, `; total layers count =`, `; estimated printing time (normal mode) = 9m 55s`. The header has `; total layer number:` and `; max_z_height:`. It emits Klipper `EXCLUDE_OBJECT_DEFINE` and `M73` progress.
- **Measurements (amd64, 4 vCPU):** 20 mm cube: 0.2 s, 87 MB peak RSS, 368 KB G-code. 50 MB binary STL (1.04M triangles): 15.5 s wall / 37.5 s CPU, 637 MB peak, 27 MB G-code. The same with `--arrange 1 --orient 1`: 20.7 s, 725 MB. **Compose limit: 2 GB memory** with headroom for bigger models. A Pi 4 will be several times slower, so the default timeout is 15 min.
