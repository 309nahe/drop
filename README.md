# drop

A tiny self-hosted clipboard and file drop for your private network.
Paste text or drop a file on one device, open it on another from the shelf, a 5-letter code, or a QR code.

- **In memory only**: nothing is written to disk; items vanish on expiry or restart
- Expiry of 10 min, 1 h, 1 day or 7 days, optional **burn after reading**, optional **hide from shelf**
- Paste images straight from the clipboard, drag and drop anywhere
- Bounded memory: 25 MB per item, 200 MB and 200 items in total
- Uploaded content is served sandboxed (`Content-Security-Policy: sandbox`, `nosniff`); only common raster images render inline, everything else downloads
- One small Rust binary (~3 MB RSS idle), one embedded HTML page, no external assets

## Stack

Rust, [axum](https://github.com/tokio-rs/axum) + tokio, `qrcode` for the SVG QR codes.

## Run

```sh
cargo build --release
BIND=127.0.0.1:8451 ./target/release/drop
```

`BIND` defaults to `127.0.0.1:8451`.

## Deploy

`deploy/drop.service` is a hardened systemd unit (read-only filesystem, private `/tmp`, memory cap).
Adjust `User=` and the binary path, then install and enable it.

There is **no authentication**: anyone who can reach the page can read and add drops.
Keep it on loopback and publish it only on a private network, for example with
[Tailscale Serve](https://tailscale.com/kb/1242/tailscale-serve):

```sh
sudo tailscale serve --bg --set-path=/drop http://127.0.0.1:8451
```

Open it as `/drop/` with the trailing slash; the page uses relative URLs.

## API

| Method | Path | Notes |
|---|---|---|
| `POST` | `/api/drops` | multipart: `text` or `file`, `ttl` (600, 3600, 86400, 604800), `burn` (1), `private` (1) → `{"id"}` |
| `GET` | `/api/drops` | public items (newest first) and usage |
| `GET` | `/api/drops/{id}` | metadata, never consumes a burn item |
| `DELETE` | `/api/drops/{id}` | remove |
| `GET` | `/raw/{id}` | content; `?dl=1` forces download; burn items are deleted as they are served |
| `GET` | `/qr?t=…` | SVG QR code |

```sh
curl -F 'text=hello' -F ttl=600 http://127.0.0.1:8451/api/drops
```
