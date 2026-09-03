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
- [x] AR-4 Multiple mediahosts/transcoders per hub; multiple collections and
      independently enrolled hubs per mediahost. One local engine scans and
      discovers once; each hub link reconnects and renews independently
- [x] AR-5 All-in-one: hub + in-process mediahost in one process — the
      module logic unchanged, the link transport replaced by channels
      (no gRPC/TLS/enrollment for the local module) and the byte plane
      replaced by direct file reads (AR-11 short-circuit); the satellite
      listener stays up so external mediahosts/transcoders dial in. Plain
      hub workers stop at remux/audio-only encode; AIO may additionally
      enable full local video transcoding
- [x] AR-6 Disconnect tolerance: collections go unavailable, never deleted.
      A transcoder that drops has its sessions moved to another box
      (`reschedule_for_transcoder`), and only the ones that cannot be moved
      are ended. A mediahost has nothing to be moved to — the bytes are on
      it — so its sessions are ended rather than left stalling on a dead
      lease, which turns silence into the 410 the recovery contract defines.
      Starting again then answers 503, not 409: every other refusal is about
      the item and will refuse forever, this one is about the moment
- [x] AR-7 Versioned protocol; Hello/HelloAck major-version gate
- [ ] AR-8 *(optional v1.x)* Delegated direct-fetch tokens
- [x] AR-9 Control plane client ↔ hub only
- [x] AR-10 Direct play mediahost → hub → client with byte ranges; hub-side
      remux and audio-only transcode with video copied. Any video encode,
      filter or subtitle burn requires a full external or AIO transcoder
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
      running it at 5×. Plain hub declares no video capability and runs
      no video benchmark; its lightweight audio execution is structural.
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

- [x] MH-1 Collections: the configured name is the stable per-mediahost
      collection identity; each root has a deterministic full-SHA-256 token
      derived from its absolute lexically normalized configured path. Exact
      root identity survives scanning, manifests/worklists, storage, sessions
      and byte leases; equal relative filenames in separate roots stay distinct
- [x] MH-2 Scan on start/demand + watching + sweeps; on-demand scans are
      collection-scoped only (the global rescan is gone), with interim
      progress reports every 500 files. Files and failures commit to the local
      SQLite catalogue before any hub observes them
- [x] MH-3 GStreamer discovery for technical metadata, including exact PAR,
      normalized display orientation and resulting display dimensions, and
      distinct recording Artist and Album Artist tags. Existing music rows
      advance through a durable local mapper generation during ordinary scans;
      a generation advance republishes even identical discovery JSON so the
      hub reruns its mapper, while a failed re-probe retains the playable old
      row at the old generation for retry.
      Other legacy technical rows use bounded local exact-source scheduler
      worklists; results and terminal failures are source-owned and
      catalogue-revision-guarded
- [x] MH-4 Sidecars + artwork + attachment declaration: embedded fonts are
      declared (name/mime/byte range, payload never read) in the file record
      at scan via a sparse EBML walk; missing records are retried by one local
      fixed-priority worklist shared across hubs (SeekHead-guided early stop).
      VobSub sidecar pairs (.idx/.sub) are discovered at scan, one entry
      per track inside the .idx, and served through the image pipeline
      (extraction/OCR; no tap, so no overlay/burn).
      Subtitle sidecars are part of the MH-5 sidecar signature, so a
      pair appearing next to an unchanged media file busts the fast-path
      and is discovered on the next scan
- [x] MH-5 Content identity (size/mtime fast path; head/tail xxh3 + oshash) with
      local seen-generation reconciliation and versioned projection replay
- [x] MH-6 Byte-range lease serving (dedicated byte-plane connection); reads
      resolve the one root token carried by the source and never search roots
      in configuration order
- [x] MH-7 Scan batching + incremental rescans keep large scans cheap
      *(no explicit rate-limit knob; stat batching + in-sync skip in practice)*
- [x] MH-8 Unreadable files reported with diagnostics (FileError)
- [x] MH-9 ED2K hashing: locally selected fixed-priority background job,
      eMule/AniDB variant, filename-CRC32 verify in the same pass; exact-source
      results persist locally and replicate to every subscribed hub
- [x] MH-10 Protocol 4 local catalogue: per-collection epoch/version, current
      rows plus tombstones, hub-supplied durable cursors, incremental replay or
      full live snapshot. Committed versions push each connected hub to resume
      from its own cursor without polling; hub ACKs are independent and
      reconnect never walks the filesystem. Protocol 4 intentionally rejects
      protocol-3 satellites
- [x] MH-11 One source-owned scheduler admits scans, watcher installation,
      exact-source probes/hashes/extraction, segment/loudness analysis and
      standalone or all-in-one reads as atomic CPU + storage-domain bundles.
      Fixed semantic priority never ages; conflicting higher work interrupts
      at checkpoints, independent I/O-only work overlaps, and protocol-4.1
      playback/up-next hints temporarily make matching segment work demand
- [~] MH-12 WITHDRAWN 2026-08-02, false premise — see the amendment in
      the requirement. Built and reverted the same night: probing one
      host read 30 s for `movies` and 23 ms for `anime` on the same
      disk, so the cost varies per FILE, not per host, and a per-host
      declaration cannot describe it. Nothing gates on host access; a
      host too slow to walk an index is too slow to serve video and
      fails at playback, where the failure is

- [x] MH-13 Source-local season analysis: the mediahost groups and selects
      exact sources from its local catalogue, size/mtime-guards them, stores
      boundaries locally and projects them to each hub. A hub can wake or hint
      exact demand but not schedule it. Audio stops video before decode, video stops audio, and
      keyframe inspection decodes neither

## Hub — registry, resolution, enrichment

- [x] HUB-1 Registry of mediahosts, collections, transcoders (live + persistent)
- [x] HUB-2 Libraries composed from same-typed collections
- [x] HUB-3 Collection-scoped identity: each item belongs to one collection;
      alternate sources deduplicate only within that item/collection. Libraries
      compose collections and reuse the same item IDs/watch state; equal works
      in different collections remain independent (provider/manual/query/watch
      state included). Playable renditions are explicit: one source row owns
      one ordered file set, multipart families are root/directory/release-local,
      and incomplete or ambiguous editions cannot mix with another edition.
      `files` contains physical facts only; rendition ownership and ordinals
      live in the explicit source tables. Direct level-52→59 migration and
      conflict-detecting migration-56 replay are runnable and real-catalogue
      proven.
- [x] HUB-4 Filename/dirname parsing (movies, episodes, anime conventions,
      music layout). Music albums group on Album Artist while tracks retain
      their recording Artist; missing Album Artist falls back to path artist,
      then recording Artist. Mapper-only correction reparents stable track
      identities (merging state only on a real slot collision), invalidates an
      obsolete automatic MusicBrainz answer while preserving a human pin, and
      never transfers state across different content at the same path
- [x] HUB-20 Mediahost deletion cascade + watch-state/match archives restored on re-enroll
- [x] HUB-5 Provider trait + declared chains + walker (TMDB, TVDB, anime
      composite, MusicBrainz + CAA), plus Fanart.tv → TheAudioDB Album Artist
      artwork
      keyed only by an exact MusicBrainz artist identity. Album credits supply
      it first; otherwise a direct artist search must return exactly one strict
      name match. Which record an item IS is derived
      by triggers from stored answers, chain order, refusals and pins —
      design in implementation §4.1/§4.2; per-media-type ordering is the
      2026-07-26 amendment recorded in the requirement.
- [x] HUB-5a No pass is gated on another chain's credentials. The TMDB
      key is optional and its provider is added only when set; TMDB's
      own passes (episode detail, detail backfill) skip themselves
      without one. Every chain pass runs regardless — `run_chain`
      already skips providers the set does not hold, and which
      providers exist is the operator's choice, so an absent one is
      silent rather than a warning. A stored credential that will not
      open is that provider's absence too, not the end of the run
      (`Enricher::usable`), which is how both hub-wide reads answer.
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
      TheAudioDB defaults to its public `123` key at 30 requests/minute and an
      admin may store a premium user key for its documented 100/minute tier.
      TMDB, TheTVDB and AniDB plaintext snapshots carry revocable runtime
      leases: replacement wakes queued/paced work before its next send,
      creates no retry debt, and coalesces one fresh follow-up pass.
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
- [x] HUB-32a ASS fallback policy — an ORDERED, capability-driven ladder.
      `native` (the client's own renderer) always wins when declared and
      is not orderable; the fallbacks — flatten, rasterised overlay
      (HUB-32d), burn-in — are a per-user ORDER (`ass_order`, default
      flatten → overlay → burn) and the server takes the first rung this
      client and this fleet can actually serve. The order is a
      permutation, never a subset: reordering only, so the ladder always
      resolves and no session can be refused for want of a tier. Picking
      a track says WHICH subtitles, never how they are delivered.
      Burning runs `assrender` in the encode
      chain: embedded tracks take the demuxer's own pad (the only path that
      carries the release's attached fonts — verified live, a script asking
      for Calibri on a box that has no Calibri), a user's sidecar `.ass` is
      played in from an appsrc and ships to the worker like display sets do.
      A burn forces the video encode that carries it and hard-filters
      placement onto a box reporting `ass_burn`. Nothing ever flattens
      silently: with no capable box the session refuses with a 422 the
      client matches (`ass_burn_unavailable`) and offers flatten or stop.
      A client may also declare it renders no timed text at all
      (`vtt_render`, maskable like the others): the flatten rung
      disappears from the ASS ladder and plain text — SRT, sidecars,
      HUB-32c OCR output — falls to burn-in, the same last resort an
      image track takes when the client cannot composite. Every browser
      renders WebVTT, so this exists to make that fallback reachable
      from the capability mask rather than only in theory.
      *(Per-LIBRARY policy is not built — per-user only. The refusal was
      verified structurally, not live: every box in this fleet has
      assrender, including the hub's own, so the condition is unreachable
      here.)*

## Hub — users, API, playback

- [x] HUB-32d Rasterise ASS server-side into a HUB-32b bitmap track for
      overlay-capable clients — no encode, full typesetting. Generated
      when the ladder reaches that rung at session start — never by a
      button, because rasterising is how a tier gets served and not a
      decision a user makes — deduped by `derived_from` like HUB-32c
      OCR, stored as a first-class `raster` row plus the same NDJSON
      the PGS tap writes, and served item-level rather than through the
      session tap — so it needs no running pipeline and works on direct
      play. Rendered once at the source's coded size; the client scales
      it uniformly by width exactly as it does PGS and burn-in.
      Auto-selection ranks client-native ASS → rasterised overlay →
      flattened text. Cost was the reason this was deferred and is now
      measured: ~15 MB per 24-minute episode (~18 MB with a sung
      OP/ED), because cost follows the COMPOSITION CHANGE RATE and
      every real script sits at 2-7% — numbers and method in the
      `assraster` module doc.
- [x] HUB-10 Multi-user: accounts with create and delete, a settable admin
      flag,
      per-user watch state, and per-library access grants — a
      `users.all_libraries` flag plus a `user_libraries` list
      (`hub/grants.rs`), enforced on browse, cross-library search, item
      detail, children, artwork, fonts, subtitles, collections and
      playback. Admins are not bound by grants; denials answer 404.
      Managed from the admin UI's users panel and `kahawai-users.sh`.
      `PUT /admin/v1/users/{id}/admin` promotes and demotes
      (`kahawai-users.sh promote|demote`), including self-demotion when another
      admin remains; it refuses to demote the last admin with 409. The role and
      durable access generation change in one statement, so the token that
      authorized a demotion is rejected on its next request and mutable admin
      state always comes from the user row. `DELETE /admin/v1/users/{id}` still
      refuses deleting the account currently making the request, refuses the
      last admin with 409, and answers 404 for a stranger. Its last-admin guard
      and delete are one guarded statement under an immediate transaction, so a
      concurrent demotion/delete cannot take the total to zero; a refused delete
      ends no sessions.
      Grants are untouched by a demotion — the account falls back to the
      `user_libraries` rows it already had. Every user-facing route below a
      playback session id crosses one owner middleware; stream, playlist,
      segment, subtitle, seek, progress and end all return the same 404 for an
      absent id and another user's live id. `direct_play_ranges_end_to_end`
      exercises every shape with two users; admin session routes remain behind
      their separate administrator gate.
      Parental control needs no separate mechanism: it is a library the
      admin composes and grants.
      Watch state is writable without playing: `PUT
      /api/v1/items/{id}/watched` marks an item watched or unwatched
      (`kahawai-watched.sh`), for something seen elsewhere or a tick
      undone. Either direction clears the resume position, since
      "watched, and also 40 minutes in" is not a state a card can draw.
      `play_count` only climbs — unmarking changes what is shown, not
      what happened.
      Progress writes the same two fields on the same rule. `played` is a
      boolean and NOT a high-water mark: it is what the last report said,
      so watching something again clears it and finishing sets it once
      more. With one exception, and it is load-bearing: a report at
      position ZERO decides nothing and leaves the mark and its viewing
      timestamp alone. Zero
      arrives for things nobody has touched — the audio queue pings it
      every ten seconds for the track it has preloaded for a gapless
      handover (`keepalive.ts`), and the video player answers with the
      resume position, now absent on a played item, until the media
      element has its metadata. Letting those clear the mark wiped the
      seen ticks one row ahead of the playhead through an album already
      heard. So the first position that is NOT zero decides, which puts
      the reset a ping behind a restart and off the path of anything that
      never started. The exception covers BOTH halves of the rule — the
      stored column and the session's own memory of where it got to — or
      an item reads as played while the session that played it has
      forgotten, and the play goes uncounted.
      `play_count` is the record of how many times it was finished, and it
      rises once per WATCH, written when the session stops (`sessions.rs`,
      the one funnel every teardown goes through). Not when the 90 percent
      line is crossed: that counted a second play for anyone who scrubbed
      back over the line and forward again, and counted one for a viewer
      who then abandoned the thing half way.
      Which session counts is decided by which one did the watching, not
      by who ended it. A session counts when it stops past the line AND it
      is the one that took the item there — its flag is seeded from the
      position it OPENED at, so a session that opens past the line never
      sees a crossing. That is what keeps a sitting split in two by a
      reaped pause, a dead transcoder or an admin from counting twice: the
      client answers a lost session by starting again at the same position
      (`recovery.ts`), and that continuation earns nothing while the watch
      that crossed the line keeps its play, wherever it happened to stop.
      Asking instead WHO stopped the session cannot work in either
      direction — the answer would have to exclude the reaper, and a
      viewer who finishes something and closes a laptop that never sends
      its DELETE is reaped too.
      Both flags are per SESSION and not read back from
      `watch_state.played`, so nothing else writing that column — a mark
      by hand, another device — can put a play in a session's name, and a
      session that reported no position at all cannot bank the play a
      previous watch left marked.
      **Known gap, accepted:** that flag lives in memory, so a hub restart
      while a finished watch is still open loses its play. Counting at the
      90 percent crossing was durable and bought that at the price of the
      two miscounts above; a durable version of this would need the
      crossing written somewhere a restart survives, which no one has
      wanted yet. A restart is not one of the teardown paths listed
      above.
      What makes "again" happen at all is that a played item is served with NO
      `resume_position_ms`, so the next Play begins at the beginning
      rather than dropping the viewer into the last tenth. That is
      deliberately server-side: a client that had to know "ignore the
      position when played" is a rule the API failed to express, and every
      third-party client would have to carry it. Answered at read time and
      NOT by storing a zero, because `watch_state.position_ms` is also
      where a re-dispatched transcode picks the stream back up (AR-6) —
      zeroing it restarts a failed-over film from the top for anyone in
      its last ten minutes, which is what the first cut of this did.
      The cost is named: a viewer who watches to the end and then rewinds
      and stops half way has neither the mark nor the play — `played`
      follows the playhead, and the count reads it where the watch
      stopped.
      An `items` list marks a batch — a season, or a whole show — in one
      call and one statement, so it cannot half-apply; the client decides
      which episodes a season holds, because the season a viewer sees may
      be a projection of absolute numbering (HUB-31). Every id must be the
      addressed item or one of its children, which is what lets a single
      access check cover the batch: access keys on
      `COALESCE(parent_id, id)`. Ids outside it are skipped, not reported.
      Checked by `tests/watch_mark.rs`, including that a batch cannot
      reach into another show.
- [x] HUB-11 Versioned HTTP/JSON API + /api/v1/events SSE channel
      (invalidation hints: scan progress, satellite connectivity,
      sessions, enrichment; cookie-authenticated for EventSource).
      API clients use explicit bearer mode. Browser mode keeps its access JWT
      only in memory, schedules rotation from the returned `expires_in` rather
      than decoding the bearer, and uses host-only HttpOnly refresh/media
      cookies; reload rotates the refresh family, and logout clears both cookies.
      Rotation accepts the PREVIOUS generation as well as the current one: an
      honest client holds a retired token whenever its response was lost, or
      when a second tab rotated first — `navigator.locks` serialises tabs only
      per ORIGIN, while cookies ignore the port, and the LAN address over plain
      HTTP is not a secure context and has no locks at all. Anything older is
      still replay and still revokes the family. Known gap, pinned by
      `a_jar_left_on_the_earlier_answer_is_the_one_case_still_lost`: two
      overlapping refreshes in a row, where the jar keeps the earlier of two
      racing answers, still lose the session — a third generation would close
      it at the cost of another rotation's life for a stolen token.
      The media cookie is accepted only by the explicit read allowlist, while mutations
      remain bearer-only. When `hub.public_url` is configured, browser login,
      refresh and logout require that exact canonical Origin; when absent,
      Origin validation is disabled. Access JWTs retain the explicit HS256-only
      allowlist, fixed issuer,
      API audience and signed `access` credential type; mutable account state
      and `auth_version` come from the database on every request. Refresh
      families remain hashed, single-row and single-winner, with replay, logout
      and password-reset revocation
- [x] HUB-12 Browse/search/filter/sort. Hierarchical browse, one
      endpoint for browse and cross-library search with server-side sort
      and paging, item detail with stream info, artwork at named sizes,
      playback and admin endpoints. Client behaviour and the API shape
      are in implementation §4.4/§4.7.
      Music libraries additionally expose a paged Album Artist index and a
      paged, chronological artist-album route. Both web grids virtualise those
      pages and fetch visible chunks without a load-more control. Grouping
      happens before paging, and chronology uses the same embedded-or-enriched
      release year the album card displays;
      search returns matching artists alongside albums and songs and matches
      track titles within an artist's albums. A strict synthetic `artist_key`
      keeps fuzzy-search equivalents such as “One” and “1” distinct; a rename
      changes the route key. A durable artwork projection requires exactly one
      credited-artist MusicBrainz identity, tries Fanart.tv before TheAudioDB,
      prefetches the selected portrait and every named size before exposing its
      cache version, and otherwise generates the same sizes from a stable
      collage of the newest four covered albums. Collages are scoped to the
      requested library, and the serving path revalidates every contributing
      album against current library membership so a cached cover cannot cross
      a grant boundary after any composition change. Both are served through a
      library-grant-checked endpoint (`kahawai-list.sh -r/-A`).
      Legacy MusicBrainz rows are resolved per distinct release group and
      synthetic artist until every identity agrees or a conflict makes the
      portrait ambiguous; backfill never crosses into a differently tagged
      artist group that shares the release.
      Personal Fanart credentials use its documented secret-bearing `client-key`
      header, and the enrichment status covers the full prefetch rather than
      ending before it. A provider backoff stops the batch and schedules one
      artist-only retry after the quiet period; it does not repeat the full
      catalogue enrichment pipeline. A Fanart authentication refusal stops
      immediately and waits for a replacement credential rather than failing
      once per artist. Provider transport/server failures coalesce into the
      same artist-only retry; a systemic local cache-write failure stops rather
      than downloading once per remaining artist. A ready row whose original
      or any named derivative is absent/empty is re-prefetched and versioned
      again instead of remaining a permanent 404. Saving Fanart or TheAudioDB
      credentials or
      attaching an existing collection schedules this artist-only pass, not
      full enrichment. Portrait selection prefers Fanart's square
      `artistthumb`, then uses an `artistbackground` when the square form is
      absent.
      `in_progress=true` narrows the same endpoint to what is meaningfully
      started and unfinished, most recently watched first — the
      continue-watching row (`kahawai-list.sh -p`). Meaningful means both one
      minute and one percent of the reported runtime; the larger threshold
      wins, so a probe of a long film is not promoted while a short episode
      does not inherit a film-sized fixed delay. Its own query shape, driven
      from `watch_state` rather than from `items`, because the set is "rows
      this account has a position in": 3525 items answered as 19 without
      a candidate scan. It is not a `sort` name — the browse's watch join
      is in the outer dressing query, and pulling it into the candidate
      scan is the join-first shape that costs 912 ms. `sort` and `q` do
      not apply to it; `library` still scopes it, and grants still bind
      it. Checked by `tests/browse_in_progress.rs`, including that a
      withheld library's item cannot appear even when the account has a
      position in it.
      `GET /api/v1/up-next` is the row beside it: one episode per series,
      the one after the last you FINISHED — not the first unwatched,
      which is a different answer whenever an episode was skipped. A
      series appears when this account has finished an episode of it, is
      not meaningfully part-way through one (the same
      one-minute-and-one-percent predicate continue watching is made of, so
      the two rows partition the series between them instead of both claiming
      one). A barely opened next episode therefore remains eligible in Up
      Next. The series must also be still current: watched within the last
      month, OR the episode it would
      offer was added within it — the second half is the season that
      starts up again after a year off. "Added" is the item id, which is
      a ULID and so already sorted by the moment it was minted; the cut
      is the smallest ULID of that moment, which is the same thing
      `sort=-added` orders by. Ordered by the series' last viewing, most
      recent first (`kahawai-list.sh -n`). Same shape and same page as
      the browse: `ItemsResponse`, `library` scopes it, grants bind it —
      correlated on the SHOW, since an episode belongs to a library
      through its parent. Checked by `tests/up_next.rs`, including a
      series brought back by a new episode alone beside an otherwise
      identical one that stays quiet.
- [x] HUB-13 All hub state in embedded storage; survives restart without rescan
- [x] HUB-14 Capability-profile negotiation: browser-probed profile with
      every play request, hub decides per stream (`negotiate.rs`,
      `tests/negotiate_play.rs`); explicit mode = operator force. Session
      responses carry aggregate plan cost separately from pipeline mode, so
      player/admin labels say TRANSCODE when either elementary stream is
      encoded even if an audio-only encode runs in the hub-local HLS pipeline
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
      candidate judged, direct > copy > audio-enc > video-enc, rank ties.
      The same line is the execution boundary: audio-only encode remains
      hub-local; video encode requires a full external/AIO transcoder.
      Completeness outranks cost: a source with a stream this client
      cannot be given loses to one that delivers everything, even at a
      full video encode — a silent playback is a defect, an encode is a
      bill. Strictly a DROPPED stream; a source that never had video
      (music) or audio is not penalised for what it never had.
- [x] HUB-17 HLS delivery for remux/transcode (EVENT playlists, mid-stream seek).
      `parsebin` is stopped from parsing any stream we parse ourselves
      (`autoplug-continue` answers with `parser_for`), so each stream
      has exactly ONE parser. Double-parsing was a no-op for h264/ac3
      and fatal for AV1 — parsebin parses it to `alignment=frame`,
      losing the buffer timestamps, and every AV1 copy session then
      died producing no playlist (fixed 2026-08-04).
      `EXT-X-TARGETDURATION` is declared from the source's measured
      keyframe spacing on copies (median 10.0 s here, worst 147 s; the
      2 s previously declared was wrong for 87% of files, RFC 8216
      §4.3.3.1). The client states which of three things it needs
      (`target_duration`, required, no default): `ignore` keeps the old
      constant and the old violation, `accurate` declares the truth,
      `short` guarantees a ceiling and forces a video encode when the
      source cannot meet it.
      *Why keyframes enter at all: `splitmuxsink` and `isofmp4mux` both
      close a fragment at the first keyframe past the fragment target,
      and a copy has no encoder to request one from. HLS does not
      require it (§6.2.1 SHOULD; §3 permits discarded leading frames),
      so a segmenter cutting on a time grid would make the declared
      value a constant of our choosing and delete this whole
      mechanism. Not built: no shipped GStreamer segmenter does it.*
      *This does NOT fix the ExoPlayer hang it was prompted by; the
      pacer does. That was a liveness failure — production held at
      viewer+120 s, the playlist unchanged for 40+ s (measured, copy AND
      transcode), and ExoPlayer errors after 3.5x the declared value of
      no change — and a larger declaration only widened the margin. The
      pacer now releases on playlist age as well as viewer position, so
      a client that never reports a position keeps being served at
      about real time instead of freezing: measured on a live session,
      20 segments then frozen for 65 s before, 20 → 33 segments (1.03x)
      after. A paused player never tripped it in the first place — that
      claim was wrong, see `kahawai-implementation.md` §4.6 — and a VOD
      playlist would remove the contradiction rather than bound it, but
      nothing observed now requires one: `kahawai-vod-plan.md`.*
- [x] HUB-18 Sessions: per-user concurrency caps, progress checkpoints/resume,
      idle reaping, seek-anywhere with pipeline restart. A reaped or otherwise
      absent session answers the same owner-scoped 404 as a foreign live id,
      and both players recover from it automatically
- [x] HUB-19 Music: playback + queue live, gapless delivery (two elements,
      the idle one warmed 30 s ahead) and ReplayGain pass-through. Every
      track plays direct — the browser gets a byte-range URL and no
      pipeline is built

## Hub — web interface

- [x] HUB-25 Embedded web UI compiled into the binary. Every production web
      response carries the static CSP, no-referrer, explicit minimal
      Permissions Policy and nosniff boundary; API/setup responses carry
      nosniff too. The production-bundle Chromium gate executes JASSUB's real
      worker/WASM path and both positive and negative policy cases in CI and
      release source gates (`scripts/kahawai-csp-check.sh`)
- [x] HUB-26 Admin UI: enrollments, satellites, libraries (with per-library
      refresh + live per-collection scan progress), providers, users,
      match review
- [x] HUB-27 MVP player: login, browse, detail w/ stream info, direct/remux playback,
      audio/video/subtitle track selection, resume, watch state
- [x] HUB-28 Web UI is a pure client of the public API — including session
      recovery, which is driven by the owner-scoped 404 contract rather than by
      any client-side copy of the hub's idle timeout. Every 4xx/5xx is
      `{code, message, request_id}` with `code` enumerated in the OpenAPI
      document and the server-generated correlation id also returned in the
      response header; the status carries transience (429/503 clear, other 4xx
      are final), so a third-party client needs no table of kahawai's codes
      (`tests/error_bodies.rs`, `web/test/errors.test.ts`)
      Library grants take a `grants_version`, so two admins editing one account
      cannot silently discard each other (UI-25). Two more gaps are closed on
      the wire and not yet on screen, so they stay open in the UI ledger:
      `duration_ms` (UI-4) and `source_id`/`part`/`parts` (UI-27)

## Hub — anime (HUB-29..33)

- [x] HUB-29 AniDB/AniList providers: titles-dump identity, anime-lists ID
      mapping, AniList metadata + relations, UDP FILE-by-ED2K gold path,
      question-keyed never-ask-twice (`provider_queries`, 0044)
- [x] HUB-30 Fansub filename conventions: group prefixes, absolute
      numbering, CRC tags, bracket stripping, designators with season-0
      bands, per-episode hash identity + re-binding (`ed2k_aid`,
      `tests/hash_binding.rs`), generic release revisions (0043),
      bare-file identification + movie minting, batch-marker spans
      (0045). Cross-aid re-binding DECLINED 2026-08-06 (measured: 213 of
      217 aid disagreements are AniDB's per-season splitting of Pokemon,
      already correctly placed — moving them would break correct slots).
      Collision breaking BUILT: several files on one slot whose hashes
      name different eids are different episodes; the lowest eid keeps
      the slot, the rest take free numbers in the SAME season (absolute
      stays absolute), watch state follows the file
      (`break_slot_collisions`, tests/hash_binding.rs). Proven live:
      Megazone 23 pt.03-a/b shared S00E023 (eids 39483/39484) and came
      apart into absolute 23 and 24
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
      Covers embedded image tracks AND VobSub sidecar tracks. OCR attempts
      interrupted by a mediahost disconnect retry when that host reconnects;
      corrupt/local failures remain suppressed for the hub run.
      Deferred: the bandwidth-threshold selection (needs measurement)
- [x] Subtitle unification (HUB-32c mechanics amendment, 2026-07-31):
      one `subtitle_tracks` keyspace for every origin
      (embedded/sidecar/downloaded/ocr), synced at scan with stable ids,
      OCR lineage via `derived_from` (per source — the multi-source flag
      bug died with the name parsing). Every row has one direct owner:
      physical streams and their OCR/raster derivatives carry stable
      `source_id`; downloaded/manual tracks carry `item_id`; derivatives
      inherit their parent's owner. Source rebinding needs no track rewrite
      and source/stream deletion evicts reproducible derivatives. Capability
      adjusts each track's
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
- [x] HUB-37 Recap / intro / credits detection, stored per episode and
      carried on the item QUERY; the web player offers a skip
      button while the playhead is inside one. The hub's background sweep
      chooses seasons in watched-first order and remains the sole persistence
      authority; the owning mediahost performs the decode work locally, yielding
      to scans and viewer leases, so analysis sends no media bytes over a lease.
      The admin API reports what is left and can run the next season now.
      Successful scan rows retain the historical rendition-mtime membership
      rule. Unreadable exact module/collection/root/path/size/mtime revisions
      land in a separate failure table, so another rendition may be tried and
      hourly retries stop only when every current source has failed. Source or
      analyzer changes retry and later success clears remembered failures.
      Chapter-name analysis stays hub-side over stored source facts: a season
      that names its own opening and credits is answered without
      dispatching work. Per-file chapter-kind masks are normalized on ingest and
      generation-backfilled from stored JSON, so old mediahosts skip generic
      chapter lists without hiding genuinely complete named seasons. Design in
      implementation §4.9; detector parity remains measured against
      intro-skipper in `docs/intro-detection-results.md`.
- [x] HUB-38 Measured audio loudness normalization: the mediahost background-decodes
      every non-music audio stream once and meters the untouched decoded layout
      plus every smaller canonical output matrix playback may choose. The hub
      stores revision-guarded EBU R128 integrated-loudness/true-peak pairs keyed
      by exact channel count and mask. Workers select gain only after their
      post-conversion caps are known, apply one static move toward −18 LUFS
      capped at −1 dBTP, and never derive a fold from native scalar facts.
      An account-global setting defaults to gains on existing encodes, can
      disable gain, or can force measured direct/copied audio through an encode.
      Force retains the ordinary video mode and falls back before transcoding
      when the source is multipart, the measurement is stale/missing, the
      executor lacks layout-map support, or its exact output layout is not
      measured and locally preflighted. Music remains owned by ReplayGain.
      Full-file work stays source-local, revision retry remains possible after
      later scans, and session facts state the exact applied dB gain.
      Full-file rebuild work is source-local and admitted by the universal
      scheduler. Any higher fixed-priority job needing the same CPU or storage
      domain preempts loudness at the next audio buffer; the same pipeline
      resumes afterward without re-decoding.
      A 60-second no-buffer watchdog advances past decoders that produce neither
      audio, EOS nor an error; active callbacks are exempt so intentional
      foreground/segment pauses remain unbounded.
      One cross-collection queue chooses movie files before series/anime and
      newer source mtimes first within each category, re-evaluated after every
      file and after scheduler waits; an in-flight file pauses at decoder
      checkpoints and resumes in place.
      Long-running segment and loudness jobs trim glibc's freed decoder-thread
      arenas every 30 seconds and again at completion, so sequential full-file
      work returns resident memory instead of retaining its high-water mark;
      non-glibc targets are unchanged.
- [x] Chapters as a first-class fact: read at scan beside the attachment
      declaration, backfilled for older Matroska/WebM rows (other containers
      keep whatever the demuxer's TOC declared at discovery, so a
      pre-existing MP4 row gains chapters only when its file changes),
      carried on the item (offset
      onto its timeline for a multi-part work), drawn as ticks on the
      player's seek bar and as a list on the item page that starts playback
      at a chapter. Checked against ffprobe by
      `scripts/kahawai-chapters.sh`. No requirement of its own — an
      unplanned feature, recorded here because the three consumers are
      real.
- [x] HUB-36 Pace-aware video placement, on measured capability. Full
      external/AIO transcoders benchmark video encoders and GL tone-map;
      plain hub does neither. Only successful current-fingerprint benchmarks
      become serving capabilities; a crashed child durably quarantines its
      capability until explicit successful remeasurement or fingerprint
      invalidation. Workers meter the un-throttled phase of real sessions into
      a persisted per-(box, work class) EWMA; placement ranks on it and states
      a below-realtime prediction in the verdict rather than letting a viewer
      discover it. Design in implementation §4.5.
      NOTE this requirement's original text asserted that TC-4 "already
      reports a realtime multiple per session". It did not — nothing
      measured pace before this work. Corrected at TC-4.

## Transcoder (TC)

- [x] TC-1 Capability probe reported on registration
- [x] TC-2 Capability + inverse-load placement; admin enable/disable for
      enrolled transcoders. The AIO full local video executor is instead a
      structural `[all_in_one] transcoder` setting (default true): false
      suppresses its video-encoder dry-runs, benchmark and video placement
      while external transcoders remain schedulable. Plain hub still performs
      remux and audio-only transcode and never enters video placement
- [x] TC-3 Sessions fully specified by the hub
- [x] TC-4 Dynamic GStreamer pipelines, HLS segments, supervised worker
      process. Progress reporting is PARTIAL, and the requirement was
      long recorded as if it were not: a per-run pace sample exists
      (HUB-36 — the un-throttled phase, once per run), but there is
      still no continuous progress percentage per session
- [x] TC-5 Cancellable sessions; transcode-ahead pacing window
- [x] TC-6 Resource ceilings, as amended 2026-08-08: `max_sessions` at
      placement, and CPU shares as `[transcoder] worker_nice` +
      `worker_threads`, which each pipeline worker applies to itself
      (both default to 0 = today's behaviour; on all-in-one they govern
      the hub's own remux workers too). Runtime degradation to software
      comes from the per-session worker probing its preference list and
      rejecting candidates whose sink caps exclude the session dimensions;
      the session log names the encoder it got. Startup probes likewise choose
      dimensions from each encoder's own caps, avoiding false hardware
      failures from a one-size test frame.
      Struck by the amendment, with reasons and measurements there:
      scratch eviction (unreachable without giving up the EVENT playlist
      players seek in; a run costs 3.0–5.4 GB per content-hour and is
      deleted whole at teardown) and GPU session count (not discoverable
      on VA-API or VideoToolbox). Still open: a hardware failure
      MID-RUN ends the session — the one retry in `dispatch_to` covers
      the sink at start only. cgroup CPU weight is documented, not
      enforced.
- [ ] TC-7 *(optional v1.x)* Offline pre-transcode

## Operations (OPS)

- [x] OPS-1 First-run setup mode: public API locked; atomic initial-admin
      creation through either the loopback-only, same-origin browser listener
      on a port distinct from the public and satellite listeners
      or the mode-0600 local Unix socket, bound only after its mode-0700 control
      directory exists, used by `kahawai hub init-admin`; typed setup outcomes
      distinguish invalid input, a completed race, and internal failure
- [x] OPS-2 Login throttling: consecutive-failure lockout with
      exponential backoff (30 s → 15 min cap), keyed per account (5) and
      per source address (20, higher so a shared NAT survives), failures
      logged with source IP; in-memory, X-Forwarded-For untrusted until
      OPS-8 adds proxy-trust config
- [x] OPS-3 `doctor` command with plugin/encoder checks
- [x] OPS-4 Clock-skew tolerance (backdated certs, enrollment skew warning)
- [x] OPS-5 Online backup/restore (`kahawai hub backup|restore`), taken while
      the hub keeps serving, including the PKI so restored hubs accept existing
      satellite certs without re-enrolment, and excluding caches as
      re-derivable. Manifest v3 inventories every included regular file by
      safe relative path, size and SHA-256, validates into private sibling
      staging, and consumes only those stable bytes before live mutation;
      v1/v2 remain restorable.
- [x] OPS-6 Quota-bounded caches with eviction — satisfied by there being
      nothing eligible to evict; requirement amended 2026-07-26 with the
      audit, reasoning in implementation §10. The one deletion is
      unreachability, not quota: resized artwork whose size left the code
      list, or whose original is gone, dropped at startup.
- [x] OPS-7 Cross-version satellite compatibility: protocol gated on major
      version (Hello/HelloAck). Protocol 4 is a coordinated authority cutover:
      protocol-3 peers are refused with both versions and an upgrade
      instruction. A full mediahost rescan is explicitly accepted; hub-owned
      user state survives while the physical projection is replaced.
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
      The unsanitised bundle remains behind the administrator gate; its
      persistent directory and live run directories are owner-only, and new
      bundle files are created mode 0600.
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
      Five full video executors exercised together (four enrolled
      transcoders plus AIO's enabled local transcoder): eleven concurrent
      video transcodes filled every box to its own max_sessions and no
      further, overflow staying in AIO's local executor. Ten
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
      0.0.4, behind its own static token in `<data_dir>/metrics.secret`;
      no file = not served at all), SIGHUP reload for what can change
      under a running process.
- [x] NFR-7 Versioned client API (`/api/v1`)
      *(One sanctioned exception, taken in place rather than as `/api/v2`:
      the item resource was split by method — `GET` for what was
      discovered, `QUERY` (RFC 10008) for what this client would be
      served — and `GET /items/{id}/subtitles` plus `sources[].streams`
      on `GET` were deleted with it. There are no external clients yet;
      NFR-7 governs from the first one. See §4.4 of the implementation
      doc for the shape and the reasoning.)*
- [x] NFR-8 Codec support delegated to system GStreamer; MIT throughout —
      the OCR tier links leptess/Tesseract (MIT/Apache-2.0), not
      subtile-ocr, so no GPL combined-work consequence exists;
      --no-default-features additionally drops the Tesseract linkage;
      hosted CI run 31788811563 built the complete feature-off workspace

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
