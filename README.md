# printpit

Warning: vibe coded

A minimal web interface for running several [Moonraker](https://github.com/Arksine/moonraker)/Klipper 3D printers from one place. It is written in Rust, ships as one small Docker image (~19 MB), and looks like a terminal UI.

- **Built-in authentication.** First-run setup, users with viewer / operator / admin roles, and an audit log.
- **Many printers.** Add printers by Moonraker URL and optional API key. Keys are encrypted at rest.
- **Dashboard.** Live print status, temperatures with sparklines, camera, and the core controls: start, pause/resume, cancel, e-stop.
- **Files, Logs and System tabs.** Browse, upload, move and print G-code; tail and download logs; watch the printer host's load, services and updates.
- **A real server-side client.** The browser only ever talks to printpit. Moonraker needs no CORS setup, and printer traffic can stay on its own network.
- **Optional slicing.** Upload an STL or 3MF from your phone and have OrcaSlicer, in its own sandboxed worker, slice it and send it to a printer.
- **Lazy loading.** A tab queries a printer only when you open it. Live connections exist only while someone is watching, and auto-refresh can be switched off entirely.

<!-- screenshot: docs/dashboard.png -->

## Quickstart

```sh
echo "PRINTPIT_SECRET_KEY=$(openssl rand -base64 32)" > .env
# Only for plain-HTTP testing on a trusted network (no TLS in front):
# echo "PRINTPIT_SECURE_COOKIES=false" >> .env
docker compose up -d --build
docker compose logs printpit | grep "setup token"
```

Open <http://localhost:8080> and enter the setup token to create the admin account. Then add printers from the **[p] Printers** tab.

Keep `PRINTPIT_SECRET_KEY` safe and stable. It encrypts the printer API keys stored in `/data`; if it changes, printpit shows those keys as "unreadable" and you re-enter them.

## Images

Multi-arch images (`linux/amd64`, `linux/arm64`, so a Raspberry Pi 4/5 works) are built by `.github/workflows/release.yml` and published to `ghcr.io/astr0n8t/printpit` (`:main` and `:sha-…` from `main`; `:X.Y.Z`, `:X.Y` and `:latest` from `vX.Y.Z` tags). The optional slicer worker is published the same way as `ghcr.io/astr0n8t/printpit-slicer`. To use a published image, replace `build: .` with `image: ghcr.io/astr0n8t/printpit:latest` in the compose file. While the repository is private, `docker login ghcr.io` first.

## Deployment

### Simple

`docker-compose.yml` runs printpit alone on port 8080. Put a TLS-terminating reverse proxy in front of it (see below). Session cookies are `Secure` by default, so plain HTTP only works with `PRINTPIT_SECURE_COOKIES=false`.

### Segmented (recommended)

`docker-compose.segmented.yml` keeps the web UI and printer traffic on separate networks:

```
browser ──TLS──▶ caddy ──(frontend bridge)──▶ printpit ──(printers macvlan / VLAN)──▶ Moonraker
```

- printpit publishes no ports. Caddy (`deploy/Caddyfile`) is the only way in.
- `PRINTPIT_BIND` binds the web listener to printpit's frontend address only, so the UI isn't reachable from the printer network.
- `PRINTPIT_EGRESS_BIND` makes all printer traffic (HTTP, WebSocket, camera) leave from printpit's address on the printer VLAN.
- `PRINTPIT_PRINTER_CIDRS` limits which addresses printpit will contact at all. It is checked on every connection *after* DNS resolution, so a hostname can't be pointed somewhere else later (DNS rebinding).

Edit the `printers` network (parent interface, subnet, gateway, a free `ip_range`) and the matching addresses before starting it. With macvlan, the Docker host itself can't reach printpit's printer-side address. That is expected and harmless here.

### Behind a reverse proxy

The dashboard uses Server-Sent Events, so the proxy must not buffer responses. Set `PRINTPIT_TRUSTED_PROXIES` to the proxy's address so rate limiting and the audit log see real client IPs.

**Caddy**

```caddyfile
printpit.example.com {
	reverse_proxy printpit:8080 {
		flush_interval -1
	}
}
```

**nginx**

```nginx
location / {
    proxy_pass http://printpit:8080;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_buffering off;            # live dashboard (SSE)
    proxy_read_timeout 1h;          # long-lived streams
    client_max_body_size 1024m;     # match PRINTPIT_MAX_UPLOAD_MB
}
```

**Traefik** (labels on the printpit service)

```yaml
labels:
  - traefik.enable=true
  - traefik.http.routers.printpit.rule=Host(`printpit.example.com`)
  - traefik.http.routers.printpit.tls.certresolver=letsencrypt
  - traefik.http.services.printpit.loadbalancer.server.port=8080
```

Traefik streams SSE without extra settings. Raise its read timeout if you use long camera streams.

### Single sign-on (auth proxy)

If an auth proxy such as Authelia, authentik or oauth2-proxy already signs people in, printpit can trust the username it passes on:

```sh
PRINTPIT_AUTH_HEADER=Remote-User
PRINTPIT_AUTH_PROXY_CIDRS=172.30.0.2        # the proxy's address as printpit sees it
PRINTPIT_AUTH_LOGOUT_URL=https://auth.example.com/logout   # optional
```

- The header is believed only on connections coming **directly** from an address in `PRINTPIT_AUTH_PROXY_CIDRS`. `X-Forwarded-For` doesn't count, and requests from anywhere else are treated normally.
- It only signs in users that already exist in printpit and are enabled: create them on the Users tab with the same usernames. Anyone else gets a "no enabled user by that name" page. printpit never creates users from the header.
- Header sign-ins skip printpit's own two-factor step (the proxy does MFA). Password sign-in keeps working for everyone, for example when the proxy is down.
- If the header names a different user from the current session, that session ends and the header's user is signed in.
- Logout sends people to `PRINTPIT_AUTH_LOGOUT_URL` when it's set. Without it, the next request would sign them straight back in.

**The proxy must remove any `Remote-User` header the client sends** and set its own. Otherwise anyone who can reach the proxy can claim to be anyone. With Caddy and Authelia:

```caddyfile
printpit.example.com {
	request_header -Remote-User
	forward_auth authelia:9091 {
		uri /api/authz/forward-auth
		copy_headers Remote-User
	}
	reverse_proxy printpit:8080 {
		flush_interval -1
	}
}
```

With Traefik, strip the header before the forward-auth middleware:

```yaml
labels:
  - traefik.http.middlewares.strip-user.headers.customrequestheaders.Remote-User=
  - traefik.http.middlewares.authelia.forwardauth.address=http://authelia:9091/api/authz/forward-auth
  - traefik.http.middlewares.authelia.forwardauth.authResponseHeaders=Remote-User
  - traefik.http.routers.printpit.middlewares=strip-user,authelia
```

## Configuration

All settings are environment variables.

| Variable | Default | Purpose |
|---|---|---|
| `PRINTPIT_SECRET_KEY` | *required* | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts printer API keys. |
| `PRINTPIT_SECRET_KEY_FILE` | unset | Read the secret key from a file instead (Docker secrets). Set one of the two, not both. |
| `PRINTPIT_BIND` | `0.0.0.0:8080` | Address the web UI listens on. Also used by `printpit healthcheck`. |
| `PRINTPIT_DATA_DIR` | `/data` | Where the SQLite database lives. |
| `PRINTPIT_SECURE_COOKIES` | `true` | `false` only when serving plain HTTP. |
| `PRINTPIT_SESSION_IDLE_MINS` | `720` | Sign out after this long without activity. |
| `PRINTPIT_SESSION_ABSOLUTE_HOURS` | `168` | Sign out after this long regardless. |
| `PRINTPIT_PRINTER_CIDRS` | `10.0.0.0/8,172.16.0.0/12,192.168.0.0/16` | The only address ranges printpit may contact. Bare IPs are allowed. |
| `PRINTPIT_EGRESS_BIND` | unset | Local IP that printer traffic leaves from. |
| `PRINTPIT_TRUSTED_PROXIES` | unset | CIDRs whose `X-Forwarded-For` is trusted. |
| `PRINTPIT_UI_REFRESH_MS` | `1000` | Fastest rate the dashboard pushes updates per printer. |
| `PRINTPIT_WS_IDLE_GRACE_SECS` | `30` | Keep a printer's live connection open this long after the last viewer leaves. |
| `PRINTPIT_SYSTEM_REFRESH_SECS` | `5` | System tab refresh interval (when auto-refresh is on). |
| `PRINTPIT_MAX_UPLOAD_MB` | `1024` | Largest G-code upload. |
| `PRINTPIT_NOTIFY_WEBHOOK_URL` | unset | Where print notifications are POSTed (see below). Never logged or shown. |
| `PRINTPIT_NOTIFY_WEBHOOK_URL_FILE` | unset | Read the webhook URL from a file instead (Docker secrets). |
| `PRINTPIT_AUTH_HEADER` | unset | Trusted-header single sign-on, e.g. `Remote-User` (see above). |
| `PRINTPIT_AUTH_PROXY_CIDRS` | unset | Required with `PRINTPIT_AUTH_HEADER`: the only peers whose header is believed. |
| `PRINTPIT_AUTH_LOGOUT_URL` | unset | Where logout sends people (the auth proxy's logout). |
| `PRINTPIT_SLICER_URL` | unset | The slicer worker, e.g. `http://slicer:8090` (see Slicing). Unset means no slicing. |
| `PRINTPIT_SLICER_TOKEN` / `_FILE` | unset | Required with the URL: the worker's token. Never logged or shown. |
| `PRINTPIT_SLICER_TIMEOUT_SECS` | `1800` | Give up on a slice (upload, slicing and download) after this long. |
| `PRINTPIT_SLICER_MAX_MODEL_MB` | `200` | Largest model upload. |
| `PRINTPIT_SLICER_RETENTION_HOURS` | `72` | Delete models and sliced G-code from `/data/slicer` after this long. |
| `PRINTPIT_LOG_FORMAT` | `pretty` | `pretty` or `json`. |
| `RUST_LOG` | `info` | Log filter, e.g. `info,printpit=debug`. |

### Printers and API keys

Add printers on the **[p] Printers** tab with Moonraker's address *as seen from the container*. Installs with Mainsail or Fluidd usually proxy Moonraker on port 80, so `http://printer.local` often works; otherwise use port `7125`. If Moonraker doesn't already trust printpit's address (`[authorization] trusted_clients`), create an API key in Moonraker and paste it in. **Test** shows Klipper's state and versions, or why it can't connect.

### Print notifications

Set `PRINTPIT_NOTIFY_WEBHOOK_URL` and a **notify** checkbox appears on each printer's settings. For printers with it ticked, printpit POSTs JSON when a print finishes, fails (including a Klipper shutdown mid-print) or is cancelled:

```json
{"printer": "Voron", "event": "complete", "filename": "benchy.gcode", "duration": 3725.0,
 "message": "", "text": "Voron: finished benchy.gcode in 1:02:05", "content": "…same as text…"}
```

`text` suits Slack-style incoming webhooks and `content` suits Discord, so those work as they are; anything else can read the structured fields. Failed deliveries are retried twice.

Normally printpit only talks to a printer while someone is looking at it. A printer with notifications on is the exception: printpit keeps its connection open all the time so it can see the print end. The webhook request goes out through the container's normal network, not the printer network (`PRINTPIT_EGRESS_BIND` and `PRINTPIT_PRINTER_CIDRS` don't apply to it).

### Slicing (optional)

Upload an STL, 3MF, OBJ or STEP file, from your phone if you like. printpit slices it with [OrcaSlicer](https://github.com/OrcaSlicer/OrcaSlicer), puts the G-code on the printer under `printpit/`, and starts the print if you ask it to. No desktop needed.

OrcaSlicer is big, and model files are untrusted input, so it never runs inside printpit. It runs in a separate worker image, `printpit-slicer` (OrcaSlicer 2.4.2 behind a small HTTP API, about 325 MB, amd64 and arm64), that printpit reaches with a shared token. In the compose files the worker sits on an internal network that only printpit shares. It can't reach the internet or your printers, and it runs with a read-only root, a tmpfs for its work, no capabilities and CPU/memory limits. printpit itself only checks a model's size and type before passing it on.

**Turn it on** (the first build downloads ~270 MB of OrcaSlicer):

```sh
echo "PRINTPIT_SLICER_URL=http://slicer:8090" >> .env
echo "PRINTPIT_SLICER_TOKEN=$(openssl rand -hex 32)" >> .env
docker compose --profile slicer up -d --build
```

The same works with `docker-compose.segmented.yml`. To use the published image instead of building it, set `image: ghcr.io/astr0n8t/printpit-slicer:latest` on the `slicer` service and drop its `build:`. Without `PRINTPIT_SLICER_URL` there's no slicing at all: no tabs, no routes, no worker.

**Set up profiles (admins, [s] Slicer tab).** Every printer slices with one *machine* profile (bed, nozzle, limits, start/end G-code); operators then choose a *process* and a *filament*. Two ways in:

- **From Orca's library:** *browse printers* → vendor → printer and nozzle → tick the processes and filaments you want. Only the ones that fit that printer are offered.
- **Your own presets:** in OrcaSlicer use *File → Export → Export preset bundle…* (`.orca_printer`, `.orca_filament`) or the plain *Export presets* zip, or take the `.json` files from Orca's user preset folder, and upload them under *import your presets*. Presets only store their differences from Orca's built-in ones. The worker fills in the rest, so what printpit stores is complete. Importing a profile that's already there updates it.

Then, on each printer's page under **[p] Printers**, pick its machine profile and, if you like, a default process and filament. *test the worker* on the Slicer tab shows the OrcaSlicer version and limits.

**Slice (operators, [6] Slice tab).** Pick the printer, process and filament. Under *settings* you can override the layer height, infill, walls, supports (off, normal, tree), brim, copies, arrange and auto-orient, and for a 3MF use the project's own settings and pick its plate. Optionally name the file and tick *start printing when it's done*, then choose the model and press **slice**. The job panel follows the slice (only while auto-refresh is on, as everywhere else) and then offers **print**, **slice again** with other settings, **upload again** and **delete**. Viewers can follow jobs but not start them.

Good to know:

- One slice runs at a time per worker; the others wait their turn. A worker that's busy or down just keeps jobs waiting.
- printpit won't overwrite a file of the same name on the printer unless you tick *replace*. It starts a print only if the printer is idle, both when you submit and again when the G-code arrives.
- **No thumbnails:** the OrcaSlicer 2.4.2 command line doesn't write G-code thumbnails, so sliced files show none in the Files tab.
- Models and G-code are kept under `/data/slicer` for `PRINTPIT_SLICER_RETENTION_HOURS` (3 days), then deleted. Each user can have 5 unfinished jobs.
- With `PRINTPIT_NOTIFY_WEBHOOK_URL` set, finished and failed slices are posted too (`"event": "slice_done"` / `"slice_failed"`).

**Running the worker on another machine** (say, something faster than the Pi that runs printpit):

```sh
docker run -d --name printpit-slicer --restart unless-stopped -p 8090:8090 \
  --read-only --tmpfs /tmp:size=2g --cap-drop ALL --security-opt no-new-privileges \
  -e PRINTPIT_SLICER_TOKEN=<the same token> ghcr.io/astr0n8t/printpit-slicer:latest
```

and point printpit at it with `PRINTPIT_SLICER_URL=http://<that host>:8090`. The token travels with every request, so keep that link on a network you trust, or put TLS in front and use `https://`. Don't expose the worker to the internet.

Worker settings (environment variables of `printpit-slicer`):

| Variable | Default | Purpose |
|---|---|---|
| `PRINTPIT_SLICER_TOKEN` / `_FILE` | *required* | The shared token, at least 24 characters. The worker won't start without it. |
| `PRINTPIT_SLICER_BIND` | `0.0.0.0:8090` in the image | Where the API listens. |
| `PRINTPIT_SLICER_TIMEOUT_SECS` | `900` | A slice running longer than this is killed. |
| `PRINTPIT_SLICER_MAX_MODEL_MB` | `256` | Largest model it accepts. |
| `PRINTPIT_SLICER_WORK_DIR` | `/tmp` | Per-slice temp folders (a tmpfs in the compose files). |

## Users and roles

| Role | Can |
|---|---|
| viewer | Watch the dashboard and camera, browse and download files, read logs and system info. |
| operator | Also start, pause, resume, cancel and e-stop prints, set heater temperatures and apply presets, upload, move and delete files, and slice models. |
| admin | Also manage printers, temperature presets, the slicer profile library and users, and read the audit log. Only admins see printer addresses. |

Anyone can turn on **two-factor sign-in** on the **[a] Account** tab: scan the QR code with an authenticator app (any TOTP app: Aegis, 2FAS, Google Authenticator, 1Password…), confirm with a code, and save the ten one-time recovery codes. Turning it off or making new recovery codes needs both your password and a code. If someone loses their phone and their recovery codes, an admin can **reset 2fa** for them on the Users tab.

Admins manage users on the **[u] Users** tab. Disabling a user, resetting their password or deleting them signs them out at once. Every control action and change is recorded on the **[l] Audit** tab.

## Keyboard

`1`–`5` switch tabs (dashboard, files, logs, system, history), plus `6` for Slice when slicing is on. `a` opens your account, and admins have `p`/`u`/`l` (and `s` for the Slicer). `r` refreshes what's on screen, `/` filters a list, `j`/`k`/`Enter` move through it, and `y`/`n` answer confirmations. Press `?` for the full list.

## Backup and restore

Everything printpit stores (users, printers, preferences, audit log, the slicer profile library and recent slice jobs) is in the `printpit-data` volume. Back it up with the container stopped so SQLite's write-ahead log is folded in:

```sh
docker compose stop printpit
docker run --rm -v printpit_printpit-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/printpit-$(date +%F).tar.gz -C /data .
docker compose start printpit
```

Restore by extracting into an empty volume. You need the **same `PRINTPIT_SECRET_KEY`** to read the stored API keys. Keep it with the backup, but not in the same place.

## Security model

- Passwords are hashed with Argon2id. Sessions are server-side, with only a SHA-256 of the session ID stored, in `HttpOnly`, `SameSite=Lax` cookies (`__Host-` prefixed with TLS). Every change needs a CSRF token.
- Optional trusted-header single sign-on, believed only from the configured proxy addresses (the direct peer, not `X-Forwarded-For`) and only for existing, enabled users.
- Optional TOTP two-factor sign-in (RFC 6238). Secrets are encrypted with `PRINTPIT_SECRET_KEY`, each code works once, recovery codes are stored hashed, and wrong codes count towards the login rate limit. No session exists until the code step succeeds.
- Failed logins are rate-limited per IP and per username. First-run setup needs a one-time token from the container log.
- A strict Content-Security-Policy is applied, with no inline scripts and no third-party assets. Everything is served from the binary.
- Printer URLs, hostnames, IP addresses and API keys never reach non-admin browsers. API keys never reach any browser.
- The container runs as a non-root user with a read-only root filesystem. The database file is `0600`, slice job folders `0700` and their files `0600`.
- The optional slicer worker is the only thing that parses model files. It runs separately, with no network but printpit's, a read-only root and resource limits. Every request needs the token, and slicer profile settings are typed and whitelisted, never raw Orca keys.

## Development

```sh
cargo run --example mock_moonraker      # fake printers on 127.0.0.1:7201-7203
cargo run --example mock_slicer         # fake slicer on 127.0.0.1:8091 (no OrcaSlicer needed)
PRINTPIT_SECRET_KEY=$(openssl rand -base64 32) PRINTPIT_SECURE_COOKIES=false \
  PRINTPIT_PRINTER_CIDRS=127.0.0.0/8 PRINTPIT_BIND=127.0.0.1:8080 cargo run
# add PRINTPIT_SLICER_URL=http://127.0.0.1:8091 PRINTPIT_SLICER_TOKEN=mock-slicer-token-0123456789abcdef to slice
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
docker build -f Dockerfile.slicer -t printpit-slicer . && slicer-worker/golden/run.sh printpit-slicer   # real OrcaSlicer
```

See [`PLAN.md`](PLAN.md) for the design, [`CHECKLIST.md`](CHECKLIST.md) for progress and [`CLAUDE.md`](CLAUDE.md) for project conventions.

## License

GPL-3.0. See [`LICENSE`](LICENSE).
