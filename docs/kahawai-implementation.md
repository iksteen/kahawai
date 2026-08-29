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
| Web UI | TypeScript + Vite + Vue 3 (`<script setup>`), Tailwind, TanStack Query, `hls.js` for playback; assets embedded via `rust-embed` | HUB-25..28; single-binary distribution preserved |

### 1.1 Media framework: why GStreamer, not FFmpeg

Every incumbent in this space (Jellyfin, Plex, Emby) is built on FFmpeg, so this choice is deliberate and should be understood — and periodically re-examined — against the reasoning below.

**Rationale.**
1. **The architecture is a pipeline graph.** The negotiation engine emits a per-stream plan (copy / encode / overlay / mux), and GStreamer's programming model is dynamically constructed graphs of exactly those operations. The FFmpeg alternatives are string-typed CLI construction (the Jellyfin approach — fragile, unstructured, hard to test) or the low-level libav* C APIs. `TranscodeSpec → gst pipeline` is a direct, typed mapping.
2. **Rust bindings quality.** `gstreamer-rs` is maintained upstream by the GStreamer project itself and is among the best multimedia bindings in the Rust ecosystem; FFmpeg's Rust bindings are community-run, unsafe-heavy, and chase API churn.
3. **Feature synergies.** `GstDiscoverer` *is* the mediahost scanner (MH-3); runtime element enumeration *is* the transcoder capability report (TC-1) and the `doctor` plugin inventory (OPS-3); `appsrc`/`appsink` make the byte-plane feed and the in-hub remuxer (§4.6) natural; caps negotiation inside pipelines mirrors the hub's own client negotiation model.
4. **Licensing structure.** The base/good/bad/ugly plugin tiers give distributors a deliberate, per-plugin patent posture (NFR-8) that FFmpeg's monolithic build does not.
5. **FFmpeg isn't actually excluded.** Builds ship the `gst-libav` plugin set, wrapping FFmpeg's demuxers/decoders inside GStreamer elements — most of libav's famed tolerance for malformed rips is retained, behind the better API.

**Risk register.**

| Risk | Assessment | Mitigation |
|---|---|---|
| Robustness on broken/weird rips (libavformat is the gold standard) | High likelihood, medium impact | Ship `gst-libav`; discovery failures surface per-file (MH-8) instead of failing scans; fixture corpus (§9) grows a "hostile files" set from real-world bug reports |
| In-process pipeline crash takes down the transcoder (FFmpeg-CLI designs get process isolation free) | Medium likelihood, high impact | Each transcode session's pipeline runs in a supervised child process (see §6); a segfault kills one session, the supervisor reports `SessionError`, hub reschedules per AR-6 |
| HDR→SDR tone mapping maturity (libplacebo, FFmpeg-adjacent, currently leads) | RETIRED (2026-07) | Element research settled it: no libplacebo GStreamer element exists; `vapostproc hdr-tone-mapping` exposes the property but silently no-ops below TGL (Intel media-driver feature matrix: "HDR10 TM" is TGL+ only — proven by pixel-identical A/B on the J5005); `videoconvert gamma-mode=remap` linearizes PQ with no tone operator (output crushed to black). The shipped path is our own `glshader` PQ→BT.2390-ish fragment (kahawai-media/src/tonemap.frag), which is universal across GL boxes; quality bar per HUB-15a |
| Smaller prior-art corpus: hwaccel quirks per driver generation are folklore in FFmpeg land, undocumented for GStreamer | High likelihood, medium impact | Encoder dry-run verification at registration (§6) catches broken drivers early; `doctor` output designed to make hardware bug reports actionable; maintain our own driver-quirk notes in-repo |
| Pipeline debugging is arcane (`not-negotiated` errors) | Certain, low impact | `GST_DEBUG` category presets and pipeline-graph (`.dot`) dumps wired into transcoder diagnostics; every `SessionError` carries the source element and its last error/state |
| Framework lock-in if a specific path underperforms | Low | The `PlayPlan → pipeline` mapping is confined to `kahawai-media`'s pipeline builder; a per-path alternative backend (e.g., spawning FFmpeg for one problematic conversion) would be an isolated change, not a rewrite |

The standing decision: GStreamer, with `gst-libav` bundled, per-session process isolation, and the pipeline-construction layer kept thin enough to hedge. Revisit trigger: if M3 hardware-transcoding validation on the reference targets (VA-API Gemini Lake/Alder Lake, NVENC) reveals quirks that cost more than two weeks of unplanned work, evaluate an FFmpeg-CLI fallback for the affected paths before GA.

## 2. Workspace layout

```
kahawai/
├── Cargo.toml                  # workspace
├── crates/
│   ├── kahawai-core/           # shared types: capability model, stream model,
│   │                           # content identity, negotiation engine (pure logic)
│   ├── kahawai-proto/          # .proto files + tonic/prost codegen
│   ├── kahawai-transport/      # mTLS, enrollment identity and certificate
│   │                           # renewal shared by networked satellites
│   ├── kahawai-media/          # gstreamer wrappers: discovery, pipeline builder,
│   │                           # encoder capability probing
│   ├── kahawai-hub/            # hub library (registry, libraries, enrichment,
│   │                           # sessions, in-hub remuxer, client API)
│   ├── kahawai-mediahost/      # mediahost library (scanner, watcher, file server)
│   ├── kahawai-transcoder/     # transcoder library (session runner)
│   ├── kahawai-runtime/        # config, startup checks and worker plumbing
│   ├── kahawai-mediahostd/     # lean mediahost binary
│   ├── kahawai-transcoderd/    # lean transcoder binary
│   └── kahawai/                # the binary crate
├── web/                        # TS + Vite + Vue 3 SPA (browse, player, admin);
│                               # `vite build` output embedded into kahawai-hub
│                               # via rust-embed at compile time
└── migrations/
```

The single `kahawai` binary exposes subcommands: `kahawai all-in-one`,
`kahawai hub`, `kahawai mediahost`, `kahawai transcoder` (AR-5). Networked
mediahosts and transcoders use tonic over mTLS. All-in-one does not send that
traffic over loopback and does not run tonic against an in-memory socket: it
starts the mediahost engine with a pair of bounded Tokio queues carrying the
same generated `HostToHub` / `HubToHost` Rust values used by the wire adapter.
They are moved as ordinary Rust values — no protobuf encoding, HTTP/2 or TLS.
The hub drains the local host queue through the same `handle_host_msg` function
as the network link, keeping reconciliation, manifests, extraction worklists,
ordering and backpressure identical between deployment modes.

`[all_in_one] transcoder = false` makes the full local video executor
structurally unavailable before startup video-encoder dry-runs, capability
benchmarking and placement. It deliberately does not create a synthetic
satellite: the admin satellite toggle is a live drain for an enrolled remote
worker, whereas this setting describes what the AIO machine may run across
restarts. Hub-local remux and audio-only transcode workers remain available
(AR-10), and external transcoders continue to enroll and receive video encode
work. Plain `hub` has this same lightweight worker boundary unconditionally:
it never probes video encoders or competes for video placement.

The AIO byte path is short-circuited separately. `Sessions::set_local_source`
registers the in-process collection resolver; opening a lease for that module
resolves the path directly and serves it from a local Tokio file. The lease
retains the same request/chunk shape for its consumers, but no bytes cross gRPC
or a loopback interface. Local lightweight work and AIO's optional full encode work use the hub's
supervised worker path, not an in-process `TranscoderLink`; external
transcoders still use that gRPC service normally. The satellite listener remains active in every case.

## 3. Inter-module protocol (`kahawai-proto`)

Three gRPC services, all initiated module→hub (AR-3) over mTLS (the satellite's client certificate *is* its authentication — no token field in `Register`; the hub reads module type and ID from the certificate's SAN, see §7), each opening a long-lived bidirectional stream carrying an envelope `{ protocol_version, msg }` (AR-7). A fourth, minimally privileged `Enrollment` service (§7) is the only endpoint reachable without a client certificate.

**`MediahostLink`** — `Register(hostinfo)`, then bidi stream:
- host→hub: `CatalogOffer{ collections<(id, type, roots, epoch, version, replay_floor)> }`, `CatalogDelta{ collection, epoch, records, through_version, snapshot, done }`, `DiscoveryStatus`, `Heartbeat`
- hub→host: `CatalogCursor{ collection, epoch, version, snapshot }`, `CatalogAck`, collection-scoped `RequestScan`, `DiscoveryWake`, hub-owned subtitle/attachment extraction requests, and `OpenRead{ source, lease_token }`
- `CatalogRecord` carries a stable `(kind, key)`, mutation version, tombstone bit and typed encoded payload. Physical/source-derived kinds project into ordinary hub tables; this envelope is the replay contract, not a second hub data model.

**`TranscoderLink`** — `Register(capability_report)`, then:
- hub→tc: `StartSession{ spec: TranscodeSpec }`, `Seek{ session, offset }`, `SetQuality{ session, ladder_step }`, `Cancel{ session }`
- tc→hub: `SegmentReady{ session, seq, uri|inline }`, `PlaylistUpdate`, `Progress{ realtime_x, position }`, `SessionError`, `Load{ cpu, gpu_sessions }`

**Byte plane.** For a networked mediahost, bulk media bytes ride a separate gRPC connection: tonic byte-chunk streams over mTLS, with a one-time token minted on the control stream binding the channel to a specific read lease. The hard-won invariant (AR-12): **the byte plane MUST be a separate HTTP/2 connection from the control link**. HTTP/2 flow control is per-connection as well as per-stream: in early implementation, a single stalled lease stream (client paused, pipeline backpressured) exhausted the shared connection-level window and froze heartbeats for 40 s at a time, producing false disconnects and spurious failovers. Separate connections make a stalled lease stall only itself. The in-process mediahost takes neither connection: path resolution and file reads stay local as described in §2.

Content identity (MH-5):

```rust
struct ContentId { size: u64, head_xxh3: u64, tail_xxh3: u64 } // 64 KiB head + tail
// FileRecord additionally carries oshash: u64 — the OpenSubtitles moviehash
// (size + wrapping u64 sum of first/last 64 KiB), computed in the same read pass.
```

Fast-path change detection uses `(path, size, mtime)`; `ContentId` resolves renames/moves so the hub carries item identity and watch state across them.

## 4. Hub internals

### 4.1 Data model (SQLite)

Core tables: `satellites` (module ID/type/name/certificate fingerprints; the mTLS allowlist), `collections`, `collection_roots` (one validated root-token/path binding), `items` (logical entities owned by `(module_id, collection_id)`; parent and child are constrained to the same collection), `files` (the authoritative physical source row with stable integer ID, exact root, relative path and technical metadata), `playable_sources` plus `playable_source_parts` (one collection item rendition and its ordered physical files; ordinary files are one-part sources), `libraries` plus `library_collections` (composition only), `subtitle_tracks` (exactly one direct owner: physical streams and their OCR/raster derivatives reference `files.id`; downloaded/manual rows and their derivatives reference the collection item), `users` (including `all_libraries`), `user_libraries`, `watch_state (user, item, position_ms, play_count, updated_at)`, `watch_state_archive`, `sessions`, and append-only `satellite_audit`. There is no item-level library membership cache. Playable-source rows are explicit rendition identity, not a library presentation projection: they prevent multipart editions from being assembled by choosing one file per ordinal at read time.

Watch-state writes are batched but flushed on session teardown and every 10 s (NFR-3).

**Inputs and derivations.** Every enrichment table is one of two kinds, and which one decides who writes it. **Inputs** are facts nothing can recompute: `provider_metadata` (what each provider answered, per collection item), `provider_ranks` (chain order per media type), `rejected_matches`, `manual_match`, `anime_ids`, `enrichment_queue`, and the collection ownership on `items`. **Derivations** are functions of those, stored only because a read cannot afford to compute them: `item_match` (which provider record an item IS) and `items.sort_title`. Library visibility is not a derivation to synchronize: browse joins `library_collections` to the collection-scoped item index. A retitle flows answer → `items.sort_title`; moving an item to another collection immediately repicks its provider chain.

Nothing in the codebase writes a derivation. Triggers do, on every write to an input. This replaced an earlier `merged_metadata` table that was maintained by explicit calls and spent its life subtly stale — the rule against storing what a read can derive was right, and the reason it was right is staleness, so the answer is to remove the human step rather than the storage. Consequences worth knowing:

- A derivation must not carry an input as a column. The human pin used to be `item_match.manual`, which forced the pick to recompute *around* rows it must not touch; it lives in `manual_match` now and wins as the pick's first sort key.
- Trigger bodies must not use `(?N IS NULL OR col = ?N)` optional-filter guards. That form is unavoidable with a bound parameter and it defeats every index; in a trigger the filter is known at compile time, so it is substituted as a plain equality. Getting this wrong made a rescan quadratic.
- `INSERT OR REPLACE` into an input is forbidden: SQLite fires no DELETE triggers for REPLACE unless `recursive_triggers` is on, and it is off.

The reference for what each table means is the `hub/providers.rs` module doc, next to the code that enforces it. `tests/item_match_derived.rs` and `tests/sort_title.rs` re-derive the truth independently after every kind of write, raw SQL included.

**Connection pool.** 8 connections, WAL, foreign keys on, and `cache_size = -8192` (8 MiB per connection). SQLite's 2 MB default is smaller than the index a deep browse page walks, which made the same query cost 253 ms or 50 ms depending on which pooled connection served it. Measured at 2/8/16/64 MiB: the bimodality disappears at 8 and nothing improves above it. The memory ceiling is 8 × 8 MiB, allocated lazily.

### 4.2 Item resolution pipeline

Runs per file-upsert batch, incrementally:

1. **Parse** filename/dirs → `NameGuess` (title, year, S/E including `S01E01E02`, specials `S00`, absolute numbering, `Artist/Album/NN - Track` for music). Anime collections use a dedicated tokenizer variant for fansub conventions: `[Group] Title - 01v2 [1080p][A1B2C3D4].mkv` → group, title, absolute episode, version, CRC32, quality tags; batch/OVA/ONA/movie markers. Table-driven tokenizer, not regex soup; per-library overrides.
2. **Bind within the source's collection**: prior content binding → collection-local normalized identity/provider evidence → else create an unmatched collection item for review (HUB-8). No title/year/provider query may cross the collection boundary.
3. **Dedup within that collection only**: another physical copy may bind to the same item and is ranked by resolution/bitrate/codec modernity. The same work in another collection is a different item with independent provider and watch state (HUB-3).
4. **Enrich** the collection item via its media-type provider chain (below).

**Which record an item IS** (`item_match`) is derived, never assigned. The pick orders every candidate answer by: the owner's pin first, then match strength (a strong match beats a weak one whatever the ranking says), then `local` (HUB-9), then the media type's chain order, then provider name for determinism. Refused records are not candidates, and no candidate means NO ROW — absence is the only representation of "unmatched", covering "never asked", "only misses" and "everything refused" alike.

Because it is recomputed from scratch on every input write, a more preferred provider that later gains information replaces an automatic match by itself, a chain reorder re-decides ownership of a whole media type without contacting anyone, and a pin whose backing answer is withdrawn stops winning rather than stranding a match nothing supports. Top-level items only: episodes and tracks carry no assignment and render through their parent's.

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

Implementations: `thetvdb` (v4 API, JWT login flow), `tmdb`, `musicbrainz` (+ Cover Art Archive), `local` (NFO + sidecar art + embedded tags). Providers compose into per-media-type chains with **first-claim-wins** field merging (HUB-5) — the earliest provider to supply a field owns it.

**How that is stored.** Each provider's answer is a row in `provider_metadata (item_id, provider, …)`, and one row per top-level item — `item_match` — says which of those records the item IS, plus whether a human chose it. Nothing descriptive is stored merged: the row the API serves is resolved per read by the `resolved_metadata` view, assigned provider first and then `provider_ranks`, first non-null per field. Episodes and tracks carry no assignment and render through their parent's, so an episode of a TMDB-assigned show shows TMDB's episode data and side-fills from TVDB where TMDB has none.

That shape is deliberate, and it replaced a stored merge that produced a day of bugs: identity flipping to a weak match, a decline erasing a human's correction, a weak stranger donating fields, two manual rows tying on insertion order. Each fix added a rule to the merge. With one assignment and a read-time resolve there is no merge to get wrong, and re-deciding costs nothing — which is what makes the order editable at runtime (`provider_ranks`, per media type) and a reorder free: it re-decides from answers already on disk and contacts nobody. Assignment is strongest-match-first, then order, so a strong match beats a weak one whatever the ranking says; it is re-picked whenever an answer lands, which is how a more preferred provider that gains info replaces an automatic match without a special case. A human pin is an input like any other (`manual_match`) and wins as the pick's *first sort key* rather than by being exempt from recomputation — see §4.2. Refusing a match records the refused *records* and keeps every answer, so the item stays unmatched until a provider offers something that was not refused — "there is currently no correct record, try again when something new pops up".

**Descriptive fields (HUB-6).** Genres and cast ride the same TMDB details request that already fetches `original_language`: `append_to_response=credits` folds the credits sub-request into one call, so the pair costs no extra provider traffic — which is the only thing that made cast affordable under the pacing above. Cast is stored as JSON in billing order and capped at 15; TMDB returns 68 for a 1995 film and nothing renders that.

**Caching (HUB-7) is the answer store, not a response cache.** Every answer is kept permanently in `provider_metadata`, *including recorded misses* — an empty `provider_id` paired with `confidence = "miss"` says the provider was consulted and had nothing. Never-ask-twice, however, is keyed on the **question**, not the outcome: `provider_queries` records what was actually sent (a title-search anchor, a bridge fetch by mapped id; ED2K hashes keep their own content-keyed ledger in `ed2k_aid`), and a provider is due again exactly when its *current* question has no recorded row. A repaired title, a hash that lands after the first walk, or a bumped `QUERY_REV` (a derivation fix) each re-ask automatically — one paced request per changed question, ever — while an unchanged question is never re-sent, whatever its outcome was. (Adopted 2026-07-28 after a name-based miss permanently sealed the hash path for Doomed Megalopolis; misses had been the gate.) Provider-mandated TTLs are honoured where they exist (AniDB 24 h per anime, the daily titles dump, the weekly anime-lists mapping). A separate TTL cache would be a second copy of what the answer store already is, with its own way of disagreeing.

The view is installed on open rather than by a migration (it derives rather than stores, so its definition is free to change), and it has one non-obvious rule with a runnable check: a `JOIN` in its FROM makes it unflattenable inside a `LEFT JOIN`, which every read site is, and per-item reads then go from sub-millisecond to ~45 ms while still returning correct rows.

**Provider pacing (HUB-7), one chokepoint.** Every outbound provider request — TMDB, TheTVDB, MusicBrainz, Cover Art Archive, AniList, the AniDB HTTP API, OpenSubtitles, artwork CDNs — goes through `hub/gate.rs`, which keeps **one queue per provider host**: a single request in flight, spaced by that provider's published limit, and a `429`/`503` treated as silence for that provider alone (honouring `Retry-After`, capped at an hour) rather than a retry that walks into a ban. The queues are process-wide, because that is the unit providers count: per IP, not per struct. There is deliberately no unpaced path — `Http::send` is the only way out, so the next provider added inherits pacing instead of needing someone to remember it.

Credentialed TMDB, TheTVDB and AniDB work also carries a runtime lease over the plaintext snapshot and its provider revision. Replacing or deleting that provider's fields wakes requests parked on the host/token/UDP mutex or pacing delay before they transmit; an operation already on the wire may finish. Revision is neutral cancellation rather than provider failure: its existing `enrichment_queue` row is left untouched, no retry debt is created, later providers still run, and the zero-delay scheduler coalesces save requests into one pass using the new snapshot. Debt for a disconnected network provider stays dormant until that provider is configured again; local-provider debt remains runnable.

The numbers are each provider's own, and are the thing to re-check when behaviour changes (they move): MusicBrainz and CAA 1 req/s per IP (they answer 503 above it, and require an identifying User-Agent with contact); AniList 2.1 s — its documented 90/min has been *degraded to 30/min* for years; AniDB 2.2 s ("one page every two seconds", ban decaying only after ~24 h of silence); OpenSubtitles 1.1 s (1 req/s standard tier); TMDB 60 ms (~40/s, unpublished); TheTVDB 200 ms (no published limit); CDNs unpaced. An unknown host gets 500 ms — the forgotten provider is the dangerous one. Corrected against the published rules on 2026-07-26, after three of these numbers turned out to be wrong in our favour.

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
    fn quota(&self) -> QuotaState; // remaining, resets_at, whether deployment-shared
}
```

`opensubtitles` implements it against the current REST API (`api.opensubtitles.com/api/v1`): the `Api-Key` header carries Kahawai's own registered application key, compiled into the binary — an application identifier, not a secret, and registered to this project rather than borrowed from another (registering one is free and takes minutes). No configuration is required to use the feature. Their standard tier allows **1 request/second** (enforced by the §4.3 gate's queue, shared across the process — we had this at 5/s, which their docs do not grant) and **5 downloads per 24 hours** anonymously; attaching an account via `POST /login` swaps in the user JWT and raises the download entitlement. Search works either way. Queries go by `moviehash`+`moviebytesize` first (exact-release matches, flagged `hash_matched`), falling back to `tmdb_id`/`imdb_id` from enrichment, then title/season/episode. Candidates are ranked for display: hash match ≫ external-ID match + release-string similarity, then download count and rating — but the *user* always picks; there is no automatic selection.

Entitlement handling: remaining downloads and reset time come from the download response headers, are persisted, and ride along with every search and download response so the UI can show "3 of 5 downloads left today, resets 04:12" — with the anonymous case labelled as a *server-wide* budget, since 5/24h shared across a household is small enough that a user needs to know they're spending a common resource (and that hash-exact matches are worth spending it on). Exhaustion fails the download with the reset time; nothing queues in the background (HUB-24). A 429 never reaches this code — that is rate limiting, and the gate owns it; `402/406/407` are the entitlement itself running out. Operational note: if the embedded application key is ever rate-limited or revoked upstream, `api_key` in config overrides it without a release.

**Storage — hub only.** Payloads are normalized on ingest (encoding sniff → UTF-8; SRT kept as master, converted on demand) and written under the hub's `data_dir/subtitles/{item_source_id}/{lang}-{n}.srt` with a `subtitles` table row (`item_source_id, lang, origin: local|embedded|opensubtitles|ocr, provider_file_id, uploader, downloaded_by_user, format, created_at`). Nothing is ever written to a mediahost — the mediahost link has no write operation to abuse (MH-6), so this holds by construction. At negotiation time these rows are merged into `SourceStreams` as external text-subtitle streams; delivery is the normal §4.5 path — pass-through/convert served straight from hub disk, or shipped to the transcoder over the byte plane as an extra input when the plan says burn-in.

**Flow — strictly user-initiated (HUB-24).** The only trigger is `POST /api/v1/items/{id}/subtitles` from an authenticated user after a search; there is no import hook, no scheduler, no playback side effect. Once downloaded, the subtitle is available to all users with access to the item (it's a property of the item source, not of the requesting user, though the requester is recorded). Users can list, replace, and delete downloaded subtitles per item.

### 4.3b Anime pipeline (HUB-29..33)

**Exact-file episode identity (HUB-30).** AniDB's `FILE` reply names the episode, group and version of the exact bytes on disk, keyed by ED2K hash — identity no filename heuristic can match. Every hashed episode file is asked once, budgeted per enrichment run and paced by the client's flood rule; the full reply is cached in `ed2k_aid` (misses included, terminally). A binder then re-binds files whose cached answer disagrees with their name-derived slot: the hash wins (HUB-30a), watch state follows the file, and a misnumbered episode item left sourceless is deleted rather than haunting the season view. The binder is deliberately narrow where numbering spaces differ: `epno` is scoped to one AniDB entry, so only files whose aid matches their show's move — and that narrowness is now a decision rather than a gap (2026-08-06): AniDB splits Pokemon into an entry per season, so 213 of this library's 217 aid disagreements are files ALREADY in the right slot, and moving them to their entry's numbering would break a correct `Pokemon 06x01` to satisfy a keyspace kahawai presents as a per-user projection anyway (HUB-31). What the hash still settles across aids is collisions: several files on ONE slot whose hashes name DIFFERENT eids are different episodes and get split apart, numbered from the hash — several sources sharing an eid remain the legitimate two-copies case, so the eid is the test and the count is not; regular numbers apply to absolute-keyed episodes only; every typed number lands in season 0 under a banded layout (S=n, C=100+n, T=200+n, P=300+n, O=400+n — the hub's own layout, collision-free by construction): specials, credits reels and trailers are precisely the files name-parsing cannot place, and one squatting on an episode slot is an artifact of the numbering the hash exists to correct. Binding runs BEFORE the provider chain so the bridge projection writes titles onto corrected slots in the same pass. Name-side, the fansub tokenizer slots release designations (NCOP/NCED, OVA/OAV/ONA, SP/SPECIAL, MOVIE; arabic or roman indexes) into the same season-0 bands, with precedence calibrated on real filenames: an explicitly-indexed designator beats a stray title number, an indexless one loses to a real episode number, and SxxEyy names never reach designator logic at all. Files bound to NOTHING answer to their hash: looked up by ED2K, bound under whatever their aid names — and when nothing owns the aid and AniDB's type says Movie, the item is MINTED from the provider's answer (title, year from the cached per-anime XML) or an aid-less twin adopted. That is the one place an item originates from an answer rather than a filename, and it is deliberate: a yearless "Akira.mkv" can never earn an item any other way, and every minted field is AniDB's statement about the exact bytes. "Movie-shaped" includes single-episode OVA/Web entries (Kite Liberator is `type OVA, episodecount 1` — a movie in everything but the type string); a MULTI-episode series-type aid stays bare — one stray file must not scaffold a show.

**Batch markers are spans, not duplicates.** "OVA 1-2" and "S01E01-E02" parse to an episode range, and the range becomes ONE episode item covering `episode..=episode_end` (`items.episode_end`, 0045), rendered "E01-02". Two entries would be dishonest twice over: an explicit playable source binds the physical file to exactly one collection item (everything from sessions to watch state leans on that), and with no per-episode byte offsets, "play episode 2" could only ever play the whole file. Span slots are exempt from hash re-binding — a single-epno FILE reply must not collapse a range — and a span learned on a later scan widens the existing slot (and its auto-generated title) in place. Range detection is deliberately conservative: a dashed number pair counts only immediately after a designator or as the name's final token, so "Ranma 1-2" stays a title; and among designator tokens an explicitly-indexed one outranks an earlier indexless one, so the adjective in "Kite Special Edition Uncut OVA 1-2" cannot shadow the real "OVA 1-2".

**Matching order** for `anime` collections: (1) ED2K hash → AniDB file endpoint, which returns the exact anime/episode/group/version — this is the gold path and why MH-9 exists; (2) anime tokenizer output → AniDB titles index (see below) with the AniList search API as tie-breaker; (3) manual review queue, same as everything else. Absolute numbering is authoritative; the season-style view is derived through the mapping, never the other way around. **Hashes are canonical (HUB-30a):** a late-arriving ED2K result re-verifies whatever match the item currently holds and overrides it on disagreement — manual matches included, because the hash states what the file *is*, and a user who hand-matched a mislabeled file was matching the label. Absent disagreement, manual matches are never re-decided: anime-service IDs join a manually matched item via the reverse anime-lists mapping only.

**AniDB discipline.** AniDB's API is aggressively rate-limited and ban-happy, so the client is built around *never asking twice*: the daily anime-titles dump is downloaded once per day and indexed locally for all title search (zero API calls for search); per-anime and per-file responses are cached effectively forever (invalidated only by explicit admin refresh); **both halves of the flood rule are enforced** — short-term one packet per 2 s *and* long-term one per 4 s sustained, implemented as a 5-packet burst allowance (the server's own grace before it starts counting) draining at one packet per 4.2 s. Enforcing only the short-term half is what earned this deployment its bans: a bulk identification run then sits at double the sustained rate for as long as it lasts. A 555 records a 24 h silence in `anidb-session.json`, checked before a socket is even opened, because contact is what keeps a ban alive. The client identifies itself with the registered client ID — **`kahawai`, clientver `1`**. Operational constant: bumping the clientver requires updating the registration on anidb.net *before* the release ships, or every install starts getting rejected. All of this lives behind the provider trait — the rest of the hub doesn't know AniDB is special.

**AniList** (GraphQL, generous limits) supplies descriptions, cover art, seasonal data, and the **relations graph** (`SEQUEL`/`PREQUEL`/`SIDE_STORY`/`ALTERNATIVE`), stored as an `item_relations (from_item, to_item, kind)` table; the item-detail endpoint walks it into a linearized suggested watch order. The community **anime-lists** mapping (AniDB↔TVDB↔TMDB JSON, refreshed weekly) provides the season-view projection (HUB-31) and carries the mapped IDs through which the chain's TMDB/TVDB tail (§4.3) claims artwork/description fields the anime services leave empty — bridged, never independently re-matched.

**Embedded subtitle & font retrieval (HUB-34).** Nothing subtitle-shaped is extracted at scan time — the mediahost only *declares* subtitle streams and font attachments (identity + in-file location) in the file record. When the hub actually needs the data (user toggles a track, negotiation picks `ClientRender`, transcoder needs fonts), it walks an efficiency ladder and caches the result so each stream is materialized at most once:

1. **Cache / sidecar** — already stored hub-side, or a sidecar file readable directly.
2. **Live session tap** — *the primary mechanism, and the design insight of the subtitle arc*: if a playback session for this file is running, its pipeline is already demuxing every stream. The hub (remux sessions) or transcoder (encode sessions) attaches an `appsink` to the subtitle pad and captures packets as they flow — sub-second availability at any playback position, zero extra I/O, and it covers the overwhelmingly common case, since "I need this subtitle" almost always happens *during playback of that file*.
3. **Mediahost extraction** — `ExtractStream` asks the owning mediahost to pull the stream or attachment as efficiently as the file permits: index-driven sparse reads where the container provides one (Matroska Cues/SeekHead → read only subtitle clusters and the attachment element), header-walk seeking otherwise, full sequential read as the floor.
4. **Hub read lease** — last resort: the hub opens an `OpenRead` lease and demuxes the file itself.

Fonts follow the same path: extracted on demand (usually a single sparse read, since the scan recorded the attachment's location), cached hub-side by content hash so shared fonts across a release dedupe.

**ASS/SSA path.** Negotiation gains `subs: ClientRender` alongside the existing outcomes, chosen when the capability profile declares `ass_render: true` and the stream is ASS: the hub serves the ASS stream and its font set (materialized via the ladder above), and the web player renders it with a libass-wasm engine (JASSUB) on a canvas overlay — typesetting and karaoke intact.

For clients without ASS rendering, `ass_fallback` decides between two outcomes (HUB-32a):

- **`burn`** — transcoder burn-in via GStreamer's libass-based `assrender`, fonts delivered over the byte plane as session inputs so typeset signs render with real glyphs. Full fidelity, but note the cost model: on hardware-encode boxes (J5005-class with Quick Sync), the encode is nearly free while the overlay is not — frames must be composited, which on the common path means decode → system-memory RGB/overlay blend → re-upload, and that memory-bandwidth round trip dwarfs the encode on low-power SoCs. (A zero-copy VA-API `dmabuf` + `overlaycomposition` path exists and the transcoder should use it when the driver cooperates, but it can't be assumed.)
- **`flatten`** — the hub converts ASS dialogue to WebVTT itself: strip override tags, drop drawing-command events (`\p`) and comment lines, keep dialogue text and timing. Typesetting, positioned signs, and karaoke are lost — but no video work happens at all. Crucially, the negotiation engine re-evaluates the plan after substituting the flattened track: if video was only being encoded for the burn-in, the session degrades to remux or direct play with a text track, served hub-only with zero transcoder involvement.

The policy is a server default with per-library and per-user overrides, and the player's "playback info" overlay reports which path was taken and why; a user can also flip it per session (e.g., accept flattening tonight because the transcoder is busy). Nothing ever flattens silently.

**ASS burn-in: how the script and its fonts reach libass (HUB-32a).** Established by experiment 2026-08-02, because the obvious route does not work and the working one is not documented anywhere. `subparse` cannot feed `assrender`: it emits `text/x-raw`, which is the FLATTEN path — the two fallbacks are not two ends of one pipeline. `assrender` takes `application/x-ass` on `text_sink`, with the script's header (everything through the `[Events] Format:` line) as `codec_data` on the caps and each Dialogue line as a separately timed buffer carrying only the fields after the timestamps. That is what a demuxer produces, which is why embedded tracks appear to work for free.

*The path follows the track's ORIGIN, not the file's form.* Fonts exist in exactly two places: as attachments inside the mkv, or nowhere.

**Embedded tracks take the demuxer's own subtitle pad.** It carries the attachments with it, needs no synthesis, and avoids feeding back our own reconstruction of the script. The extracted `.ass` the hub caches for an embedded track is for external native players (HUB-34) and is never re-read by our pipeline: routing it through `appsrc` would throw away attachments the demuxer was about to hand us, for nothing.

**A user's own `.ass` beside the media takes `appsrc`**, with the stream synthesised from the file — the inverse of what `subindex` does when composing one. It burns with system fonts, because that is all a standalone script ever had: it references fonts by name and relies on the host, exactly as any other player treats it. Nothing is lost that was ever there.

So the two paths are chosen by where the subtitles came from, and the one combination that must not happen — our extracted copy of an embedded track, fed back through `appsrc` — is the only one that would lose anything.

One case is left open deliberately: a sidecar `.ass` for a film whose fonts live in the mkv. Rare, and the mechanism is known if it ever matters — `assrender` exposes no font directory, collecting fonts only from GST_TAG_ATTACHMENT samples arriving as a tag event on `text_sink`, and such a sample must be shaped exactly as matroskademux shapes it: buffer = font bytes, caps = the font mimetype, **and an info structure carrying `filename`**. Without the filename the event still arrives and `handle_tags` still runs, but no font is registered and libass substitutes while everything reports success (verified 2026-08-02 by watching `gst_ass_render_handle_tag_sample` fire, which it does not until the filename is there).

*After a seek* the demuxer pad starts mid-stream, so a line already on screen at the seek point is missing until the next one. The same shape as the PGS gap HUB-32b answers with a pre-read timeline, but ASS lines last seconds rather than persisting, so the miss is brief and not worth pre-reading for. Now measured rather than assumed (`an_embedded_ass_track_burns_into_the_picture`): after a flushing seek to 1 s, matroskademux issues a segment starting at 1 s and then EOS on the subtitle pad — the block at t=0 lives in an earlier cluster and never arrives. The test asserts the gap, so if a future demuxer closes it the assertion fails and this paragraph goes with it.

Two ordering rules the wiring has to respect, both of which fail silently. The renderer's `text_sink` may only be published to the rendezvous **after** the encode chain is linked and running: pads must share a pipeline to link at all, and an `appsrc` linked to a not-yet-active pad pushes once, fails, and pauses its own streaming task — after which every `push_buffer` returns Ok into a task that will never run again, while `assrender` logs "rendering disabled, doing buffer passthrough" for every frame. And `wait-text` must be turned on (it is off by default), or the encode simply outruns the subtitles: a 50-frame clip reached EOS in 45 ms, before the text caps had landed.

**Image subtitles — bitmap streaming before burn-in (HUB-32b).** PGS/VobSub get a third, cheaper tier that precedes burn-in in the policy order: the hub decodes the stream server-side (RLE display sets → RGBA tiles with palette applied) and serves it as a timed bitmap track — PNG tiles plus a timing/position manifest fetched alongside the media, sourced through the same HUB-34 ladder. Clients declaring `graphics_overlay: true` (the web player does — it's the same canvas the ASS renderer uses) composite the display sets themselves: full visual fidelity, zero video decode/encode/blend on the server, so a PGS-subtitled direct-play or remux stays transcoder-free. Burn-in remains only for clients that can't composite an overlay.

**Burn-in (HUB-32b last resort).** For clients that declare no compositing, the image subtitle is composited into the picture by the encoder, which per the owner's policy call is fidelity-first: such a client always gets its subtitles, so the burn FORCES the video encode that carries them — negotiation vetoes direct play and copy alike when one is wanted. Compositing is `overlaycomposition` fed from a display-set timeline read up front through the container's own index (`subindex::extract_image_track` → `imagesubs` → `burnin::Timeline`), NOT from the demuxer's live subtitle pad. That distinction is the whole design: display sets are sparse, so a session that starts mid-set — every resume, every seek-restart — is fed nothing by a live pad until the next set arrives and the subtitle on screen simply vanishes for seconds (measured against mpv: present at 25.5 s played from zero, absent after a flushing seek to the same timestamp). A timeline knows what is on screen at any instant. The overlay sits after the tone map (subtitle white is already SDR; the PQ curve would crush it) and after the scaler (blit at output size). Two facts that only a pixel comparison surfaces: overlay rectangles take BGRA — RGBA silently yields a NULL rectangle and aborts from a non-unwinding FFI frame — and the canvas must be scaled UNIFORMLY by width, since it shares the picture's width but not always its cropped height (a 3840x1600 scope film with 1920x1080-authored subtitles; independent axis scaling squashed the text by a quarter, and a box fit shrinks it by the same). Positions that then fall outside the frame are clamped into it, which is what puts bottom-anchored dialogue on screen and what mpv does too. VobSub's canvas comes from the `size:` line of its `.idx` (CodecPrivate), which need not match the video.

**Where the display sets come from.** The index walk that yields them is disk-speed locally and round-trip-bound over the byte plane — measured at ~4 KB/s hub→mediahost→NAS, so a walk costing milliseconds on the host does not finish inside a session start at all. It therefore runs on the MEDIAHOST (`ExtractImageSubs` → `subindex::extract_image_track` → `ImageSubtitles`), which reads its own disk; the hub caches the raw blocks per (module, collection, path, track) and hands the file to whichever worker runs the encode — by path for a local worker, in `StartSession.burn_sets` for a dispatched one, which can no more walk the source index than the hub can. Extraction is **on demand at session start**, not at scan: it costs milliseconds per file, while pre-walking all ~1200 image-sub files would cost roughly 12 GB of cache (OPS-6 never evicts) for content that only a non-compositing client ever needs. Text subtitles already have both shapes — urgent on demand plus an idle pre-warm worklist — so pre-warming image sets on the same idle tier is the upgrade if first-play latency ever matters. A burn is only promised once the sets exist: if they do not arrive, negotiation re-plans with the tier withdrawn rather than encoding video that burns nothing, and the walk itself runs under a read budget so a session always starts.

**Three faults that only the real fleet exposed**, each of which the dev box hid: image tracks may carry Matroska per-track compression (this library's PGS is zlib), so payloads must be inflated before any decoder sees them — the text path had the same latent gap and now shares the fix; `overlaycomposition` only blends when downstream does *not* claim to support overlay metadata, and the VA encoder claims it and then drops it, so burn-in worked on NVENC and silently did nothing on silence — we now blend explicitly via `gst_video_overlay_composition_blend`, which needs nothing of the encoder; and a burned frame's own timestamp does not say where in the FILE it is, so the blend reads its position from the frame's SEGMENT instead (the same conversion the seek gate uses for `start.pos`). The seek gate rolls the whole chain to PAUSED with data from the top of the file before the flushing seek can happen, so the blender sees pre-seek frames stamped ~0 and post-seek frames stamped at the snapped keyframe on the same pad — a difference that reads as "timestamps are absolute on one box and rebased on another" and was diagnosed that way for a while. Deciding a time base once, from the first frame, therefore latched onto a preroll and put every subtitle a resume offset out of place: a 1 h resume into The Truman Show looked up 2 h and burned nothing at all, while playback from zero was frame-exact and looked like proof the code was right.

**Decoder rank calibration (OPS-9).** GStreamer ranks decide which decoder autoplugs, and a rank is a vendor's claim about its own element, not a measurement of this box. Two ways that goes wrong, and presence checks see neither. The first is a hardware decoder that is present, advertises the codec, works, and is catastrophically slow: on the J5005, measured through `doctor --calibrate`, `vah265dec` decodes the reference clip at **8 fps against `avdec_h265`'s 145**, and `vah264dec` at **9 against 341** — a 16x and a 38x inversion of what the ranks assert. The second is a decoder that is fast and sees less of the stream, of which `dtsdec` is the instance: libdca decodes only the lossy DTS core, so a scan autoplugging it files a DTS-HD MA 7.1 track as 5.1 (312 titles in this library did exactly that). Only the first class is measurable — the second is a fixed known-bad list, because timing finds nothing wrong with being quickly incorrect. The measurement names the candidate element EXPLICITLY rather than autoplugging, since the point is to time the element GStreamer would have chosen against the one it would not; it reuses the TC-1 benchmark harness (`bench.rs`) but needed a new asset, because those clips are h264 and the pathology is h265 — `ref-1080p.h265` is the same 24 frames re-encoded, so a decoder's two numbers describe the same pictures. Candidates are filtered to hardware elements by name prefix (`va`, `nv`, `v4l2`, `vt`, `qsv`, …), a heuristic with a stated ceiling: without it the check times software siblings against each other and reports the loser as a pathology. Because it is timed it is opt-in — `startup_checks` shares the same check list and a boot must not spend seconds decoding to warn nobody is reading. Codecs without a checked-in reference bitstream are simply not timed (OPS-9a); the tool says what it measured rather than warning an operator about work we have not done. Remediation is the half that makes the check worth having: `doctor --fix` writes the demotions into the box's own config through `toml_edit`, format-preserving, additive and idempotent, into both `[transcoder]` and `[mediahost]` because a decoder that decodes the wrong thing also files the wrong thing. It never removes an entry a human put there — the measurement is one box at one moment, and the asymmetry is stark: a spurious demotion costs some speed, a removed one silently files hundreds of files wrong again. The requirement exists because the DTS check already warned, in exactly the words that describe the bug, on a box nobody was reading the output of.

**Image subtitles — OCR text tier (HUB-32c).** Between bitmap and burn-in sits an OCR tier: the hub converts the image stream to a plain text track, for clients that can't composite (better than forcing a burn-in encode) and for constrained links (a text track is a few KB; even the bitmap tile track is orders of magnitude heavier — this is the tier you want on a high-latency remote session). In v1 the text-over-tiles choice is a user preference — selectable per session, remembered per user; an automatic bandwidth threshold waits for hub-side bandwidth measurement, which lands with the quality-ladder machinery. Pipeline (as built): the HUB-32b display-set cache is the input — the mediahost already walked the index, and `burnin::timeline_from_file` already decodes BOTH PGS and VobSub blocks to positioned RGBA bitmaps, so `subtile-ocr` (whose value is parsing `.idx/.sub`/`.sup` files we deliberately strip) is not used at all; that also removes its GPL-3.0 licensing consequence (NFR-8, amended). Per display set: binarize to black-on-white (ink = opaque ∧ bright, the shape of subtitle glyphs), upscale sub-40px bitmaps ×3, hand-rolled 8-bit BMP in memory → Tesseract via `leptess`, PSM 6 (a set is one subtitle of 1–3 uniform lines — measured on real 1080p and 2160p PGS tracks at conf 70–91, ~16 ms/set, a feature film in ~15–30 s). Identical adjacent sets merge into one cue (PGS re-issues screen states; zero-length re-issues are dropped). Result rides the downloaded-subtitles payload machinery with `provider: 'ocr'` — stored, served and selected like any text track, but owned by the exact physical source and evicted when that source or parent stream disappears; `kind: "ocr"` in the API. Generation: an idle sweep walks every image track in the library (one at a time, playback outranks it, failures stick for the hub run), with the per-track button on the item page as the urgent path (synchronous, ~15–30 s, cached; an inflight lock keeps sweep and button from double-generating); the language model comes from the track's tag via a 639-1/2 mapping probed against the installed models. NOTE the tag can lie — a real track tagged `en` carried Romanian, which OCRs readably under `eng` minus diacritics; that is a metadata defect, visible rather than a crash. Marked machine-derived throughout and regenerated if its physical parent is replaced. **VobSub sidecars** (`.idx`/`.sub` pairs, the DVD-rip era's external image subs) feed the same pipeline: the scanner reads the `.idx` (small text) and emits one sidecar entry per track inside it; the mediahost's extraction keys off the `.idx` extension and reproduces the exact shape a Matroska demux would yield (idx text as codec_private, bare SPUs as blocks — `kahawai-media::vobsub_file`, an idx parser plus a ~60-line MPEG-PS depacketizer), so the KBS1 cache, zstd, OCR and the sweep handle sidecars with no further changes. SPU stop-display commands give real durations. No session tap exists for a sidecar, so overlay and burn don't apply; OCR text is their serving path. Measured on the library's 42 real pairs: every idx entry assembled to a complete SPU. One systematic artifact handled: DVD fonts render capital I as a bare bar, so word-position '|' is corrected to 'I'.

Feature gating: cargo feature `ocr` on `kahawai-hub` (forwarded by the `kahawai` binary), **default-on**, gating the `leptess` dependency — `--no-default-features` builds have no Tesseract linkage for minimal deployments. Runtime, model presence is probed by asking Tesseract itself (a `LepTess::new` per model, cached — the one probe that cannot disagree with TESSDATA_PREFIX); `doctor` reports engine usability and the common models present; the API answers 501 with the reason on feature-off builds. Negotiation: a cached OCR text row flips the image stream's tier from Burn to `Ocr` — the forced video encode disappears and direct play comes back (the tier order bitmap → OCR text → burn, HUB-32c). Licensing (NFR-8, amended): all-MIT-side linkage; no copyleft consequence in any build.

**Dual audio.** Per-user, per-library preference `audio: original_subbed | dubbed(lang)` feeds default stream selection at negotiation time (HUB-33); the chosen default is overridable per session in the player as usual.

### 4.4 Client API (v1 sketch)

```
POST /api/v1/auth/token                     # login → access+refresh
POST /api/v1/auth/refresh                   # rotate one refresh family
POST /api/v1/auth/logout                    # bearer + refresh → revoke that family
GET  /api/v1/libraries
GET  /api/v1/items?library=&q=&sort=&limit=&offset=   # browse AND search; returns total
GET  /api/v1/up-next?library=&limit=&offset=          # next episode per series (same rows)
GET  /api/v1/items/{id}                     # as DISCOVERED: sources[] without StreamInfo
QUERY /api/v1/items/{id}                    # as NEGOTIATED: the above + streams + verdict
                                            # body: { profile?, audio_track?, video_track?,
                                            #         subtitle_track?, mode? }
GET  /api/v1/items/{id}/children            # seasons/episodes, album/tracks
GET  /api/v1/items/{id}/artwork?size=       # named size, resized + cached
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
POST /admin/v1/libraries/{id}/refresh       # fan out RequestScan to each member collection
POST /admin/v1/collections/{id}/refresh     # single collection (for UIs that enumerate them)
GET  /admin/v1/enrollments                  # pending CSRs (fingerprint, type, name, age)
POST /admin/v1/enrollments/approve          # body: { code }
GET  /admin/v1/satellites                   # enrolled modules + cert fingerprints + status
DELETE /admin/v1/satellites/{id}            # delete = allowlist removal + cascade (see §7.4)
```

`/playback/decisions` is side-effect-free and returns the full negotiation verdict (per-stream direct/remux/transcode + reasons) so clients can display "why is this transcoding".

**A session resource is owner-scoped, and absence answers 404.** Every user-facing route below `/api/v1/playback/sessions/{id}` — direct stream, playlist, segment, subtitle tap, seek, progress and end — crosses one ownership middleware after authentication. An absent id and another user's live id return the same `404` body, so the id is not a bearer capability or a session-enumeration oracle. Administrative session routes remain separately administrator-gated.

Sessions end for reasons a client cannot predict — idle reaping (HUB-18), a hub restart, `end_for_user`, a module going away, an admin ending them. A `404` from a session resource therefore tells its owner to start a new session at the current position. This deliberately spends the old distinction between a dead session and a missing subordinate artifact: tenant isolation takes precedence, and generated artifact URLs should exist for the life of a healthy session. The web player detects the response on whichever comes first: the 10-second progress ping, an hls.js fragment/playlist error carrying `response.code`, or a probe after a media-element error, since the element exposes no status of its own. Recovery is automatic and bounded: a restart at a position the previous restart already tried is refused and surfaced as an error, because two attempts at the same position mean the first never played, and retrying forever would spend a user's whole concurrent-session budget on a fault that is not going to clear.

**The method carries the question** (RFC 10008). `GET /items/{id}` answers *what did we find* — the item, its sources, its metadata. `QUERY /items/{id}` answers *what would you get*, taking a whole `CapabilityProfile` in the request body and returning the same body plus per-source `StreamInfo` and a `negotiated` block: the source it judged, the mode, the cost, the per-stream verdicts, and the subtitle track list with each track's delivery. The library browser uses GET; the item viewer uses QUERY. The split exists because the old shape asked the same question two ways and got two answers: a separate `GET /items/{id}/subtitles` computed each track's delivery from two booleans in a query string, resolved *its own* source by size while negotiation resolved one by cost, and so could promise `burn` to a client that would refuse the video encode carrying it. One negotiation now answers both halves, over the source it actually chose, so they cannot disagree. That endpoint is deleted, and `sources[].streams` is gone from GET — "what is in the file" is only ever an answer to a question about playing it.

QUERY is **safe and idempotent, and returns only what is knowable now**: it starts no extraction, generates no raster, opens no lease and claims no transcoder, so no session is ever slower because someone asked a question about it. Tiers gated on an artefact report the artefact that already exists — the overlay rung only where a raster row is already there — which under-promises on first play rather than over-promising the expensive tier. `Accept-Query: application/json` advertises it, a missing or inconsistent `Content-Type` is refused per the RFC, and an unsupported method answers 405 with `Allow: GET, QUERY`.

**Generated contract.** The public listener serves the code-first OpenAPI 3.2
document at `/api-docs/openapi.json` and a vendored Swagger UI at
`/swagger-ui`. The document has 62 distinct method/path operations for all 63
application bindings: the public and trusted-local listeners share
`GET /api/v1/bootstrap`. The SPA catch-all and the Swagger/document-serving
routes remain mounted infrastructure, not self-described application
operations.

Every handler request and JSON response is a concrete Rust model. Producer-owned
wire values stay owned by their domain modules — registry overviews and events,
health, grants, enrichment candidates, subtitle tracks and negotiation verdicts
— rather than being copied into API-local JSON assembly. `ToSchema` on those
same types supplies every nested component. Required nullable fields and
`serde`-omitted fields are marked separately, preserving the deployed
null-versus-absent distinction. The operations also declare path/query/header
parameters, response statuses and content types, binary/streaming headers, and
their JWT bearer, media query-token or static metrics-token boundary.

`utoipa` and `utoipa-swagger-ui` are pinned together at commit
`e092565a9724b07a5ebf122e80ffa3d70addbe5d`, after its OpenAPI 3.2 model and
`version = "3.2.0"` derive support landed but before the 6.0 release. The model
has `PathItem.query`; the path macro still has no QUERY verb. Until it does,
the real handler is described through the macro's POST arm and
`openapi_document` moves that generated operation from `post` to `query`.
`api::tests::openapi_covers_exact_application_surface_with_typed_bodies` fails
closed on the exact 62-operation set, 3.2/QUERY placement, typed JSON bodies,
security schemes and nullable/omitted schema boundaries.
`admin_api::admin_flow_enrollments_satellites_archive_restore` then fetches the
served document and proves every documented protected method/path reaches a
mounted authentication boundary rather than the SPA fallback. The vendored
Swagger assets keep builds and rendering independent of a CDN.

`web/openapi.json` is the one checked-in generated contract. `npm run
api:export` runs the `kahawai-hub` `export_openapi` example with web building
disabled, stamps its temporary output with a SHA-256 of the Rust contract
inputs, and asks Orval 8.24.0 to parse it before atomically replacing the
committed file. No running hub is required. A working-tree fingerprint check
runs before development, tests, typechecking and every web build; the
repository's pre-commit hook checks the staged blobs instead, so partial commits
cannot pair sources with the wrong document. Install that hook with `git config
core.hooksPath .githooks`. The Rust
`checked_in_openapi_matches_generated_document` test is the definitive semantic
comparison and catches an incomplete fingerprint manifest.

Orval regenerates the ignored fetch client and models under
`web/src/generated` from the committed JSON during `npm install` and before
each web entry point. Every ordinary web API operation now calls those
generated bindings. `api.ts` remains the application-behaviour facade for
token rotation, preference write ordering, capability refinement, timeouts
and view-model narrowing; it no longer owns HTTP methods, application route
strings or JSON serialization. The custom mutator in `api-client.ts` is the
single authenticated transport: bearer injection, one refresh-and-retry,
typed `ApiError`, empty/JSON/text/binary decoding, and raw `Response` access
for the progress ping whose 404 drives session recovery. EventSource,
streaming subtitle readers and media elements still use their native browser
transports, but take application URLs from generated URL builders or
server-returned session URLs. The post-1.0 compatibility baseline remains
ENG-6 work.

*This breaks v1 in place, against NFR-7* ("breaking changes only in a new major API version"). Deliberate, with the maintainer's sanction: there are no external clients yet, and carrying a `/api/v2` for a pre-release keyspace costs more than it protects. NFR-7 governs from the first outside consumer.

**Every browse page is a deferred join.** An inner query chooses WHICH ≤200 ids make the page using only indexed scalar columns — the membership covering index for a library browse, the sort index for search and unscoped — and the resolved-metadata view, watch state and source counts join onto those ids afterwards. Joining first and paging second resolves the view for every candidate the sort visits, which is the recurring 900 ms failure mode whenever an ORDER BY stops matching an index. A search page streams the sort index and stops early; when it underfills, the scan saw everything, so the total is known without a counting pass — only a full page pays one.

**Browse and search are one endpoint** (HUB-12). Omitting `library` searches every library, which is what makes cross-library search a parameter rather than a second route; `q` matches the folded filename and the resolved title, so an item is found by what it is called now as well as by what it is called on disk. `sort` is a name (`title`, `-title`, `year`, `-year`, `added`, `-added`) mapped to a fixed ORDER BY — never interpolated from the request — and each name resolves to `items.sort_title`/`items.year` only, both carried by one index, so a page is a range scan. `added` needs no column: item ids are ULIDs and sort by mint time. The response carries `total`, `limit` and `offset` so a client can size the whole result set before fetching it.

**Artwork sizes.** `?size=` names one of a fixed list in code (`thumb` 96 px, `card` 480 px, longest edge), resized on first request and cached thereafter. Names rather than free-form `w=`/`h=`: a client that can ask for any width can mint unbounded cache entries. An unknown name serves the original rather than failing, so retiring a size cannot break a page already open.

### 4.5 Capability negotiation (`kahawai-core::negotiate`)

**Capability is the architecture's spine, not a transcoder detail (AR-13).** The client declares its probed profile (HUB-14), every full transcoder declares its dry-run-verified inventory (TC-1), and topology determines the hub worker's deliberately narrower role. Plain hub capability is structural: remux and audio-only transcode while copying video. It is not treated as a full transcoder and declares no video encoder, tone-map or burn capability. AIO's enabled in-process transcoder is measured on the same terms as an external one. Everything below was learned by getting one of them wrong.

*What a declaration must be.* **Measured, never assumed** — encoders are dry-run-verified because a box can own `vah264enc` and a broken driver, and the GL tone-map segment is dry-run as a whole pipeline because element presence does not prove a headless box can open a GL context. **Decision-bearing, or it is a protocol version** — a `burnin: bool` reported by every build that has the code is not a capability but a version marker, and version belongs in the AR-7 handshake where an incompatible peer is refused once and loudly; that flag was deleted and `PROTOCOL_MAJOR` bumped to 2 instead. **Expressed in the terms of the decision** — the tone-map boolean says "yes" for a box that sustains 1.7× realtime at 1080p and 0.65× at 2160p (measured: the GL segment alone on a J5005, with the upload/download round trip costing more than the shader), so it answers "can you" when placement needed "can you keep up"; that is HUB-36's gap. **Honest in absence** — a missing capability re-plans *before* the work starts and the verdict says what was actually done, because the alternative is what burn-in shipped for one afternoon: a plan that claimed a burn, forced a video encode for it, and produced no subtitles.

*What is genuinely per-peer* — and therefore worth declaring — has a pattern: it is what the peer's **hardware, drivers, or position in the topology** decide, never what its software version decides. Encoders and decoders differ per box (`nvh264enc` / `vah264enc` / `vtenc_h264_hw`, and a box that decodes AV1 at 0.3× realtime should not be sent AV1). Locality differs per topology: a sparse index walk is milliseconds on the mediahost's own disk and ~4 KB/s through the hub's byte plane, which is the difference between a feature and a hang, and is why image display sets are extracted host-side (HUB-32b). Behaviour differs per client in ways no feature test exposes — Firefox decodes HEVC and renders PQ untouched, so `hdr` means "will display HDR acceptably", a claim the client makes about itself rather than a codec it lists.

*Falsifiability.* Any client declaration can be masked from the player (HUB-14), which is the only practical way to reach the branches a real browser never takes — and the mask found a live defect within minutes of existing (a stereo channel ceiling that delivered mono). A capability nobody can force is a capability nobody tests.

*Honest degradation has a channel.* The verdict is computed before the pipeline runs, so anything the pipeline learns afterwards would die in the worker's log — which is exactly how a DTS-HD 7.1 track shipped as an undecodable stream with every log green. Workers therefore report **session facts**: one JSONL line per fact (`kahawai-media::facts`) in the run directory, written the moment a pipeline callback learns something the plan did not know (the AAC layout pin folding 7.1 → 5.1, or finding no encodable layout at all). The supervisor — transcoder and hub run the same worker, so both read the same file — collects them when the playlist goes ready: the transcoder attaches them to `SessionReady` (protocol 2.2), the hub folds them into the per-kind verdict, and the client's `streams.audio` reads `dts → aac (transcoded) · 7.1 → 5.1`. Folding is idempotent because a seek-restart re-learns the same facts. The same Hello that gates the protocol now carries a **build stamp** (commit + date, stamped by `kahawai-core`'s build script), logged at `satellite connected` and surfaced in `/admin/v1/satellites` — "which build is that box running?" was, for one whole evening, answerable only by ssh.

Pure function, exhaustively unit-tested:

```rust
fn negotiate(source: &SourceStreams, cap: &CapabilityProfile, policy: &Policy)
    -> PlayPlan // { container: Keep|Remux(fmt), video: Copy|Encode(spec),
                //   audio: Copy|Encode(spec),
                //   subs: Passthrough|ClientRender|BitmapStream|Ocr|Convert|Flatten|Burn|Drop,
                //   ladder: Vec<LadderStep>, reasons: Vec<Reason> }
```

Decision order per HUB-16: try full direct play; else keep every stream that fits and remux; else encode only failing streams. **Across sources, completeness outranks cost**: dropping a stream is not a cheaper plan, it is a worse one, so a source whose audio this client cannot be given loses to a source that delivers everything even when that costs a full video encode. It is the same rule `negotiate` already applies choosing TS vs fMP4 within one source, lifted to the source loop. The distinction that makes it safe is DROPPED versus ABSENT: a music file has no video row to lose and a video-only rip is not silent, so neither is penalised for a stream it never had. Observed before the rule existed: a client declaring hdr:false against a title with an HDR mkv and an SDR mp4 took the SDR file at `copy` and played it SILENTLY, because its eac3 had no target this client accepted, while the mkv would have delivered ac3 for the price of a tone-mapped encode. Rules include: profile/level comparison for H.264/HEVC/AV1; HDR→SDR tone-map (HUB-15a), delivered as a single GL shader segment spliced into the video ENCODE chain: `glupload ! glcolorconvert ! RGBA ! glshader ! glcolorconvert ! gldownload ! NV12-capsfilter ! capssetter(colorimetry=bt709)`. The fragment shader (kahawai-media/src/tonemap.frag) does PQ EOTF → exposure at 203-nit reference white → extended Reinhard at a fixed 1000-nit mastering peak → BT.2020→709 gamut matrix → 709 OETF; the NV12 pin is load-bearing (a VA encoder with no converter after the segment refuses system-memory RGBA — observed not-negotiated on the J5005); the capssetter rewrites the colorimetry tag so the encoder's VUI matches the rewritten pixels. Element research that led here (2026-07): `vapostproc hdr-tone-mapping` exposes its property everywhere but the driver only implements it on TGL+ (silent no-op on GLK, proven pixel-identical); no libplacebo GStreamer element exists; `videoconvert gamma-mode=remap` has no tone operator and crushes PQ to black. Capability is DRY-RUN verified per box (the real segment against videotestsrc — element presence does not prove a headless box can open a GL context), reported via TC-1 (`CapabilityReport.tonemap`), surfaced by the doctor (`hdr tone-map` row), and preferred by placement (`PlacementNeed.needs_tonemap`). Text delivery has a declaration of its own: `profile.vtt_render`. Everything textual reaches a client as WebVTT and nothing else — a `<track>` accepts no other format and the live cue tap feeds the same TextTrack renderer — so one bit covers converted SRT, flattened ASS and HUB-32c OCR output alike. False removes `Flatten` from the ASS ladder and sends plain text to burn-in, the same last resort an image track takes when the client cannot composite; with no burn-capable box the answer is `none`, said out loud rather than promised and dropped. It is true for every browser, which is the point: HUB-14's rule is that a capability nobody can force is a capability nobody tests, and until it existed the SRT→burn path was unreachable from the mask. It also ends `AssPolicy::choose`'s totality — `Flatten` was 'always possible' only because text always rendered — so it returns an `Option` now, with the `None` mapping to the refusal `ass_burn_unavailable` already produced. The decision arm: `profile.hdr` means "this browser DISPLAYS HDR acceptably" — Chrome/Safari tone-map PQ in their own compositor even on SDR screens and declare true; Firefox decodes HEVC but renders PQ untouched (washed out, observed live) and declares false. An hdr10 source + `hdr:false` + a tone-map-capable executor vetoes copy/direct and forces the tone-mapped encode; without a capable box the copy stands (washed beats washed-plus-generation-loss) with the as-is verdict. HLG is never mapped or vetoed (SDR-compatible by design). Via HUB-16 source preference, an SDR source of the same title now beats an HDR source for hdr:false clients automatically. The HUB-15a quality bar closed 2026-07-29: a 10-title matrix (animation, night scenes, daylight, grain, DoVi-base) fitted and verified against what mpv actually DISPLAYS — vo=gpu/libplacebo window captures via IPC `screenshot-to-file` — at per-title percentile-curve RMS ≤ 0.006 (9/10 titles; joint-fit loss 0.0025), and the owner-flagged live scene (Duplicity 2m16) matches the mpv window within 0.01 mean signal. Measurement trap for posterity: mpv's `--vo=image` screenshots (BOTH formats) run zimg's software tone mapper, a different and brighter renderer than the libplacebo playback path — never a reference; an entire calibration round chased it before live playback exposed the gap. Further rules: channel downmix when layout unsupported; text subs converted SRT↔WebVTT with `subparse`; ASS handled per the `ass_fallback` policy (§4.3b: `ClientRender` when the client declares it, else `Burn` or `Flatten` — and after a `Flatten` substitution the plan is re-evaluated, since removing the burn-in often demotes a video encode back to remux/direct); PGS/VOBSUB → `BitmapStream` when the client declares `graphics_overlay` (server-side decode to a timed bitmap track, no video work, §4.3b), `Ocr` when the tier is available and preferred (client can't composite, or the user's per-session/remembered preference selects text over tiles — an automatic bandwidth threshold is deferred until the hub measures client bandwidth, alongside the quality-ladder machinery), else burn-in — piggybacked when the plan already encodes video, otherwise forcing a video encode only as the true last resort; bandwidth cap forces a ladder whose top rung ≤ cap.

**Placement on measured throughput (HUB-36).** The tone-map boolean above is the archetype of a declaration that answers "can you" where placement needed "can you keep up", so capability now carries a rate at three levels, and each exists because the one above it cannot see something.

*Benchmarks* answer "what is this hardware". Every verified encoder and the GL tone-map segment are timed against an **embedded 24 fps reference clip** decoded through the real converter chain, not a synthetic pattern: noise measured its generator and SMPTE bars are nearly free for software encoders. Results cache by `gst::version()`. Only successful current-fingerprint measurements become serving capabilities; a cache miss connects idle and missing jobs fill in from isolated children after startup. Each element runs in its own CHILD process, while the tone-map child measures no encoder, so `svtav1enc` SIGSEGV on the J5005 costs one capability rather than the transcoder process.

A parent-observed child crash or timeout is a durable **quarantine**, not a timer. Elapsed time is no evidence that a driver repaired itself, so quarantined work is neither advertised nor retried automatically. A child that exits normally with an incomplete measurement leaves the job missing and retryable; transient pipeline setup failure is not evidence of a process crash. The cache-semantics version that first enforced this retries each quarantine written by the earlier mixed-provenance format once; a genuine crash is isolated and recorded authoritatively again by the parent. Recovery from quarantine has two explicit proofs: a changed GStreamer fingerprint invalidates the complete cache, or an operator stops the module, runs its `benchmark` subcommand against that module's `benchmarks.json` (`--only <element>` or `--tonemap`), and restarts it. The successful isolated run writes the measurement and clears quarantine; service restart, power loss, and child-launch failure write nothing.

*Observed pace* answers "what does this box do with THIS work", which a benchmark cannot: source decode cost is invisible to an encoder measurement (software AV1 decode is the case that motivated carrying the source codec in the class key). The trap is that steady-state production is deliberately throttled to viewer+120 s, so any measurement of it reads ≈1.0× however fast the box is. Workers therefore meter **only the un-throttled phase** — from the first buffer until the pace probe's window check first fails, capped at 60 s — and discard samples shorter than 5 s of wall or content, which are preroll burst rather than throughput. One sample per run lands in `pace.json`; the transcoder's supervision poll takes it (renaming it, so the file's absence is the "taken" flag that survives a seek-restart replacing the watcher) and ships it on the existing heartbeat. The hub folds it into `transcoder_pace` as an EWMA at α=0.3 per `(module_id, work_class)`, where the class is `{res}|{src}|{dst}[|tm]` — schema meaning in the `hub/pace.rs` module doc. Samples carry only a session id: the hub derives the class where the plan and MediaInfo are both in scope, so the measurement and the thing measured can never disagree.

*Link rate* answers "can the bytes even arrive". Reads ≥1 MiB through the lease bridge fold into an EWMA (α=0.2); smaller reads measure round-trip latency, not bandwidth. It is deliberately in-memory and cleared on disconnect — a rate describes one connection over one network, and a persisted stale one lies confidently.

*The scorer* (`Registry::place`) keeps every hard filter — a box that cannot decode the source or encode the target is not a candidate at any speed — and changes only the order among those that can. Observed pace wins outright when present and is never blended with the benchmark, because a real run already contains the decode, the tone-map, the encode AND that box's link stalls; blending would count the same cost twice. Unobserved work falls back to the components, and the SLOWEST governs (encoder, tone-map when planned, link bytes against source bitrate). Nothing measured at all yields `None`, which ranks as CAPABLE: refusing work for want of evidence is how a fleet never earns any. Rank is `sustains(≥1.2×)` → tone-map fit → hardware → prediction → load, where 1.2 rather than 1.0 because a box that exactly matches realtime stalls the moment anything else happens on it. Audio-only encode stays in the hub because it is lightweight AR-10 work. For video encode, the fleet still wins by default (§4.5 policy: hub cores serve clients); only an enabled AIO full transcoder may compete locally, and work repatriates to it only when no fleet box sustains and it does. A placement predicting below realtime is placed anyway — refusing would strand a slow fleet — but never silently: the verdict gains `predicted 0.7× realtime — may stall` through the same facts channel as the 7.1→5.1 fold.

**Session diagnostics (OPS-10).** A bundle is assembled per session and stored under `<data_dir>/session-logs/{unix}-{item}-{session}.log`, newest 40 kept. The item id rides in the FILENAME because sessions are ephemeral and leave no row behind — that is what makes "the last session for this item" a directory glob rather than a schema change.

*Where it is captured is forced by teardown.* The satellite's `Runner::end` deletes the run dir synchronously the moment the worker exits, and the hub's `EndSession` is fire-and-forget, so there is no later moment to ask: the bundle is gathered inside `end()` before `remove_dir_all` and pushed as `SessionLogs`. The hub's own local worker does the same in `Sessions::end` before its own wipe. A live session can also be asked (`CollectLogs` → `SessionLogs`), which is what the download button does while a problem is on screen.

*The hub half is structured state, not log lines* — item, user, mode, plan, verdict, placed box, work class, sink. Not a stylistic choice: the hub cannot read its own log, which goes to stdout and is redirected by whatever launched it (a shell redirect under `kahawai-restart.sh`, discarded entirely by launchd on macOS). The same reason excludes the transcoder's own log, which does not exist as a file on macOS at all.

*One directory, whatever went wrong* (`<data_dir>/session-logs/`). There is deliberately no separate crash store: whoever opens a log does not yet know whether the session failed, hung, or was fine, so a split would force that decision first. Retention is a number to raise; two folders is a tax on every investigation. The session id is minted before any work that can fail, so an early bail — "no source is currently available" — still files a log under its item instead of vanishing into a 409.

*The cut keeps head AND tail.* A panic's message is at the end; a hang's evidence is at the start — the plan, the caps negotiation, which encoder was chosen. Measured bundles run ~27 KB against a 256 KB cap (the noisiest real session: 82 subtitle tracks, tone-map, E-AC-3; `worker.log` was 21,803 bytes and FLAT over three minutes, because worker logging is entirely front-loaded), so the cut only ever fires on a warning storm, where both ends beat one.

*One line earns its place disproportionately*: whether `segment00000` carries SPS, PPS and an IDR. That single fact separates "the pipeline is healthy and the player is wedged on an undecodable first segment" from every other failure, and it was the whole diagnosis of two distinct bugs. It costs ~40 bytes against a 400 KB segment, and is stated as unavailable rather than silently omitted for non-TS pipelines.

### 4.6 Session manager

State machine per session: `Negotiated → Provisioning → Streaming → (Seeking|SwitchingQuality)* → Ended`. Direct play sessions hold an `OpenRead` lease against the mediahost and proxy ranges with `Accept-Ranges`/`206`.

**Lightweight sessions run entirely inside the hub** — this is why `kahawai-hub` depends on `kahawai-media`. For pure remux, the hub feeds the mediahost byte stream through a local demux-only pipeline (`appsrc ! parsebin ! <selected streams, no decode> ! cmafmux → hlssink3`-style segmenting). The same supervised pipeline may copy video while decoding and encoding audio for codec conversion or downmix. Both are cheap relative to video decode/filter/encode, need no placement, work with zero transcoders attached (AR-10), and keep full transcoders free for video work. Seek = pipeline restart at the target keyframe, same as §6.

**Measured audio loudness normalisation (HUB-38).** A scalar native LUFS/true-peak pair cannot predict a downmix: output energy contains cross-channel correlation terms, post-matrix true peak depends on sample phase, and EBU relative gating must be rerun over the converted blocks. The owning mediahost therefore decodes each non-music stream once and tees it into bounded `audioconvert` + `ebur128` histogram branches for the untouched decoded layout and every smaller canonical layout playback may emit (7.1 variants, 5.1, stereo, mono). Measurements are keyed by exact `(channels, channel-mask)`, revision-guarded on the hub, target −18 LUFS, and cap measured true peak at −1 dBTP. Playback waits for the worker's actual post-conversion caps and selects only the matching static gain; it never derives one layout from another.

The global preference has three states: the empty/default value applies gain only when negotiation already encodes audio, `off` suppresses it, and `force` asks negotiation to replace measured direct/copied audio with an encode. Force is admitted only for a single-part source, an exact measured output layout, a compatible audio encoder, and an unchanged video mode. The hub preflights its local AAC/Opus layout before replacing direct/copy; an unsupported layout retains the ordinary plan rather than paying for a no-op transcode. Protocol 4.0 includes exact per-layout loudness maps as baseline wire state: an executor may fold a nominal 7.1 source to 5.1, so source channel count cannot prove which scalar will apply. Default normalization re-probes against an exact-gain-capable worker but accepts the result only when the complete video path (codec, container, filters and subtitle burn) is unchanged. If no such worker can run the ordinary encode—or final ASS/capacity placement races the probe—placement retries the original plan without optional gain, so playback remains usable without normalization. Force likewise falls back to the ordinary plan rather than paying for a no-op encode. Explicit `mode=direct` remains original bytes.

Protocol thresholds live in `kahawai_proto::ProtocolFeature`; every feature inherited by the breaking 4.0 cutover has minimum minor zero, while future additive features can acquire a later threshold. Planners and registries carry a typed required feature rather than comparing minor numbers. The exact-gain requirement is repeated on the final `PlacementNeed`, not only the earlier capability probe, because load/pace may choose a different box between planning and reservation. Track switches cannot move an existing session, so they likewise suppress unsupported normalization and undo a force-only encode. Scalar wire fields use protobuf presence, and explicit NaN/zero sentinels preserve absence through the worker argv boundary.

Rebuild cost is one source-local full audio decode plus one conversion/meter branch per bounded output layout. That is deliberately more CPU than the former native+stereo pair, paid once in background, because retaining enough covariance and oversampled phase history to derive arbitrary LUFS and true peak would approach decoded-signal storage. Point-of-use cost remains one indexed lookup and one fixed `volume` multiplier; no rolling normalizer changes programme dynamics.

**`EXT-X-TARGETDURATION` and what it is really about.** The spec defines it as one thing — "the maximum Media Segment duration", and every segment's EXTINF rounded to nearest must be ≤ it (RFC 8216 §4.3.3.1). It says nothing about keyframes: §6.2.1 only says a server *SHOULD* divide "on packet and key frame boundaries", and §3 explicitly allows a segment whose leading frames are "downloaded but possibly discarded". Segment length is the server's choice.

Keyframes enter through OUR TOOLING, not the format. `splitmuxsink`/`hlssink3` and `isofmp4mux` all close a fragment at the first keyframe at-or-past the fragment target, and a stream copy has no encoder to request keyframes from — so on a copy the source's keyframe spacing sets segment length, and the honest declaration becomes `fragment target + max keyframe gap`. That gap is measured from the container index at scan (`subindex::max_keyframe_interval_ms`, MKV Cues / MP4 `stss` / AVI `idx1`; kilobytes per file, no decoding) and stored per video stream. Measured across this library: median 10.0 s, worst 147 s, and the previously hardcoded 2 s was wrong for 87% of files. **A segmenter that cut on a fixed time grid would make the declaration a constant of our choosing and delete this entire mechanism** — no shipped GStreamer segmenter does, which is the only reason it exists.

Because the right value differs per client, the client states which of three things it needs (`CapabilityProfile.target_duration`, required, no default): `ignore` keeps the old constant and its violation (hls.js does not check), `accurate` declares the measured truth, `short { max_secs }` guarantees a ceiling and forces a video encode when the source's keyframes are too far apart to cut inside it. The value is fixed for the session's life (§6.2.1 forbids it changing) and applied where the playlist is SERVED, because the sink's own property is the fragment interval it cuts on and raising that would just produce longer fragments.

**What this does NOT fix, and the mistake worth recording.** It was prompted by an ExoPlayer hang, and it does not address it. That hang is a LIVENESS failure: the pacer holds production at `viewer + 120 s` (`worker.rs`), so once the pipeline runs ahead the playlist stops changing — measured at 42 s for a copy and 47 s for a transcode with no progress pings — and ExoPlayer raises `PlaylistStuckException` after `3.5 × targetDuration` of *no change* (`DefaultHlsPlaylistTracker`, current androidx/media). Declaring 12 instead of 2 lifts that threshold from 7 s to 42 s, which clears a normal 10 s ping cadence — a wider margin, not a fix. (An earlier draft claimed a paused player trips it unconditionally. It does not: the exception surfaces through the loading path, so a player sitting on a full buffer never asks. Source-validated, 2026-08-05. The exposure is a *playing* client whose pacer-stalled playlist stops changing for longer than the threshold — bounded by how fast the viewer drains the window, and measured at 42 s and 47 s, i.e. right at it.) The live model requires continuous appends and the pacer exists to stop appending; the resolution taken is to make the pacer release on playlist age as well as viewer position, so a stalled playlist is refreshed at about one segment per target duration whatever the viewer reports. A VOD playlist (complete segment list + `EXT-X-ENDLIST`, so idleness is legal) would remove the contradiction outright rather than bound it, but with the paused case withdrawn nothing observed requires it: `kahawai-vod-plan.md` records what it would take, buy and cost, and why it is not scheduled.

Sessions with video marked `Encode` are placed on a full transcoder by a scorer (capability fit ≥ hw-accel ≥ inverse load); audio-only encode with video copied remains in the hub. Dispatched sessions are monitored via `Progress`; on transcoder loss the spec is re-issued to the next candidate with `start_offset = last served segment` (AR-6). If no full transcoder is connected, plans requiring video `Encode` fail fast at `/playback/decisions` with a distinct reason so clients can fall back (e.g., pick a lower-quality source or disable the offending subtitle) rather than time out. Idle timeout 90 s without segment fetch or progress ping → teardown. Concurrency limits enforced per user (HUB-18).

### 4.7 Embedded web UI (HUB-25..28)

**Serving.** `vite build` output is embedded with `rust-embed` and served by an axum fallback route: `/app/*` → SPA `index.html` (client-side routing), hashed assets with immutable cache headers, `/` redirects to `/app`. `web/dist` is generated and ignored, never committed: web CI proves a clean checkout builds it, while every release/container path runs the pinned Node build before Cargo and sets `KAHAWAI_REQUIRE_WEB=1` so an accidentally UI-less artifact fails. Ordinary Rust-only checks and satellite builds need no Node installation and may compile with an empty asset set. A `--dev-web-proxy` flag proxies to the Vite dev server for frontend development against a live hub. The SPA authenticates with the same JWT flow as any client and calls only `/api/v1` and `/admin/v1` — no private endpoints (HUB-28); admin routes render only for users whose token carries the admin role, but authorization is enforced server-side as usual.

**Capability profile.** On startup the player probes the browser honestly rather than shipping a static profile: `MediaSource.isTypeSupported()` / `mediaCapabilities.decodingInfo()` across the codec matrix (H.264 profiles/levels, HEVC, AV1, AAC/AC-3/Opus/FLAC), container support (fMP4 via MSE; native HLS on Safari), HDR via `matchMedia('(dynamic-range: high)')` + codec profile support, and screen dimensions — serialized into the `CapabilityProfile` sent to `/playback/decisions`. This makes the web player the reference implementation of negotiation from the client side.

**Capability debug mask.** The negotiation matrix and the subtitle tiers have branches most browsers never take — no HEVC decode, no HDR display, no ASS renderer, no display-set compositor — and hunting for a browser that genuinely lacks each one is slow and unrepeatable. So the player can SUBTRACT from its own probe: a mask (`localStorage`, edited from a panel next to the playback-info verdict) is applied at the single choke point where the profile is built, after the source-aware refinements so a precise cap cannot smuggle back a family the mask dropped. What it changes is not cosmetic — the same masked answer drives the player's own rendering (`ass_render: false` really takes the flattened-VTT path instead of JASSUB; `graphics_overlay: false` really asks the hub to withhold image subtitles), so a masked client behaves like the real thing rather than merely reporting different verdict text. Codec and container entries can only be dropped, since claiming a decoder the browser lacks would produce a stream it cannot play; the three declaration booleans may go either way, because they are claims rather than probes. A mask only reaches the hub on a NEW session (the hub stores the effective profile per session and re-plans track switches against it), so applying one restarts playback at the current position, and the active mask is always printed beside the verdict — a forgotten mask must never read as a bug in the hub. The panel also copies the effective profile as JSON for `kahawai-play.sh -P` and `kahawai-sweep --profile`, so a browser-side finding reproduces headlessly across the whole library. Its first catch was the HUB-15 channel ceiling: `channels=[1,2]` range caps fixate to their minimum, so every client declaring a stereo limit had been receiving mono.

**Video playback.** Direct play binds the range endpoint straight to `<video src>` (browsers do range requests natively); remux/transcode plans load the session's `master.m3u8` via `hls.js` (MSE) with native HLS fallback on Safari. Seek beyond the transcoded window and ladder switches go through the session endpoints from §4.6. Text subtitles attach as WebVTT `<track>` elements (hub converts on demand); ASS/SSA streams render client-side via JASSUB (libass compiled to WASM) on a canvas overlay, loading the item's served font set; PGS/VobSub arrive as the server-decoded bitmap track (§4.3b) composited on the same canvas — the player accordingly declares both `ass_render: true` and `graphics_overlay: true` in its capability profile; burned-in subtitles arrive inside the video and the UI marks them as such from the negotiation verdict, which is also surfaced in a "playback info" overlay (direct/remux/transcode + per-stream reasons). Progress posts every 10 s and on pause/unload.

**Browsing.** One search box in the header, whose meaning follows the screen: on the home screen it queries every library at once and shows at most five hits each, listing only libraries that have any; clicking a library's name follows those results into it with the query still standing, where the same box becomes that library's filter. The box is rendered only on those two screens — on the player or admin pages it would silently do nothing.

A library grid reserves the full height of the result set from the first response and fetches 100-item chunks as rows scroll into view, so only the visible rows exist in the DOM (25–44 cells for a library of 881) and the scrollbar never moves under the thumb — the property that separates this from infinite scroll, where the page grows as you go. Row height and column count are measured from the DOM rather than copied from the CSS, because the card art is `aspect-ratio: 1` on a fluid grid track and both are therefore functions of window width. Cards are a fixed height (titles clamped to two lines) since an exact reservation is impossible over variable rows, and the placeholder for a row that has not arrived is structurally identical to a loaded card so nothing shifts when a chunk lands.

**Music playback.** A persistent queue over `<audio>` with preloading of the next track via a second element swapped at track boundary (near-gapless; true gapless via Web Audio API is post-MVP), plus album/artist views. Delivery is **direct, always**: the player asks for `mode: "direct"` explicitly rather than sending a capability profile, so a track binds to `<audio src>` as a byte-range URL and no pipeline is ever built. That is not a stopgap around a missing tier — it is the whole of HUB-19, and browsers decode every container a library realistically holds. Both elements — the playing one and the warmed one — hold their own session and post progress every 10 s, since an unpinged session is idle-reaped out from under the preload.

**Admin UI.** Thin CRUD over `/admin/v1` plus the `/api/v1/events` WebSocket: the enrollments page updates live as CSRs arrive (approve-by-code inline), satellites page shows fingerprints/status with delete-and-cascade confirmation spelling out consequences (HUB-20), a drag-to-compose library builder over announced collections — each library carries a **Refresh** action that fans `RequestScan` out to its member collections and shows live per-collection `ScanProgress` aggregated on the library row (HUB-35; the old global refresh-all button is gone, and refreshing an already-scanning collection joins the running scan instead of stacking another). A per-collection refresh exists in the API (`POST /admin/v1/collections/{id}/refresh`) and appears as a row action wherever the UI enumerates individual collections. Also: the manual-match review queue with provider candidate side-by-side, subtitle/enrichment provider settings, user/grant management, and a sessions dashboard streaming per-session state and throughput.

### 4.9 Segment detection (HUB-37)

**What it finds.** Three boundaries per episode: the recap ("previously on"), the opening, and the end credits. `kahawai-intro` is a port of the Jellyfin plugin [intro-skipper](https://github.com/intro-skipper/intro-skipper) onto Kahawai's own decode stack — same constants, same tie-breaks — and `docs/intro-detection-plan.md` says why the port is deliberately faithful: it can then be *checked* against theirs, which `scripts/kahawai-intro-compare.py` does at three levels and `docs/intro-detection-results.md` records.

**Chapters first.** Before any of that, the file gets asked. Plenty of rips mark their own `Recap` / `Intro` / `Credits` chapters, and a boundary somebody wrote down beats one we infer: the analyzer maps recognised chapter names (including `Opening Credits`, which is an opening, and excluding `End of Intro`, which is a marker for where one stopped) to segments with `source = chapter`. A season whose every episode names both an opening and its credits is finished there — no fingerprints, no black-frame search, not one byte across the byte plane — and a season that names only some of it is still compared, with the named boundaries kept on top of what the comparison found. This is intro-skipper's `ChapterAnalyzer`, which the port originally skipped because Kahawai did not index chapter titles; it does now (§5), and on this library 210 files name their credits and 204 their intro (counted with the original substring matcher; the analyzer has since adopted upstream's word-boundary matching and 15 s–450 s duration bounds, which can only trim those counts). A numbered `Chapter 1..12` list names nothing and is left alone rather than guessed at.

**How.** Where the file says nothing, an opening is shared audio, so the search is a Chromaprint fingerprint of each episode's first quarter, compared pairwise across the season: the longest run of near-identical fingerprint points (≤ 6 differing bits, gaps ≤ 3.5 s) is the opening. Credits follow the media type, as upstream's defaults do: for most live action a binary search from the end of the file finds the first mostly-black frame — the credits music differs every week, so audio has nothing to match — while anime, whose ending theme is shared, is fingerprinted in the last 450 s the same way as the opening, with the other method as each one's fallback. A recap is the earliest *short* shared card, ending at the last black frame before the opening. Ends are then pulled back to the pause after the theme (silence detection) and snapped to a keyframe.

Two things learned by measuring rather than by reading the source they came from: seeking into the middle of a GOP and reading pixels immediately measures the decoder recovering, not the film — 92% of a frame reads as black where 4% of it is — so every video window decodes two seconds of lead-in first; and the black-frame search must carry its position from one episode to the next, as theirs does, or it converges on a different black run and lands minutes out.

**Where it runs.** Protocol 4 makes the mediahost the scheduler and source-fact
authority. It groups exact current sources from its local catalogue, picks one
newest rendition per episode, runs the unchanged `kahawai-intro` comparison,
re-stats every source and commits the revision-bound result locally. Chapter
meaning remains normalized in `streams_json`, so chapter-only boundaries still
avoid decode. Every subscribed hub projects the same result into
`media_segments` / `media_segment_scans`; the hub can wake local scheduling but
cannot choose a season or source. This keeps measured full-season decode off
the byte plane and ensures a second hub neither repeats the work nor controls
its priority. Preflight source failures and comparison insufficiency retain the
existing retryable-versus-terminal semantics, now in the local queue.

An unreadable episode is a terminal answer only for the exact `(item, module, collection, root, path, size, mtime, detector)` physical revision. `media_segment_failures` stores that error separately from successful scan rows and never erases older usable boundaries. Both periodic queries skip failed sources rather than whole items: another connected rendition may be tried, and a season leaves the queue only when every current rendition of each unresolved episode has failed. Rename/replacement, size/mtime change or detector bump retries; later success clears that episode's failures. A mediahost disconnect remains transient and records nothing.

Pending-season aggregation counts episodes with at least one unfailed current rendition and requires two comparison-capable episodes, not merely two catalogue rows. A lone repaired episode beside terminal siblings waits until another comparison source becomes eligible instead of being selected every hour. When an unfailed rendition exists only on an offline host, the pass records the season as awaiting; it does not repeatedly analyze the remaining siblings.

**What the client gets.** The boundaries ride on `QUERY /api/v1/items/{id}` — the call a player already makes on its way into playback, carrying the negotiation verdict and the subtitle listing — as `segments`, in milliseconds on the item's own timeline. There is no separate endpoint: a second one would be a second round trip on the path that matters and a second thing for a client to know, and the standalone case (marking boundaries on a page that is not playing anything) has not turned up yet. The field sits outside `negotiated`, because an item whose source is offline still has the boundaries somebody found last week. The web player takes them from that response and shows a single button — *Skip recap* / *Skip intro* / *Skip credits* — while the playhead is inside a segment and not within a second and a half of its end; pressing it seeks to the end of the segment (or, for credits that run to the file end, just inside it, so the up-next countdown takes over). Nothing is skipped automatically: the button is an offer.

## 5. Mediahost internals

**Chapters.** For Matroska/WebM, read at scan from the container itself (other containers keep whatever the demuxer's TOC declared at discovery — MP4 chapters arrive that way), next to the attachment declaration and off the same walk of the header — two facts, one pass, one backfill worklist for files whose records predate either. From the container rather than from the demuxer's TOC because the demuxer misses some: measured on 80 files here, `matroskademux` posts no TOC at all for two that `ffprobe` reads out fine, both ordinary WEBRip episodes with `Recap`/`Intro`/`Credits` marked. Against `ffprobe` the sparse read agrees on all 80 (`scripts/kahawai-chapters.sh`). They are carried on the item, shifted onto its timeline for a multi-part work, and the web app draws them as ticks on the seek bar and as a list on the item page that starts playback at a chapter.

**Scanner and authoritative catalogue.** The mediahost owns `catalog.db` in its state directory (WAL, foreign keys enabled). The configured collection name is its stable ID; its epoch changes only when the namespace is removed/recreated or changes media type, while every file, tombstone and derived fact increments a collection-local version in the same transaction. `catalog_files` is the stat/discovery fast path and seen-generation journal; `catalog_records` stores one current value per replicated entity plus deletion tombstones. A root token is `root-sha256-` plus unpadded base64url of the complete `SHA-256(utf8("kahawai-root-path-v1") || 0x00 || normalized_path_utf8)`. The source key is `(collection, root token, path_rel)` locally and gains the enrolled module ID only when a hub projects it. Changed files are probed with GStreamer and sidecar/container declarations in one scan path; unavailable roots retain their prior rows. Nothing waits for a hub commit.

**Watcher.** Recursive watch installation runs off the async startup path because it can walk a slow network mount. `notify` events wait for a 3 s quiet period and coalesce per directory before feeding the same queue; periodic reconciliation catches missed events (network mounts). Cross-collection overlaps fan one event into every matching namespace. A watcher or scan failure for one unavailable root reports that exact root and leaves its previous manifest rows seen, so reconciliation neither deletes that mount's catalogue nor substitutes another root; other roots continue and later sweeps retry it.

**Protocol-3 exact-source migration (historical).** Protocol 3 introduced the required `SourcePath { root_token, path_rel }` shape and rejected protocol 2. Direct migration 53 replaced unpublished migrations 53–56: each item gained explicit collection ownership, every old file/source pair became one stable `files.id`, source-bound subtitles and image-failure memory referenced that ID, and libraries remained composition only. Migrations 54, 57 and 59 completed the relational physical-source model that protocol 4 still projects into. This history explains existing hub schema, but none of the protocol-3 manifest/generation exchange remains on a protocol-4 link.

A protocol-4 offer validates and persists `collection_roots` before accepting records. Removing a collection from one hub's filter archives/removes only that hub projection; it neither retires the local namespace nor affects another hub. Exact source/cache keys prevent cross-root collisions.

**Protocol-4 projection sync (MH-10).** After `Hello`, a brand-new mediahost keeps the link alive with heartbeats until its first complete manifest exists, then offers selected collections with `(epoch, current version, oldest replayable version)`; a partial version-zero catalogue is never allowed to reconcile away an old hub projection. Later restarts can immediately offer the prior completed SQLite state while their incremental startup scan runs. The hub replies with its durable cursor. A valid cursor gets only newer current rows; a missing epoch, stale epoch, future cursor or cursor below retained history receives a live snapshot. The cursor also records the hub minor that consumed it. A hub minor change forces a fresh snapshot, so an older hub can ignore an additive record kind and remain functional while a later upgraded hub still receives that kind's current state. Snapshot upserts reuse the existing projection and its stable item IDs, then the final chunk reconciles sources absent from the live manifest; keeping the durable cursor at zero until that final reconciliation makes an interrupted snapshot restart safely and preserves hub-owned metadata, matches, subtitles and library membership. Snapshot rows stream file-first from one WAL read view, so every derived fact has a projected source even when a later metadata-only update gave that file row a newer version. Pages and 4 MiB wire chunks remain bounded rather than materializing a whole 100k-file catalogue in memory; adjacent file rows in one chunk commit as one hub batch and flush before any version-ordered derived fact or tombstone. Each hub runs at most one such stream. Sync concurrency is deliberately per-link rather than process-global: the cost is one paged SQLite read view and at most one bounded wire message per syncing hub, while a shared quota would let two slow hubs delay every other hub and violate AR-4. Every mediahost outbound control message and the hub's projection queue have deadlines below the three-heartbeat liveness window; crossing either cycles only that link, aborts requester-owned extraction work, releases any local activity or WAL read view and replays from its durable cursor instead of letting remote backpressure pin local storage, discovery, or another hub indefinitely. ACK follows the final projection commit and is recorded monotonically per hub, so a crash or apply error before ACK causes harmless idempotent replay. Direct protocol-3 catalogue mutation/reconciliation messages are rejected after a protocol-4 hello; accepting both authority models on one link could advance a cursor past state changed outside its journal. One source change can skip intermediate versions because only its latest state matters. Reconnect is database-only and never installs another watcher or starts a scan. Protocol 4 is a breaking authority change and deliberately rejects protocol-3 peers; no hub-to-mediahost catalogue migration exists, so first start builds a fresh local catalogue and each hub takes a snapshot.

**Multiple hubs.** `[mediahost].hub` retains the legacy single identity in the state-directory root. Each `[[mediahost.hubs]]` entry has a stable ID, address and optional collection filter; its key/certificate live below `hubs/<id>/`. One `LocalRuntime` owns the database, watcher, scanners and discovery workers. Independent supervisors enroll, renew and reconnect each outbound link, so an unreachable new hub cannot stop an existing one. In all-in-one mode the intrinsic in-process link is likewise isolated and recreated after failure from the hub's durable cursor without replacing the shared runtime or external supervisors. Every hub remains authoritative only for its own libraries, users and extracted subtitle payloads.
All-in-one's in-process hub is an intrinsic subscriber to that same runtime;
explicit remote hub entries add mTLS links without starting a second scanner.

**Job runners (MH-11/MH-13).** Missing local facts, not hub worklists, select ED2K, loudness and segment work. Missing attachment/chapter declarations, keyframe bounds and video geometry likewise enter bounded local exact-source retry lists (64 sparse reads, or 8 geometry probes, at a time); a retryable I/O-weather failure explicitly releases its still-running local claim, while a stored measured-unknown or terminal answer settles that revision. Every running claim captures the catalogue source version, not only size/mtime, so replacement bytes with preserved timestamps cannot accept an old worker result. The scheduler never overwrites a running claim on a timer: worker completion or process restart releases it, preventing a late answer from attaching to replacement bytes. A stale-revision result is discarded, but a SQLite/I/O failure while committing a current result is retried in the process-local fact sink until it lands, so the durable claim cannot remain stranded merely because the process stayed up. The scheduler groups series by normalized show/year/season and absolute-numbered anime by containing directory, picks one newest exact source per episode, and persists results against those revisions. Analyzer generations live beside the catalogue; changing one invalidates that fact kind and schedules it again. Metadata-only changes retain expensive facts bound to unchanged media bytes, carry in-flight claims onto the verified-identical file version, and replay retained source facts after the replacement file record. Segment work stays one season at a time; after exhausting viable cohorts it waits for the completed scan generation to change instead of reparsing the whole library every 15 seconds. Loudness and hashing retain their source-local idle gates and segment preemption. Discovery counts are computed once process-wide and shared by every link. A hub administrative request is only a wake hint. Urgent subtitle/image/attachment extraction remains hub-initiated because its result is hub cache state tied to a viewer or hub policy. All runners keep control-link intake and heartbeats independent of blocking file work.

**Selected decode is one primitive, not one analysis graph.** `kahawai_media::selected_decode` uses `parsebin` to expose parsed elementary pads, links only the requested kind/index into a plain `decodebin`, and leaves every other pad unlinked before any decoder is constructed. This avoids `decodebin3`'s cross-stream multiqueue—the source of the secondary-Opus stall—and provides the dynamic-pad link and missing-output fact to both loudness and segment analysis. Loudness builds one full-file multi-layout meter fanout; segment detection builds one accurate-seek window with one appsink.

**Loudness backfill order.** The runner drains collection worklists into one file queue instead of consuming a 128-file message inline. Movie collections outrank series/anime; within either category, the local source's current `mtime` sorts newest first. It re-drains intake before every choice and while foreground/segment/background work holds the permit, so a newly announced movie can jump the queued series backlog after the current file. The current file is never interrupted. Queue cost is one local metadata stat per source plus an in-memory sort—no media read and no protocol field.

**A pause names its owner.** The foreground gate exposes separate scan, viewer-lease and urgent-work counts. Loudness logs those counts once when it pauses and logs the matching resume; foreground byte leases log their collection/path at open and close. This is diagnostic rather than a timeout: an actual paused viewer may hold a lease intentionally, while a 25-minute pause with zero hub sessions is an orphan to fix, not permission for background work to read through it.

**A decoder must make progress.** The loudness bus wakes every ten seconds and fails a track after 60 seconds without a decoded audio buffer. A callback currently active is exempt—the callback is exactly where foreground and segment preemption block—so an intentional pause has no deadline. A selected Opus stream that produces neither buffers, EOS nor an error becomes a revision-guarded terminal measurement failure instead of holding the entire movie queue forever.

`OpenRead.background` remains for small hub-owned work such as NFO reads, but intro detection no longer opens one. The mediahost sees the analysis job directly and schedules it from its real scan/lease activity rather than forcing the hub to infer the host's queues.

**ED2K hasher (MH-9).** Anime files missing a local `file_hashes` record are selected at idle priority. Full-file ED2K (9.28 MiB chunked MD4), optional filename CRC32 verification, and terminal read failures are committed to the local catalogue, revision-invalidated when bytes change, then projected to every hub. A projected terminal failure clears any prior hash for that exact source before logging the diagnostic, including when replacement bytes retained the same size and mtime. Scan completion never waits on this expensive rebuild work.

**File server.** Serves `OpenRead` leases only. Each protocol-4 request carries the persisted collection and required exact `SourcePath`; the mediahost resolves that one configured root and canonicalizes the candidate only for confinement, never searches other roots. Missing sources and empty/unknown root tokens are protocol errors. Opens remain read-only; canonical confinement rejects `..` and symlink escapes (NFR-4).

## 6. Transcoder internals

**Capability probing at startup.** Enumerate the `gst::ElementFactory` list and rank encoders: `vah264enc`/`vaapih264enc`, `nvh264enc`, `qsvh264enc`, VideoToolbox, then software (and the HEVC/AV1 equivalents). Presence is insufficient: each candidate encodes five test frames before it can be declared (TC-1), and failures retain the full GStreamer error before the preference list falls through (TC-6). The test dimensions are fixed to the nearest ordinary 640×480 size allowed by that encoder's own system-memory sink caps, rather than one universal size: Mesa's gfx1200 `vah265enc`, for example, accepts widths from 384 while the old 320×240 probe falsely classified working AMD HEVC hardware as broken. A session whose demuxed caps state exact dimensions applies the same sink-cap check before selecting its encoder; an incompatible candidate falls through instead of failing during pipeline negotiation.

**Pipeline construction per `TranscodeSpec`.** Source is a custom `appsrc`-backed element fed from the hub byte plane (or direct file in all-in-one), pushed into:

```
appsrc ! parsebin ! streamselect
  video: decodebin3 ! [deinterlace] ! [tonemap] ! [scale] ! [subtitle-overlay] ! {enc} ! parse
  audio: decodebin3 ! audioconvert ! audioresample ! [downmix] ! {aac|opus}enc
  subs (burn): decode to overlay branch; (convert): subparse ! webvttmux path
  → hlssink3 (cmafmux) → segments + playlist events
```

Streams marked `Copy` bypass decode: `parse ! queue` straight into the muxer. Segment duration 4 s (2 s for LL-HLS later). Seek = teardown + rebuild with `segment start` at target keyframe (accurate seek via `parsebin` index when available), playlist continues with `EXT-X-DISCONTINUITY` (TC-4). Transcode-ahead window: pause pipeline (`appsink` backpressure on segment sink) when > N minutes ahead of last-fetched segment (TC-5).

**Resource control.** Each session's pipeline runs in a supervised child process (§1.1 risk register): the transcoder main process holds the control-plane connection and spawns one worker per session communicating over a local socket, so a decoder crash on a corrupt file kills that session only — the supervisor reaps it, emits `SessionError` with the captured GStreamer diagnostics, and the hub reschedules or fails the session per AR-6. Session slots = `[transcoder] max_sessions`, enforced by the hub's placement filter. **No scratch eviction** (TC-6 amended 2026-08-08): the earlier plan here — "LRU eviction of segments already fetched by the hub" — was wrong twice over. Nothing evicts, and fetched-ness was never the right test: `fetch_artifact` streams a segment into memory and keeps no copy, so a segment the hub has already fetched is precisely one the player may ask for again. A run's scratch is bounded by its own length (measured: 3.0–5.4 GB per hour of content) and deleted whole at teardown. CPU shares are `worker_nice` and `worker_threads`, which each pipeline worker applies to itself out of the `[transcoder]` section — the same route `demote_decoders` takes, so on all-in-one they govern the hub's own remux workers too. cgroup CPU weight stays documented rather than enforced (`kahawai-deployment.md`), because nothing inside a process can grant itself a share of a CPU it does not own.

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

### 7.4 Validation and deletion (SEC-5..6)

Admission is **allow-list based**. Both mTLS listeners use a custom `rustls` `ClientCertVerifier`: verify chain to the hub CA and expiry, then require `sha256(cert_DER)` to be present in the in-memory allowlist — the set of `cert_fingerprint` values from the `satellites` table (SQLite-backed, reloaded on change). A certificate that chains perfectly but isn't the currently registered cert of an enrolled satellite is refused: fail closed, no separate deny list to maintain, and a leaked or mis-issued CA-signed cert is inert unless its fingerprint was explicitly admitted. Satellites use a `ServerCertVerifier` that requires the hub chain to terminate at the *pinned* CA cert byte-for-byte — a different CA with the same name fails.

Renewal (§7.3) interacts with the allowlist atomically: the hub inserts the new fingerprint in the same transaction that issues the renewed certificate, and the satellite row carries both fingerprints until the satellite reconnects with the new cert — at which point the *old* one is dropped. If the 24 h grace elapses without that reconnect, the unused *new* fingerprint is retired instead and the renewal is marked failed for retry; the fingerprint in active use is never removed by its own renewal, so a satellite cannot lock itself out (SEC-7).

Allowlist removal only happens through satellite deletion. `DELETE /admin/v1/satellites/{id}` runs one transaction-plus-teardown sequence:

1. delete the satellite row — its fingerprint leaves the allowlist, and the fingerprint is recorded in an append-only `satellite_audit` log (used to rate-limit re-enrollment spam from known-deleted keys and for forensics);
2. registry closes the satellite's control and byte-plane connections; active sessions fail over (transcoder) or terminate with a client-visible error (mediahost, AR-6);
3. mediahost only — cascade: archive `watch_state` rows and manual match bindings for its collection items into `watch_state_archive (user_id, content_id, position_ms, play_count)` / `binding_archive (content_id, ext_id)`, then delete the mediahost's collection-owned files/items and their `library_collections`/`collections` rows;
4. emit a library-changed event on `/api/v1/events`.

A transient disconnect touches none of this — it only flips availability flags. On any later import, resolution step 2 (§4.2) checks the archives by `ContentId` first, restoring identity, manual matches, and watch state before consulting providers. Re-admitting a deleted machine is a fresh §7.2 enrollment with a new key.

In all-in-one mode the bounded local queues bypass TLS and enrollment entirely — the in-process mediahost shares the hub's fate and trust is intrinsic. The PKI machinery applies only to external satellites, including external mediahosts and transcoders attached to an AIO hub.

### 7.5 Client API

Client authentication has explicit transport modes. `client: "api"` login and
refresh return a 15-minute access JWT and a rotating 30-day refresh bearer;
logout takes that refresh bearer alongside the access bearer, and none of
those responses sets authentication cookies. `client: "browser"` returns only
`{access_token, expires_in}`. The SPA holds that access token in module memory
and schedules rotation from `expires_in` one minute before expiry. Refresh
scheduling does not decode the bearer, so claim-decoding failure cannot disable
rotation. Deliberate sign-out
broadcasts a non-secret invalidation to every open same-origin tab before the
shared refresh family is revoked.
The server owns `kahawai_refresh` (`Path=/api/v1/auth`, 30 days) and
`kahawai_media` (`Path=/api/v1`, 15 minutes) as host-only, `HttpOnly`,
`SameSite=Strict` cookies. Reload bootstraps publicly, then refreshes through
the cookie; the one-time protocol cutover deletes rather than migrates the old
Web Storage and JavaScript-readable cookie credentials.

Each login still has one database row containing only its current refresh-token
hash. Rotation conditionally replaces that hash in an immediate transaction,
concurrent use has one winner, and presentation of a consumed token revokes
that family. API and browser logout revoke that login only; password reset
revokes every family. Access JWTs carry the account's durable `auth_version`.
Every protected request verifies signature and expiry, loads the user by primary
key, compares that generation, and derives mutable username and administrator
state from the row. Password resets and role changes increment the generation
with the mutation; deletion removes the authoritative row, so invalidation is
immediate across processes and restart.

Bearer authentication is the default and never falls back after a malformed or
invalid `Authorization` header. The media cookie is accepted only for
`GET`/`HEAD` events, item artwork/subtitle/font files, and playback
session streams/files. Item grants and session ownership remain inside that
authentication boundary. Catalogue/detail/children/font-list reads,
preferences, playback mutations and every admin route remain bearer-only.

Browser login, refresh and logout require an exact, non-`null` Origin only when
`hub.public_url` is configured; the configured value is authoritative. Without
`public_url`, Origin validation is disabled. Cookie security remains
request-aware: the rightmost `X-Forwarded-Proto`/`X-Forwarded-Host` identifies
HTTPS only for a socket peer in `trusted_proxies`; otherwise cookies use the
hub's direct HTTP view. HTTPS sets `Secure`; configured HTTP logs a cleartext
credential warning. CORS stays independent and credential-free because
cross-origin clients use API mode.

Passwords remain Argon2id. Establishment and reset require 12 Unicode scalar
values, impose no composition rules, and retain login throttling. Existing
shorter Argon2id hashes remain valid at login; the minimum applies only before
a new hash is written.

#### Refusals

Every 4xx and 5xx is `application/json` — `{"code": …, "message": …}`. `code`
is drawn from the `ErrorCode` enumeration published in the OpenAPI document
and is stable; `message` is written for a person and its wording is not
contractual.

Two responses sit outside that. Item artwork's 404 carries the same body but
is CACHEABLE, because a shelf of coverless cards was otherwise one live
request per card on every render; and `stream_session`'s 416 has no body at
all, which is what RFC 9110 asks for — the answer is in `Content-Range`, and
a code would add nothing.

**The status says whether to retry; the code says what happened.** 429 and 503
clear on their own, 5xx is worth a backoff, every other 4xx is final. That
split is HTTP's, so a third-party client (HUB-28) gets the retry decision right
without a table of kahawai's codes in it. There is no `retryable` field: it
would be the same decision computed in two places, free to disagree.

Two consequences of that rule are behaviour changes, not restatements. The
per-account session cap is 429 `session_cap` rather than the 409 it shared with
"this item has no playable source" — one clears the moment a session ends and
the other never does, and a client playing a queue has to tell them apart.
And an internal failure returns a fixed sentence: the anyhow chain, which
carried the hub's scratch layout, the pipeline worker's argv and GStreamer's
stderr, goes to the log. `item_artwork` had answered this way since SEC-WEB-7;
it is the rule now.

`GET`/`QUERY` on an item reports the same distinction in a success body:
`query.unavailable` is an error body rather than a string, so a detail page can
tell a mediahost that is away (`source_offline`, comes back) from an item with
nothing to play (`unplayable`).

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
client_version = 1                    # must match anidb.net registration — update
                                      # the registration BEFORE bumping this

[hub.playback]
ass_fallback = "burn"                 # server default: "burn" | "flatten";
                                      # overridable per library and per user
image_sub_ocr = true                  # runtime switch for the OCR tier (needs the
                                      # `ocr` cargo feature + tesseract + models)
# v1: choosing OCR text over bitmap tiles is a client/user PREFERENCE
# (per-session selectable, remembered per user). An automatic
# `ocr_below_kbps` threshold is deferred until the hub measures client
# bandwidth — a capability that also feeds the quality ladder, so it
# lands with that machinery rather than as a one-off here.

[hub.subtitles.opensubtitles]        # optional block — the feature works with no
                                     # config at all (embedded application key;
                                     # 1 req/s, 5 downloads/24h shared across this
                                     # deployment). Accounts are per USER, in the
                                     # settings page — not here.
api_key = "${KAHAWAI_OS_KEY}"         # optional override of the embedded application key

[hub.pki]
satellite_cert_days = 90
enrollment_ttl_minutes = 15

[gstreamer]
# Process-global decoder policy. If this section is absent, legacy
# mediahost/transcoder demotion lists are merged for compatibility.
demote_decoders = ["vah264dec", "vah265dec", "dtsdec"]

[mediahost]
state_dir = "/var/lib/kahawai-mediahost"   # keypair, cert, pinned ca.crt
detect_segments = true                     # local source-owned policy
[[mediahost.hubs]]
id = "home"; address = "hub.lan:8421"
[[mediahost.hubs]]
id = "family"; address = "family.lan:8421"; collections = ["Anime"]
[[mediahost.collections]]
name = "Movies A"; media_type = "movies"; roots = ["/tank/movies"]
[[mediahost.collections]]
name = "Music"; media_type = "music"; roots = ["/tank/music"]
[[mediahost.collections]]
name = "Anime"; media_type = "anime"; roots = ["/tank/anime"]  # local ED2K scheduling

[transcoder]
hub = "hub.lan:8421"
state_dir = "/var/lib/kahawai-transcoder"
hw = ["vaapi"]          # probe order; "auto" default
max_sessions = 3
scratch_dir = "/var/tmp/kahawai"
```

All-in-one reads the same file. Its co-resident hub is an intrinsic in-process
subscriber; explicitly configured mediahost hubs additionally receive the same
local catalogue over mTLS.

```toml
[all_in_one]
transcoder = false       # external transcoders only; remux still runs in the hub
```

## 9. Testing strategy

Negotiation engine: exhaustive table-driven unit tests (capability × source matrix). Media layer: fixture corpus generated by `gst-launch` scripts (each codec/container/subtitle permutation, tiny durations) committed via Git LFS; discovery snapshots asserted. Integration: the bounded local link runs the mediahost engine and the hub's real message handler in one process without network transport; a second suite runs the same client-visible tests against a docker-compose modular topology (acceptance criterion 3). Chaos tests: kill/reconnect mediahost and transcoder mid-session (criterion 4). PKI tests: full enrollment happy path, wrong/expired code, CSR substitution (fingerprint mismatch), a CA-signed certificate *not* on the allowlist refused at handshake (fail-closed), renewal near expiry including the old/new fingerprint overlap window and grace lapse (unused new fingerprint retired, active one untouched), satellite refusing a hub presenting a foreign CA, and the deletion path — delete a mediahost, assert connection drop + collection removal + TLS-level reconnection refusal, then re-enroll, rescan, and assert manual matches and watch state are restored from the archives (criterion 5). Web UI: Playwright end-to-end suite driven against the all-in-one binary (login, enrollment approval, library composition, direct/remux/transcode playback with the real capability probe, subtitle search+download), run in Chromium and WebKit to cover both the MSE and native-HLS player paths. Performance: `criterion` micro-benches for negotiation and scan throughput; k6 scripts for API latency targets (NFR-1).

## 10. Operational readiness (OPS-1..8)

**Bootstrap.** Empty DB locks the public API and starts two trusted-local transports: the setup SPA on `hub.setup_bind` (loopback-only, default `127.0.0.1:8422`, with a port distinct from the public and satellite listeners so wildcard and OS-specific loopback overlap cannot occur) and a mode-0600 `control/bootstrap.sock` in the data directory for interactive `kahawai hub init-admin`. The control directory is created atomically as mode 0700 before the socket is bound, so the socket's defense-in-depth chmod has no permissive pre-chmod interval. The public router contains no setup mutation. The browser POST requires an HTTP Origin whose authority exactly matches a loopback Host, preventing a foreign page or DNS rebinding from claiming localhost. Both transports call one `BEGIN IMMEDIATE` create-if-empty operation, so concurrent browser/CLI attempts have exactly one winner. That operation returns typed validation, already-completed, and internal failures; the browser transport maps them to 400, 409, and 500 rather than inferring the cause from mutable setup state, and logs internal details instead of returning them. After commit, the loopback listener and socket close and are absent on later starts. No bootstrap bearer secret enters logs, arguments, environment variables or the clipboard. `kahawai hub reset-password <user>` writes a new Argon2id hash and revokes every refresh family for that user in one transaction. Login throttling via an in-memory failure counter keyed on `(account)` and `(source_ip)` with exponential backoff and `tracing` audit events; source IP taken from the socket or from `X-Forwarded-For` only when the peer is in the configured `trusted_proxies` list.

**`doctor`.** Shared implementation in `kahawai-media` + per-module checks. GStreamer probe reuses the transcoder's startup enumeration (§6) and maps it against a static feature matrix table (`capability → required elements`), printing a report like `HEVC decode: OK (vah265dec) / DoVi: missing (dlbvision) → will tone-map`. When the `ocr` feature is compiled in, the hub's doctor also probes Tesseract and enumerates trained models: `OCR: OK (tesseract 5.x; models: eng, deu) / jpn: missing → OCR tier off for Japanese`. Also checks: registry/DB writability, scratch-dir space, `/dev/dri` access when VA-API configured, and `|system_clock - build_time|` sanity. Exit code non-zero on essential failures; `--json` for scripting. The same checks run at startup with warnings-vs-fatal per the same matrix.

**Health and metrics (NFR-6).** `GET /health` is public — an uptime check holds no credential, and it reveals nothing a failed login does not. `GET /metrics` (Prometheus text 0.0.4) sits behind its OWN static credential, the token in `<data_dir>/metrics.secret`, not an admin login token: access tokens live 15 minutes and no scraper refreshes one, so an admin-token endpoint would serve a single scrape and 401 ever after. No such file — the default — means `/metrics` is not served at all (404), so a hub nobody configured for scraping does not advertise what its library holds; a wrong token is 401, which keeps "off here" distinguishable from "wrong secret". Health is reported **per module but served by the hub**: satellites dial out and never listen (AR-3), so an endpoint on each would invert the architecture and be unreachable through NAT anyway. A satellite being away is `degraded`, not down — its collections go unavailable and nothing is lost (AR-6), and a check that fails the whole server because one Pi is unplugged gets muted. Metrics are gathered at scrape time from state the hub already keeps; counters that would need instrumenting every call site are deliberately absent rather than half-present.

**Clock skew.** Leaf certs issued with `notBefore = now - 24h`; the hub's `ClientCertVerifier` and the satellite's `ServerCertVerifier` allow ±5 min on `notAfter`/`notBefore` boundaries. A satellite failing validation compares peer-reported time (TLS handshake wall clock via a pre-flight `Enrollment.Status` ping that echoes hub time) against its own and logs `clock skew: local is 37 min behind hub — fix NTP` instead of a raw handshake error.

**Backup.** `kahawai hub backup <path>` produces a tar: SQLite snapshot via the online backup API (consistent under load), `pki/`, `subtitles/`, and the active config; `kahawai hub restore <path>` refuses on a non-empty data dir. Because `pki/` and satellite rows travel with the snapshot, restored hubs accept existing satellite certs immediately — no re-enrollment (OPS-5). Documented cron-friendly: exit codes + `--quiet`.

**Disk bounds (OPS-6) — no cache eviction, deliberately.** Two costs decide whether a cache entry may be thrown away, and every cache the hub keeps is expensive on at least one of them.

*Rebuild cost.* **Extracted cues and font bundles** look like the obvious candidates and are the worst of them: rebuilding one demuxes the entire source file over a byte-plane lease — gigabytes across the network for a few hundred KB of text, which is the whole reason the HUB-34 ladder exists. (Measured on the live deployment: 45k entries, all embedded extractions, not one sidecar parse.) **Downloaded subtitles** are database-referenced, shared between every user of the item (HUB-23) and cost a rate-limited provider entitlement. **AniDB dumps/XML** are ban-risk traffic, see §4.3.

*Latency at point of use.* **Artwork** is genuinely cheap to refetch — one small ranged read, or one GET to an unmetered image CDN — and it is still not evictable, because it is tiny and wanted *instantly*: a grid scroll wants dozens of posters at once, and a miss is a blank tile plus a round trip precisely where latency is visible. Cheap to reproduce is not the same as cheap to miss. The arithmetic settles it: capping artwork at 100 MiB reclaimed 89 MB out of a 2.7 GB data dir, in exchange for stalls.

What is left is transient and already bounded by lifecycle: session scratch is wiped at startup, torn down per session, and idle-reaped. So there is no janitor. Disk is not the scarce resource here — provider entitlements, mediahost I/O and interaction latency are. Should a deployment genuinely need a cap (hub `data_dir` on a small SD card), the honest design is an admin-triggered purge that states what it will cost, not a silent hourly sweep. Hub stream proxying uses fixed bounded buffers (64 KiB chunks, bounded channel per session) so a slow client applies backpressure to the mediahost read instead of ballooning hub memory — this also caps per-session memory for the in-hub remuxer via `appsrc` `max-bytes`.

*The one exception, and why it is not a quota.* At startup the artwork cache drops resized derivatives that can never be served again: a size no longer in the code's list, or a copy whose original is gone. That is unreachability, not size — nothing is removed for being large, and the sizes still in use are kept forever like everything else here. Variant directories are named for their pixel count, so editing a size is itself what makes the old copies stale; a derivative is named after its original's cache key, so "is the original still there" is one `exists()`. `tests/artwork_sizes.rs` pins which files the sweep may touch, since it is code that deletes.

**Upgrades.** The proto envelope's `protocol_version` is `(major, minor)`; compatibility is simply *equal major* (OPS-7). Minors are strictly additive — new optional fields and messages only — enforced mechanically by a `buf breaking` lint in CI against the last released proto, so "additive" is a build failure rather than a review convention. Versions are exchanged in the link `Hello`: a major mismatch is refused with an error naming both versions and which side needs upgrading. CI runs the integration suite in a version-skew matrix (hub@HEAD × satellite@last-release, and inverted) to keep both directions of minor skew honest. Within a major there is no upgrade order — release notes only carry ordering/migration instructions on a major bump.

**Reverse proxy.** Config: `[hub.http] trusted_proxies = [...]`, `cors_origins = [...]`; docs ship known-good nginx/Caddy/Traefik snippets including the `/api/v1/events` WebSocket upgrade, streaming-friendly settings (`proxy_buffering off` for session endpoints), and correct `application/wasm` MIME for the SPA's JASSUB assets — a proxy or CDN that re-serves WASM as `octet-stream` silently breaks streaming compilation and with it client-side ASS rendering (OPS-8).

## 11. Milestones

The web UI is built in vertical slices alongside its backend features rather than as a trailing milestone:

**M1 — Skeleton (4.5 wks).** Workspace, transports (local + mTLS tcp), hub CA + enrollment flow + revocation, mediahost scan + discovery, hub registry + SQLite, minimal browse API. Web: SPA scaffold, embedding pipeline, first-run setup flow (OPS-1), login, enrollment approval + satellite pages. Also: `doctor` skeleton with GStreamer inventory (OPS-3). *Exit:* all-in-one scans a library; admin completes setup and approves an enrollment from the browser and sees file/stream info.

**M2 — Direct play + remux (4 wks).** Byte plane, item resolution v1 (movies), sessions, range streaming, in-hub remuxer (MKV→fMP4/HLS, all-copy pipeline), watch state. Web: browse/detail views, capability probe, video player with direct + remux playback, resume. *Exit:* seekable direct play and hub-only remux from a modular 2-machine deployment with no transcoder, played in the web player.

**M3 — Transcoding (5 wks).** Transcoder module, capability probing, negotiation engine, HLS output, seek + quality switch, hw accel (VA-API first). Web: hls.js path, playback-info overlay with negotiation verdict, sessions dashboard. *Exit:* acceptance criterion 2 driven from the web player.

**M4 — Enrichment + series/music/anime (6 wks).** Provider trait + TheTVDB/TMDB/MusicBrainz/local, episode/track resolution, dedup/multi-source, matching review queue, image pipeline, subtitle acquisition (OpenSubtitles: embedded application key, anonymous entitlement handling with account upgrade, moviehash search, user-initiated download only, hub-side storage, entitlement surfacing). Anime: fansub tokenizer, AniDB (titles-dump search + ed2k exact match) + AniList + anime-lists mapping, relations/watch order, font-attachment extraction, ASS client-render (JASSUB) and `assrender` burn-in paths, dual-audio preference. Web: enriched browse with artwork, search, match-review queue, library composer, subtitle search/download flow, music player with queue. *Exit:* criterion 1.

**M5 — Hardening (3.5 wks).** Failover paths, metrics/health, remaining admin surfaces (users/grants, provider settings), backup/restore, cache quotas + janitor, login throttling, clock-skew handling, version-skew CI matrix, reverse-proxy docs (OPS-2, 4..8), docs, packaging (binaries + containers with bundled GStreamer plugin set), performance passes, Playwright suite green in Chromium + WebKit. *Exit:* criteria 3–5, NFR-1 numbers on reference hardware.

**Planned (post-v1, spec'd).** Per-session text-over-tiles subtitle preference for overlay-capable clients (HUB-32c case b, remembered per user), and the bandwidth-threshold automatic selection — both waiting on hub-side bandwidth measurement (quality-ladder machinery).

**Post-v1 candidates.** Delegated direct delivery (AR-8), LL-HLS/DASH, offline pre-transcode (TC-7), Dolby Vision profile handling beyond fallback, OIDC, sync-play.
