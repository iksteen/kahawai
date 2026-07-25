# Requirements checklist

Status of every numbered requirement from `kahawai-technical-requirements.md`,
plus the v1 acceptance criteria. Checked = implemented and exercised against
the live deployment. Unchecked items carry a note when partially done.

Last updated: 2026-07-25 (anime providers, MusicBrainz).

## Architecture (AR)

- [x] AR-1 Three modules (hub, mediahost, transcoder) + shared core crates
- [x] AR-2 Clients talk only to the hub
- [x] AR-3 Satellites dial out to the hub
- [x] AR-4 Multiple mediahosts/transcoders per hub; multiple collections per mediahost
- [x] AR-5 All-in-one binary (`kahawai all-in-one`)
- [x] AR-6 Disconnect tolerance: collections go unavailable, never deleted
- [x] AR-7 Versioned protocol; Hello/HelloAck major-version gate
- [ ] AR-8 *(optional v1.x)* Delegated direct-fetch tokens
- [x] AR-9 Control plane client ↔ hub only
- [x] AR-10 Direct play mediahost → hub → client with byte ranges; hub-side remux
- [x] AR-11 Transcoder pulls source bytes via hub-brokered leases

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
- [x] MH-2 Scan on start/demand + filesystem watching (inotify, debounced,
      read-event feedback filtered) + periodic sweep backup
- [x] MH-3 GStreamer discovery for technical metadata
- [x] MH-4 Sidecar detection (subtitles, artwork); local artwork served via hub cache
- [x] MH-5 Content identity (size/mtime fast path; head/tail xxh3 + oshash) with
      incremental rescan (manifest + FilesSeen reconciliation, sync-version handshake)
- [x] MH-6 Byte-range lease serving (dedicated byte-plane connection)
- [x] MH-7 Scan batching + incremental rescans keep large scans cheap
      *(no explicit rate-limit knob; stat batching + in-sync skip in practice)*
- [x] MH-8 Unreadable files reported with diagnostics (FileError)
- [x] MH-9 ED2K hashing: idle-priority background job, eMule/AniDB variant,
      filename-CRC32 verify in the same pass, at-most-once per content identity

## Hub — registry, resolution, enrichment

- [x] HUB-1 Registry of mediahosts, collections, transcoders (live + persistent)
- [x] HUB-2 Libraries composed from same-typed collections
- [x] HUB-3 Dedup: same logical item from multiple sources → one item, source list
- [x] HUB-4 Filename/dirname parsing (movies, episodes, anime conventions, music layout)
- [x] HUB-20 Mediahost deletion cascade + watch-state/match archives restored on re-enroll
- [ ] HUB-5 Providers behind a common trait *(TMDB, TVDB, AniDB titles,
      AniList, MusicBrainz + Cover Art Archive live; no formal trait yet)*
- [ ] HUB-6 Descriptive metadata *(titles, plots, dates, ratings, posters, episode
      stills live; cast/genres not stored)*
- [ ] HUB-7 Provider rate limits/caching *(API keys via settings + poster caching
      live; no per-provider TTL cache layer)*
- [x] HUB-8 Ambiguous matches flagged for manual review (card-based review UI,
      per-item re-match/search dialog)
- [ ] HUB-9 Local metadata as authoritative provider *(embedded music tags win;
      NFO files not read)*

## Hub — subtitles

- [ ] HUB-21 External subtitle providers *(OpenSubtitles designed, not built;
      blocked on API key)*
- [ ] HUB-22 Hash-preferred subtitle matching *(oshash already computed per file)*
- [x] HUB-23 Subtitles stored hub-side only (mediahost link has no write operation)
- [ ] HUB-24 User-initiated subtitle downloads
- [x] HUB-32 ASS/SSA first-class: faithful extraction (header + re-timed events),
      JASSUB rendering with embedded fonts, live session-pipeline tap (all embedded
      text codecs), mediahost extraction facility with index-driven sparse reads
- [ ] HUB-32a ASS fallback policy *(flatten live and labeled; burn-in not built;
      no per-library/user policy or playback-info reporting yet)*

## Hub — users, API, playback

- [ ] HUB-10 Multi-user *(accounts, admin flag, per-user watch state live;
      per-library access grants and parental controls missing)*
- [ ] HUB-11 Versioned HTTP/JSON API *(live; no WebSocket/SSE channel yet)*
- [ ] HUB-12 Browse/search/filter/sort *(hierarchical browse + client-side title
      filter live; no server-side search/sort/filter endpoints)*
- [x] HUB-13 All hub state in embedded storage; survives restart without rescan
- [ ] HUB-14 Capability-profile negotiation *(mode chosen per source
      container/codecs; no client-supplied capability profile yet)*
- [ ] HUB-15 Full negotiation matrix *(container/codec compatibility + text subtitle
      delivery live; subtitle burn-in, HDR/channel-layout decisions pending)*
- [x] HUB-16 Cheapest-path preference: direct play > remux > transcode
- [x] HUB-17 HLS delivery for remux/transcode (EVENT playlists, mid-stream seek)
- [x] HUB-18 Sessions: per-user concurrency caps, progress checkpoints/resume,
      idle reaping, seek-anywhere with pipeline restart
- [ ] HUB-19 Music: playback + queue live *(gapless delivery and ReplayGain
      pass-through not implemented)*

## Hub — web interface

- [x] HUB-25 Embedded web UI compiled into the binary
- [x] HUB-26 Admin UI: enrollments w/ code entry, satellites, library composition,
      providers, rescan, users, match review
- [x] HUB-27 MVP player: login, browse, detail w/ stream info, direct/remux playback,
      audio/video/subtitle track selection, resume, watch state
- [x] HUB-28 Web UI is a pure client of the public API

## Hub — anime (HUB-29..33)

- [x] HUB-29 AniDB/AniList providers: titles-dump identity, anime-lists ID
      mapping, AniList metadata + relations, UDP FILE-by-ED2K gold path
      (registered client "kahawai", account via admin page, optional
      encrypted session, never-ask-twice cache)
- [ ] HUB-30 Fansub filename conventions *(group prefixes, absolute numbering,
      CRC tags, bracket stripping, hash-exact show identification live;
      per-EPISODE hash identification and version tags pending)*
- [ ] HUB-31 Native anime structure *(absolute numbering via TVDB absolute
      order; relations graph + related-items UI live; season-view projection
      pending — the mapping's per-entry season data is already parsed)*
- [x] HUB-32 (see subtitles above)
- [ ] HUB-33 Dual-audio defaults *(manual audio track switching live; per-user
      original/dub preference not built)*

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
- [ ] OPS-2 Login throttling / lockout
- [x] OPS-3 `doctor` command with plugin/encoder checks
- [x] OPS-4 Clock-skew tolerance (backdated certs, enrollment skew warning)
- [ ] OPS-5 Online backup/restore command
- [ ] OPS-6 Quota-bounded caches with eviction *(subtitle/artwork/session caches
      currently unbounded)*
- [ ] OPS-7 Cross-version satellite compatibility *(major-version gate only;
      no previous-minor guarantee mechanism yet)*
- [ ] OPS-8 Reverse-proxy support *(no forwarded-header/base-URL handling)*

## Non-functional (NFR)

- [ ] NFR-1 Start-latency targets *(direct/remux starts are fast in practice;
      not formally measured against targets)*
- [ ] NFR-2 Scale targets *(37k files / 1 mediahost / 3 transcoders live;
      250k/10/5 untested)*
- [x] NFR-3 No user-state loss on crash; media never written
- [x] NFR-4 mTLS everywhere inter-module; token auth on client API
- [ ] NFR-5 Portability *(Linux x86_64 + macOS transcoder proven; aarch64 untested)*
- [ ] NFR-6 Operability *(structured logging live; metrics + health endpoints missing)*
- [x] NFR-7 Versioned client API (`/api/v1`)
- [x] NFR-8 Codec support delegated to system GStreamer; no bundled patent codecs
      (README documents the licensing posture)

## v1 acceptance criteria

- [ ] 1. All-in-one mixed-library end-to-end *(video+music+enrichment+web player
      live; anime AniDB matching and PGS handling missing)*
- [x] 2. Direct play w/ byte-range seek; remux w/ mid-stream subtitle/audio switching
- [x] 3. Modular three-machine deployment (dev box hub + NAS + macOS transcoder)
- [x] 4. Mediahost kill mid-playback: unavailability surfaces, reconnect restores
- [x] 5. Enrollment / deletion / re-enroll with watch-state + match restore
