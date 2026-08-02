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
- host→hub: `AnnounceCollection{ id, media_type, roots, sync_generation }` (hub replies in-sync / out-of-sync per collection, §5), `FileUpsert{ collection_id, batch<FileRecord> }`, `FileRemove`, `FilesSeen{ collection_id, batch<file_id> }` (reconciliation after generation mismatch), `FileHashes{ batch<(file_id, ed2k, crc32_verified?)> }` (MH-9 background results), `ScanProgress`, `FileError` (MH-8), `Heartbeat`
- hub→host: `RequestScan{ collection_id }` (always collection-scoped, HUB-35; coalesces with a running scan), `RequestHashes{ collection_id }`, `ExtractStream{ file_id, stream|attachment, session_token }` (targeted subtitle/font extraction, §4.3b ladder step 3), `OpenRead{ file_id, session_token }` → host responds by accepting a byte-stream channel keyed by the token
- `FileRecord = { path_rel, size, mtime, identity: ContentId, streams: StreamInfo[], sidecars[], tags{} }`

**`TranscoderLink`** — `Register(capability_report)`, then:
- hub→tc: `StartSession{ spec: TranscodeSpec }`, `Seek{ session, offset }`, `SetQuality{ session, ladder_step }`, `Cancel{ session }`
- tc→hub: `SegmentReady{ session, seq, uri|inline }`, `PlaylistUpdate`, `Progress{ realtime_x, position }`, `SessionError`, `Load{ cpu, gpu_sessions }`

**Byte plane.** Bulk media bytes ride gRPC too — tonic byte-chunk streams, mTLS under the hub CA, with the one-time token minted on the control stream binding each stream to a specific read lease or transcode session. The hard-won invariant (AR-12): **the byte plane MUST be a separate HTTP/2 connection from the control link** — a distinct tonic channel, never a stream multiplexed onto the control connection. HTTP/2 flow control is per-connection as well as per-stream: in early implementation, a single stalled lease stream (client paused, pipeline backpressured) exhausted the shared connection-level window and froze heartbeats for 40 s at a time, producing false disconnects and spurious failovers. Separate connections give each plane its own window; a stalled lease now stalls only itself. In all-in-one mode the byte plane is a function call. This keeps bulk media bytes off the gRPC streams. In all-in-one mode the byte plane is a function call.

Content identity (MH-5):

```rust
struct ContentId { size: u64, head_xxh3: u64, tail_xxh3: u64 } // 64 KiB head + tail
// FileRecord additionally carries oshash: u64 — the OpenSubtitles moviehash
// (size + wrapping u64 sum of first/last 64 KiB), computed in the same read pass.
```

Fast-path change detection uses `(path, size, mtime)`; `ContentId` resolves renames/moves so the hub carries item identity and watch state across them.

## 4. Hub internals

### 4.1 Data model (SQLite)

Core tables: `mediahosts`, `collections`, `files` (technical metadata as JSON column + indexed scalar columns), `libraries`, `library_collections`, `items` (logical entities; `kind` = movie|show|season|episode|artist|album|track), `item_sources` (item ↔ file, quality rank), `subtitles` (downloaded/registered external subtitle streams, §4.3a), `users`, `grants`, `watch_state (user, item, position_ms, play_count, updated_at)`, `watch_state_archive` and `binding_archive` (content-identity-keyed survivors of mediahost deletion, §7.4), `sessions`, `satellites (module_id, type, name, cert_fingerprint(s), enrolled_at)` — this table *is* the mTLS allowlist — plus append-only `satellite_audit`.

Watch-state writes are batched but flushed on session teardown and every 10 s (NFR-3).

**Inputs and derivations.** Every enrichment table is one of two kinds, and which one decides who writes it. **Inputs** are facts nothing can recompute: `provider_metadata` (what each provider answered, per item), `provider_ranks` (chain order per media type), `rejected_matches` (records a human refused), `manual_match` (the record a human pinned), `anime_ids`, `enrichment_queue`. **Derivations** are functions of those, stored only because a read cannot afford to compute them: `item_match` (which provider record an item IS), `items.sort_title`, `item_libraries` — which also carries copies of each item's sort keys (0040), so a library page in sort order is one range scan of a single covering index. A deep page skips OFFSET rows at one index step each; when skipping also meant probing `items` per row, the last page of a 250k library cost 1.2 s against 41 ms now. A retitle flows answer → `items.sort_title` (0035) → the membership copies (0040) through chained triggers.

Nothing in the codebase writes a derivation. Triggers do, on every write to an input. This replaced an earlier `merged_metadata` table that was maintained by explicit calls and spent its life subtly stale — the rule against storing what a read can derive was right, and the reason it was right is staleness, so the answer is to remove the human step rather than the storage. Consequences worth knowing:

- A derivation must not carry an input as a column. The human pin used to be `item_match.manual`, which forced the pick to recompute *around* rows it must not touch; it lives in `manual_match` now and wins as the pick's first sort key.
- Trigger bodies must not use `(?N IS NULL OR col = ?N)` optional-filter guards. That form is unavoidable with a bound parameter and it defeats every index; in a trigger the filter is known at compile time, so it is substituted as a plain equality. Getting this wrong made a rescan quadratic.
- `INSERT OR REPLACE` into an input is forbidden: SQLite fires no DELETE triggers for REPLACE unless `recursive_triggers` is on, and it is off.

The reference for what each table means is the `hub/providers.rs` module doc, next to the code that enforces it. `tests/item_match_derived.rs` and `tests/sort_title.rs` re-derive the truth independently after every kind of write, raw SQL included.

**Connection pool.** 8 connections, WAL, foreign keys on, and `cache_size = -8192` (8 MiB per connection). SQLite's 2 MB default is smaller than the index a deep browse page walks, which made the same query cost 253 ms or 50 ms depending on which pooled connection served it. Measured at 2/8/16/64 MiB: the bimodality disappears at 8 and nothing improves above it. The memory ceiling is 8 × 8 MiB, allocated lazily.

### 4.2 Item resolution pipeline

Runs per file-upsert batch, incrementally:

1. **Parse** filename/dirs → `NameGuess` (title, year, S/E including `S01E01E02`, specials `S00`, absolute numbering, `Artist/Album/NN - Track` for music). Anime collections use a dedicated tokenizer variant for fansub conventions: `[Group] Title - 01v2 [1080p][A1B2C3D4].mkv` → group, title, absolute episode, version, CRC32, quality tags; batch/OVA/ONA/movie markers. Table-driven tokenizer, not regex soup; per-library overrides.
2. **Bind** to an item: exact match on prior manual binding by `ContentId` → else provider match ≥ confidence threshold → else create *unmatched* item flagged for review (HUB-8).
3. **Dedup**: same external ID (tvdb/tmdb/musicbrainz release+track) or same normalized identity ⇒ attach as additional `item_source`, rank by resolution/bitrate/codec modernity (HUB-3).
4. **Enrich** via provider chain (below).

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

**Exact-file episode identity (HUB-30).** AniDB's `FILE` reply names the episode, group and version of the exact bytes on disk, keyed by ED2K hash — identity no filename heuristic can match. Every hashed episode file is asked once, budgeted per enrichment run and paced by the client's flood rule; the full reply is cached in `ed2k_aid` (misses included, terminally). A binder then re-binds files whose cached answer disagrees with their name-derived slot: the hash wins (HUB-30a), watch state follows the file, and a misnumbered episode item left sourceless is deleted rather than haunting the season view. The binder is deliberately narrow where numbering spaces differ: `epno` is scoped to one AniDB entry, so only files whose aid matches their show's move; regular numbers apply to absolute-keyed episodes only; every typed number lands in season 0 under a banded layout (S=n, C=100+n, T=200+n, P=300+n, O=400+n — the hub's own layout, collision-free by construction): specials, credits reels and trailers are precisely the files name-parsing cannot place, and one squatting on an episode slot is an artifact of the numbering the hash exists to correct. Binding runs BEFORE the provider chain so the bridge projection writes titles onto corrected slots in the same pass. Name-side, the fansub tokenizer slots release designations (NCOP/NCED, OVA/OAV/ONA, SP/SPECIAL, MOVIE; arabic or roman indexes) into the same season-0 bands, with precedence calibrated on real filenames: an explicitly-indexed designator beats a stray title number, an indexless one loses to a real episode number, and SxxEyy names never reach designator logic at all. Files bound to NOTHING answer to their hash: looked up by ED2K, bound under whatever their aid names — and when nothing owns the aid and AniDB's type says Movie, the item is MINTED from the provider's answer (title, year from the cached per-anime XML) or an aid-less twin adopted. That is the one place an item originates from an answer rather than a filename, and it is deliberate: a yearless "Akira.mkv" can never earn an item any other way, and every minted field is AniDB's statement about the exact bytes. "Movie-shaped" includes single-episode OVA/Web entries (Kite Liberator is `type OVA, episodecount 1` — a movie in everything but the type string); a MULTI-episode series-type aid stays bare — one stray file must not scaffold a show.

**Batch markers are spans, not duplicates.** "OVA 1-2" and "S01E01-E02" parse to an episode range, and the range becomes ONE episode item covering `episode..=episode_end` (`items.episode_end`, 0045), rendered "E01-02". Two entries would be dishonest twice over: `item_sources` binds a file to exactly one item by primary key (everything from sessions to watch state leans on that), and with no per-episode byte offsets, "play episode 2" could only ever play the whole file. Span slots are exempt from hash re-binding — a single-epno FILE reply must not collapse a range — and a span learned on a later scan widens the existing slot (and its auto-generated title) in place. Range detection is deliberately conservative: a dashed number pair counts only immediately after a designator or as the name's final token, so "Ranma 1-2" stays a title; and among designator tokens an explicitly-indexed one outranks an earlier indexless one, so the adjective in "Kite Special Edition Uncut OVA 1-2" cannot shadow the real "OVA 1-2".

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

**Image subtitles — bitmap streaming before burn-in (HUB-32b).** PGS/VobSub get a third, cheaper tier that precedes burn-in in the policy order: the hub decodes the stream server-side (RLE display sets → RGBA tiles with palette applied) and serves it as a timed bitmap track — PNG tiles plus a timing/position manifest fetched alongside the media, sourced through the same HUB-34 ladder. Clients declaring `graphics_overlay: true` (the web player does — it's the same canvas the ASS renderer uses) composite the display sets themselves: full visual fidelity, zero video decode/encode/blend on the server, so a PGS-subtitled direct-play or remux stays transcoder-free. Burn-in remains only for clients that can't composite an overlay.

**Burn-in (HUB-32b last resort).** For clients that declare no compositing, the image subtitle is composited into the picture by the encoder, which per the owner's policy call is fidelity-first: such a client always gets its subtitles, so the burn FORCES the video encode that carries them — negotiation vetoes direct play and copy alike when one is wanted. Compositing is `overlaycomposition` fed from a display-set timeline read up front through the container's own index (`subindex::extract_image_track` → `imagesubs` → `burnin::Timeline`), NOT from the demuxer's live subtitle pad. That distinction is the whole design: display sets are sparse, so a session that starts mid-set — every resume, every seek-restart — is fed nothing by a live pad until the next set arrives and the subtitle on screen simply vanishes for seconds (measured against mpv: present at 25.5 s played from zero, absent after a flushing seek to the same timestamp). A timeline knows what is on screen at any instant. The overlay sits after the tone map (subtitle white is already SDR; the PQ curve would crush it) and after the scaler (blit at output size). Two facts that only a pixel comparison surfaces: overlay rectangles take BGRA — RGBA silently yields a NULL rectangle and aborts from a non-unwinding FFI frame — and the canvas must be scaled UNIFORMLY by width, since it shares the picture's width but not always its cropped height (a 3840x1600 scope film with 1920x1080-authored subtitles; independent axis scaling squashed the text by a quarter, and a box fit shrinks it by the same). Positions that then fall outside the frame are clamped into it, which is what puts bottom-anchored dialogue on screen and what mpv does too. VobSub's canvas comes from the `size:` line of its `.idx` (CodecPrivate), which need not match the video.

**Where the display sets come from.** The index walk that yields them is disk-speed locally and round-trip-bound over the byte plane — measured at ~4 KB/s hub→mediahost→NAS, so a walk costing milliseconds on the host does not finish inside a session start at all. It therefore runs on the MEDIAHOST (`ExtractImageSubs` → `subindex::extract_image_track` → `ImageSubtitles`), which reads its own disk; the hub caches the raw blocks per (module, collection, path, track) and hands the file to whichever worker runs the encode — by path for a local worker, in `StartSession.burn_sets` for a dispatched one, which can no more walk the source index than the hub can. Extraction is **on demand at session start**, not at scan: it costs milliseconds per file, while pre-walking all ~1200 image-sub files would cost roughly 12 GB of cache (OPS-6 never evicts) for content that only a non-compositing client ever needs. Text subtitles already have both shapes — urgent on demand plus an idle pre-warm worklist — so pre-warming image sets on the same idle tier is the upgrade if first-play latency ever matters. A burn is only promised once the sets exist: if they do not arrive, negotiation re-plans with the tier withdrawn rather than encoding video that burns nothing, and the walk itself runs under a read budget so a session always starts.

**Three faults that only the real fleet exposed**, each of which the dev box hid: image tracks may carry Matroska per-track compression (this library's PGS is zlib), so payloads must be inflated before any decoder sees them — the text path had the same latent gap and now shares the fix; `overlaycomposition` only blends when downstream does *not* claim to support overlay metadata, and the VA encoder claims it and then drops it, so burn-in worked on NVENC and silently did nothing on silence — we now blend explicitly via `gst_video_overlay_composition_blend`, which needs nothing of the encoder; and frame timestamps are the file's own on one box but rebased to the seek point on another (15500 ms vs 0 ms for the same session), so the blend measures its own time base on the first frame and logs which it found, rather than assuming one and putting every subtitle at the wrong time.

**Image subtitles — OCR text tier (HUB-32c).** Between bitmap and burn-in sits an OCR tier: the hub converts the image stream to a plain text track, for clients that can't composite (better than forcing a burn-in encode) and for constrained links (a text track is a few KB; even the bitmap tile track is orders of magnitude heavier — this is the tier you want on a high-latency remote session). In v1 the text-over-tiles choice is a user preference — selectable per session, remembered per user; an automatic bandwidth threshold waits for hub-side bandwidth measurement, which lands with the quality-ladder machinery. Pipeline (as built): the HUB-32b display-set cache is the input — the mediahost already walked the index, and `burnin::timeline_from_file` already decodes BOTH PGS and VobSub blocks to positioned RGBA bitmaps, so `subtile-ocr` (whose value is parsing `.idx/.sub`/`.sup` files we deliberately strip) is not used at all; that also removes its GPL-3.0 licensing consequence (NFR-8, amended). Per display set: binarize to black-on-white (ink = opaque ∧ bright, the shape of subtitle glyphs), upscale sub-40px bitmaps ×3, hand-rolled 8-bit BMP in memory → Tesseract via `leptess`, PSM 6 (a set is one subtitle of 1–3 uniform lines — measured on real 1080p and 2160p PGS tracks at conf 70–91, ~16 ms/set, a feature film in ~15–30 s). Identical adjacent sets merge into one cue (PGS re-issues screen states; zero-length re-issues are dropped). Result rides the downloaded-subtitles machinery with `provider: 'ocr'` — stored, served, selected and deleted like any downloaded track, `kind: "ocr"` in the API. Generation: an idle sweep walks every image track in the library (one at a time, playback outranks it, failures stick for the hub run), with the per-track button on the item page as the urgent path (synchronous, ~15–30 s, cached; an inflight lock keeps sweep and button from double-generating); the language model comes from the track's tag via a 639-1/2 mapping probed against the installed models. NOTE the tag can lie — a real track tagged `en` carried Romanian, which OCRs readably under `eng` minus diacritics; that is a metadata defect, visible and deletable, not a crash. Marked machine-derived throughout; delete + regenerate from the same UI. **VobSub sidecars** (`.idx`/`.sub` pairs, the DVD-rip era's external image subs) feed the same pipeline: the scanner reads the `.idx` (small text) and emits one sidecar entry per track inside it; the mediahost's extraction keys off the `.idx` extension and reproduces the exact shape a Matroska demux would yield (idx text as codec_private, bare SPUs as blocks — `kahawai-media::vobsub_file`, an idx parser plus a ~60-line MPEG-PS depacketizer), so the KBS1 cache, zstd, OCR and the sweep handle sidecars with no further changes. SPU stop-display commands give real durations. No session tap exists for a sidecar, so overlay and burn don't apply; OCR text is their serving path. Measured on the library's 42 real pairs: every idx entry assembled to a complete SPU. One systematic artifact handled: DVD fonts render capital I as a bare bar, so word-position '|' is corrected to 'I'.

Feature gating: cargo feature `ocr` on `kahawai-hub` (forwarded by the `kahawai` binary), **default-on**, gating the `leptess` dependency — `--no-default-features` builds have no Tesseract linkage for minimal deployments. Runtime, model presence is probed by asking Tesseract itself (a `LepTess::new` per model, cached — the one probe that cannot disagree with TESSDATA_PREFIX); `doctor` reports engine usability and the common models present; the API answers 501 with the reason on feature-off builds. Negotiation: a cached OCR text row flips the image stream's tier from Burn to `Ocr` — the forced video encode disappears and direct play comes back (the tier order bitmap → OCR text → burn, HUB-32c). Licensing (NFR-8, amended): all-MIT-side linkage; no copyleft consequence in any build.

**Dual audio.** Per-user, per-library preference `audio: original_subbed | dubbed(lang)` feeds default stream selection at negotiation time (HUB-33); the chosen default is overridable per session in the player as usual.

### 4.4 Client API (v1 sketch)

```
POST /api/v1/auth/token                     # login → access+refresh
GET  /api/v1/libraries
GET  /api/v1/items?library=&q=&sort=&limit=&offset=   # browse AND search; returns total
GET  /api/v1/items/{id}                     # incl. sources[] with full StreamInfo
GET  /api/v1/items/{id}/children            # seasons/episodes, album/tracks
GET  /api/v1/items/{id}/artwork?size=       # named size, resized + cached
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
POST /admin/v1/libraries/{id}/refresh       # fan out RequestScan to each member collection
POST /admin/v1/collections/{id}/refresh     # single collection (for UIs that enumerate them)
GET  /admin/v1/enrollments                  # pending CSRs (fingerprint, type, name, age)
POST /admin/v1/enrollments/approve          # body: { code }
GET  /admin/v1/satellites                   # enrolled modules + cert fingerprints + status
DELETE /admin/v1/satellites/{id}            # delete = allowlist removal + cascade (see §7.4)
```

`/playback/decisions` is side-effect-free and returns the full negotiation verdict (per-stream direct/remux/transcode + reasons) so clients can display "why is this transcoding".

**Every browse page is a deferred join.** An inner query chooses WHICH ≤200 ids make the page using only indexed scalar columns — the membership covering index for a library browse, the sort index for search and unscoped — and the resolved-metadata view, watch state and source counts join onto those ids afterwards. Joining first and paging second resolves the view for every candidate the sort visits, which is the recurring 900 ms failure mode whenever an ORDER BY stops matching an index. A search page streams the sort index and stops early; when it underfills, the scan saw everything, so the total is known without a counting pass — only a full page pays one.

**Browse and search are one endpoint** (HUB-12). Omitting `library` searches every library, which is what makes cross-library search a parameter rather than a second route; `q` matches the folded filename and the resolved title, so an item is found by what it is called now as well as by what it is called on disk. `sort` is a name (`title`, `-title`, `year`, `-year`, `added`, `-added`) mapped to a fixed ORDER BY — never interpolated from the request — and each name resolves to `items.sort_title`/`items.year` only, both carried by one index, so a page is a range scan. `added` needs no column: item ids are ULIDs and sort by mint time. The response carries `total`, `limit` and `offset` so a client can size the whole result set before fetching it.

**Artwork sizes.** `?size=` names one of a fixed list in code (`thumb` 96 px, `card` 480 px, longest edge), resized on first request and cached thereafter. Names rather than free-form `w=`/`h=`: a client that can ask for any width can mint unbounded cache entries. An unknown name serves the original rather than failing, so retiring a size cannot break a page already open.

### 4.5 Capability negotiation (`kahawai-core::negotiate`)

**Capability is the architecture's spine, not a transcoder detail (AR-13).** Four participants declare what they can do and the hub decides from those declarations: the client's probed profile (HUB-14), the transcoder's dry-run-verified inventory (TC-1), what a mediahost's access to a file permits (MH-12), and the hub's own in-process worker on the same terms as any transcoder. Everything below was learned by getting one of them wrong.

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

Decision order per HUB-16: try full direct play; else keep every stream that fits and remux; else encode only failing streams. Rules include: profile/level comparison for H.264/HEVC/AV1; HDR→SDR tone-map (HUB-15a), delivered as a single GL shader segment spliced into the video ENCODE chain: `glupload ! glcolorconvert ! RGBA ! glshader ! glcolorconvert ! gldownload ! NV12-capsfilter ! capssetter(colorimetry=bt709)`. The fragment shader (kahawai-media/src/tonemap.frag) does PQ EOTF → exposure at 203-nit reference white → extended Reinhard at a fixed 1000-nit mastering peak → BT.2020→709 gamut matrix → 709 OETF; the NV12 pin is load-bearing (a VA encoder with no converter after the segment refuses system-memory RGBA — observed not-negotiated on the J5005); the capssetter rewrites the colorimetry tag so the encoder's VUI matches the rewritten pixels. Element research that led here (2026-07): `vapostproc hdr-tone-mapping` exposes its property everywhere but the driver only implements it on TGL+ (silent no-op on GLK, proven pixel-identical); no libplacebo GStreamer element exists; `videoconvert gamma-mode=remap` has no tone operator and crushes PQ to black. Capability is DRY-RUN verified per box (the real segment against videotestsrc — element presence does not prove a headless box can open a GL context), reported via TC-1 (`CapabilityReport.tonemap`), surfaced by the doctor (`hdr tone-map` row), and preferred by placement (`PlacementNeed.needs_tonemap`). The decision arm: `profile.hdr` means "this browser DISPLAYS HDR acceptably" — Chrome/Safari tone-map PQ in their own compositor even on SDR screens and declare true; Firefox decodes HEVC but renders PQ untouched (washed out, observed live) and declares false. An hdr10 source + `hdr:false` + a tone-map-capable executor vetoes copy/direct and forces the tone-mapped encode; without a capable box the copy stands (washed beats washed-plus-generation-loss) with the as-is verdict. HLG is never mapped or vetoed (SDR-compatible by design). Via HUB-16 source preference, an SDR source of the same title now beats an HDR source for hdr:false clients automatically. The HUB-15a quality bar closed 2026-07-29: a 10-title matrix (animation, night scenes, daylight, grain, DoVi-base) fitted and verified against what mpv actually DISPLAYS — vo=gpu/libplacebo window captures via IPC `screenshot-to-file` — at per-title percentile-curve RMS ≤ 0.006 (9/10 titles; joint-fit loss 0.0025), and the owner-flagged live scene (Duplicity 2m16) matches the mpv window within 0.01 mean signal. Measurement trap for posterity: mpv's `--vo=image` screenshots (BOTH formats) run zimg's software tone mapper, a different and brighter renderer than the libplacebo playback path — never a reference; an entire calibration round chased it before live playback exposed the gap. Further rules: channel downmix when layout unsupported; text subs converted SRT↔WebVTT with `subparse`; ASS handled per the `ass_fallback` policy (§4.3b: `ClientRender` when the client declares it, else `Burn` or `Flatten` — and after a `Flatten` substitution the plan is re-evaluated, since removing the burn-in often demotes a video encode back to remux/direct); PGS/VOBSUB → `BitmapStream` when the client declares `graphics_overlay` (server-side decode to a timed bitmap track, no video work, §4.3b), `Ocr` when the tier is available and preferred (client can't composite, or the user's per-session/remembered preference selects text over tiles — an automatic bandwidth threshold is deferred until the hub measures client bandwidth, alongside the quality-ladder machinery), else burn-in — piggybacked when the plan already encodes video, otherwise forcing a video encode only as the true last resort; bandwidth cap forces a ladder whose top rung ≤ cap.

**Placement on measured throughput (HUB-36).** The tone-map boolean above is the archetype of a declaration that answers "can you" where placement needed "can you keep up", so capability now carries a rate at three levels, and each exists because the one above it cannot see something.

*Benchmarks* answer "what is this hardware". Every verified encoder and the GL tone-map segment are timed against an **embedded 24 fps reference clip** decoded through the real converter chain, not a synthetic pattern: `videotestsrc pattern=snow` was tried first and measured the noise generator rather than the encoder (2.98× / 0.78× — implausible enough to catch by eye, not by test), and SMPTE bars are nearly free for a software encoder and would report a J5005 at tens× where real film runs below 1×. Results cache keyed on `gst::version()`, report instantly at link-up and re-measure in the background, pushing a fresh `CapabilityReport` on drift. Each element is measured in a CHILD process: `svtav1enc` SIGSEGVs on the J5005, and a benchmark that takes the transcoder down with it is worse than no benchmark — a crash demotes that one capability and the sweep continues. Absence is `Option<f32>`, never `0.0`: zero on the wire means unmeasured, and a box that reports 0.00 has told you something quite different from a box that has not been asked.

*Observed pace* answers "what does this box do with THIS work", which a benchmark cannot: source decode cost is invisible to an encoder measurement (software AV1 decode is the case that motivated carrying the source codec in the class key). The trap is that steady-state production is deliberately throttled to viewer+120 s, so any measurement of it reads ≈1.0× however fast the box is. Workers therefore meter **only the un-throttled phase** — from the first buffer until the pace probe's window check first fails, capped at 60 s — and discard samples shorter than 5 s of wall or content, which are preroll burst rather than throughput. One sample per run lands in `pace.json`; the transcoder's supervision poll takes it (renaming it, so the file's absence is the "taken" flag that survives a seek-restart replacing the watcher) and ships it on the existing heartbeat. The hub folds it into `transcoder_pace` as an EWMA at α=0.3 per `(module_id, work_class)`, where the class is `{res}|{src}|{dst}[|tm]` — schema meaning in the `hub/pace.rs` module doc. Samples carry only a session id: the hub derives the class where the plan and MediaInfo are both in scope, so the measurement and the thing measured can never disagree.

*Link rate* answers "can the bytes even arrive". Reads ≥1 MiB through the lease bridge fold into an EWMA (α=0.2); smaller reads measure round-trip latency, not bandwidth. It is deliberately in-memory and cleared on disconnect — a rate describes one connection over one network, and a persisted stale one lies confidently.

*The scorer* (`Registry::place`) keeps every hard filter — a box that cannot decode the source or encode the target is not a candidate at any speed — and changes only the order among those that can. Observed pace wins outright when present and is never blended with the benchmark, because a real run already contains the decode, the tone-map, the encode AND that box's link stalls; blending would count the same cost twice. Unobserved work falls back to the components, and the SLOWEST governs (encoder, tone-map when planned, link bytes against source bitrate). Nothing measured at all yields `None`, which ranks as CAPABLE: refusing work for want of evidence is how a fleet never earns any. Rank is `sustains(≥1.2×)` → tone-map fit → hardware → prediction → load, where 1.2 rather than 1.0 because a box that exactly matches realtime stalls the moment anything else happens on it. Fleet still wins by default (§4.5 policy: hub cores serve clients); work repatriates to the hub only when no fleet box sustains and the hub does. A placement predicting below realtime is placed anyway — refusing would strand a slow fleet — but never silently: the verdict gains `predicted 0.7× realtime — may stall` through the same facts channel as the 7.1→5.1 fold.

**Session diagnostics (OPS-10).** A bundle is assembled per session and stored under `<data_dir>/session-logs/{unix}-{item}-{session}.log`, newest 40 kept. The item id rides in the FILENAME because sessions are ephemeral and leave no row behind — that is what makes "the last session for this item" a directory glob rather than a schema change.

*Where it is captured is forced by teardown.* The satellite's `Runner::end` deletes the run dir synchronously the moment the worker exits, and the hub's `EndSession` is fire-and-forget, so there is no later moment to ask: the bundle is gathered inside `end()` before `remove_dir_all` and pushed as `SessionLogs`. The hub's own local worker does the same in `Sessions::end` before its own wipe. A live session can also be asked (`CollectLogs` → `SessionLogs`), which is what the download button does while a problem is on screen.

*The hub half is structured state, not log lines* — item, user, mode, plan, verdict, placed box, work class, sink. Not a stylistic choice: the hub cannot read its own log, which goes to stdout and is redirected by whatever launched it (a shell redirect under `kahawai-restart.sh`, discarded entirely by launchd on macOS). The same reason excludes the transcoder's own log, which does not exist as a file on macOS at all.

*The cut keeps head AND tail*, unlike `crashlog`'s tail-only. A panic's message is at the end; a hang's evidence is at the start — the plan, the caps negotiation, which encoder was chosen. Measured bundles run ~27 KB against a 256 KB cap (the noisiest real session: 82 subtitle tracks, tone-map, E-AC-3; `worker.log` was 21,803 bytes and FLAT over three minutes, because worker logging is entirely front-loaded), so the cut only ever fires on a warning storm, where both ends beat one.

*One line earns its place disproportionately*: whether `segment00000` carries SPS, PPS and an IDR. That single fact separates "the pipeline is healthy and the player is wedged on an undecodable first segment" from every other failure, and it was the whole diagnosis of two distinct bugs. It costs ~40 bytes against a 400 KB segment, and is stated as unavailable rather than silently omitted for non-TS pipelines.

### 4.6 Session manager

State machine per session: `Negotiated → Provisioning → Streaming → (Seeking|SwitchingQuality)* → Ended`. Direct play sessions hold an `OpenRead` lease against the mediahost and proxy ranges with `Accept-Ranges`/`206`.

**Remux sessions run entirely inside the hub** — this is why `kahawai-hub` depends on `kahawai-media`. When the plan is `container: Remux` with every stream `Copy`, the hub feeds the mediahost byte stream through a local demux-only pipeline (`appsrc ! parsebin ! <selected streams, no decode> ! cmafmux → hlssink3`-style segmenting) and serves the result like any HLS session. Parsing and repackaging elementary streams is cheap (no codec work, a few % CPU), so it needs no scheduling, works with zero transcoders attached (AR-10), and keeps transcoders free for real encoding jobs. Seek = pipeline restart at the target keyframe, same as §6 but without the decode path.

Transcode sessions (any stream marked `Encode`) are placed on a transcoder by a scorer (capability fit ≥ hw-accel ≥ inverse load), monitored via `Progress`; on transcoder loss the spec is re-issued to the next candidate with `start_offset = last served segment` (AR-6). If no transcoder is connected, plans requiring `Encode` fail fast at `/playback/decisions` with a distinct reason so clients can fall back (e.g., pick a lower-quality source or disable the offending subtitle) rather than time out. Idle timeout 90 s without segment fetch or progress ping → teardown. Concurrency limits enforced per user (HUB-18).

### 4.7 Embedded web UI (HUB-25..28)

**Serving.** `vite build` output is embedded with `rust-embed` and served by an axum fallback route: `/app/*` → SPA `index.html` (client-side routing), hashed assets with immutable cache headers, `/` redirects to `/app`. A `--dev-web-proxy` flag proxies to the Vite dev server for frontend development against a live hub. The SPA authenticates with the same JWT flow as any client and calls only `/api/v1` and `/admin/v1` — no private endpoints (HUB-28); admin routes render only for users whose token carries the admin role, but authorization is enforced server-side as usual.

**Capability profile.** On startup the player probes the browser honestly rather than shipping a static profile: `MediaSource.isTypeSupported()` / `mediaCapabilities.decodingInfo()` across the codec matrix (H.264 profiles/levels, HEVC, AV1, AAC/AC-3/Opus/FLAC), container support (fMP4 via MSE; native HLS on Safari), HDR via `matchMedia('(dynamic-range: high)')` + codec profile support, and screen dimensions — serialized into the `CapabilityProfile` sent to `/playback/decisions`. This makes the web player the reference implementation of negotiation from the client side.

**Capability debug mask.** The negotiation matrix and the subtitle tiers have branches most browsers never take — no HEVC decode, no HDR display, no ASS renderer, no display-set compositor — and hunting for a browser that genuinely lacks each one is slow and unrepeatable. So the player can SUBTRACT from its own probe: a mask (`localStorage`, edited from a panel next to the playback-info verdict) is applied at the single choke point where the profile is built, after the source-aware refinements so a precise cap cannot smuggle back a family the mask dropped. What it changes is not cosmetic — the same masked answer drives the player's own rendering (`ass_render: false` really takes the flattened-VTT path instead of JASSUB; `graphics_overlay: false` really asks the hub to withhold image subtitles), so a masked client behaves like the real thing rather than merely reporting different verdict text. Codec and container entries can only be dropped, since claiming a decoder the browser lacks would produce a stream it cannot play; the three declaration booleans may go either way, because they are claims rather than probes. A mask only reaches the hub on a NEW session (the hub stores the effective profile per session and re-plans track switches against it), so applying one restarts playback at the current position, and the active mask is always printed beside the verdict — a forgotten mask must never read as a bug in the hub. The panel also copies the effective profile as JSON for `kahawai-play.sh -P` and `kahawai-sweep --profile`, so a browser-side finding reproduces headlessly across the whole library. Its first catch was the HUB-15 channel ceiling: `channels=[1,2]` range caps fixate to their minimum, so every client declaring a stereo limit had been receiving mono.

**Video playback.** Direct play binds the range endpoint straight to `<video src>` (browsers do range requests natively); remux/transcode plans load the session's `master.m3u8` via `hls.js` (MSE) with native HLS fallback on Safari. Seek beyond the transcoded window and ladder switches go through the session endpoints from §4.6. Text subtitles attach as WebVTT `<track>` elements (hub converts on demand); ASS/SSA streams render client-side via JASSUB (libass compiled to WASM) on a canvas overlay, loading the item's served font set; PGS/VobSub arrive as the server-decoded bitmap track (§4.3b) composited on the same canvas — the player accordingly declares both `ass_render: true` and `graphics_overlay: true` in its capability profile; burned-in subtitles arrive inside the video and the UI marks them as such from the negotiation verdict, which is also surfaced in a "playback info" overlay (direct/remux/transcode + per-stream reasons). Progress posts every 10 s and on pause/unload.

**Browsing.** One search box in the header, whose meaning follows the screen: on the home screen it queries every library at once and shows at most five hits each, listing only libraries that have any; clicking a library's name follows those results into it with the query still standing, where the same box becomes that library's filter. The box is rendered only on those two screens — on the player or admin pages it would silently do nothing.

A library grid reserves the full height of the result set from the first response and fetches 100-item chunks as rows scroll into view, so only the visible rows exist in the DOM (25–44 cells for a library of 881) and the scrollbar never moves under the thumb — the property that separates this from infinite scroll, where the page grows as you go. Row height and column count are measured from the DOM rather than copied from the CSS, because the card art is `aspect-ratio: 1` on a fluid grid track and both are therefore functions of window width. Cards are a fixed height (titles clamped to two lines) since an exact reservation is impossible over variable rows, and the placeholder for a row that has not arrived is structurally identical to a loaded card so nothing shifts when a chunk lands.

**Music playback.** A persistent queue over `<audio>` with preloading of the next track via a second element swapped at track boundary (near-gapless; true gapless via Web Audio API is post-MVP), album/artist views, and the same negotiation path (FLAC direct where supported, else transcoded Opus/AAC).

**Admin UI.** Thin CRUD over `/admin/v1` plus the `/api/v1/events` WebSocket: the enrollments page updates live as CSRs arrive (approve-by-code inline), satellites page shows fingerprints/status with delete-and-cascade confirmation spelling out consequences (HUB-20), a drag-to-compose library builder over announced collections — each library carries a **Refresh** action that fans `RequestScan` out to its member collections and shows live per-collection `ScanProgress` aggregated on the library row (HUB-35; the old global refresh-all button is gone, and refreshing an already-scanning collection joins the running scan instead of stacking another). A per-collection refresh exists in the API (`POST /admin/v1/collections/{id}/refresh`) and appears as a row action wherever the UI enumerates individual collections. Also: the manual-match review queue with provider candidate side-by-side, subtitle/enrichment provider settings, user/grant management, and a sessions dashboard streaming per-session state and throughput.

## 5. Mediahost internals

**Scanner.** Bounded-concurrency walker (`ignore` crate for traversal, N=4 discoverers) feeding a work queue persisted in a small local SQLite journal so scans resume (MH-7). Each file: fast-path check → if changed, `GstDiscoverer::discover_uri` with 30 s timeout → map `GstDiscovererInfo` into `StreamInfo` (codec caps → normalized codec enum + profile/level from caps fields; HDR from mastering-display/CLL caps and DOVI configuration boxes; tags via `GstTagList` for music). Sidecar association by stem-matching within the directory (MH-4). Failures → `FileError` with the GStreamer diagnostic (MH-8).

**Watcher.** `notify` events debounced 2 s, coalesced per directory, feeding the same queue; nightly reconciliation walk catches missed events (network mounts).

**Reconnect sync (MH-10).** The scanner's local journal (MH-7) carries a per-collection `sync_generation`, advanced in the same transaction that records a change batch as acknowledged by the hub. On reconnect, `AnnounceCollection` carries it and the hub compares against its stored value. Match → in sync: no manifest, no directory walk, availability flips on — that's the entire handshake, so a hub or mediahost restart over a 250k-file collection costs one message. Mismatch → the mediahost replays un-acknowledged changes and streams a `FilesSeen` enumeration so the hub prunes rows for files that vanished while offline: incremental in both directions. Combined with content-identity copy-forward, reconnection never costs a rescan (criterion 4). The inverse knob exists too: `POST /admin/v1/libraries/{id}/refresh?deep=true` marks each member collection so the hub answers the NEXT manifest request empty — first-scan semantics, every file re-probed regardless of stat — which is how rows written by an older probe pick up newly extracted stream facts (HDR/profile/level, MH-3). One-shot and hub-side only, so it works against any satellite version; the incremental scan stays the default because a deep pass re-reads hash windows and re-probes 900-file collections for tens of minutes.

**Job runner (MH-11).** All auxiliary work funnels through a three-tier priority runner. Tier 0 — `ExtractStream` requests (HUB-34 ladder step 3): a viewer is waiting, so these are never idle-gated and preempt everything below. Tier 1 — ED2K hashing, idle-gated. Tier 2 — subtitle pre-warm, idle-gated and strictly below hashing: opportunistically materializes embedded subtitle streams for recently added items so the HUB-34 ladder later hits step 1 instead of 3. *Idle* means exactly: no scan in progress **and** no read lease currently being served — background work never steals I/O from a scan or from someone's playback.

**ED2K hasher (MH-9).** Tier 1 of the job runner, enabled per collection on hub request (anime): full-file ED2K (9.28 MiB chunked MD4) computed at bounded read rate, optionally verifying a filename CRC32 in the same pass. Results ship as dedicated `FileHashes` messages — not `FileUpsert` amendments — and the mediahost keeps no hash state: the hub persists the hash on the `files` row, and content-identity copy-forward (a renamed/moved file inherits the row's hash along with its identity) gives at-most-once hashing with zero new persistence. The hub's `RequestHashes` simply enumerates files whose rows lack a hash. Scan completion never waits on any of this — hash matches upgrade an item's identification asynchronously.

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

**Resource control.** Each session's pipeline runs in a supervised child process (§1.1 risk register): the transcoder main process holds the control-plane connection and spawns one worker per session communicating over a local socket, so a decoder crash on a corrupt file kills that session only — the supervisor reaps it, emits `SessionError` with the captured GStreamer diagnostics, and the hub reschedules or fails the session per AR-6. Session slots = min(configured, hw session limit); scratch dir with LRU eviction of segments already fetched by the hub; cgroup-friendly CPU shares documented for containerized runs (TC-6).

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

Negotiation engine: exhaustive table-driven unit tests (capability × source matrix). Media layer: fixture corpus generated by `gst-launch` scripts (each codec/container/subtitle permutation, tiny durations) committed via Git LFS; discovery snapshots asserted. Integration: `LocalTransport` lets the full three-module flow run in one test process; a second suite runs the same client-visible tests against a docker-compose modular topology (acceptance criterion 3). Chaos tests: kill/reconnect mediahost and transcoder mid-session (criterion 4). PKI tests: full enrollment happy path, wrong/expired code, CSR substitution (fingerprint mismatch), a CA-signed certificate *not* on the allowlist refused at handshake (fail-closed), renewal near expiry including the old/new fingerprint overlap window and grace lapse (unused new fingerprint retired, active one untouched), satellite refusing a hub presenting a foreign CA, and the deletion path — delete a mediahost, assert connection drop + collection removal + TLS-level reconnection refusal, then re-enroll, rescan, and assert manual matches and watch state are restored from the archives (criterion 5). Web UI: Playwright end-to-end suite driven against the all-in-one binary (login, enrollment approval, library composition, direct/remux/transcode playback with the real capability probe, subtitle search+download), run in Chromium and WebKit to cover both the MSE and native-HLS player paths. Performance: `criterion` micro-benches for negotiation and scan throughput; k6 scripts for API latency targets (NFR-1).

## 10. Operational readiness (OPS-1..8)

**Bootstrap.** Empty DB → hub serves only `/app/setup` and prints `Setup token: XXXX-XXXX` to console/logs; the flow (token → admin credentials → done) flips a `setup_complete` flag. `kahawai hub reset-password <user>` writes a new Argon2id hash directly. Login throttling via an in-memory failure counter keyed on `(account)` and `(source_ip)` with exponential backoff and `tracing` audit events; source IP taken from the socket or from `X-Forwarded-For` only when the peer is in the configured `trusted_proxies` list.

**`doctor`.** Shared implementation in `kahawai-media` + per-module checks. GStreamer probe reuses the transcoder's startup enumeration (§6) and maps it against a static feature matrix table (`capability → required elements`), printing a report like `HEVC decode: OK (vah265dec) / DoVi: missing (dlbvision) → will tone-map`. When the `ocr` feature is compiled in, the hub's doctor also probes Tesseract and enumerates trained models: `OCR: OK (tesseract 5.x; models: eng, deu) / jpn: missing → OCR tier off for Japanese`. Also checks: registry/DB writability, scratch-dir space, `/dev/dri` access when VA-API configured, and `|system_clock - build_time|` sanity. Exit code non-zero on essential failures; `--json` for scripting. The same checks run at startup with warnings-vs-fatal per the same matrix.

**Health and metrics (NFR-6).** `GET /health` is public — an uptime check holds no credential, and it reveals nothing a failed login does not. `GET /metrics` (Prometheus text 0.0.4) sits behind its OWN static credential, `hub.metrics_token`, not an admin login token: access tokens live 15 minutes and no scraper refreshes one, so an admin-token endpoint would serve a single scrape and 401 ever after. Unset — the default — means `/metrics` is not served at all (404), so a hub nobody configured for scraping does not advertise what its library holds; a wrong token is 401, which keeps "off here" distinguishable from "wrong secret". Health is reported **per module but served by the hub**: satellites dial out and never listen (AR-3), so an endpoint on each would invert the architecture and be unreachable through NAT anyway. A satellite being away is `degraded`, not down — its collections go unavailable and nothing is lost (AR-6), and a check that fails the whole server because one Pi is unplugged gets muted. Metrics are gathered at scrape time from state the hub already keeps; counters that would need instrumenting every call site are deliberately absent rather than half-present.

**Clock skew.** Leaf certs issued with `notBefore = now - 24h`; the hub's `ClientCertVerifier` and the satellite's `ServerCertVerifier` allow ±5 min on `notAfter`/`notBefore` boundaries. A satellite failing validation compares peer-reported time (TLS handshake wall clock via a pre-flight `Enrollment.Status` ping that echoes hub time) against its own and logs `clock skew: local is 37 min behind hub — fix NTP` instead of a raw handshake error.

**Backup.** `kahawai hub backup <path>` produces a tar: SQLite snapshot via the online backup API (consistent under load), `pki/`, `subtitles/`, and the active config; `kahawai hub restore <path>` refuses on a non-empty data dir. Because `pki/` and satellite rows travel with the snapshot, restored hubs accept existing satellite certs immediately — no re-enrollment (OPS-5). Documented cron-friendly: exit codes + `--quiet`.

**Disk bounds (OPS-6) — no cache eviction, deliberately.** Two costs decide whether a cache entry may be thrown away, and every cache the hub keeps is expensive on at least one of them.

*Rebuild cost.* **Extracted cues and font bundles** look like the obvious candidates and are the worst of them: rebuilding one demuxes the entire source file over a byte-plane lease — gigabytes across the network for a few hundred KB of text, which is the whole reason the HUB-34 ladder exists. (Measured on the live deployment: 45k entries, all embedded extractions, not one sidecar parse.) **Downloaded subtitles** are database-referenced, shared between every user of the item (HUB-23) and cost a rate-limited provider entitlement. **AniDB dumps/XML** are ban-risk traffic, see §4.3.

*Latency at point of use.* **Artwork** is genuinely cheap to refetch — one small ranged read, or one GET to an unmetered image CDN — and it is still not evictable, because it is tiny and wanted *instantly*: a grid scroll wants dozens of posters at once, and a miss is a blank tile plus a round trip precisely where latency is visible. Cheap to reproduce is not the same as cheap to miss. The arithmetic settles it: capping artwork at 100 MiB reclaimed 89 MB out of a 2.7 GB data dir, in exchange for stalls.

What is left is transient and already bounded by lifecycle: session scratch is wiped at startup, torn down per session, and idle-reaped. So there is no janitor. Disk is not the scarce resource here — provider entitlements, mediahost I/O and interaction latency are. Should a deployment genuinely need a cap (hub `data_dir` on a small SD card), the honest design is an admin-triggered purge that states what it will cost, not a silent hourly sweep. Hub stream proxying uses fixed bounded buffers (64 KiB chunks, bounded channel per session) so a slow client applies backpressure to the mediahost read instead of ballooning hub memory — this also caps per-session memory for the in-hub remuxer via `appsrc` `max-bytes`.

*The one exception, and why it is not a quota.* At startup the artwork cache drops resized derivatives that can never be served again: a size no longer in the code's list, or a copy whose original is gone. That is unreachability, not size — nothing is removed for being large, and the sizes still in use are kept forever like everything else here. Variant directories are named for their pixel count, so editing a size is itself what makes the old copies stale; a derivative is named after its original's cache key, so "is the original still there" is one `exists()`. `tests/artwork_sizes.rs` pins which files the sweep may touch, since it is code that deletes.

**Upgrades.** The proto envelope's `protocol_version` is `(major, minor)`; compatibility is simply *equal major* (OPS-7). Minors are strictly additive — new optional fields and messages only — enforced mechanically by a `buf breaking` lint in CI against the last released proto, so "additive" is a build failure rather than a review convention. Versions are exchanged at `Register`: a major mismatch is refused with an error naming both versions and which side needs upgrading. CI runs the integration suite in a version-skew matrix (hub@HEAD × satellite@last-release, and inverted) to keep both directions of minor skew honest. Within a major there is no upgrade order — release notes only carry ordering/migration instructions on a major bump.

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
