# CLAUDE.md

Project memory for printpit. Keep this short and current. Update it when a decision changes or a gotcha is found.

## What this is
printpit is a Rust web app, shipped as a Docker image, for controlling several Moonraker/Klipper 3D printers. It has built-in auth and a minimal TUI-style UI.
- `PLAN.md`: architecture and design (the source of truth for *why*).
- `CHECKLIST.md`: the implementation checklist. **Tick items off (`[x]`) as you finish them and add new items as you discover work.** Read it before you start a session's work.

## Non-negotiable rules
1. **The browser never talks to a printer.** All Moonraker HTTP and WebSocket traffic comes from the backend. Printer URLs and hosts must never appear in any response body, HTML, JS or log line, with one exception: the admin-only printer settings pages (`web/printers.rs`). API keys never appear anywhere, not even there. The browser only knows `/p/{printer_id}/...`. `MoonrakerError` messages are written to be address-free (`reqwest::Error::without_url`, plus only the root cause), so they are safe to show.
2. **Lazy loading.** A tab or view makes Moonraker calls only when the user opens it. There is no prefetching and no background polling without an active viewer. The dashboard's live data goes through `PrinterHub` (refcounted WebSocket, one per printer, closed after an idle grace period). Don't add a Moonraker call to a shared layout or shell. The user's `auto_refresh` pref (a checkbox on the dashboard, stored in `user_prefs`) gates all automatic refreshing: when it is off, there is no SSE and no timers, only manual refresh.
3. **Auth on everything** except `/login`, `/setup` (while no users exist), `/healthz` and `/static/*`. State-changing routes require a CSRF token and the right role (`admin` > `operator` > `viewer`).
4. **Egress is segmented.** Outbound printer requests use the egress client from `moonraker/net.rs`, which applies `PRINTPIT_EGRESS_BIND` and the `PRINTPIT_PRINTER_CIDRS` allowlist, checked *after* DNS resolution. Never build an ad-hoc `reqwest::Client` for printer traffic.
5. **Secrets.** Printer API keys are encrypted with AES-GCM (`crypto.rs`) using `PRINTPIT_SECRET_KEY`. Passwords use Argon2id. Session IDs are stored as SHA-256 hashes only.
6. **No CORS.** Don't add CORS layers. Everything is same-origin.

## Stack and conventions
- Rust edition 2024, `rustfmt.toml` max_width 110. Library crate (`src/lib.rs`) + thin `main.rs`; integration tests in `tests/` drive `web::router` with `tower::ServiceExt::oneshot`.
- axum 0.8 + tokio, askama 0.16 templates (`templates/`), htmx **2.0.10** (vendored at `static/js/htmx.min.js`, sha512 checked against npm), SSE for live updates, sqlx + SQLite, reqwest (rustls) + tokio-tungstenite for Moonraker.
- Static files: `static/` is embedded via rust-embed; reference them in templates as `{{ layout.asset("css/tui.css") }}`, which returns `/static/{hash}/...`.
- No JS framework and no frontend build step. Keep `static/js/app.js` small. CSS custom properties for theming live in `static/css/tui.css`.
- `/ui/*` routes return an htmx partial when `HX-Request` is set, or the full shell otherwise, so deep links work. Use `web::layout::page(hx, Tab::X, html)`. It also re-renders the tab strip out of band (`hx-swap-oob`). New tabs go in `layout::Tab`.
- CSP is strict (`script-src 'self'; style-src 'self'`): no inline `<script>`, no `style=""` attributes, no `on*=` handlers. htmx is configured through the `htmx-config` meta tag in `base.html`. Indicator styles are off, and 4xx/5xx responses *are* swapped so error panels show.
- Errors: `AppError` renders just an error panel. The `error::wrap_full_page` middleware wraps it in page chrome for non-htmx requests. `MoonrakerError` variants (offline, unauthorized, klippy not ready, …) render as an inline TUI error box and never panic.
- Auth plumbing: protected routes live in the `protected` router in `web/mod.rs`, behind `session::require_auth` then `csrf::verify`. Handlers take `CurrentUser` or `RequireRole<Admin|Operator>`. **When you add a protected route, also add it to `PROTECTED_GET`/`PROTECTED_POST` in `tests/auth.rs`.**
- Audit every state-changing action with `db::audit::record` (it never fails the request).
- DB: runtime `sqlx::query*` with `&'static str` SQL (sqlx 0.9 requires `SqlSafeStr`), no compile-time macros or `DATABASE_URL`. Timestamps are unix seconds (`db::now()`). Add schema changes as new files in `migrations/`; never edit an applied one.
- Tests: `tests/common` has `TestApp` (temp DB) and a cookie-tracking `Client` (`setup`, `login`, `post_with`, `session_csrf`). `TestApp::with_admin()` gives a signed-in admin.
- Config comes only from `PRINTPIT_*` env vars (plus `*_FILE` variants for Docker secrets).
- Before committing: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Moonraker notes
- Build clients with `state.printer_client(&printer)`. It decrypts the key and validates the URL against the allowlist. Show errors with `MoonrakerError::label()` (badge) + `detail()`.
- Egress (`moonraker/net.rs`): one shared `reqwest::Client` with the allowlist DNS resolver, `no_proxy()`, no redirects, a 3 s connect timeout and an optional `local_address`. IP-literal hosts bypass DNS, so `Egress::check_url` covers them (`MoonrakerClient::new` calls it). Use `Egress::vet` at save time for friendly form errors.
- TLS: reqwest is built with `rustls-no-provider`, and `Egress::new` installs rustls' `ring` provider. Don't enable reqwest's default features; they pull in aws-lc-rs (C/CMake, painful on musl).
- Auth header: `X-Api-Key: <key>`. The WebSocket sends it on the upgrade request.
- WebSocket: `MoonrakerClient::websocket()` upgrades a request on the egress client (`Response::upgrade`) and wraps it with `tokio_tungstenite::WebSocketStream::from_raw_socket`. Never open sockets any other way. `rpc::Rpc` does sequential JSON-RPC calls; `recv()` is cancel-safe for `select!`.
- Hub (`moonraker/hub.rs`): `state.hub.subscribe(id)` returns a `Subscription` (a watch receiver plus a refcount guard). Call `state.printer_changed(id)` after editing, toggling or deleting a printer. `poll_once()` is the one-shot HTTP status used for manual refresh and the fallback.
- Webcams: relative URLs from `server.webcams.list` resolve against the printer host, dropping port 7125 (served by nginx, not Moonraker). The admin's webcam override is a stream URL; the snapshot URL is derived from `action=stream`.
- JSON-RPC over the WebSocket: `printer.objects.subscribe`. Updates arrive as `notify_status_update` with *partial* objects, so merge them into the cached state. Don't replace it.
- File roots: `gcodes` (manage), `logs` (read). Paths are relative to the root. Always run user paths through `moonraker::files::clean_path` / `clean_name`, and use `encode_path` for URL paths. Directory endpoints take root-prefixed paths (`gcodes/sub`); metadata and print start take root-relative names.
- Per-printer tabs (Files, Logs, System) use `web::select::select()` for the remembered printer selector, and `select::enabled_printer()` for `/p/{id}/...` routes. `web::files::attachment()` streams any upstream file as a download.
- Log tails: `MoonrakerClient::tail()` sends `Range: bytes=-N` (Tornado honours it) and falls back to streaming with a bounded ring buffer. `Tail::last_lines()` drops the partial first line when the window starts mid-file.
- Uploads: `files::upload` pipes the browser's multipart field into a `reqwest::Body` through an mpsc channel, joined with the upstream request, so nothing is buffered. Form fields must precede the file input. The route has its own `DefaultBodyLimit` (max upload + 1 MB).
- Timed refresh: render `data-every="<ms>"` on a refresh button (only when the user's auto-refresh pref is on) and `app.js` clicks it on that interval while the page is visible. The clock starts on `htmx:load`. Don't add other timers.
- Host identifiers (interface IPs, MACs, hostname) count as printer addresses under rule 1: admin-only on the System tab.
- `/printer/*` endpoints fail with an error when Klippy isn't ready. Show that state instead of treating it as offline.

## Commands
```
PRINTPIT_SECRET_KEY=$(openssl rand -base64 32) PRINTPIT_SECURE_COOKIES=false PRINTPIT_BIND=127.0.0.1:8080 cargo run
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
docker build -t printpit .
docker compose up            # needs PRINTPIT_SECRET_KEY in .env
printpit healthcheck         # probes GET /healthz on PRINTPIT_BIND (used by Docker HEALTHCHECK)
cargo run --example mock_moonraker   # fake printers on 127.0.0.1:7201-7203 (run printpit with PRINTPIT_PRINTER_CIDRS=127.0.0.0/8)
```

## Gotchas / decisions log
- *(add entries as `YYYY-MM-DD: note`)*
- 2026-10-30: The project started. Decided on server-rendered HTML + htmx + SSE instead of an SPA, to meet the speed and minimal-UI goals and so that tab partials are lazy by construction.
- 2026-10-30: The auto-refresh checkbox lives on the dashboard and is remembered per user (`user_prefs.auto_refresh`, default on). It controls dashboard SSE and System tab auto-refresh. Logs follow mode is separate. The compose file ships both the simple and the segmented variants.
- 2026-10-30: `/healthz` is routed *after* the TraceLayer so Docker health probes don't spam the logs. It still gets the security headers.
- 2026-10-30: Runtime image is `gcr.io/distroless/static-debian12:nonroot` (UID 65532, CA certs included). The builder is `rust:1.97-alpine`, which cross-compiles static musl binaries with `cargo-zigbuild` (`--platform=$BUILDPLATFORM`), so amd64 and arm64 both build at native speed. Zig comes from the `ziglang` PyPI wheel, downloaded with busybox `wget` and checked by SHA-256 (Alpine doesn't package it). To bump Zig, update the wheel paths and hashes from `https://pypi.org/pypi/ziglang/<version>/json`.
- 2026-10-30: Claude cloud sandbox only: `dockerd` isn't running by default (start it with `dockerd &`). Builds there need the proxy CA: build with a scratch copy of the Dockerfile that `COPY`s `/root/.ccr/ca-bundle.crt`, sets `CARGO_HTTP_CAINFO`/`SSL_CERT_FILE`, and appends it to `/etc/ssl/certs/ca-certificates.crt` (for busybox `wget`). Never commit that to the real Dockerfile. The Alpine CDN is blocked there (don't add `apk add` steps), and binfmt/QEMU can't be installed, so arm64 images can be built but not run.
- 2026-10-30: Session and pre-session cookies use `SameSite=Lax`, not the `Strict` the plan first said. With `Strict`, following a link to printpit from chat or email showed the login page. Every state change still needs a CSRF token.
- 2026-10-30: Login rate limiting is hand-rolled and in memory. It counts failures only: 10 per username and 20 per IP in 15 minutes. A successful login clears the username counter, not the IP counter. The limit resets on restart, which is acceptable.
- 2026-10-30: Argon2 is built with opt-level 3 even in dev (`[profile.dev.package.argon2]`), otherwise tests crawl.
- 2026-10-30: SQLite can `fchmod` an empty database file back to 0644 when it opens it. This happened inside the container (non-root) but not locally (root). `db::open` therefore re-applies 0600 to `printpit.db`, `-wal` and `-shm` after opening. Verify permission changes in the Docker image, not only in local tests. The image is 17.1 MB with bundled SQLite.
- 2026-10-30: API keys are sealed with AES-256-GCM under an HKDF-derived subkey, in one blob (`version || nonce || ct`) in `printers.api_key_enc`, with AAD `printer-api-key`. If `PRINTPIT_SECRET_KEY` changes, the key shows as "unreadable" and test connection reports CONFIG.
- 2026-10-30: Confirmations use `hx-confirm="…?"`. `app.js` turns that into the TUI `<dialog id="confirm">` (y/n/Esc) in `shell.html`. Reuse it for cancel and e-stop in M3.
- 2026-10-30: Release profile: fat LTO with `opt-level = "s"`, but Argon2 and Blake2 stay at `opt-level = 3`. The binary went from 9.8 MB to 6.8 MB. Do **not** use `panic = "abort"`: a panic in one handler would kill the whole server.
- 2026-10-30: Live updates use server-rendered HTML pushed over **one multiplexed SSE stream** (`/ui/dashboard/stream`). Event data is `<element id>\n<html>`, and `app.js` applies it with `htmx.swap` (no htmx SSE extension). One stream instead of one per printer avoids the browser's ~6-connections-per-host limit. Card controls (`ctl-{id}`) are pushed only when they change, so buttons aren't replaced under the cursor (that broke `hx-confirm`).
- 2026-10-30: Sparklines are rendered server-side from the hub's temperature history (40 samples, every 2 s), so there is no client-side state. In manual mode there is no history, so no sparklines.
- 2026-10-30: Unauthenticated `Accept: text/event-stream` requests get a 401, not a redirect, so EventSource stops retrying. A 204 from the stream means auto-refresh is off.
- 2026-10-30: Shell gotcha: `pkill -f <pattern>` kills its own shell if the same command line contains the pattern text anywhere. Run `pkill` as a separate Bash call (exit code 144 is the symptom).
- 2026-10-30: Askama escapes `&` as `&#38;` (not `&amp;`). Keep that in mind when asserting on rendered URLs in tests.
- 2026-10-30: The mock Moonraker records request paths without the HTTP method, so `POST` and `GET /server/files/directory` look the same in `mock.paths()`.
- 2026-10-30: Follow mode on the Logs tab is a browser-side timer that clicks `button[data-log-refresh]` every 3 s while `input[data-follow]` is ticked and the page is visible. It's a per-view toggle, not the remembered auto-refresh pref.
- 2026-10-30: Releases: `.github/workflows/release.yml` pushes multi-arch images to `ghcr.io/<owner>/printpit` on `main` and `v*` tags. CI builds both architectures on every PR (no push) and runs `cargo deny check` (see `deny.toml`).
- 2026-10-30: CI uses `dtolnay/rust-toolchain@stable`, which can be newer than the local toolchain, so new clippy lints show up there first. Run `rustup run stable cargo clippy --all-targets -- -D warnings` before pushing. Clippy 1.99's `result_large_err` flags `Result<_, Response>` (128 bytes), so helpers that return an early response use `Box<Response>` and the caller does `return *res`.
- 2026-10-31: M8 decisions. Notifications: an admin opts in per printer, and printpit then holds a hub subscription for that printer permanently, the only exception to rule 2. The webhook URL comes only from env (`PRINTPIT_NOTIFY_WEBHOOK_URL`) and goes out through its own client, not the printer egress. Trusted-header SSO signs in **existing** users only, trusts the header only from `PRINTPIT_AUTH_PROXY_CIDRS` (checked against the direct peer, not XFF), and skips TOTP. Temperature presets are global and admin-defined. Job history gets its own tab (`[5]History`).
- 2026-10-31: Slicer ETA (8.1): `Snapshot.estimated_time` comes from file metadata, fetched once per loaded file and shared through `state.slicer` (`status::SlicerTimes`). The hub asks over the open WebSocket (`server.files.metadata`), so a live dashboard still makes no HTTP calls. Failed lookups aren't cached; the next connection retries. The mock prints `PRINTING_FILE` (benchy.gcode, 3000 s estimate), which has metadata but isn't in the `gcodes` listing.
- 2026-10-31: Camera tiles (8.2) load lazily: the card has `.cam` with `hx-get="/p/{id}/camera/tile" hx-trigger="intersect once"`, and the tile handler renders the `<img>` from the cached webcam lookup (the same lookup the snapshot uses, so no extra Moonraker calls). Orientation is CSS only: `.cam-flip-h/v` and `.cam-rot-*` set custom properties on one `transform`, and quarter turns resize the image box inside a fixed 16:9 `.cam-frame`. `printers.webcam_transform` NULL means "use Moonraker's setting".
- 2026-10-31: Temperatures (8.3): targets are G-code (`SET_HEATER_TEMPERATURE`, `SET_TEMPERATURE_FAN_TARGET`, `TURN_OFF_HEATERS`) sent with `dashboard::run_action` to `/printer/gcode/script`. **Never put user text into G-code** without `temps::is_heater` (name charset `[A-Za-z0-9_-]`, no spaces/newlines/`;`) *and* a check that the object exists on that printer; targets are parsed to `f64` and capped at 500 (Klipper enforces `max_temp`). `hub.peek(id)` reads a live snapshot without subscribing, so it never opens a connection. Presets are global (`temp_presets`), sorted by hotend temperature.
