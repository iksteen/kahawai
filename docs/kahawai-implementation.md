# Kahawai — Implementation Document

**Version:** 0.1 (draft)
**Companion to:** kahawai-technical-requirements.md

---

## 1. Technology choices

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust (edition 2024, stable toolchain) | Requirement; memory safety around long-running media daemons |
| Async runtime | `tokio` | Ecosystem default; timers, I/O, task management |
| Media | `gstreamer-rs` (GStreamer ≥ 1.24) | Discovery (`GstDiscoverer`), pipelines, hw-accel plugin ecosystem |
| Client API | `axum` + `serde` JSON, WebSocket via `axum::extract::ws` | Tower middleware stack, TLS via `rustls` |
| Inter-module RPC | gRPC via `tonic` + `prost`; separate raw byte-stream channel | Typed, versioned contracts; streaming built in |
| Persistence | SQLite via `sqlx` (WAL mode), migrations via `sqlx::migrate` | Embedded, zero-dependency default (HUB-13) |
| Filesystem watch | `notify` crate + periodic reconciliation scan | MH-2 |
| Config | TOML via `figment` (file + env overlay) | NFR-6 |
| Logging/metrics | `tracing` + `tracing-subscriber`, `metrics` + Prometheus exporter | NFR-6 |
| Auth / PKI | `jsonwebtoken` (client API); embedded hub CA with `rcgen` + `x509-parser`, mTLS via `rustls` custom verifiers (inter-module) | HUB-11, SEC-1..7, NFR-4 |
| IDs | ULID (`ulid` crate) for all entities | Sortable, opaque |
| Web UI | TypeScript + Vite + React, `hls.js` for playback; assets embedded via `rust-embed` | HUB-25..28; single-binary distribution preserved |

## 2. Workspace layout

```
kahawai/
├── Cargo.toml                  # workspace
├── crates/
│   ├── kahawai-core/           # shared types: capability model, stream model,
│   │                           # content identity, negotiation engine (pure logic)
│   ├── kahawai-proto/          # .proto files + tonic/prost codegen
│   ├── kahawai-transport/      # Transport trait: TcpTransport (tonic) and
│   │                           # LocalTransport (in-process duplex channels)
│   ├── kahawai-media/          # gstreamer wrappers: discovery, pipeline builder,
│   │                           # encoder capability probing
│   ├── kahawai-hub/            # hub library (registry, libraries, enrichment,
│   │                           # sessions, in-hub remuxer, client API)
│   ├── kahawai-mediahost/      # mediahost library (scanner, watcher, file server)
│   ├── kahawai-transcoder/     # transcoder library (session runner)
│   ├── kahawai-providers/     # enrichment providers: thetvdb, tmdb, musicbrainz,
│   │                           # anidb, anilist, anime-lists mapping, local-nfo
│   │                           # (MetadataProvider trait); subtitle providers:
│   │                           # opensubtitles (SubtitleProvider trait)
│   └── kahawai/                # the binary crate
├── web/                        # TS + Vite + React SPA (admin UI + MVP player);
│                               # `vite build` output embedded into kahawai-hub
│                               # via rust-embed at compile time
└── migrations/
```

The single `kahawai` binary exposes subcommands: `kahawai all-in-one`, `kahawai hub`, `kahawai mediahost`, `kahawai transcoder` (AR-5). Each module crate exports `async fn run(cfg, transport) -> Result<()>`; the binary merely wires the chosen transport:

```rust
match cli.command {
    Cmd::AllInOne => {
        let (hub_side, host_side, tc_side) = kahawai_transport::local_mesh();
        try_join!(
            kahawai_hub::run(cfg.hub, hub_side),
            kahawai_mediahost::run(cfg.mediahost, host_side),
            kahawai_transcoder::run(cfg.transcoder, tc_side),
        )?;
    }
    Cmd::Hub => kahawai_hub::run(cfg.hub, TcpTransport::listen(cfg.bind).await?).await?,
    ...
}
```

`LocalTransport` implements `tower::Service` over in-memory duplex streams so tonic clients/servers run unchanged in-process; the media byte channel maps to direct `tokio::fs` reads in all-in-one mode (AR-11 short-circuit).

## 3. Inter-module protocol (`kahawai-proto`)

Three gRPC services, all initiated module→hub (AR-3) over mTLS (the satellite's client certificate *is* its authentication — no token field in `Register`; the hub reads module type and ID from the certificate's SAN, see §7), each opening a long-lived bidirectional stream carrying an envelope `{ protocol_version, msg }` (AR-7). A fourth, minimally privileged `Enrollment` service (§7) is the only endpoint reachable without a client certificate.

**`MediahostLink`** — `Register(hostinfo)`, then bidi stream:
- host→hub: `AnnounceCollection{ id, media_type, roots }`, `FileUpsert{ collection_id, batch<FileRecord> }`, `FileRemove`, `ScanProgress`, `FileError` (MH-8), `Heartbeat`
- hub→host: `RequestScan`, `OpenRead{ file_id, session_token }` → host responds by accepting a byte-stream channel keyed by the token
- `FileRecord = { path_rel, size, mtime, identity: ContentId, streams: StreamInfo[], sidecars[], tags{} }`

**`TranscoderLink`** — `Register(capability_report)`, then:
- hub→tc: `StartSession{ spec: TranscodeSpec }`, `Seek{ session, offset }`, `SetQuality{ session, ladder_step }`, `Cancel{ session }`
- tc→hub: `SegmentReady{ session, seq, uri|inline }`, `PlaylistUpdate`, `Progress{ realtime_x, position }`, `SessionError`, `Load{ cpu, gpu_sessions }`

**Byte plane.** A separate framed TCP listener on the hub, also mTLS under the hub CA; a connecting satellite is authenticated by its client certificate, and a one-time token minted on the control stream binds the connection to a specific read lease or transcode session. This keeps bulk media bytes off the gRPC streams. In all-in-one mode the byte plane is a function call.

Content identity (MH-5):

```rust
struct ContentId { size: u64, head_xxh3: u64, tail_xxh3: u64 } // 64 KiB head + tail
// FileRecord additionally carries oshash: u64 — the OpenSubtitles moviehash
// (size + wrapping u64 sum of first/last 64 KiB), computed in the same read pass.
```

Fast-path change detection uses `(path, size, mtime)`; `ContentId` resolves renames/moves so the hub carries item identity and watch state across them.

## 4. Hub internals

### 4.1 Data model (SQLite)

Core tables: `mediahosts`, `collections`, `files` (technical metadata as JSON column + indexed scalar columns), `libraries`, `library_collections`, `items` (logical entities; `kind` = movie|show|season|episode|artist|album|track), `item_sources` (item ↔ file, quality rank), `item_metadata` (per-provider, per-language), `subtitles` (downloaded/registered external subtitle streams, §4.3a), `images` (cached artwork, size variants), `users`, `grants`, `watch_state (user, item, position_ms, play_count, updated_at)`, `watch_state_archive` and `binding_archive` (content-identity-keyed survivors of mediahost deletion, §7.4), `sessions`, `revoked_certs (fingerprint, revoked_at)`, `provider_cache (key, body, expires_at)`.

Watch-state writes are batched but flushed on session teardown and every 10 s (NFR-3).

### 4.2 Item resolution pipeline

Runs per file-upsert batch, incrementally:

1. **Parse** filename/dirs → `NameGuess` (title, year, S/E including `S01E01E02`, specials `S00`, absolute numbering, `Artist/Album/NN - Track` for music). Anime collections use a dedicated tokenizer variant for fansub conventions: `[Group] Title - 01v2 [1080p][A1B2C3D4].mkv` → group, title, absolute episode, version, CRC32, quality tags; batch/OVA/ONA/movie markers. Table-driven tokenizer, not regex soup; per-library overrides.
2. **Bind** to an item: exact match on prior manual binding by `ContentId` → else provider match ≥ confidence threshold → else create *unmatched* item flagged for review (HUB-8).
3. **Dedup**: same external ID (tvdb/tmdb/musicbrainz release+track) or same normalized identity ⇒ attach as additional `item_source`, rank by resolution/bitrate/codec modernity (HUB-3).
4. **Enrich** via provider chain (below).

### 4.3 Enrichment providers

```rust
#[async_trait]
trait MetadataProvider {
    fn id(&self) -> &'static str;
    fn supports(&self, kind: MediaKind) -> bool;
    async fn search(&self, q: &NameGuess) -> Result<Vec<Candidate>>;
    async fn fetch(&self, ext_id: &ExtId, lang: &Lang) -> Result<ItemMetadata>;
    async fn images(&self, ext_id: &ExtId) -> Result<Vec<ImageRef>>;
}
```

Implementations: `thetvdb` (v4 API, JWT login flow), `tmdb`, `musicbrainz` (+ Cover Art Archive; hard 1 req/s limiter), `local` (NFO + sidecar art + embedded tags). Each provider wrapped in a `governor` rate limiter and the `provider_cache` table (HUB-7). Per-library provider order implements HUB-9. A background refresher re-fetches continuing series weekly.

### 4.3a Subtitle acquisition (HUB-21..24)

```rust
#[async_trait]
trait SubtitleProvider {
    fn id(&self) -> &'static str;
    async fn search(&self, q: &SubQuery) -> Result<Vec<SubCandidate>>;
    //  SubQuery { oshash: Option<u64>, size: u64, ext_ids: Vec<ExtId>,
    //             name: Option<NameGuess>, langs: Vec<Lang> }
    //  SubCandidate { provider_file_id, lang, format, release, rating,
    //                 download_count, uploader, hash_matched: bool }
    async fn download(&self, id: &ProviderFileId) -> Result<SubtitlePayload>;
    fn quota(&self) -> QuotaState; // remaining, resets_at
}
```

`opensubtitles` implements it against the current REST API (`api.opensubtitles.com/api/v1`): API key header, `POST /login` for the user JWT that downloads require, `/subtitles` search by `moviehash`+`moviebytesize` first (exact-release matches, flagged `hash_matched`), falling back to `tmdb_id`/`imdb_id` from enrichment, then title/season/episode. Candidates are ranked for display: hash match ≫ external-ID match + release-string similarity, then download count and rating — but the *user* always picks; there is no automatic selection. The client tracks the account's daily download quota from response headers and returns it with every search and download response so clients can show "N downloads left today"; an exhausted quota fails the download with the reset time, never queues anything in the background. Search responses go through `provider_cache` like everything else.

**Storage — hub only.** Payloads are normalized on ingest (encoding sniff → UTF-8; SRT kept as master, converted on demand) and written under the hub's `data_dir/subtitles/{item_source_id}/{lang}-{n}.srt` with a `subtitles` table row (`item_source_id, lang, origin: local|embedded|opensubtitles, provider_file_id, uploader, downloaded_by_user, format, created_at`). Nothing is ever written to a mediahost — the mediahost link has no write operation to abuse (MH-6), so this holds by construction. At negotiation time these rows are merged into `SourceStreams` as external text-subtitle streams; delivery is the normal §4.5 path — pass-through/convert served straight from hub disk, or shipped to the transcoder over the byte plane as an extra input when the plan says burn-in.

**Flow — strictly user-initiated (HUB-24).** The only trigger is `POST /api/v1/items/{id}/subtitles` from an authenticated user after a search; there is no import hook, no scheduler, no playback side effect. Once downloaded, the subtitle is available to all users with access to the item (it's a property of the item source, not of the requesting user, though the requester is recorded). Users can list, replace, and delete downloaded subtitles per item.

### 4.3b Anime pipeline (HUB-29..33)

**Matching order** for `anime` collections: (1) ED2K hash → AniDB file endpoint, which returns the exact anime/episode/group/version — this is the gold path and why MH-9 exists; (2) anime tokenizer output → AniDB titles index (see below) with the AniList search API as tie-breaker; (3) manual review queue, same as everything else. Absolute numbering is authoritative; the season-style view is derived through the mapping, never the other way around.

**AniDB discipline.** AniDB's API is aggressively rate-limited and ban-happy, so the client is built around *never asking twice*: the daily anime-titles dump is downloaded once per day and indexed locally for all title search (zero API calls for search); per-anime and per-file responses are cached effectively forever (invalidated only by explicit admin refresh); a global limiter enforces one request per 2+ seconds with long backoff on error codes; and the client identifies itself with a registered client ID. All of this lives behind the provider trait — the rest of the hub doesn't know AniDB is special.

**AniList** (GraphQL, generous limits) supplies descriptions, cover art, seasonal data, and the **relations graph** (`SEQUEL`/`PREQUEL`/`SIDE_STORY`/`ALTERNATIVE`), stored as an `item_relations (from_item, to_item, kind)` table; the item-detail endpoint walks it into a linearized suggested watch order. The community **anime-lists** mapping (AniDB↔TVDB↔TMDB JSON, refreshed weekly) provides the season-view projection (HUB-31) and lets artwork/description fall back to TVDB/TMDB when the anime services are sparse — fallback goes through the normal per-library provider order.

**ASS/SSA path.** The mediahost extracts MKV font attachments during scan (matroska attachments surfaced by the demuxer; stored by content hash so shared fonts dedupe) and ships them with the file record; the hub stores them beside subtitles. Negotiation gains `subs: ClientRender` alongside the existing outcomes, chosen when the capability profile declares `ass_render: true` and the stream is ASS: the hub serves the ASS stream and its font set verbatim, and the web player renders it with a libass-wasm engine (JASSUB) on a canvas overlay — typesetting and karaoke intact.

For clients without ASS rendering, `ass_fallback` decides between two outcomes (HUB-32a):

- **`burn`** — transcoder burn-in via GStreamer's libass-based `assrender`, fonts delivered over the byte plane as session inputs so typeset signs render with real glyphs. Full fidelity, but note the cost model: on hardware-encode boxes (J5005-class with Quick Sync), the encode is nearly free while the overlay is not — frames must be composited, which on the common path means decode → system-memory RGB/overlay blend → re-upload, and that memory-bandwidth round trip dwarfs the encode on low-power SoCs. (A zero-copy VA-API `dmabuf` + `overlaycomposition` path exists and the transcoder should use it when the driver cooperates, but it can't be assumed.)
- **`flatten`** — the hub converts ASS dialogue to WebVTT itself: strip override tags, drop drawing-command events (`\p`) and comment lines, keep dialogue text and timing. Typesetting, positioned signs, and karaoke are lost — but no video work happens at all. Crucially, the negotiation engine re-evaluates the plan after substituting the flattened track: if video was only being encoded for the burn-in, the session degrades to remux or direct play with a text track, served hub-only with zero transcoder involvement.

The policy is a server default with per-library and per-user overrides, and the player's "playback info" overlay reports which path was taken and why; a user can also flip it per session (e.g., accept flattening tonight because the transcoder is busy). Nothing ever flattens silently.

**Dual audio.** Per-user, per-library preference `audio: original_subbed | dubbed(lang)` feeds default stream selection at negotiation time (HUB-33); the chosen default is overridable per session in the player as usual.

### 4.4 Client API (v1 sketch)

```
POST /api/v1/auth/token                     # login → access+refresh
GET  /api/v1/libraries
GET  /api/v1/libraries/{id}/items?kind=&sort=&page=
GET  /api/v1/items/{id}                     # incl. sources[] with full StreamInfo
GET  /api/v1/items/{id}/children            # seasons/episodes, album/tracks
GET  /api/v1/search?q=
GET  /api/v1/images/{id}?w=&h=              # resized, cached
GET  /api/v1/items/{id}/subtitles           # available streams: embedded/sidecar/downloaded
GET  /api/v1/items/{id}/subtitles/search?lang=   # provider candidates (quota state included)
POST /api/v1/items/{id}/subtitles           # body: { provider, provider_file_id }
DELETE /api/v1/items/{id}/subtitles/{sub_id}
POST /api/v1/playback/decisions             # body: item_id + CapabilityProfile
POST /api/v1/playback/sessions              # start; returns manifest or direct URL
GET  /api/v1/playback/sessions/{id}/stream  # direct-play byte-range endpoint
GET  /api/v1/playback/sessions/{id}/master.m3u8
POST /api/v1/playback/sessions/{id}/progress
DELETE /api/v1/playback/sessions/{id}
WS   /api/v1/events                         # library changes, session events
GET  /admin/v1/...                          # registry, libraries, matching queue
GET  /admin/v1/enrollments                  # pending CSRs (fingerprint, type, name, age)
POST /admin/v1/enrollments/approve          # body: { code }
GET  /admin/v1/satellites                   # enrolled modules + cert fingerprints + status
DELETE /admin/v1/satellites/{id}            # delete = revoke cert + cascade (see §7.4)
```

`/playback/decisions` is side-effect-free and returns the full negotiation verdict (per-stream direct/remux/transcode + reasons) so clients can display "why is this transcoding".

### 4.5 Capability negotiation (`kahawai-core::negotiate`)

Pure function, exhaustively unit-tested:

```rust
fn negotiate(source: &SourceStreams, cap: &CapabilityProfile, policy: &Policy)
    -> PlayPlan // { container: Keep|Remux(fmt), video: Copy|Encode(spec),
                //   audio: Copy|Encode(spec),
                //   subs: Passthrough|ClientRender|Convert|Flatten|Burn|Drop,
                //   ladder: Vec<LadderStep>, reasons: Vec<Reason> }
```

Decision order per HUB-16: try full direct play; else keep every stream that fits and remux; else encode only failing streams. Rules include: profile/level comparison for H.264/HEVC/AV1; HDR→SDR tone-map (`gst` `tonemap`/shader path) when client lacks HDR; channel downmix when layout unsupported; text subs converted SRT↔WebVTT with `subparse`; ASS handled per the `ass_fallback` policy (§4.3b: `ClientRender` when the client declares it, else `Burn` or `Flatten` — and after a `Flatten` substitution the plan is re-evaluated, since removing the burn-in often demotes a video encode back to remux/direct); PGS/VOBSUB burn-in when the plan already encodes video, else forced video encode if user enabled subtitles the client can't render; bandwidth cap forces a ladder whose top rung ≤ cap.

### 4.6 Session manager

State machine per session: `Negotiated → Provisioning → Streaming → (Seeking|SwitchingQuality)* → Ended`. Direct play sessions hold an `OpenRead` lease against the mediahost and proxy ranges with `Accept-Ranges`/`206`.

**Remux sessions run entirely inside the hub** — this is why `kahawai-hub` depends on `kahawai-media`. When the plan is `container: Remux` with every stream `Copy`, the hub feeds the mediahost byte stream through a local demux-only pipeline (`appsrc ! parsebin ! <selected streams, no decode> ! cmafmux → hlssink3`-style segmenting) and serves the result like any HLS session. Parsing and repackaging elementary streams is cheap (no codec work, a few % CPU), so it needs no scheduling, works with zero transcoders attached (AR-10), and keeps transcoders free for real encoding jobs. Seek = pipeline restart at the target keyframe, same as §6 but without the decode path.

Transcode sessions (any stream marked `Encode`) are placed on a transcoder by a scorer (capability fit ≥ hw-accel ≥ inverse load), monitored via `Progress`; on transcoder loss the spec is re-issued to the next candidate with `start_offset = last served segment` (AR-6). If no transcoder is connected, plans requiring `Encode` fail fast at `/playback/decisions` with a distinct reason so clients can fall back (e.g., pick a lower-quality source or disable the offending subtitle) rather than time out. Idle timeout 90 s without segment fetch or progress ping → teardown. Concurrency limits enforced per user (HUB-18).

### 4.7 Embedded web UI (HUB-25..28)

**Serving.** `vite build` output is embedded with `rust-embed` and served by an axum fallback route: `/app/*` → SPA `index.html` (client-side routing), hashed assets with immutable cache headers, `/` redirects to `/app`. A `--dev-web-proxy` flag proxies to the Vite dev server for frontend development against a live hub. The SPA authenticates with the same JWT flow as any client and calls only `/api/v1` and `/admin/v1` — no private endpoints (HUB-28); admin routes render only for users whose token carries the admin role, but authorization is enforced server-side as usual.

**Capability profile.** On startup the player probes the browser honestly rather than shipping a static profile: `MediaSource.isTypeSupported()` / `mediaCapabilities.decodingInfo()` across the codec matrix (H.264 profiles/levels, HEVC, AV1, AAC/AC-3/Opus/FLAC), container support (fMP4 via MSE; native HLS on Safari), HDR via `matchMedia('(dynamic-range: high)')` + codec profile support, and screen dimensions — serialized into the `CapabilityProfile` sent to `/playback/decisions`. This makes the web player the reference implementation of negotiation from the client side.

**Video playback.** Direct play binds the range endpoint straight to `<video src>` (browsers do range requests natively); remux/transcode plans load the session's `master.m3u8` via `hls.js` (MSE) with native HLS fallback on Safari. Seek beyond the transcoded window and ladder switches go through the session endpoints from §4.6. Text subtitles attach as WebVTT `<track>` elements (hub converts on demand); ASS/SSA streams render client-side via JASSUB (libass compiled to WASM) on a canvas overlay, loading the item's served font set, and the player declares `ass_render: true` in its capability profile accordingly (§4.3b); burned-in subtitles arrive inside the video and the UI marks them as such from the negotiation verdict, which is also surfaced in a "playback info" overlay (direct/remux/transcode + per-stream reasons). Progress posts every 10 s and on pause/unload.

**Music playback.** A persistent queue over `<audio>` with preloading of the next track via a second element swapped at track boundary (near-gapless; true gapless via Web Audio API is post-MVP), album/artist views, and the same negotiation path (FLAC direct where supported, else transcoded Opus/AAC).

**Admin UI.** Thin CRUD over `/admin/v1` plus the `/api/v1/events` WebSocket: the enrollments page updates live as CSRs arrive (approve-by-code inline), satellites page shows fingerprints/status with delete-and-cascade confirmation spelling out consequences (HUB-20), a drag-to-compose library builder over announced collections, the manual-match review queue with provider candidate side-by-side, subtitle/enrichment provider settings, user/grant management, and a sessions dashboard streaming per-session state and throughput.

## 5. Mediahost internals

**Scanner.** Bounded-concurrency walker (`ignore` crate for traversal, N=4 discoverers) feeding a work queue persisted in a small local SQLite journal so scans resume (MH-7). Each file: fast-path check → if changed, `GstDiscoverer::discover_uri` with 30 s timeout → map `GstDiscovererInfo` into `StreamInfo` (codec caps → normalized codec enum + profile/level from caps fields; HDR from mastering-display/CLL caps and DOVI configuration boxes; tags via `GstTagList` for music). Sidecar association by stem-matching within the directory (MH-4). Failures → `FileError` with the GStreamer diagnostic (MH-8).

**Watcher.** `notify` events debounced 2 s, coalesced per directory, feeding the same queue; nightly reconciliation walk catches missed events (network mounts).

**ED2K hasher (MH-9).** A separate low-priority queue, enabled per collection on hub request (anime): full-file ED2K (9.28 MiB chunked MD4) computed at bounded read rate during idle, optionally verifying a filename CRC32 in the same pass, results journaled by content identity and pushed as `FileUpsert` amendments. Scan completion never waits on it — hash matches upgrade an item's identification asynchronously.

**File server.** Serves `OpenRead` leases only — file IDs map to paths server-side; client-supplied paths never exist in the protocol, making traversal structurally impossible (NFR-4). `tokio::fs` + `sendfile`-style chunked copy, read-only `O_RDONLY|O_NOFOLLOW` opens.

## 6. Transcoder internals

**Capability probing at startup.** Enumerate `gst::ElementFactory` list, rank encoders: `vaapih264enc/vah264enc`, `nvh264enc`, `qsvh264enc`, `x264enc` (and HEVC/AV1 equivalents); verify by dry-running a 1-frame pipeline per encoder so a broken driver is discovered at registration, not mid-session (TC-1, TC-6 fallback list retained).

**Pipeline construction per `TranscodeSpec`.** Source is a custom `appsrc`-backed element fed from the hub byte plane (or direct file in all-in-one), pushed into:

```
appsrc ! parsebin ! streamselect
  video: decodebin3 ! [deinterlace] ! [tonemap] ! [scale] ! [subtitle-overlay] ! {enc} ! parse
  audio: decodebin3 ! audioconvert ! audioresample ! [downmix] ! {aac|opus}enc
  subs (burn): decode to overlay branch; (convert): subparse ! webvttmux path
  → hlssink3 (cmafmux) → segments + playlist events
```

Streams marked `Copy` bypass decode: `parse ! queue` straight into the muxer. Segment duration 4 s (2 s for LL-HLS later). Seek = teardown + rebuild with `segment start` at target keyframe (accurate seek via `parsebin` index when available), playlist continues with `EXT-X-DISCONTINUITY` (TC-4). Transcode-ahead window: pause pipeline (`appsink` backpressure on segment sink) when > N minutes ahead of last-fetched segment (TC-5).

**Resource control.** Session slots = min(configured, hw session limit); scratch dir with LRU eviction of segments already fetched by the hub; cgroup-friendly CPU shares documented for containerized runs (TC-6).

## 7. Security implementation — hub as CA

### 7.1 CA bootstrap (hub)

On first start the hub generates an ECDSA P-256 CA keypair and a self-signed CA certificate (CN `Kahawai Hub CA <hub_id>`, `basicConstraints: CA=true, pathlen:0`, 10-year validity) via `rcgen`, stored under `data_dir/pki/` (`ca.key` mode 0600, `ca.crt`). It also issues itself a leaf server certificate (SAN: configured hostnames/IPs + URI `kahawai://hub/<hub_id>`) used on the control-plane, byte-plane, and enrollment listeners; this leaf is what satellites validate against the pinned CA (SEC-5), and it is auto-rotated by the hub well before expiry.

### 7.2 Enrollment flow (SEC-2..4)

A satellite that finds no certificate in its `state_dir` enters enrollment:

```
satellite                                   hub
─────────                                   ───
generate P-256 keypair (key stays local)
build CSR { CN: <name>,
            URI SAN: kahawai://<type>/<module_id> }
                    ── TLS (server-only, unverified) ──▶  Enrollment.Submit(CSR)
print code = base32(SHA-256(CSR_DER))[0..8]              store as pending (TTL 15 min)
e.g.  "Enrollment code: Q7RM-3KP2"                       log + expose in admin API/CLI
        ...poll Enrollment.Status(csr_fp)...
                                            admin runs `kahawai hub enroll` or uses UI,
                                            types code → hub recomputes fingerprint of
                                            each pending CSR, exact match required
                    ◀── { signed_cert, ca_cert } ──      sign via rcgen, record satellite row
persist key/cert/ca in state_dir, pin ca.crt
reconnect with full mTLS ──────────────────▶  normal Register
```

The code commits to the CSR (and therefore the satellite's public key), so a machine-in-the-middle on the unverified enrollment channel cannot substitute its own CSR: the admin types the code shown on the *satellite's* console, and a substituted CSR's fingerprint won't match it. The enrollment listener accepts nothing but `Submit`/`Status`, is rate-limited per source address, and never returns whether a code was "close". Wrong code → enrollment rejected and removed (SEC-3). The satellite's private key is generated and kept locally only; nothing but the CSR and certificates cross the wire.

### 7.3 Certificate profile and renewal

Satellite leaf certs: `extendedKeyUsage = clientAuth, serverAuth` (serverAuth reserved for future delegated delivery, AR-8), URI SAN `kahawai://<mediahost|transcoder>/<module_id>`, 90-day validity. The hub reads module type and ID from the SAN on every connection — the certificate is the identity; no separate token database. When less than 30 days of validity remain, the satellite submits a fresh CSR over the already-authenticated control channel and the hub signs it automatically (SEC-7); renewal keeps or rotates the keypair per config.

### 7.4 Validation and revocation (SEC-5..6)

Both mTLS listeners use a custom `rustls` `ClientCertVerifier`: verify chain to the hub CA, reject expired certs, then check `sha256(cert_DER)` against the `revoked_certs` table (kept in memory, backed by SQLite; also consulted by the enrollment service to stop re-submission spam from revoked keys). Satellites use a `ServerCertVerifier` that requires the hub chain to terminate at the *pinned* CA cert byte-for-byte — a different CA with the same name fails.

Revocation only happens through satellite deletion. `DELETE /admin/v1/satellites/{id}` runs one transaction-plus-teardown sequence:

1. insert cert fingerprint into `revoked_certs`;
2. registry closes the satellite's control and byte-plane connections; active sessions fail over (transcoder) or terminate with a client-visible error (mediahost, AR-6);
3. mediahost only — cascade: archive `watch_state` rows and manual match bindings for affected items into `watch_state_archive (user_id, content_id, position_ms, play_count)` / `binding_archive (content_id, ext_id)`, then delete `item_sources` for the host's files, delete items with zero remaining sources, delete `files`, `collections`, and `library_collections` rows;
4. emit a library-changed event on `/api/v1/events`.

A transient disconnect touches none of this — it only flips availability flags. On any later import, resolution step 2 (§4.2) checks the archives by `ContentId` first, restoring identity, manual matches, and watch state before consulting providers. Re-admitting a deleted machine is a fresh §7.2 enrollment with a new key.

In all-in-one mode `LocalTransport` bypasses TLS and enrollment entirely — the three modules share a process and trust is intrinsic; the PKI machinery activates only for network transports.

### 7.5 Client API

Unchanged by the PKI: Argon2id password hashes, 15-min JWT access tokens, rotating refresh tokens with a server-side revocation table, per-route authorization middleware mapping user grants → library visibility. The client-facing listener may use the hub CA's leaf cert, an ACME cert, or sit behind a reverse proxy; client apps are *not* enrolled in the internal CA.

## 8. Configuration example

```toml
[hub]
bind = "0.0.0.0:8420"
data_dir = "/var/lib/kahawai"
[hub.enrichment]
providers = ["local", "thetvdb", "tmdb", "musicbrainz"]
anime_providers = ["local", "anidb", "anilist"]   # used by anime libraries
[hub.enrichment.thetvdb]
api_key = "${KAHAWAI_TVDB_KEY}"
[hub.enrichment.anidb]
client_id = "kahawai"                 # registered AniDB client
client_version = 1

[hub.playback]
ass_fallback = "burn"                 # server default: "burn" | "flatten";
                                      # overridable per library and per user

[hub.subtitles.opensubtitles]        # feature off unless this block exists
api_key = "${KAHAWAI_OS_KEY}"
username = "…"                        # account needed for downloads
password = "${KAHAWAI_OS_PASS}"
[hub.subtitles]
default_langs = ["en", "de"]          # default search filter only — downloads are
                                      # always user-initiated, never automatic

[hub.pki]
satellite_cert_days = 90
enrollment_ttl_minutes = 15

[mediahost]
hub = "hub.lan:8421"
state_dir = "/var/lib/kahawai-mediahost"   # keypair, cert, pinned ca.crt
[[mediahost.collections]]
name = "Movies A"; media_type = "movies"; roots = ["/tank/movies"]
[[mediahost.collections]]
name = "Music"; media_type = "music"; roots = ["/tank/music"]
[[mediahost.collections]]
name = "Anime"; media_type = "anime"; roots = ["/tank/anime"]  # enables ed2k job on hub request

[transcoder]
hub = "hub.lan:8421"
state_dir = "/var/lib/kahawai-transcoder"
hw = ["vaapi"]          # probe order; "auto" default
max_sessions = 3
scratch_dir = "/var/tmp/kahawai"
```

All-in-one reads the same file with all three sections present and `hub`-address fields ignored.

## 9. Testing strategy

Negotiation engine: exhaustive table-driven unit tests (capability × source matrix). Media layer: fixture corpus generated by `gst-launch` scripts (each codec/container/subtitle permutation, tiny durations) committed via Git LFS; discovery snapshots asserted. Integration: `LocalTransport` lets the full three-module flow run in one test process; a second suite runs the same client-visible tests against a docker-compose modular topology (acceptance criterion 3). Chaos tests: kill/reconnect mediahost and transcoder mid-session (criterion 4). PKI tests: full enrollment happy path, wrong/expired code, CSR substitution (fingerprint mismatch), renewal near expiry, satellite refusing a hub presenting a foreign CA, and the deletion path — delete a mediahost, assert connection drop + collection removal + TLS-level reconnection refusal, then re-enroll, rescan, and assert manual matches and watch state are restored from the archives (criterion 5). Web UI: Playwright end-to-end suite driven against the all-in-one binary (login, enrollment approval, library composition, direct/remux/transcode playback with the real capability probe, subtitle search+download), run in Chromium and WebKit to cover both the MSE and native-HLS player paths. Performance: `criterion` micro-benches for negotiation and scan throughput; k6 scripts for API latency targets (NFR-1).

## 10. Operational readiness (OPS-1..8)

**Bootstrap.** Empty DB → hub serves only `/app/setup` and prints `Setup token: XXXX-XXXX` to console/logs; the flow (token → admin credentials → done) flips a `setup_complete` flag. `kahawai hub reset-password <user>` writes a new Argon2id hash directly. Login throttling via a `governor` keyed on `(account)` and `(source_ip)` with exponential backoff and `tracing` audit events; source IP taken from the socket or from `X-Forwarded-For` only when the peer is in the configured `trusted_proxies` list.

**`doctor`.** Shared implementation in `kahawai-media` + per-module checks. GStreamer probe reuses the transcoder's startup enumeration (§6) and maps it against a static feature matrix table (`capability → required elements`), printing a report like `HEVC decode: OK (vah265dec) / DoVi: missing (dlbvision) → will tone-map`. Also checks: registry/DB writability, scratch-dir space, `/dev/dri` access when VA-API configured, and `|system_clock - build_time|` sanity. Exit code non-zero on essential failures; `--json` for scripting. The same checks run at startup with warnings-vs-fatal per the same matrix.

**Clock skew.** Leaf certs issued with `notBefore = now - 24h`; the hub's `ClientCertVerifier` and the satellite's `ServerCertVerifier` allow ±5 min on `notAfter`/`notBefore` boundaries. A satellite failing validation compares peer-reported time (TLS handshake wall clock via a pre-flight `Enrollment.Status` ping that echoes hub time) against its own and logs `clock skew: local is 37 min behind hub — fix NTP` instead of a raw handshake error.

**Backup.** `kahawai hub backup <path>` produces a tar: SQLite snapshot via the online backup API (consistent under load), `pki/`, `subtitles/`, and the active config; `kahawai hub restore <path>` refuses on a non-empty data dir. Because `pki/` and satellite rows travel with the snapshot, restored hubs accept existing satellite certs immediately — no re-enrollment (OPS-5). Documented cron-friendly: exit codes + `--quiet`.

**Disk bounds.** `images` and `provider_cache` get byte caps (defaults: 2 GiB / 512 MiB) with LRU eviction driven by an hourly janitor task; subtitle-store size exposed as a metric and on the admin dashboard, never evicted. Hub stream proxying uses fixed bounded buffers (64 KiB chunks, bounded channel per session) so a slow client applies backpressure to the mediahost read instead of ballooning hub memory — this also caps per-session memory for the in-hub remuxer via `appsrc` `max-bytes`.

**Upgrades.** The proto envelope's `protocol_version` is `(major, minor)`; the hub advertises `supported: [N, N-1]` in the `Register` response, satellites pick the highest common. CI runs the integration suite in a version-skew matrix (hub@HEAD × satellite@last-release) to keep N-1 honest (OPS-7). Release notes template includes a "upgrade order: hub first" banner.

**Reverse proxy.** Config: `[hub.http] trusted_proxies = [...]`, `cors_origins = [...]`; docs ship known-good nginx/Caddy/Traefik snippets including the `/api/v1/events` WebSocket upgrade and streaming-friendly settings (`proxy_buffering off` for session endpoints).

## 11. Milestones

The web UI is built in vertical slices alongside its backend features rather than as a trailing milestone:

**M1 — Skeleton (4.5 wks).** Workspace, transports (local + mTLS tcp), hub CA + enrollment flow + revocation, mediahost scan + discovery, hub registry + SQLite, minimal browse API. Web: SPA scaffold, embedding pipeline, first-run setup flow (OPS-1), login, enrollment approval + satellite pages. Also: `doctor` skeleton with GStreamer inventory (OPS-3). *Exit:* all-in-one scans a library; admin completes setup and approves an enrollment from the browser and sees file/stream info.

**M2 — Direct play + remux (4 wks).** Byte plane, item resolution v1 (movies), sessions, range streaming, in-hub remuxer (MKV→fMP4/HLS, all-copy pipeline), watch state. Web: browse/detail views, capability probe, video player with direct + remux playback, resume. *Exit:* seekable direct play and hub-only remux from a modular 2-machine deployment with no transcoder, played in the web player.

**M3 — Transcoding (5 wks).** Transcoder module, capability probing, negotiation engine, HLS output, seek + quality switch, hw accel (VA-API first). Web: hls.js path, playback-info overlay with negotiation verdict, sessions dashboard. *Exit:* acceptance criterion 2 driven from the web player.

**M4 — Enrichment + series/music/anime (6 wks).** Provider trait + TheTVDB/TMDB/MusicBrainz/local, episode/track resolution, dedup/multi-source, matching review queue, image pipeline, subtitle acquisition (OpenSubtitles: moviehash search, user-initiated download only, hub-side storage, quota surfacing). Anime: fansub tokenizer, AniDB (titles-dump search + ed2k exact match) + AniList + anime-lists mapping, relations/watch order, font-attachment extraction, ASS client-render (JASSUB) and `assrender` burn-in paths, dual-audio preference. Web: enriched browse with artwork, search, match-review queue, library composer, subtitle search/download flow, music player with queue. *Exit:* criterion 1.

**M5 — Hardening (3.5 wks).** Failover paths, metrics/health, remaining admin surfaces (users/grants, provider settings), backup/restore, cache quotas + janitor, login throttling, clock-skew handling, version-skew CI matrix, reverse-proxy docs (OPS-2, 4..8), docs, packaging (binaries + containers with bundled GStreamer plugin set), performance passes, Playwright suite green in Chromium + WebKit. *Exit:* criteria 3–5, NFR-1 numbers on reference hardware.

**Post-v1 candidates.** Delegated direct delivery (AR-8), LL-HLS/DASH, offline pre-transcode (TC-7), Dolby Vision profile handling beyond fallback, OIDC, sync-play.
