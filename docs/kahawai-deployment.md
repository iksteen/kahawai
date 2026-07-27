# Deployment notes (OPS-8)

The hub serves the client API and the embedded web UI on `[hub] bind`
(default `127.0.0.1:8420`). Most real deployments put a reverse proxy
in front. Everything below is opt-in config; the defaults are
same-origin only, no proxy trust.

## Reverse proxy

```toml
[hub]
bind = "127.0.0.1:8420"
# Peers allowed to speak for clients via X-Forwarded-For. Exact IPs
# and/or CIDR ranges. REQUIRED for login throttling (OPS-2) to see
# real client addresses — without it, every client behind the proxy
# shares the proxy's own per-IP bucket (per-account throttling still
# works). Never list a network clients can occupy.
trusted_proxies = ["127.0.0.1"]          # proxy on the same host
# trusted_proxies = ["172.16.0.0/12"]    # docker/traefik bridge (the
#                                        # proxy's address changes per
#                                        # restart; trust the subnet)
# trusted_proxies = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
#                                        # all RFC1918 — covers any pool
#                                        # docker might assign
# trusted_proxies = ["0.0.0.0/0"]        # trust EVERYTHING: X-Forwarded-For
#                                        # is taken at face value (leftmost
#                                        # entry). Only safe when the bind
#                                        # address is unreachable except
#                                        # through the proxy — any peer that
#                                        # CAN connect directly can spoof
#                                        # its address and dodge or frame
#                                        # per-IP throttling.
```

X-Forwarded-For is resolved right-to-left: the first address that is
not itself a trusted proxy wins, so clients cannot spoof their way
into someone else's throttle bucket. `X-Forwarded-Proto` is currently
unused (no absolute URLs are generated; cookies are set client-side).

### nginx

```nginx
location / {
    proxy_pass http://127.0.0.1:8420;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_http_version 1.1;
    # Media: range requests pass through untouched; raise the body
    # timeout if clients pause long direct-play streams.
}
```

Notes:
- `/api/v1/events` is SSE. The hub sends `X-Accel-Buffering: no`,
  which nginx honors — no extra config needed. Other proxies must not
  buffer `text/event-stream` responses (traefik and caddy don't).
- The web player's ASS renderer is WASM: whatever serves or caches the
  UI assets must use the `application/wasm` MIME type for `.wasm`
  files (the hub itself does).

### traefik (docker)

The hub usually runs on the host (GStreamer, disks) with traefik in a
container; trust the docker bridge network:

```toml
[hub]
trusted_proxies = ["172.16.0.0/12"]
```

and in traefik, plain forwarding — X-Forwarded-For is added by
default. If traefik itself sits behind another proxy, configure its
`forwardedHeaders.trustedIPs` and add that hop here too.

## CORS (third-party web clients)

```toml
[hub]
cors_origins = ["https://app.example.com"]   # exact origins
# cors_origins = ["*"]                       # any origin
```

Absent (default): no CORS headers — the embedded web UI is
same-origin and unaffected. Third-party browser clients authenticate
with bearer tokens (`Authorization` is an allowed header); cookies are
NOT shared cross-origin, which today means `<video>`/HLS playback —
which authenticates by cookie — remains same-origin-only. A delegated
media-token mechanism (AR-8) is the planned fix.

## Subtitles (OpenSubtitles)

Always on, with no configuration: the binary ships kahawai's
registered application key, and anonymous use is entitled to 5
requests/second and **5 downloads per 24 hours shared across the whole
deployment**.

Each user may attach their own opensubtitles.com account under
Settings → OpenSubtitles, which spends that user's own download
entitlement instead of the shared one. Subtitles they download are
shared with everyone on the server (HUB-23) — the account governs who
pays for the download, not who may use the result.

A deployment that wants its own application key — rate-limit
isolation, or if the embedded one is ever revoked — sets it in config;
this is the only way to override it:

```toml
[hub.subtitles.opensubtitles]
api_key = "${KAHAWAI_OS_KEY}"     # optional: your own registered application key
```

It honours the usual env override
(`KAHAWAI_HUB__SUBTITLES__OPENSUBTITLES__API_KEY=…`).

## Checklist for exposing a hub

1. `trusted_proxies` set to exactly the proxy hops (OPS-2 needs it).
2. TLS terminates at the proxy; the hub's client API is plain HTTP —
   never expose `bind` directly to the internet.
3. The satellite port (`satellite_bind`, mTLS) does not go through
   the reverse proxy; expose it directly or via TCP passthrough.
4. Login throttling is on by default; watch `login failed` /
   `login throttled` log lines.


## Scraping metrics (NFR-6)

`/metrics` is off until `hub.metrics_token` is set, and it takes that
token — not a login token. Access tokens expire after 15 minutes and
Prometheus has no refresh flow, so a static credential scoped to this one
read-only route is the only thing that actually scrapes.

```toml
[hub]
metrics_token = "a-long-random-string"
```

```yaml
scrape_configs:
  - job_name: kahawai
    authorization:
      credentials_file: /etc/prometheus/kahawai.token
    static_configs:
      - targets: ["hub.example:8420"]
```

Unset means the endpoint 404s for everyone, including admins; a wrong
token is 401. `/health` needs no credential and is what an uptime check
should poll — it reports every module, and a satellite being away is
`degraded` rather than a failure (AR-6).
