# Kahawai

*Kahawai* (*kah-hah-why*) is Hawaiian for **stream**. In te reo Māori, kahawai
is also the name of *Arripis trutta*, from *kaha* (strong) and *wai* (water).

## What Kahawai is

Kahawai is a self-hosted server for streaming movies, series, anime, and music
from your own media. It is written in Rust, uses GStreamer for media processing,
and includes its web interface in the server binary.

Every deployment has three roles:

- **Hub:** serves the web interface and API, owns users and library state, and is
  the only role clients contact.
- **Mediahost:** indexes read-only media roots and serves source bytes to the hub.
- **Transcoder:** re-encodes streams a client cannot play directly.

The hub chooses the cheapest complete playback path: direct play, remux in the
hub, or transcode only the incompatible streams. An **all-in-one** process runs
the hub, a mediahost, and an optional transcoder on one machine. Satellites use
mTLS, dial the hub, and never accept inbound application traffic.

Kahawai stores its state in SQLite and requires no external database. The web
interface provides playback, libraries, users, metadata, match review, watch
state, and administration. It finds the recap, opening and end credits of a
season in the background, and offers to skip them. Where it has measured
nothing — movies, or a season the sweep has not reached — each viewer can opt
in to fetching community skip times from
[TheIntroDB](https://theintrodb.org), asked for directly by their browser and
never by the server.

- [Technical requirements](./docs/kahawai-technical-requirements.md)
- [Implementation](./docs/kahawai-implementation.md)
- [Deployment details](./docs/kahawai-deployment.md)

## Install all-in-one

The supported installation is the container image. It includes the required
GStreamer stack and Kahawai's media fixes.

### Build the image

```sh
git clone https://github.com/iksteen/kahawai.git
cd kahawai
docker build -t kahawai .
```

Use `--device=/dev/dri:/dev/dri` for Intel or AMD hardware acceleration. Use
`--gpus all` with the NVIDIA Container Toolkit for NVIDIA hardware.

```sh
docker run --rm --device=/dev/dri:/dev/dri kahawai doctor
docker run --rm --gpus all kahawai doctor
```

`doctor` lists the available decode, encode, remux, and tone-mapping paths.

### Configure the server

Create persistent directories and a configuration file:

```sh
mkdir -p runtime/config/kahawai runtime/data runtime/cache
```

`runtime/config/kahawai/kahawai.toml`:

```toml
[all_in_one]
transcoder = true

[hub]
bind = "0.0.0.0:8420"
satellite_bind = "0.0.0.0:8421"

[mediahost]
name = "local"

[[mediahost.collections]]
name = "movies"
media_type = "movies"
roots = ["/media/movies"]

[[mediahost.collections]]
name = "series"
media_type = "series"
roots = ["/media/series"]
```

Collection roots are read-only. Their paths are the paths inside the container.

### Run all-in-one

```sh
docker run -d \
  --name kahawai \
  --restart unless-stopped \
  --device=/dev/dri:/dev/dri \
  -p 127.0.0.1:8420:8420 \
  -p 8421:8421 \
  -v "$PWD/runtime/config:/config" \
  -v "$PWD/runtime/data:/data" \
  -v "$PWD/runtime/cache:/cache" \
  -v /srv/media:/media:ro \
  kahawai
```

Replace `--device=/dev/dri:/dev/dri` with `--gpus all` for NVIDIA, or omit it
for software-only operation.

Create the first administrator through the private control socket:

```sh
docker exec -it kahawai kahawai hub init-admin
```

Open <http://localhost:8420>. Put a TLS reverse proxy in front before exposing
the hub outside the machine.

Kahawai reads `$XDG_CONFIG_HOME/kahawai/kahawai.toml`, `./kahawai.toml`, or the
path passed with `--config`. Environment overrides use
`KAHAWAI_<SECTION>__<KEY>`, such as `KAHAWAI_HUB__DATA_DIR=/srv/kahawai`.

## Satellites and standalone hub

Expose the hub's satellite listener directly as TCP. Do not send it through the
HTTP reverse proxy. The hub certificate must contain the name or address that
satellites use:

```toml
[hub]
satellite_bind = "0.0.0.0:8421"
hostnames = ["kahawai.example.lan"]
```

A mediahost needs its own persistent state directory and read-only media roots:

```toml
[mediahost]
name = "nas"
state_dir = "/data/kahawai-mediahost"

[[mediahost.hubs]]
id = "home"
address = "kahawai.example.lan:8421"

[[mediahost.hubs]]
id = "family"
address = "family.example.lan:8421"
collections = ["anime"] # omit to publish every collection

[[mediahost.collections]]
name = "anime"
media_type = "anime"
roots = ["/media/anime"]
```

Run it with:

```sh
kahawai mediahost
```

The mediahost scans and analyzes each file once into
`/data/kahawai-mediahost/catalog.db`; every named hub has independent mTLS
credentials under `state_dir/hubs/<id>/` and receives only catalogue versions
newer than its durable cursor. The old single `hub = "…"` setting remains
accepted and keeps its existing credentials directly in `state_dir`.

A transcoder needs persistent state and access to its GPU:

```toml
[transcoder]
hub = "kahawai.example.lan:8421"
name = "gpu-box"
state_dir = "/data/kahawai-transcoder"
max_sessions = 2
```

Run it with:

```sh
kahawai transcoder
```

The same container image runs either role by appending `mediahost` or
`transcoder` to `docker run`. On first connection, the satellite prints an
enrollment code. Approve that code in the hub's administration page. The hub
issues the satellite certificate and the satellite reconnects with mTLS.

When every collection lives on satellite mediahosts, the all-in-one process can
be replaced with `kahawai hub`. Stop all-in-one, keep the same hub configuration
and data directory, and start:

```sh
kahawai hub
```

The standalone hub retains users, libraries, certificates, and watch state. It
continues to direct-play and remux. Full video encoding requires an enrolled
transcoder.

## Harden a public hub

Terminate TLS at an HTTP reverse proxy and keep the hub's HTTP listener
unreachable from the internet. For a native hub and proxy on the same host:

```toml
[hub]
bind = "127.0.0.1:8420"
public_url = "https://kahawai.example.com"
trusted_proxies = ["127.0.0.1"]
```

For the container installation, keep `bind = "0.0.0.0:8420"` inside the
container and keep the host-side `127.0.0.1:8420:8420` port mapping.
`trusted_proxies` must contain the proxy peer address as Kahawai sees it inside
the container, not necessarily the proxy's host address.

- `public_url` enables strict Origin checking for browser login, refresh, and
  logout. The Origin must match exactly. HTTPS also marks authentication cookies
  `Secure`. Without `public_url`, Kahawai does not validate browser Origins.
- `trusted_proxies` lists the socket peers allowed to supply forwarded request
  metadata. Trust only proxy addresses; never trust a network that clients can
  occupy.
- `X-Forwarded-For` supplies the client address used by login throttling. Kahawai
  resolves the chain from right to left and selects the first untrusted address.
- `X-Forwarded-Proto` and `X-Forwarded-Host` identify the browser-facing scheme
  and host for cookie security when `public_url` is absent and they come from a
  trusted proxy.

Minimal nginx configuration:

```nginx
location / {
    proxy_pass http://127.0.0.1:8420;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-Host $http_host;
    proxy_http_version 1.1;
}
```

The API uses the HTTP `QUERY` method for `/api/v1/items/{id}`. Proxies that
restrict methods must allow `QUERY`. Do not buffer `/api/v1/events`; it is a
Server-Sent Events stream. Keep the first-run setup listener private and expose
port 8421 only to satellite networks.

## OpenAPI

The hub publishes its complete OpenAPI 3.2 contract at:

```text
/api-docs/openapi.json
```

The embedded Swagger UI is available at:

```text
/swagger-ui
```

The document covers the public and administrative APIs, authentication modes,
request and response schemas, errors, and the `QUERY` item operation. API
clients use bearer-mode login and refresh tokens; browser-cookie authentication
is for the embedded same-origin web interface.

## License

Kahawai is [MIT licensed](./LICENSE).

Kahawai dynamically links to system or container-provided
[GStreamer](https://gstreamer.freedesktop.org/) libraries, which are
LGPL-2.1-or-later. Some runtime plugins, including plugins from
`gst-plugins-ugly`, are GPL-licensed. Distributors must follow the licenses of
the GStreamer libraries and plugins included with their bundle.

Metadata from [TMDB](https://www.themoviedb.org/) and
[TheTVDB](https://thetvdb.com/) requires the attribution displayed by Kahawai.
