# Kahawai

**Kahawai** (*kah-hah-why*) is Hawaiian for *stream* — the channel a river carves through the land — which is exactly what this is: a self-hosted media streaming server for the series, movies, music, and anime you've backed up from your own media. It's also, happily, the Māori name of a strong, fast-schooling New Zealand fish ("strong water"), which we're keeping as the unofficial mascot.

## What it is

A Rust backend built on GStreamer, shipped two ways from one codebase:

- **All-in-one** — a single binary for a NAS or home server.
- **Modular** — a **hub** (the only thing clients talk to), one or more **mediahosts** (announce collections of media from their disks), and optional **transcoders** (handle playback for clients that can't play the source as-is). Satellites dial out to the hub and enroll via a console-code certificate flow — the hub is its own CA.

## What makes it different

- **Plays the cheapest sufficient path, always.** Direct play when possible; container remuxing happens *in the hub* with no transcoder needed; re-encoding is a last resort, per-stream, hardware-accelerated, and scheduled across however many transcoder machines you attach.
- **Anime as a first-class citizen.** AniDB exact-file matching via ED2K hashes, AniList relations and watch orders, fansub filename conventions, and ASS subtitles rendered faithfully — client-side with real fonts where the player can, burn-in or opt-in flattening where it can't.
- **Honest capability negotiation.** Clients report what they can actually decode; the server explains every playback decision ("why is this transcoding?") right in the UI.
- **Batteries included.** Embedded web app for admin and playback, metadata from TheTVDB/TMDB/MusicBrainz, user-initiated subtitle downloads via OpenSubtitles, multi-user watch state — all in the binary, no external services.

## Status

Running daily. Working today: direct play, in-hub remuxing to HLS,
hardware-accelerated transcoding dispatched across a fleet (NVENC, VA-API,
VideoToolbox verified) with self-healing and seek-anywhere; movies, series,
anime, and music resolution (including multi-CD rips and absolute-numbered
fansub releases); libraries; embedded and sidecar subtitles served as WebVTT;
audio/video track switching; TMDB metadata with TheTVDB fallback, posters,
and an in-place match-review flow; incremental rescans with filesystem
watching; multi-user watch state — all driven from the embedded web app.

Still ahead: episode-level metadata, subtitle downloads (OpenSubtitles),
faithful ASS rendering with fonts, AniDB/AniList, MusicBrainz, quality
ladders, and the hardening pass. Design documents:

- [Technical requirements](./docs/kahawai-technical-requirements.md)
- [Implementation design](./docs/kahawai-implementation.md)
- [Release process](./docs/kahawai-releasing.md)

## Running it

> **The container image is the recommended way to run kahawai.** It builds
> GStreamer with the fixes in [`patches/`](./patches), which are not in a
> release yet. On a stock install, files that those patches address will fail
> to play — usually looking like corrupt media rather than a missing fix.
>
> ```sh
> docker build -t kahawai .
> docker run --rm --gpus all kahawai doctor                   # NVIDIA
> docker run --rm --device=/dev/dri:/dev/dri kahawai doctor   # Intel/AMD
> ```
>
> Building from source works and is what development uses — the notes below
> cover it. Each patch carries a report and a reproducer that needs no media,
> so you can check whether your own GStreamer is affected.

### Prerequisites

- **Rust** (edition 2024 toolchain) and **protoc** (protobuf compiler) to build.
- **GStreamer 1.24+** with the plugin sets: base, good, bad, ugly, libav, and
  [gst-plugins-rs](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs)
  **1.28.6 or newer** (for `hlssink3`). Hardware encoders come from your
  platform: `gst-plugin-va` (VA-API/Quick Sync), the NVIDIA plugin set
  (NVENC), or macOS VideoToolbox.

  1.28.6 is the first release carrying two hlssink3 fixes that matter here:
  a fragment whose first buffer has no PTS, and a running time unwrapped when
  a segment is added. Both panic inside an FFI callback, so they abort the
  whole process instead of failing one session — an older gst-plugins-rs
  takes the server down on ordinary files.

  Note that these plugins report their *crate* version, not the release, so
  `gst-inspect-1.0 hlssink3` shows the same number either way; only the git
  hash it appends distinguishes them. To settle it, run the reproducer in
  [`patches/gst-plugins-rs/`](./patches/gst-plugins-rs) — it aborts the
  process on an affected build and exits cleanly on a fixed one.
- Node is **not** required to run — the web app ships prebuilt in the binary,
  and a build without npm embeds that checked-in bundle. With npm installed,
  `cargo build` rebuilds it from `web/` whenever the sources change, so the
  binary cannot ship an app older than the tree it was built from.

```sh
cargo build --release
./target/release/kahawai doctor   # names every capability your GStreamer install provides or lacks
```

`doctor` is the source of truth: each missing element is listed with exactly
what it costs you (e.g. "E-AC-3 audio cannot be transcoded — install
gst-libav"). Essential gaps abort startup; everything else degrades.

### First run (all-in-one)

```sh
./target/release/kahawai hub &
./target/release/kahawai mediahost
```

The public API remains locked on first run. Create the administrator through
the trusted-local control plane: open http://localhost:8422 on the hub (or
forward that loopback port over SSH), or run `kahawai hub init-admin` in a
second terminal. For the container image, run it inside the already-running
hub container with a TTY (the password prompt deliberately reads the terminal):

```sh
docker exec -it <container-name> kahawai hub init-admin
```

The local browser listener and private Unix socket disappear after the first
account commits. The mediahost (and any transcoder) prints an
**enrollment code** on first connect; approve it on the admin page. Satellites
receive certificates from the hub (it is its own CA) and reconnect on their
own ever after.

Add machines by running `kahawai mediahost` or `kahawai transcoder` anywhere
that can reach the hub's satellite port, with `[mediahost] hub =` /
`[transcoder] hub =` pointed at it — same enrollment flow.

### Configuration

One TOML file for every role; each binary reads only its section. Location:
`$XDG_CONFIG_HOME/kahawai/kahawai.toml` (usually `~/.config/kahawai/`), or
`./kahawai.toml`, or `--config <path>`. Any key can be overridden with an
environment variable shaped `KAHAWAI_<SECTION>__<KEY>`, e.g.
`KAHAWAI_HUB__DATA_DIR=/srv/kahawai`.

```toml
[all_in_one]
transcoder = true               # set false (then restart) to keep encoding off this machine;
                                # external transcoders can still enroll and remux stays local

[hub]
bind = "127.0.0.1:8420"          # client API + web app; put a reverse proxy in front for TLS
# public_url = "https://kahawai.example.com" # enables strict browser Origin checks; HTTPS sets Secure cookies
setup_bind = "127.0.0.1:8422"    # first-run browser only; loopback and a distinct listener port
satellite_bind = "0.0.0.0:8421"  # enrollment + mTLS link for satellites
data_dir = "~/.local/share/kahawai"  # db, PKI, caches (default shown for user installs)
hostnames = ["localhost"]        # names/IPs baked into the hub's certificate SANs —
                                 # add the LAN address remote satellites will dial
satellite_cert_days = 90
enrollment_ttl_minutes = 15
max_sessions_per_user = 4        # concurrent playback sessions ONE account may hold;
                                 # raise for a shared or kiosk account (restart to apply)

[mediahost]
hub = "localhost:8421"
name = "nas"                     # shown in the admin UI
rescan_minutes = 60              # backup sweep; the fs watcher reacts immediately where
                                 # the filesystem supports it (inotify never fires on
                                 # network mounts like sshfs — the sweep covers those). 0 = off

[[mediahost.collections]]
name = "movies"                  # stable id — renaming makes it a new collection
media_type = "movies"            # movies | series | anime | music
roots = ["/mnt/media/movies"]  # absolute paths; each gets a deterministic identity

[[mediahost.collections]]
name = "series"
media_type = "series"
roots = ["/mnt/media/series"]

[transcoder]
hub = "localhost:8421"
name = "gpu-box"
max_sessions = 2                 # concurrent encodes this machine offers
```

`hub.public_url` is optional. When set, it must be the exact browser-facing
HTTP(S) origin; browser login, refresh and logout then require that Origin
exactly. HTTPS adds `Secure` to the server-managed browser cookies; configured
HTTP is allowed but logs that authentication cookies and tokens cross the
network in cleartext. When `public_url` is unset, Kahawai does not validate
browser Origin headers. Trusted `X-Forwarded-Proto` and `X-Forwarded-Host`
values may still mark cookies `Secure`.

The live OpenAPI 3.2 document is served at `/api-docs/openapi.json`; browse and
exercise it through the embedded Swagger UI at `/swagger-ui`.

Metadata providers (TMDB key, TheTVDB key/PIN) are configured in the admin
web UI, not the config file. Mediahost roots are treated as strictly read-only —
Kahawai never writes next to your media. Root list order has no identity meaning:
every source is bound to the deterministic token of its absolute, lexically
normalized configured root path, so equal relative filenames in separate roots
remain distinct. Exact-root identity is satellite protocol 3; protocol 2
mediahosts and transcoders must be upgraded before they reconnect. This wire
break does not rescan or rematch the catalogue.

## License

Kahawai is [MIT licensed](./LICENSE).

Media plumbing is provided by [GStreamer](https://gstreamer.freedesktop.org/),
which kahawai links dynamically as system libraries (LGPL-2.1+) — install it
through your distribution. Some optional GStreamer plugins kahawai can take
advantage of (for example `x264enc` from gst-plugins-ugly, or `a52dec`) are
GPL-licensed; they are loaded at runtime from your system when present, and
kahawai's preference-ordered element lists fall back gracefully when they are
not. If you redistribute kahawai *bundled together with* GStreamer and its
plugins (e.g. a container image), the LGPL/GPL terms of those components apply
to that bundle — for this open-source project that amounts to shipping the
license notices and pointing at the (already public) sources, or simply
excluding the GPL plugin set.

The OCR subtitle tier (turning PGS/VobSub image subtitles into text) links
Tesseract via [`leptess`](https://lib.rs/crates/leptess) (MIT; Tesseract and
Leptonica are Apache-2.0/BSD-style), so it carries no copyleft consequence.
Building with `--no-default-features` drops the Tesseract linkage entirely
for minimal deployments.

Metadata courtesy of [TMDB](https://www.themoviedb.org/) and
[TheTVDB](https://thetvdb.com/) when configured; both require the in-app
attribution kahawai displays alongside their data.

## Name

*Kahawai* is Hawaiian for **stream**. The same word in te reo Māori names the kahawai fish (*Arripis trutta*), from *kaha* (strong) + *wai* (water). A streaming server could hardly ask for a better pair of meanings, and we use the word with respect for both origins.
