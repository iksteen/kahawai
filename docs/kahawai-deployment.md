# Deployment notes (OPS-8)

The hub serves the client API and the embedded web UI on `[hub] bind`
(default `127.0.0.1:8420`). Most real deployments put a reverse proxy
in front. Everything below is opt-in config; the defaults are
same-origin only, no proxy trust.

First-run setup is intentionally outside the reverse proxy. `setup_bind`
(default `127.0.0.1:8422`) must remain loopback-only and use a port distinct
from the public and satellite listeners; open it locally or
forward it over SSH. Headless operators can instead run
`kahawai hub init-admin`, which uses `control/bootstrap.sock` under the hub data
directory. The control directory is mode `0700` before the mode-`0600` socket is
bound, so no permissive create-then-chmod interval exists. With the container
image, run that command inside the already-running
hub container and allocate a TTY; the hidden password prompt reads from the
terminal rather than piped stdin:

```sh
docker exec -it <container-name> kahawai hub init-admin
```

Neither first-admin path exists after setup succeeds.

## Reverse proxy

```toml
[hub]
bind = "127.0.0.1:8420"
public_url = "https://kahawai.example.com"  # enables strict browser Origin checks
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

`public_url` is optional and must be an absolute HTTP(S) origin with no
credentials, non-root path, query or fragment. When configured, browser login,
refresh and logout require that exact Origin. HTTPS makes browser authentication
cookies `Secure`; configured HTTP is permitted but logs that browser
authentication cookies and tokens cross the network in cleartext. When
`public_url` is absent, browser Origin headers are not validated. For a trusted
socket peer only, the rightmost valid `X-Forwarded-Proto` and
`X-Forwarded-Host` may still identify HTTPS and mark the cookies `Secure`.
Untrusted or missing peers cannot influence cookie security. X-Forwarded-For is
resolved right-to-left:
the first address that is not itself a trusted proxy wins, so clients cannot
spoof their way into someone else's throttle bucket.

### nginx

```nginx
location / {
    proxy_pass http://127.0.0.1:8420;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-Host $http_host;
    proxy_http_version 1.1;
    # Media: range requests pass through untouched; raise the body
    # timeout if clients pause long direct-play streams.
}
```

Notes:
- **`QUERY` needs no configuration.** The item resource answers it
  (RFC 10008, see §4.4 of the implementation doc) and nginx forwards an
  extension method unchanged — verified on 1.30.4 with exactly the
  `proxy_pass` above: the method reaches the hub, the request BODY
  arrives (a dropped body would silently degrade every client to the
  conservative capability fallback, and the answer would still look
  plausible), and `Accept-Query` plus `405`/`Allow` come back intact.
  A proxy that whitelists methods must add `QUERY`, or item pages load
  without their negotiated half.
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

Absent (default): no CORS headers — the embedded web UI is same-origin and
unaffected. Third-party browser clients use explicit API mode and bearer
tokens (`Authorization` is an allowed header). Authentication cookies are
`SameSite=Strict` and never become a cross-origin credential; native media,
artwork and EventSource therefore remain same-origin-only.

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


## Capping what a transcode costs the box (TC-6)

Each session's pipeline is a separate `remux-worker` process, and two
knobs bound what one costs. Both live in `[transcoder]`, both default to
0 = off, and both are read by the worker itself — so on `all-in-one` they
also govern the workers the *hub* spawns for its own remuxes, which is
the deployment where this matters at all. On a dedicated transcoder box,
transcoding is the job and there is nothing to yield to.

```toml
[transcoder]
# Yield to the hub that has to serve the stream being produced.
worker_nice = 10
# Thread ceiling for SOFTWARE encoders (x264enc, x265enc, svtav1enc,
# av1enc, rav1enc, openh264enc). Hardware encoders are untouched: their
# concurrency lives in the driver.
worker_threads = 4
```

Which encoder a session got — and therefore whether `worker_threads`
applied — is in that session's `worker.log` as `video encoder selected`,
with a `hardware` flag. The preference list resolves per worker process,
so two sessions on one box can legitimately differ.

A **share** of a CPU is not something a process can grant itself; that is
a cgroup, and it belongs to whatever supervises the process. Under
systemd:

```ini
# /etc/systemd/system/kahawai-transcoder.service.d/cpu.conf
[Service]
CPUWeight=50           # relative to other services (default 100)
CPUQuota=600%          # never more than six cores' worth
IOWeight=50
```

Under Docker/Podman the equivalents are `--cpu-shares` and `--cpus`.
Neither replaces `worker_nice`: the cgroup bounds the whole service, the
niceness orders the worker *against the hub inside it*.

## Scraping metrics (NFR-6)

`/metrics` is off until `metrics.secret` exists in the data directory, and
it takes the token in that file — not a login token. Access tokens expire
after 15 minutes and Prometheus has no refresh flow, so a static credential
scoped to this one read-only route is the only thing that actually scrapes.
It sits beside `jwt.secret` and `credentials.secret` rather than in the config
file, so a config you can paste into an issue carries no credential:

```console
$ (umask 077; openssl rand -hex 32 > ~/.local/share/kahawai/metrics.secret)
```

Trailing whitespace is ignored, so `$(cat metrics.secret)` in the scraper's
configuration matches. The hub restricts the file to 0600 on startup and
says so if it had to.

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

## macOS satellites

A macOS transcoder dials the hub over the LAN, so macOS 15+ gates it
behind **Local Network** privacy: `No route to host` in a reconnect
loop until someone clicks the dialog. Corrected 2026-07-31, against
TN3179 and measured on the mini — the earlier claim that a stable
self-signed identity would hold the grant was wrong:

- The grant is tracked by code signature **only for Apple-issued
  identities** (a real Team ID). A self-signed identity is treated
  like ad-hoc, and the state then keys on the executable's `LC_UUID` —
  which changes on **every rebuild**. No local identity can fix that.
- launchd **daemons** are auto-allowed; launchd **agents** (ours) are
  not. Command-line tools run over ssh are auto-allowed too — which is
  why the problem never reproduces in an ssh session, only under the
  agent.

The durable fix is the administrative allowlist — per interface, so a
wired satellite uses the Ethernet key (run ON the mac, once):

```sh
sudo defaults write /Library/Preferences/com.apple.network.local-network \
    AllowedEthernetLocalNetworkAddresses -array "192.168.0.0/24"
```

That bypasses per-app grants for the listed CIDRs and survives every
rebuild, rename and re-sign — belt. The braces, live since 2026-07-31:
the transcoder is a launchd **daemon** (system domain, `UserName` the
deploy user), which is auto-allowed by design and starts at boot with
no login session. VideoToolbox hw encode and the GL tone-map segment
both dry-run-verified under the daemon; `kahawai-mac.sh setup` installs
it (the one sudo step), and deploys stay sudo-free — they kill the
process and `KeepAlive` respawns it. The self-signed identity stays:
it keeps the signature itself stable, which macOS wants for everything
else.

`scripts/kahawai-mac.sh` owns both halves:

```sh
# ON the mac, once. Creates a self-signed code-signing identity in its
# own keychain (never the login one, so a build script can unlock it
# without touching your login password) and grants it code-signing
# trust. That last step prompts for your password — trust settings are
# deliberately not scriptable, which is why setup is separate.
scripts/kahawai-mac.sh setup

# Per-module binaries: each satellite is its own package, so the lean
# build is the only build —
#   cargo build --release -p kahawai-mediahostd    # kahawai-mediahost
#   cargo build --release -p kahawai-transcoderd   # kahawai-transcoder
#   cargo build --release -p kahawai --bin kahawai-hub
# No feature flags: kahawai-hub is not in a satellite's dependency graph,
# so no build can give one SQLite, axum or Tesseract.
# (the `kahawai` binary keeps everything, incl. all-in-one, for the dev
# box). silence: scripts/kahawai-silence.sh builds both satellite
# binaries here (same arch) and ships + restarts them.
#
# from the dev box, per deploy: sync tracked files, build, sign,
# restart the launchd agent, wait for the link. Satellites do not contain
# the hub's web UI, so generated web/dist is neither synced nor needed.
scripts/kahawai-mac.sh deploy [user@host]
```

Deploy without an identity still works; it says plainly that the
binary stays ad-hoc signed and the Local Network grant will need
re-approving, rather than leaving a silent reconnect loop to diagnose.

### GStreamer on the mac: `provision`, and two traps

Homebrew ships GStreamer as one formula, and says on upgrade what to do
with anything of ours: *"Do not install plugins into GStreamer's prefix.
They will be deleted by `brew upgrade`."* So the patched plugins live in
`~/.local/lib/kahawai-gst`, and the daemon is pointed at them —
**`GST_PLUGIN_PATH` in the plist's `EnvironmentVariables`**, because a
launchd daemon inherits nothing from a login shell. Without that key the
transcoder loads Homebrew's stock plugins and every patch in
`patches/gstreamer` is inert: installed, and doing nothing.

`scripts/kahawai-mac.sh provision` (ON the mac) does the lot — installs
or upgrades GStreamer and the build tools, clones the tag matching the
installed version, applies every patch or stops, builds and stages the
plugins, then writes the plist and bootstraps the daemon. It is
idempotent, and it is the whole of what used to be done by hand.

Two failures are worth knowing because neither says what is wrong.

**Upgrading GStreamer requires `cargo clean`.** Cargo caches build-script
output containing the *version-stamped* Cellar path
(`Cellar/gstreamer/1.28.5/lib`), which the upgrade deletes. The next
build fails in the linker pointing at a directory that is not there, with
nothing to connect it to the upgrade. `provision` does the clean itself
when the version moved. Note the side effect: the clean removes the
transcoder binary for half a minute, and `KeepAlive` will spend that
time failing to spawn a program that does not exist —
`last exit code = 78: EX_CONFIG` and a wedged job needing
`sudo launchctl bootout` and `bootstrap`. Provision before deploy, not
during.

**Staged plugins need `install_name_tool`, not a copy.** A plugin
resolves `@rpath/libgstcodecparsers-1.0.0.dylib` through rpaths recorded
at build time: first the build tree, then Homebrew's Cellar. Once the
build tree is gone the second one answers — so a copied plugin loads
**Homebrew's unpatched library** while looking perfectly installed.
Patch 0004 in particular is then silently absent. `provision` repoints
the dependency to an absolute path in `~/.local/lib/kahawai-gst/lib` and
re-signs, since `install_name_tool` voids the signature.

Two of the nine reproducers cannot run on macOS and this is not a
missing patch: `0004`'s wants an NVIDIA decoder, and `0008` reads
`/proc/self/io`. The other seven verify normally.

Two macOS-only behaviours are already handled in code and worth knowing
about: `vtdec`/`vtdec_hw` are demoted at startup (they build a GL
texture cache and SIGSEGV without an AppKit main loop, which a headless
worker never has — `vtenc` is safe and stays preferred), and App Nap is
switched off with an NSProcessInfo activity assertion, because macOS
otherwise defers a session-less process's timers *and* socket wakeups
until the link heartbeat dies.
