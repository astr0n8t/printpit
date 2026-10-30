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
- Replacing Mainsail/Fluidor for deep tuning (macros editor, config file editing, bed mesh visualisation, console).
- Slicing.
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
| Rate limiting | `tower_governor` (or a hand-rolled limiter on login) | Brute-force protection. |

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
3. **Sessions.** Sessions live on the server in SQLite. The cookie holds a random 256-bit ID, stored only as a SHA-256 hash, and is `HttpOnly; Secure; SameSite=Strict; Path=/`. Sessions have an idle timeout and an absolute timeout. Logging out deletes the row.
4. **CSRF.** Each session has a token. htmx sends it in an `X-CSRF-Token` header (set globally through `hx-headers`), and plain forms carry it in a hidden field. Every state-changing method is checked. `SameSite=Strict` is a second layer.
5. **Roles.**
   - `admin`: everything, including users and printers.
   - `operator`: can control printers, manage files and view logs/system, but cannot manage users or printers.
   - `viewer`: read-only (dashboard, camera, file list, logs, system).
6. **Password changes** for yourself, and **admin resets** for others. Changing a password revokes that user's other sessions.
7. **Audit log.** Control actions (start, pause, cancel, e-stop, file delete/upload, printer and user changes) are recorded with user, printer, action and timestamp, and can be viewed by admins.
8. *(Later)* Optional TOTP second factor, and optional trusted-header authentication for setups behind an SSO proxy.

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
7. **System tab refresh** is opt-in: manual refresh by default, with an optional auto-refresh toggle (e.g. 5 s) that stops when you leave the tab.

---

## 7. UI and UX (TUI aesthetic)

- **Typography:** a system monospace stack (`ui-monospace, "JetBrains Mono", "Cascadia Mono", Menlo, Consolas, monospace`), one font size scale, line height tuned so it reads like a character grid.
- **Palette:** dark terminal background with a phosphor-ish foreground, plus accent colours for states (printing = green, paused = yellow, error/offline = red, standby = dim). There is a light theme through `prefers-color-scheme` and a toggle. All colours are CSS custom properties.
- **Chrome:** panels drawn with CSS borders styled like box-drawing characters, with titles inset in the top border (`┌─ Voron 2.4 ─────┐`). The top bar is a tab strip with key hints: `[1]Dash [2]Files [3]Logs [4]System [p]Printers`.
- **Widgets:**
  - Progress: `[████████░░░░░░░░] 52%  ETA 01:12:03`
  - Temperatures: `extruder  214.8 / 215.0 °C  ▁▂▃▅▆▇` (sparkline from the last N samples, kept in the browser)
  - State badge: `[PRINTING]`, `[PAUSED]`, `[READY]`, `[OFFLINE]`
- **Keyboard:** number keys switch tabs, `j/k` move the selection in lists, `Enter` opens, `/` filters, `?` shows help, `Esc` closes dialogs. Every action is also clickable, and the layout works on touch screens and phones.
- **Destructive actions** (cancel, e-stop, delete) need a TUI-style confirmation modal (`Cancel print on Voron? [y/N]`).
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
GET  /p/{id}/events                     SSE: live status for one printer
GET  /ui/dashboard/stream               SSE: multiplexed status for all visible printers
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
GET  /ui/audit (admin)
GET  /healthz                           liveness (no auth, no printer calls)
GET  /static/*                          embedded assets, long cache + content hash
```

Every `/ui/*` route returns a partial when `HX-Request` is present, or the full shell with that tab pre-rendered otherwise, so deep links and reloads work.

---

## 10. Data model (SQLite)

```sql
users(id, username UNIQUE, password_hash, role, created_at, updated_at, disabled)
sessions(id_hash PK, user_id FK, csrf_token, created_at, last_seen_at, expires_at, user_agent, ip)
printers(id, name, base_url, api_key_enc BLOB NULL, api_key_nonce BLOB NULL,
         webcam_override NULL, enabled, sort_order, created_at, updated_at)
user_prefs(user_id PK, selected_printer_id, theme)
audit_log(id, user_id, printer_id NULL, action, detail_json, created_at)
login_attempts(key, window_start, count)      -- or kept in memory
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
| `PRINTPIT_WS_IDLE_GRACE_SECS` | `30` | Keep the Moonraker WebSocket open after the last viewer leaves |
| `PRINTPIT_MAX_UPLOAD_MB` | `1024` | Upload size limit |
| `PRINTPIT_TRUSTED_PROXIES` | *(unset)* | CIDRs whose `X-Forwarded-For` is trusted (for rate limiting and audit IPs) |
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

See `CHECKLIST.md` for the task-level breakdown.
