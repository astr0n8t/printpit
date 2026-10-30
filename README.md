# printpit

A minimal web interface for controlling several [Moonraker](https://github.com/Arksine/moonraker)/Klipper 3D printers from one place. It is written in Rust, ships as one small Docker image, and is styled like a terminal UI.

- **Built-in authentication** with users and roles.
- **Server-side Moonraker client.** The browser only talks to printpit and never to a printer, so Moonraker needs no CORS setup, and printer traffic can stay on its own network.
- **Lazy loading.** Each tab queries the printer only when you open it.

> **Status:** early development. Authentication and printer management are done; the live dashboard is next.
> See [`PLAN.md`](PLAN.md) for the design and [`CHECKLIST.md`](CHECKLIST.md) for progress.

## Quickstart (Docker Compose)

```sh
echo "PRINTPIT_SECRET_KEY=$(openssl rand -base64 32)" > .env
# Only for plain-HTTP testing on a trusted network (no TLS reverse proxy):
# echo "PRINTPIT_SECURE_COOKIES=false" >> .env
docker compose up -d --build
```

Then open <http://localhost:8080>. On first start, printpit prints a one-time **setup token** to its log:

```sh
docker compose logs printpit | grep "setup token"
```

Enter it on the setup page to create the admin account.

Keep the secret key safe and stable. It encrypts the printer API keys stored in `/data`, so if you lose it you will need to re-enter those keys.

## Running from source

```sh
PRINTPIT_SECRET_KEY=$(openssl rand -base64 32) \
PRINTPIT_SECURE_COOKIES=false \
PRINTPIT_BIND=127.0.0.1:8080 \
cargo run
```

## Configuration

All settings come from environment variables. The main ones:

| Variable | Default | Purpose |
|---|---|---|
| `PRINTPIT_SECRET_KEY` / `PRINTPIT_SECRET_KEY_FILE` | *required* | 32 bytes, base64. Encrypts secrets at rest. `_FILE` reads it from a file (Docker secrets). |
| `PRINTPIT_BIND` | `0.0.0.0:8080` | Web listener address |
| `PRINTPIT_DATA_DIR` | `/data` | Where state is stored |
| `PRINTPIT_SECURE_COOKIES` | `true` | Set to `false` only when serving over plain HTTP |
| `PRINTPIT_PRINTER_CIDRS` | private ranges | Address ranges printers may live in |
| `PRINTPIT_EGRESS_BIND` | unset | Local IP that outbound printer traffic leaves from |
| `PRINTPIT_LOG_FORMAT` | `pretty` | `pretty` or `json` |
| `RUST_LOG` | `info` | Log filter |

See [`PLAN.md` §11](PLAN.md#11-configuration) for the full list.

## Development

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
docker build -t printpit .
```

## License

GPL-3.0. See [`LICENSE`](LICENSE).
