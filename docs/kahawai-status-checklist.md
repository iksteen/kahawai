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
- [x] AR-13 Capability as a cross-module contract. Client profiles
      (HUB-14) and transcoder inventories (TC-1) are declared and
      dry-run-verified, masks make client declarations falsifiable, and
      version markers are gated at the handshake instead (protocol 2).
      Honest degradation: workers report preroll facts (facts.jsonl →
      SessionReady → verdict), so a 7.1→5.1 fold reaches the client
      instead of only the segment bytes; Hello carries a build stamp so
      the hub log answers which build each satellite runs (protocol 2.2).
      Transcoder declarations now carry a RATE (HUB-36), so a box
      running a filter at 0.65× is no longer indistinguishable from one
      running it at 5×.
      Mediahosts declare nothing, and shall not: MH-12 withdrawn as a
      false premise (2026-08-02) — nothing they could declare decides
      anything the hub should act on
- [x] AR-13a Dry runs reproduce the session's chain. The tone-map probe
      ends in the encoder it claims to feed, with that encoder's own
      output pin, fed 10-bit — and `tonemap_available` means "some real
      target verified", nothing more. The probe that ended in fakesink
      passed on a box where every HDR session died at negotiation.
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
      an idle worklist (cheapest tier, SeekHead-guided early stop).
      VobSub sidecar pairs (.idx/.sub) are discovered at scan, one entry
      per track inside the .idx, and served through the image pipeline
      (extraction/OCR; no tap, so no overlay/burn).
      Subtitle sidecars are part of the MH-5 sidecar signature, so a
      pair appearing next to an unchanged media file busts the fast-path
      and is discovered on the next scan
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
- [~] MH-12 WITHDRAWN 2026-08-02, false premise — see the amendment in
      the requirement. Built and reverted the same night: probing one
      host read 30 s for `movies` and 23 ms for `anime` on the same
      disk, so the cost varies per FILE, not per host, and a per-host
      declaration cannot describe it. Nothing gates on host access; a
      host too slow to walk an index is too slow to serve video and
      fails at playback, where the failure is

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
- [x] HUB-5a No pass is gated on another chain's credentials. The TMDB
      key is optional and its provider is added only when set; TMDB's
      own passes (episode detail, detail backfill) skip themselves
      without one. Every chain pass runs regardless — `run_chain`
      already skips providers the set does not hold, and which
      providers exist is the operator's choice, so an absent one is
      silent rather than a warning.
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

- [ ] HUB-10 Multi-user *(accounts with create AND delete, admin flag,
      per-user watch state live; per-library access grants and parental
      controls missing)*
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
- [x] HUB-15 Negotiation matrix: codec/profile/level, resolution/fps
      ceilings, bandwidth cap (pref + profile), channel downmix,
      subtitle tiers with graphics_overlay/ass_render gating, HDR
      tone-map (15a), OCR (32c), burn-in (32b), encode targets (15b)
- [x] HUB-15a HDR→SDR tone-mapping tier: GL shader (BT.2390 EETF,
      scene-adaptive peak probe, libplacebo-matched display mapping),
      TC-1 `tonemap` report, doctor row, placement preference, verdict
- [x] HUB-15b Multiple encode targets: ubiquity ladders h264 → hevc →
      av1 / aac → opus, picked per session from client profile ∩ the
      placed box's dry-run-verified encoder set (codec is a HARD
      placement filter; hw rank follows the asked codec). Container by
      candidate cost with ties to TS — h264/aac sessions byte-identical,
      fMP4 (isofmp4mux + own segmenter: init.mp4/.m4s/EVENT playlist,
      muxed A/V in one stream) only where it delivers more (opus to
      aac-less clients beats dropping audio) or cheaper (av1/vp9
      COPIES, previously forced h264 encodes). Verdicts state codec and
      container; refusals name what the fleet offers. Wire fields 16-18
      + worker flags, empty = legacy; old satellites safe by
      construction (they never report the new codecs). TC-6 sink
      fallback is TS-only; fmp4 failures fail loudly
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
- [x] HUB-32b Bitmap tier for image subs: server-side PGS/VobSub decode,
      display-set streaming, web overlay rendering via the session tap,
      graphics-overlay capability gating both the offer and the client's
      own rendering; burn-in fallback for clients that cannot composite
      (mediahost-extracted display sets, seek-exact timeline, explicit
      blend in the encode chain). A burned frame takes its time from
      its SEGMENT, not its timestamp (2026-08-02): the seek gate rolls
      pre-seek frames stamped ~0 past the blender before the flush, and
      guessing a base from the first of them put every subtitle a
      resume offset out — a 1 h resume looked up 2 h and burned nothing
- [x] HUB-32c OCR text tier: Tesseract via leptess (MIT — subtile-ocr
      dropped, no copyleft), default-on `ocr` feature, idle sweep over
      the whole library (playback outranks it) + per-track button as the
      urgent path, `tier: ocr` spares the burn encode, doctor row.
      Covers embedded image tracks AND VobSub sidecar tracks.
      Deferred: the bandwidth-threshold selection (needs measurement)
- [x] Subtitle unification (HUB-32c mechanics amendment, 2026-07-31):
      one `subtitle_tracks` keyspace for every origin
      (embedded/sidecar/downloaded/ocr), synced at scan with stable ids,
      OCR lineage via `derived_from` (per source — the multi-source flag
      bug died with the name parsing). Capability adjusts each track's
      computed delivery (text/ass/overlay/burn/none), never existence;
      the UI disables instead of the API filtering. Explicit burn: an
      image track picked by id forces the encode (overrides overlay +
      OCR sparing; VobSub sidecars burn via handed sets), applied at
      start or switched mid-session through the seek-restart (track id;
      0 withdraws). Verdicts carry track ids and re-plans refresh them.
      Per-item `subs.track` memory can now pin a downloaded/OCR row
- [x] HUB-33 Dual-audio defaults, one mechanism: the hub stores a plain
      per-user KV (/api/v1/prefs) and picks nothing. Settings page holds
      per-media-type ordered language lists for audio ('original'
      resolves via the stored original_language) and subtitles; explicit
      in-player changes are remembered per series/movie. Resolution is
      client-side: item track id (subs.track, unification) > series
      memory > per-type settings > track 0 / no subs
- [x] HUB-34 Retrieval efficiency ladder: cache/sidecar → live session tap →
      mediahost sparse/sequential extraction → hub lease, cached at-most-once
      — fonts included: declared attachment ranges serve via exact lease
      reads (declared-no-fonts answers instantly; only never-declared
      records still walk gst over a lease)
- [x] HUB-35 Granular refresh: library-refresh endpoint fanning out
      per-collection scan requests, per-collection live progress in the
      admin overview, global rescan removed (endpoint + button)
- [x] HUB-36 Pace-aware placement, on measured capability. Boxes
      benchmark encoders and the GL tone-map; workers meter the
      un-throttled phase of real sessions into a persisted per-(box,
      work class) EWMA; placement ranks on it and states a
      below-realtime prediction in the verdict rather than letting a
      viewer discover it. Design in implementation §4.5.
      NOTE this requirement's original text asserted that TC-4 "already
      reports a realtime multiple per session". It did not — nothing
      measured pace before this work. Corrected at TC-4.

## Transcoder (TC)

- [x] TC-1 Capability probe reported on registration
- [x] TC-2 Capability + inverse-load placement; admin enable/disable
- [x] TC-3 Sessions fully specified by the hub
- [x] TC-4 Dynamic GStreamer pipelines, HLS segments, supervised worker
      process. Progress reporting is PARTIAL, and the requirement was
      long recorded as if it were not: a per-run pace sample exists
      (HUB-36 — the un-throttled phase, once per run), but there is
      still no continuous progress percentage per session
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
- [x] OPS-9 Decoder rank calibration, measured and remediable.
      `doctor --calibrate` times every decoder that outranks the
      software one against a reference clip and names both figures;
      `doctor --fix` writes the demotions into this box's own config,
      additively and idempotently, preserving comments and never
      removing a human's entry. Opt-in because it is timed: the same
      checks run at every module startup, which must stay instant.
      Covers h264 and hevc (OPS-9a). The DTS half stays a fixed
      known-bad list: libdca is fast and wrong, so no timing finds it.
- [x] OPS-9a Calibrate the remaining codecs. Reference bitstreams for
      av1, vp9, vp8 and mpeg2 (2.6 MB for all seven; container clips
      are demuxed rather than looped, elementary ones still loop). The
      software reference is a preference list — AV1's is `dav1ddec`,
      there is no `avdec_av1` on a normal install — and mpeg2 matches
      on `mpegversion=2` so the MPEG-1 and MPEG-4 decoders stop
      appearing as candidates. Found on the first run what OPS-9 was
      blind to: `vavp8dec` at 9 fps against 386 on silence, one of the
      three that box's operator had demoted by hand. A decoder that
      cannot decode the reference at all is reported, never
      auto-demoted: one clip cannot tell a broken element from an
      unsupported profile.
- [x] OPS-10 Session diagnostics as one downloadable bundle. Captured
      at session end (a hang never fails, so crash capture never fired)
      and on demand while live; admin download from the session list,
      the player, and item detail. Design in implementation §4.6.
      Earned its keep on 2026-08-02: the burn-in resume offset above was
      diagnosed from one downloaded bundle — `start.pos` and the blend's
      own first-frame line, side by side, named the bug.

## Non-functional (NFR)

- [x] NFR-1 Performance. Browse meets the 200 ms target at 50k items on
      every path — first page, last page, search and item detail — and
      holds it at 250k (`tests/scale_bench.rs`, worst run asserted).
      Start latency and 100-session concurrency measured against the
      live fleet by `scripts/kahawai-latency.sh` (worst of N, every run
      printed), across a local file, a 12 GB 4K-class DTS title and an
      HDR10 one.
- [x] NFR-2 Scale targets. 250k files across 10 collections hold on
      every browse path, deep pages and adversarial search included.
      Five executors exercised together (four enrolled transcoders plus
      the hub's own): eleven concurrent transcodes filled every box to
      its own max_sessions and no further, overflow staying local. Ten
      mediahosts enrolled and linked simultaneously
      (`scripts/kahawai-fanout.sh`), per the 2026-08-02 amendment that
      makes the mediahost count a claim about the HUB — allowlist,
      links, collections, per-module state — and not about ten real
      disks, which cannot be stood up here and are not claimed.
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
- [x] NFR-8 Codec support delegated to system GStreamer; MIT throughout —
      the OCR tier links leptess/Tesseract (MIT/Apache-2.0), not
      subtile-ocr, so no GPL combined-work consequence exists;
      --no-default-features additionally drops the Tesseract linkage

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
