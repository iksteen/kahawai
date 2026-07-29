# Requirements checklist

Status of every numbered requirement from `kahawai-technical-requirements.md`,
plus the v1 acceptance criteria. Checked = implemented and exercised against
the live deployment. Unchecked items carry a note when partially done.

**This file records status, not design.** An entry says what works and
what is left, in a few lines.

Do not cite evidence that something WORKS — no "verified live", no counts
or timings proving a box deserves its tick. That is a snapshot of one
afternoon, it goes stale where a fact does not, and the tests and
benchmarks hold it better. Do cite a number that shows something is
BROKEN or missing: it says how far off we are, which is the one thing the
reader cannot get from the tick.

How something works and why it was built that way belong in
`kahawai-implementation.md`; a decision that changes what a requirement
*means* is an amendment recorded in the requirement itself, dated.

## Architecture (AR)

- [x] AR-1 Three modules (hub, mediahost, transcoder) + shared core crates
- [x] AR-2 Clients talk only to the hub
- [x] AR-3 Satellites dial out to the hub
- [x] AR-4 Multiple mediahosts/transcoders per hub; multiple collections per mediahost
- [x] AR-5 All-in-one: hub + in-process mediahost in one process — the
      module logic unchanged, the link transport replaced by channels
      (no gRPC/TLS/enrollment for the local module) and the byte plane
      replaced by direct file reads (AR-11 short-circuit); the satellite
      listener stays up so external mediahosts/transcoders dial in;
      encode work runs in the hub's supervised local workers
- [x] AR-6 Disconnect tolerance: collections go unavailable, never deleted
- [x] AR-7 Versioned protocol; Hello/HelloAck major-version gate
- [ ] AR-8 *(optional v1.x)* Delegated direct-fetch tokens
- [x] AR-9 Control plane client ↔ hub only
- [x] AR-10 Direct play mediahost → hub → client with byte ranges; hub-side remux
- [x] AR-11 Transcoder pulls source bytes via hub-brokered leases
- [x] AR-12 Control/byte plane isolation: separate connections, no shared
      flow-control window (the frozen-heartbeat lesson, codified)

## Security & enrollment (SEC)

- [x] SEC-1 Hub-internal CA, generated on first start
- [x] SEC-2 Satellite keypair + CSR + console enrollment code
- [x] SEC-3 Pending enrollments; explicit admin approval (CLI code entry + admin UI)
- [x] SEC-4 Signed cert + pinned hub CA returned on approval
- [x] SEC-5 mTLS with allowlist admission on all inter-module links
- [x] SEC-6 Deletion removes fingerprint; reconnects refused at TLS layer
- [x] SEC-7 Automatic certificate renewal with atomic allowlist overlap (never locks out)

## Mediahost (MH)

- [x] MH-1 Collections: media type + roots per collection
- [x] MH-2 Scan on start/demand + watching + sweeps; on-demand scans are
      collection-scoped only (the global rescan is gone), with interim
      progress reports every 500 files
- [x] MH-3 GStreamer discovery for technical metadata
- [x] MH-4 Sidecars + artwork + attachment declaration: embedded fonts are
      declared (name/mime/byte range, payload never read) in the file record
      at scan via a sparse EBML walk; pre-existing records are backfilled by
      an idle worklist (cheapest tier, SeekHead-guided early stop)
- [x] MH-5 Content identity (size/mtime fast path; head/tail xxh3 + oshash) with
      incremental rescan (manifest + FilesSeen reconciliation, sync-version handshake)
- [x] MH-6 Byte-range lease serving (dedicated byte-plane connection)
- [x] MH-7 Scan batching + incremental rescans keep large scans cheap
      *(no explicit rate-limit knob; stat batching + in-sync skip in practice)*
- [x] MH-8 Unreadable files reported with diagnostics (FileError)
- [x] MH-9 ED2K hashing: idle-priority background job, eMule/AniDB variant,
      filename-CRC32 verify in the same pass, hub-side journal with
      content-identity copy-forward (at-most-once per content)
- [x] MH-10 Sync generation per collection: persisted mediahost-side, compared
      on reconnect, in-sync = no manifest/no walk; FilesSeen reconciliation
- [x] MH-11 Three-tier job runner: urgent extraction > ED2K > subtitle
      pre-warm, idle = no scan and no lease being served

## Hub — registry, resolution, enrichment

- [x] HUB-1 Registry of mediahosts, collections, transcoders (live + persistent)
- [x] HUB-2 Libraries composed from same-typed collections
- [x] HUB-3 Dedup: same logical item from multiple sources → one item, source list
- [x] HUB-4 Filename/dirname parsing (movies, episodes, anime conventions, music layout)
- [x] HUB-20 Mediahost deletion cascade + watch-state/match archives restored on re-enroll
- [x] HUB-5 Provider trait + declared chains + walker (TMDB, TVDB, anime
      composite, MusicBrainz + CAA). Which record an item IS is derived
      by triggers from stored answers, chain order, refusals and pins —
      design in implementation §4.1/§4.2; per-media-type ordering is the
      2026-07-26 amendment recorded in the requirement.
- [x] HUB-6 Descriptive metadata: titles, plots, dates, ratings, posters,
      episode stills, season/episode and album/release structure, genres
      and cast; all stored, so everything reads back with providers
      unreachable.
      *(Cast is TMDB-only and TVDB's own genre list is unread; both reach
      TVDB-owned items by side-fill when TMDB also answered.)*
- [x] HUB-7 Provider rate limits/caching. One queue per provider host
      (`hub/gate.rs`) spaced at each provider's documented limit, 429/503
      parks that provider alone; caching is the permanent answer store
      rather than a TTL response cache (implementation §4.3). Stored
      limits were corrected against the specs 2026-07-26 — three of four
      were wrong in our favour.
- [x] HUB-8 Ambiguous matches flagged for manual review (card-based review UI,
      per-item re-match/search dialog)
- [x] HUB-9 Local metadata as authoritative provider — unranked and
      asked before the chain (requirement amended 2026-07-26). Sidecars
      are tracked in both directions, so a `.nfo` or cover appearing
      beside an already-scanned file is picked up by an ordinary rescan
      and a vanished one makes `local` withdraw its answer.
- [x] HUB-21 External subtitle providers: SubtitleProvider trait with
      OpenSubtitles.com (REST) as the first impl. Always on: kahawai's
      application key ships in the binary (5 req/s, 5 downloads/24 h
      shared per deployment), overridable only by kahawai.toml; each
      USER may attach their own opensubtitles.com account in Settings to
      spend their own entitlement, while what they download is shared
      with everyone (HUB-23) *(manual query search is the known
      follow-up)*
- [x] HUB-22 Hash-preferred subtitle matching: two-phase search — the
      file's moviehash (which IS the mediahost's oshash) alone first,
      title/year (+ season/episode, projected for absolute-numbered
      anime) only when the hash is unknown; hash matches sort first
- [x] HUB-23 Subtitles stored hub-side only (mediahost link has no write operation)
- [x] HUB-24 User-initiated subtitle downloads: search + download from
      the item detail page, filtered by the media type's subtitle
      language preference (HUB-33) with a one-click unfiltered retry;
      the result is parsed into the normal cue/ASS cache as a "d{id}"
      track, served by every existing subtitle path, and removable
- [x] HUB-32 ASS/SSA first-class: faithful extraction (header + re-timed events),
      JASSUB rendering with embedded fonts, live session-pipeline tap (all embedded
      text codecs), mediahost extraction facility with index-driven sparse reads;
      image subtitles (PGS + VobSub) decoded server-side and rendered on a
      canvas overlay from the same tap — no video transcoding
- [ ] HUB-32a ASS fallback policy *(flatten live and labeled; burn-in not built;
      no per-library/user policy or playback-info reporting yet)*

## Hub — users, API, playback

- [ ] HUB-10 Multi-user *(accounts, admin flag, per-user watch state live;
      per-library access grants and parental controls missing)*
- [x] HUB-11 Versioned HTTP/JSON API + /api/v1/events SSE channel
      (invalidation hints: scan progress, satellite connectivity,
      sessions, enrichment; cookie-authenticated for EventSource)
- [x] HUB-12 Browse/search/filter/sort. Hierarchical browse, one
      endpoint for browse and cross-library search with server-side sort
      and paging, item detail with stream info, artwork at named sizes,
      playback and admin endpoints. Client behaviour and the API shape
      are in implementation §4.4/§4.7.
- [x] HUB-13 All hub state in embedded storage; survives restart without rescan
- [x] HUB-14 Capability-profile negotiation: browser-probed profile with
      every play request, hub decides per stream (`negotiate.rs`,
      `tests/negotiate_play.rs`); explicit mode = operator force
- [ ] HUB-15 Negotiation matrix: codec/profile/level, resolution/fps
      ceilings, bandwidth cap (pref + profile), channel downmix,
      subtitle tiers with graphics_overlay/ass_render gating. Missing:
      HDR tone-map (15a), OCR (32c), burn-in
- [ ] HUB-15a HDR→SDR tone-mapping tier: GL shader segment built + live
      (tonemap.frag, TC-1 `tonemap` report, doctor row, placement
      preference, verdict). Open: owner quality review vs real HDR matrix
- [x] HUB-16 Cheapest-path preference incl. SOURCE choice: every
      candidate judged, direct > copy > audio-enc > video-enc, rank ties
- [x] HUB-17 HLS delivery for remux/transcode (EVENT playlists, mid-stream seek)
- [x] HUB-18 Sessions: per-user concurrency caps, progress checkpoints/resume,
      idle reaping, seek-anywhere with pipeline restart
- [ ] HUB-19 Music: playback + queue live *(gapless delivery and ReplayGain
      pass-through not implemented)*

## Hub — web interface

- [x] HUB-25 Embedded web UI compiled into the binary
- [x] HUB-26 Admin UI: enrollments, satellites, libraries (with per-library
      refresh + live per-collection scan progress), providers, users,
      match review
- [x] HUB-27 MVP player: login, browse, detail w/ stream info, direct/remux playback,
      audio/video/subtitle track selection, resume, watch state
- [x] HUB-28 Web UI is a pure client of the public API

## Hub — anime (HUB-29..33)

- [x] HUB-29 AniDB/AniList providers: titles-dump identity, anime-lists ID
      mapping, AniList metadata + relations, UDP FILE-by-ED2K gold path,
      question-keyed never-ask-twice (`provider_queries`, 0044)
- [ ] HUB-30 Fansub filename conventions: group prefixes, absolute
      numbering, CRC tags, bracket stripping, designators with season-0
      bands, per-episode hash identity + re-binding (`ed2k_aid`,
      `tests/hash_binding.rs`), generic release revisions (0043),
      bare-file identification + movie minting, batch-marker spans
      (0045). Missing: cross-aid re-binding (per-season splits)
- [x] HUB-30a Hashes are canonical identity: late ED2K re-verifies name
      matches, overrides on disagreement (manual included); manual matches
      otherwise adopt anime ids only via reverse mapping (proven live)
- [x] HUB-31 Native anime structure: absolute numbering as identity,
      relations graph on the item detail (prequel-first suggested order),
      and presentation as native or TVDB-style seasons — a per-USER
      preference (settings page, default seasons; the requirement's
      per-library knob was dropped 2026-07-25 as needless bookkeeping) —
      with the projection stored per episode during the TVDB/TMDB bridge
      *(episodes TVDB never curated absolute numbers for stay unprojected
      and fall into an "Other" bucket)*
- [x] HUB-32 (see subtitles above)
- [ ] HUB-32b Bitmap tier for image subs *(server-side PGS/VobSub decode,
      display-set streaming, web overlay rendering live via the session tap;
      graphics-overlay capability profiles and policy ordering pending)*
- [ ] HUB-32c OCR text tier (subtile-ocr/Tesseract, default-on cargo feature)
      *(not built; GPL-3.0 licensing consequence pre-documented in README)*
- [x] HUB-33 Dual-audio defaults, one mechanism: the hub stores a plain
      per-user KV (/api/v1/prefs) and picks nothing. Settings page holds
      per-media-type ordered language lists for audio ('original'
      resolves via the stored original_language) and subtitles; explicit
      in-player changes are remembered per series/movie. Resolution is
      client-side: series memory > per-type settings > track 0 / no subs
- [x] HUB-34 Retrieval efficiency ladder: cache/sidecar → live session tap →
      mediahost sparse/sequential extraction → hub lease, cached at-most-once
      — fonts included: declared attachment ranges serve via exact lease
      reads (declared-no-fonts answers instantly; only never-declared
      records still walk gst over a lease)
- [x] HUB-35 Granular refresh: library-refresh endpoint fanning out
      per-collection scan requests, per-collection live progress in the
      admin overview, global rescan removed (endpoint + button)

## Transcoder (TC)

- [x] TC-1 Capability probe reported on registration
- [x] TC-2 Capability + inverse-load placement; admin enable/disable
- [x] TC-3 Sessions fully specified by the hub
- [x] TC-4 Dynamic GStreamer pipelines, HLS segments, supervised worker process
- [x] TC-5 Cancellable sessions; transcode-ahead pacing window
- [ ] TC-6 Resource ceilings *(max_sessions enforced; CPU/GPU shares and
      scratch-disk quota/eviction not enforced)*
- [ ] TC-7 *(optional v1.x)* Offline pre-transcode

## Operations (OPS)

- [x] OPS-1 First-run setup mode (console setup token → admin creation)
- [x] OPS-2 Login throttling: consecutive-failure lockout with
      exponential backoff (30 s → 15 min cap), keyed per account (5) and
      per source address (20, higher so a shared NAT survives), failures
      logged with source IP; in-memory, X-Forwarded-For untrusted until
      OPS-8 adds proxy-trust config
- [x] OPS-3 `doctor` command with plugin/encoder checks
- [x] OPS-4 Clock-skew tolerance (backdated certs, enrollment skew warning)
- [x] OPS-5 Online backup/restore (`kahawai hub backup|restore`),
      taken while the hub keeps serving, and including the PKI so
      restored hubs accept existing satellite certs without re-enrolment;
      caches excluded as re-derivable.
- [x] OPS-6 Quota-bounded caches with eviction — satisfied by there being
      nothing eligible to evict; requirement amended 2026-07-26 with the
      audit, reasoning in implementation §10. The one deletion is
      unreachability, not quota: resized artwork whose size left the code
      list, or whose original is gone, dropped at startup.
- [x] OPS-7 Cross-version satellite compatibility: protocol gated on major
      version (Hello/HelloAck) — per decision 2026-07-25, major-gating IS the
      compatibility contract; no previous-minor guarantee
- [x] OPS-8 Reverse-proxy support: trusted_proxies (exact IPs and CIDR
      ranges — docker/traefik bridges) gate X-Forwarded-For for OPS-2
      throttling (rightmost-untrusted, spoof-safe), configurable CORS
      allowlist, SSE X-Accel-Buffering: no, docs/kahawai-deployment.md
      (nginx + traefik examples, wasm MIME note)

## Non-functional (NFR)

- [ ] NFR-1 Performance. Browse meets the 200 ms target at 50k items on
      every path — first page, last page, search and item detail — and
      holds it at 250k (`tests/scale_bench.rs`, worst run asserted).
      Remaining: start-latency (direct <= 2 s, transcode <= 6 s) and 100
      concurrent direct-play sessions are unmeasured.
- [ ] NFR-2 Scale targets. 250k files across 10 collections hold on
      every browse path, deep pages and adversarial search included.
      Remaining: 5 concurrent transcoders untested — 3 are live.
- [x] NFR-3 No user-state loss on crash; media never written
- [x] NFR-4 mTLS everywhere inter-module; token auth on client API
- [x] NFR-5 Portability: Linux x86_64, macOS (transcoder), and Linux
      aarch64 — cross-compiled against a device sysroot
      (scripts/kahawai-cross-aarch64.sh), proven on a Pi 3 end to end:
      doctor green (encoders dry-run verified), enrolled as mediahost
      over mTLS, scanned, served direct play through the hub
- [x] NFR-6 Operability: structured logging, single-file TOML with env
      overrides, `GET /health` (public) and `GET /metrics` (Prometheus
      0.0.4, behind its own static `hub.metrics_token`; unset = not
      served at all), SIGHUP reload for what can change under a running
      process.
- [x] NFR-7 Versioned client API (`/api/v1`)
- [x] NFR-8 Codec support delegated to system GStreamer; MIT with the OCR
      feature's GPL-3.0 combined-work consequence pre-documented in README
      (applies when HUB-32c lands; --no-default-features stays copyleft-free)

## v1 acceptance criteria

- [x] 1. All-in-one mixed-library end-to-end: the single-machine variant
      exercised 2026-07-25 (fresh instance, movies + music collections:
      setup → scan through the in-process link → browse → direct play
      byte-identical via the function-call byte plane → remux segments →
      music streaming; enrichment/web player/AniDB/PGS are the same code
      paths proven live on the modular deployment)
- [x] 2. Direct play w/ byte-range seek; remux w/ mid-stream subtitle/audio switching
- [x] 3. Modular three-machine deployment (dev box hub + NAS + macOS transcoder)
- [x] 4. Mediahost kill mid-playback: unavailability surfaces, reconnect restores
- [x] 5. Enrollment / deletion / re-enroll with watch-state + match restore
