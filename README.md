# printpit

Warning: vibe coded

A minimal web interface for running several [Moonraker](https://github.com/Arksine/moonraker)/Klipper 3D printers from one place. It is written in Rust, ships as one small Docker image (~18 MB), and looks like a terminal UI.

- **Built-in authentication.** First-run setup, users with viewer / operator / admin roles, and an audit log.
- **Many printers.** Add printers by Moonraker URL and optional API key. Keys are encrypted at rest.
- **Dashboard.** Live print status, temperatures with sparklines, camera, and the core controls: start, pause/resume, cancel, e-stop.
- **Files, Logs and System tabs.** Browse, upload, move and print G-code; tail and download logs; watch the printer host's load, services and updates.
- **A real server-side client.** The browser only ever talks to printpit. Moonraker needs no CORS setup, and printer traffic can stay on its own network.
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

Multi-arch images (`linux/amd64`, `linux/arm64`, so a Raspberry Pi 4/5 works) are built by `.github/workflows/release.yml` and published to `ghcr.io/astr0n8t/printpit` (`:main` and `:sha-…` from `main`; `:X.Y.Z`, `:X.Y` and `:latest` from `vX.Y.Z` tags). To use a published image, replace `build: .` with `image: ghcr.io/astr0n8t/printpit:latest` in the compose file. While the repository is private, `docker login ghcr.io` first.

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

## Users and roles

| Role | Can |
|---|---|
| viewer | Watch the dashboard and camera, browse and download files, read logs and system info. |
| operator | Also start, pause, resume, cancel and e-stop prints, set heater temperatures and apply presets, and upload, move and delete files. |
| admin | Also manage printers, temperature presets and users, and read the audit log. Only admins see printer addresses. |

Admins manage users on the **[u] Users** tab. Disabling a user, resetting their password or deleting them signs them out at once. Every control action and change is recorded on the **[l] Audit** tab.

## Keyboard

`1`–`5` switch tabs (dashboard, files, logs, system, history), `a` opens your account, and admins have `p`/`u`/`l`. `r` refreshes what's on screen, `/` filters a list, `j`/`k`/`Enter` move through it, and `y`/`n` answer confirmations. Press `?` for the full list.

## Backup and restore

Everything printpit stores (users, printers, preferences, audit log) is in the `printpit-data` volume. Back it up with the container stopped so SQLite's write-ahead log is folded in:

```sh
docker compose stop printpit
docker run --rm -v printpit_printpit-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/printpit-$(date +%F).tar.gz -C /data .
docker compose start printpit
```

Restore by extracting into an empty volume. You need the **same `PRINTPIT_SECRET_KEY`** to read the stored API keys. Keep it with the backup, but not in the same place.

## Security model

- Passwords are hashed with Argon2id. Sessions are server-side, with only a SHA-256 of the session ID stored, in `HttpOnly`, `SameSite=Lax` cookies (`__Host-` prefixed with TLS). Every change needs a CSRF token.
- Failed logins are rate-limited per IP and per username. First-run setup needs a one-time token from the container log.
- A strict Content-Security-Policy is applied, with no inline scripts and no third-party assets. Everything is served from the binary.
- Printer URLs, hostnames, IP addresses and API keys never reach non-admin browsers. API keys never reach any browser.
- The container runs as a non-root user with a read-only root filesystem. The database file is `0600`.

## Development

```sh
cargo run --example mock_moonraker      # fake printers on 127.0.0.1:7201-7203
PRINTPIT_SECRET_KEY=$(openssl rand -base64 32) PRINTPIT_SECURE_COOKIES=false \
  PRINTPIT_PRINTER_CIDRS=127.0.0.0/8 PRINTPIT_BIND=127.0.0.1:8080 cargo run
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

See [`PLAN.md`](PLAN.md) for the design, [`CHECKLIST.md`](CHECKLIST.md) for progress and [`CLAUDE.md`](CLAUDE.md) for project conventions.

## License

GPL-3.0. See [`LICENSE`](LICENSE).
