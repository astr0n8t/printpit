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
- Auth header: `X-Api-Key: <key>`. For the WebSocket, use the header on the upgrade request, or fetch `GET /access/oneshot_token` and pass `?token=`.
- JSON-RPC over the WebSocket: `printer.objects.subscribe`. Updates arrive as `notify_status_update` with *partial* objects, so merge them into the cached state. Don't replace it.
- File roots: `gcodes` (manage), `logs` (read). Paths are relative to the root. URL-encode each path segment.
- `/printer/*` endpoints fail with an error when Klippy isn't ready. Show that state instead of treating it as offline.

## Commands
```
PRINTPIT_SECRET_KEY=$(openssl rand -base64 32) PRINTPIT_SECURE_COOKIES=false PRINTPIT_BIND=127.0.0.1:8080 cargo run
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
docker build -t printpit .
docker compose up            # needs PRINTPIT_SECRET_KEY in .env
printpit healthcheck         # probes GET /healthz on PRINTPIT_BIND (used by Docker HEALTHCHECK)
```

## Gotchas / decisions log
- *(add entries as `YYYY-MM-DD: note`)*
- 2026-10-30: The project started. Decided on server-rendered HTML + htmx + SSE instead of an SPA, to meet the speed and minimal-UI goals and so that tab partials are lazy by construction.
- 2026-10-30: The auto-refresh checkbox lives on the dashboard and is remembered per user (`user_prefs.auto_refresh`, default on). It controls dashboard SSE and System tab auto-refresh. Logs follow mode is separate. The compose file ships both the simple and the segmented variants.
- 2026-10-30: `/healthz` is routed *after* the TraceLayer so Docker health probes don't spam the logs. It still gets the security headers.
- 2026-10-30: Runtime image is `gcr.io/distroless/static-debian12:nonroot` (UID 65532, CA certs included). The builder is `rust:1.97-alpine`, which links musl statically.
- 2026-10-30: Claude cloud sandbox only: `dockerd` isn't running by default (start it with `dockerd &`). Builds there need the proxy CA: build with a scratch copy of the Dockerfile that `COPY`s `/root/.ccr/ca-bundle.crt` and sets `CARGO_HTTP_CAINFO`. Never commit that to the real Dockerfile.
- 2026-10-30: Session and pre-session cookies use `SameSite=Lax`, not the `Strict` the plan first said. With `Strict`, following a link to printpit from chat or email showed the login page. Every state change still needs a CSRF token.
- 2026-10-30: Login rate limiting is hand-rolled and in memory. It counts failures only: 10 per username and 20 per IP in 15 minutes. A successful login clears the username counter, not the IP counter. The limit resets on restart, which is acceptable.
- 2026-10-30: Argon2 is built with opt-level 3 even in dev (`[profile.dev.package.argon2]`), otherwise tests crawl.
- 2026-10-30: SQLite can `fchmod` an empty database file back to 0644 when it opens it. This happened inside the container (non-root) but not locally (root). `db::open` therefore re-applies 0600 to `printpit.db`, `-wal` and `-shm` after opening. Verify permission changes in the Docker image, not only in local tests. The image is 17.1 MB with bundled SQLite.
