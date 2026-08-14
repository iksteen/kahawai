# MVP readiness checklist

Work identified by the MVP readiness audit. This checklist complements
`kahawai-status-checklist.md`: requirements define intended behaviour, the
status checklist records what is built, implementation documents explain the
design, and this file records scrutinised gaps and candidate gates for closing
them. It does not redefine those documents or decide release scope by itself.

**This file records outcomes, not implementation progress.** Check an item only
when the complete behaviour is implemented, exercised by a named runnable
check, verified against the real runtime or persisted state where applicable,
and reflected in the appropriate public or operator documentation. A restart,
live query or artifact inspection is required when a compiler or isolated test
cannot establish the outcome. Use `[~]` only when a gate is partly satisfied
and state exactly what remains.

Green tests are necessary evidence, not sufficient evidence. Record their exit
codes and the observable postconditions they establish; do not substitute grep
counts or compilation for inspecting what actually ran and landed. Do cite a
measurement that demonstrates a gap, because it states how far the product
still is from the gate.

An unchecked item is an audit recommendation, not automatically an MVP release
blocker. The maintainer-owned evidence matrix classifies each item as
`release-blocker`, `post-MVP` or `not-planned`, with a short rationale. Scope
moves, compatibility withdrawals and new platform/product promises require an
explicit maintainer decision. For architecture, quota, cache or eviction work,
record the decision criteria and cost model before code; cache decisions name
both rebuild cost and latency at the moment of use. The current no-cache-
eviction/no-janitor decision remains in force unless explicitly reconsidered.

The audit found a strong functional candidate: the locked Rust workspace tests,
formatting and clippy pass, and the web application builds and passes its small
test suite. It is not yet ready for public deployment. In particular, deleted
administrator access can survive a quick restart, session endpoints are not all
owner-scoped, restore does not restore configuration and multi-root collections
can alias files. (Refresh rotation was on that list and is not any more: AUTH-4
below is ticked, with `BEGIN IMMEDIATE` and a test that releases two callers
together — this paragraph is the audit's summary and had not been re-read
against the items under it.) The CI implementation added after the
audit now includes the complete locked workspace, no-default and web gates, but
the hosted jobs and first release run remain evidence that must be observed
before those outcomes can be checked.

A follow-up audit of the media core reached a different conclusion from the
functional evidence. The GStreamer paths contain unusually valuable corpus and
field knowledge, but that knowledge is concentrated in a large, callback-heavy
implementation whose ownership, failure and resource invariants are not encoded
in its types. Passing playback tests therefore does not yet establish media-core
code quality. The `GST` findings require explicit disposition in the evidence
matrix rather than being dismissed as post-MVP cleanup.

A review of the `ui-redesign` branch found eleven hub-side issues — two of them
windows that AR-6 widened. They are written up in
`kahawai-hub-review-findings.md` for whoever owns `crates/`, rather than carried
as web work. Three were fixed on the branch after all, where leaving them would
have meant shipping a client that depended on the broken behaviour; those are
marked in that document.

## Release posture (REL)

- [ ] REL-1 Treat internet exposure as the fixed MVP security and operational
      target. The maintainer classifies every audit item as `release-blocker`,
      `post-MVP` or `not-planned` without weakening that target. Describe the
      release as an **MVP candidate** until the accepted blockers pass; a feature
      freeze is a separate maintainer choice
- [ ] REL-2 Reconcile README, requirements, implementation, deployment and
      status documents without collapsing their distinct roles. Every maturity
      or platform claim links to a release check that asserts real observable
      postconditions; requirement-status changes land in the same commit as code
- [ ] REL-3 Publish an evidence-based platform matrix. Mark only roles and
      architectures exercised by current release artifacts as supported;
      retaining, withdrawing or adding a platform is an explicit scope decision
- [x] REL-4 Exact-root source identity is satellite protocol 3 and requires a
      coordinated upgrade of every mediahost and transcoder; protocol 2 peers
      are rejected during `Hello` negotiation with an actionable upgrade
      diagnostic. Protocol 3 has one required exact-source wire shape and no
      root-less compatibility fields or empty-token semantics. This wire break
      must not become a catalogue break: upgrading may not trigger an
      authoritative rescan, reset a scan generation, clear source bindings,
      rematch media, repeat provider work or reauthenticate a satellite
- [ ] REL-5 Keep `/api/v1` explicitly unstable throughout 0.x; on the 1.0
      release, freeze its generated contract and adopt the promised policy of
      serving the current and immediately preceding API major
- [ ] REL-6 Maintain an MVP evidence matrix that maps every checked requirement
      and exit criterion to its CI/release check, asserted runtime or persisted
      postconditions, and any dated manual evidence that automation cannot cover
- [ ] REL-7 Current primary documentation supports constants and compatibility
      claims derived from authentication, browser security, providers,
      GStreamer, libass, codecs and packaging; record the source beside the
      enforcing constant or adapter rather than relying on recalled behaviour

## Authentication and tenant isolation (AUTH)

- [x] AUTH-1 Migration 52 adds `users.auth_version`, revokes every existing
      refresh family, and makes old access tokens undecodable because they lack
      the required generation claim. The field's durable-generation meaning is
      documented in `hub/auth.rs`, beside the code that enforces it
- [x] AUTH-2 Access tokens retain an explicit HS256-only algorithm allowlist,
      signature and expiry validation, and require the fixed issuer
      `urn:kahawai:hub`, API audience `urn:kahawai:api`, and signed credential
      type `access`. Every authenticated HTTP request then loads the user by
      primary key and constructs username and administrator status from the
      database. `auth_api::access_tokens_require_algorithm_signature_expiry_issuer_audience_and_type`
      exercises the complete acceptance boundary; `kahawai-auth-cycle.sh`
      inspects the same claims on tokens issued by a running hub
- [x] AUTH-3 Deletion removes the authoritative user row; role changes and
      password resets increment `auth_version` in the same committed write.
      `auth_api::password_reset_revokes_all_families_across_restart` uses a
      separate database pool and then a fresh `Auth` to prove immediate access
      invalidation across the CLI-process and restart boundaries;
      `admin_role_changes_invalidate_access_and_keep_one_admin` and
      `admin_deletes_users` cover the other two mutations
- [x] AUTH-4 Refresh-family rotation uses `BEGIN IMMEDIATE` plus a conditional
      update over the family id, current hash, revocation state and expiry.
      `auth_api::concurrent_refresh_has_one_winner_and_revokes_replay_family`
      releases two callers together and proves exactly one winner
- [x] AUTH-5 Each login has a bounded one-row refresh family. Consumed-token
      replay revokes that family, authenticated API logout revokes its current
      family, and password reset revokes every family in the password-update
      transaction. The auth integration suite proves family isolation and
      persistence across `Auth` restart; `kahawai-auth-cycle.sh` exercises the
      same rotation, replay, concurrency and logout behavior against a hub.
      The rebuilt binary passed that cycle before and after a real restart on
      2026-08-09; the live database reported migration 50, seven bounded
      families, one active setup family and no legacy token table
- [x] AUTH-6 `POST /api/v1/auth/logout` exists for API bearer clients and is
      authenticated, family-scoped and idempotent. Login, refresh and logout
      require an explicit `client: "browser" | "api"` mode; browser-cookie
      logout is implemented
- [x] AUTH-7 API login and refresh return access and refresh bearer tokens and
      set no authentication cookies. Browser login and refresh return only
      `{ "access_token", "expires_in" }`; server-set cookies own the browser
      refresh and media credentials
- [x] AUTH-8 Browser access tokens live in memory only. A reload obtains a new
      access token through the refresh cookie; neither access nor refresh tokens
      are written to local storage or a JavaScript-readable cookie
- [x] AUTH-9 Accept media-cookie authentication only for `GET`/`HEAD` on
      `/api/v1/bootstrap`, `/api/v1/events`, item artwork/subtitle/font files,
      and playback session streams and files. Protected application mutations
      require an `Authorization` bearer token. Browser refresh is authenticated
      by its refresh cookie; browser logout requires both its access bearer and
      refresh cookie. Browser refresh and logout additionally require an exact,
      present, non-`null` Origin equal to AUTH-10's canonical Origin
- [x] AUTH-10 Add `hub.public_url`. It determines the canonical Origin and
      enables `Secure` cookies when it is HTTPS; forwarded scheme/host values
      are trusted only from configured trusted proxies. An HTTP public URL is
      permitted and warned about at startup
- [x] AUTH-11 One owner middleware wraps every user-facing session resource:
      stream, playlist, segment, subtitle, seek, progress and end. Missing and
      foreign live ids return the same 404 body; administrative session routes
      remain separately administrator-gated. The web player's recovery contract
      follows the owner-scoped 404 rather than retaining the former 410 oracle
- [x] AUTH-12 Initial-admin creation is absent from the public router. First run
      exposes only a dedicated, port-distinct loopback browser listener with strict same-origin
      validation and a mode-0600 Unix socket inside an atomically private
      mode-0700 control directory for the interactive CLI; both call
      one `BEGIN IMMEDIATE` create-if-empty operation whose typed outcomes
      preserve 400/409/500 semantics, close after the sole
      winner commits, and are absent on later starts. No setup bearer secret is
      generated, logged, copied, accepted remotely, or left to brute-force
- [x] AUTH-13 Retain all existing Argon2id hashes. Require at least 12 Unicode
      scalar values when establishing or resetting a password, impose no
      composition rules, and continue to rate-limit login attempts.
      `auth_api` checks the transport split, cookie allowlist, Origin boundary,
      restart persistence and password policy. `kahawai-auth-cycle.sh` and the
      container smoke exercise both modes against a running hub; the embedded
      SPA was inspected through login, cookie refresh after reload and logout
      on 2026-08-14 with empty Web Storage and no JavaScript-readable cookies

## Browser and credential security (SEC-WEB)

- [ ] SEC-WEB-1 Serve a tested Content-Security-Policy compatible with the SPA,
      JASSUB workers and WASM, plus `frame-ancestors 'none'`, nosniff, a strict
      referrer policy and a minimal permissions policy. Derive browser-policy
      details from current primary browser specifications/documentation
- [ ] SEC-WEB-2 Remove OpenSubtitles username/password values from the generic
      preference API. Replace them with typed read/update/delete operations;
      reads expose only `configured` and non-secret account identity, reusing
      the write-only response pattern already implemented by `admin_providers`
- [~] SEC-WEB-3 Administrator provider reads already expose configuration state
      without returning keys, and their values live outside generic user
      preferences. Move the remaining user credentials out of generic
      preferences and choose the credential-store/key-management design from an
      explicit threat, recovery and operating-system support model;
      configuration-file secrets remain documented and permission-checked
- [ ] SEC-WEB-4 Protect stored credentials with a reviewed authenticated-
      encryption design whose keys are separated from ciphertext, restricted on
      disk and included in the recovery model. Bind ciphertext to its owning
      user/provider/field and cite current primary cryptographic guidance; AES-
      256-GCM plus a mode-0600 generated key is one candidate, not a preset gate
- [ ] SEC-WEB-5 Use a new immutable migration to move existing plaintext
      preference/setting credentials, verify decryption, then remove plaintext
      values; document schema meaning in the enforcing Rust module and include
      required recovery keys in the protected backup manifest
- [ ] SEC-WEB-6 Replace `(StatusCode, String)` and raw internal errors with the
      stable JSON shape `{ "code", "message", "request_id" }`; unexpected
      errors use a generic message while complete causes are logged server-side
- [ ] SEC-WEB-7 Ensure responses and logs never disclose password hashes,
      credentials, access/refresh tokens, filesystem paths, SQL text, provider
      response bodies or media pipeline internals to non-administrators

## Data, scanning and playback correctness (DATA)

- [x] DATA-1 Keep the configured collection name as its stable per-mediahost
      identity: renaming it deliberately creates a new collection. Derive each
      root's identity from its configured path as
      `root-sha256-` plus unpadded base64url of
      `SHA-256(utf8("kahawai-root-path-v1") || 0x00 || normalized_path_utf8)`,
      retaining the complete 256-bit digest. Resolve relative roots against the
      configuration file's directory, make them absolute, remove `.`/`..` and trailing
      separators lexically, and do not filesystem-canonicalize them; identity
      must not depend on mount availability or current symlink targets. Store
      the normalized path beside the token and fail loudly if one token is ever
      associated with different normalized path bytes. Document this
      config/wire representation and validate the media-type domain
- [x] DATA-2 Carry the derived root token through file records, incremental
      manifests and worklists, open/read requests, database source keys and
      session sources. Scan generations remain collection-scoped. A read
      resolves the one configured root named by the token and never searches
      roots for the first matching relative path; document the source-key
      meaning in the owning Rust module
- [x] DATA-3 Before starting a watcher or scanner, require unique non-empty
      collection names, supported media types, and absolute lexically normalized
      root paths with distinct derived tokens. Reject duplicate and nested or
      overlapping roots within one collection because they enumerate the same
      source namespace twice; overlap across deliberately separate collections
      remains valid. A temporarily missing or inaccessible root is reported as
      unavailable and is never silently substituted with another root
- [x] DATA-4 Implement REL-4 as protocol 3 with one required
      `SourcePath { root_token, path_rel }` representation in every
      source-bearing record, worklist and read request. Reject every protocol 2
      satellite at negotiation; retain no root-less wire fields, peer-minor
      branches or empty-token adapters. Database adoption remains lossless and
      separate from wire compatibility: infer a legacy single-root row without
      media access, and for a legacy multi-root row adopt an exact root only
      when stored path, size and content fingerprints prove it. Otherwise block
      that collection with an actionable diagnostic rather than guessing,
      rescanning or rematching
- [x] DATA-5 Backfill derived root tokens transactionally without an
      authoritative collection rescan or media rematch. Preserve item IDs and
      source bindings, technical metadata, libraries and grants,
      provider/manual/query state, recorded misses, queued work, relations,
      watch progress and collection scan generations. Any targeted mediahost
      check needed to disambiguate a legacy multi-root row may inspect only that
      source's stat/content identity; it must not reconcile the catalogue,
      advance the collection generation, clear an assignment or enqueue broad
      enrichment. Zero or multiple matches leave the collection blocked for
      operator correction
- [x] DATA-6 Exercise two roots containing the same relative filename and prove
      they remain distinct through scan, deduplication, source selection, byte
      leases and playback
- [x] DATA-7 The documented feature-off artifact builds completely. The former
      unconditional `spawn_ocr_sweep` failure is fixed; CI and release run
      `cargo build --locked --workspace --no-default-features` rather than
      `cargo check`. The local build exited 0, and hosted CI run 31788811563 job
      94730786238 built the complete workspace successfully on 2026-08-14
- [ ] DATA-8 Give every provider HTTP client explicit connect and request
      deadlines, bounded metadata/subtitle/artwork bodies, streamed large
      downloads, a restricted redirect policy and jittered retries only for
      idempotent requests
- [ ] DATA-9 Do not hold a provider rate-limit queue indefinitely across a hung
      request; timeout and cancellation release the queue and produce a bounded,
      typed provider error
- [ ] DATA-10 Document and enforce the authorization policy for forced remux/
      transcode controls. The proposed internet-exposed default restricts them
      to administrators or an explicit development option; any broader client
      contract is an explicit maintainer-owned product decision with abuse and
      resource-cost tests
- [ ] DATA-11 Validate the reproduced external numeric boundaries explicitly.
      `ProgressRequest.position_ms` cannot overflow the 90%-completion
      calculation or truncate when persisted from `u64` to SQLite `i64`;
      `VttQuery.shift_ms` must be finite and inside a documented shift envelope
      before rounding/casting. Test maximum integers, NaN, infinities and both
      accepted and rejected boundary values

## GStreamer and media-core quality (GST)

- [ ] GST-1 Put every operation that lets GStreamer or libass consume media-
      controlled bytes behind a supervised process boundary. This includes
      discovery, embedded subtitle/font extraction and ASS rasterisation as
      well as remux/transcode; a malformed library file must cost one job, not
      the hub or mediahost daemon. Job children remain an implementation detail:
      all-in-one still uses one parent process and the single Kahawai binary;
      its transcoder module is embedded when `[all_in_one] transcoder` is enabled
- [ ] GST-2 Run scan/extraction work through a bounded reusable worker pool,
      replace a worker after a crash or job budget, and attribute a crash or
      timeout to the exact file. Playback remains one worker process per
      session; capability probes run one timed child per element/path
- [ ] GST-3 Eliminate duplicated worker argument/protocol assembly through one
      versioned, serialisable and validated job specification. It contains
      sources, stable stream selectors, routes, encoder/container choices,
      required transforms, pacing and worker resource settings; unknown enum
      values or fields are errors, never legacy defaults. `PipelineSpec` is the
      proposed representation, not a required type name or crate boundary
- [ ] GST-4 A worker spawned by a process using an explicit `--config` receives
      the exact effective demotions, niceness and thread ceiling in its spec.
      The hidden worker entry point does not independently load the full hub
      configuration or provider credentials
- [ ] GST-5 Spawn workers with null stdin, a session working directory and a
      minimal environment allowlist containing only required GStreamer, dynamic
      loader, GPU and runtime variables. Do not inherit application secrets or
      unrelated operator environment
- [ ] GST-6 Replace audio/video/subtitle ordinal selection with a persisted
      `StreamSelector`: container track UID/stream ID where available, plus a
      deterministic technical fingerprint fallback. Runtime matching fails on
      absence or ambiguity instead of trusting asynchronous pad-added order to
      match discoverer order
- [ ] GST-7 Make pipeline assembly transactional: element creation, required
      properties, request pads and statically knowable links are validated
      before PLAYING; dynamic callback failures post one structured pipeline
      error and cannot leave a claimed mux pad waiting for a generic startup
      timeout. A dedicated `PipelineAssembler` is one implementation option
- [ ] GST-8 Remove panic-based control flow from Rust functions invoked by C.
      Wrap every pad probe, appsink/appsrc callback and dynamic signal handler
      in a common no-unwind boundary; eliminate production `unwrap` calls on
      element, pad, link, state and shared callback state operations
- [ ] GST-9 Represent required transforms as hard invariants. A requested tone
      map, deinterlace, image/ASS burn, channel layout or encoder path either
      reaches the negotiated output and is reported in `PipelineActual`, or the
      worker fails before readiness so the hub can choose an explicit fallback
- [ ] GST-10 Centrally declare and validate correctness-sensitive element
      properties instead of silently skipping them through
      `set_prop_if_present`. For every supported encoder, parser and segmenter,
      record required properties, units, caps and evidence-based plugin versions;
      an unsupported requirement fails that path rather than silently changing
      bitrate, GOP, playlist or channel behaviour. Typed adapters are the
      preferred design, subject to the recorded architecture criteria
- [ ] GST-11 Generate doctor inventory, capability reports, benchmark paths and
      real pipeline construction from the same adapter registry. A box never
      advertises an element/path that the worker will not select, and a crashed
      preferred encoder exposes the next verified fallback instead of removing
      the codec or selecting the known-crashing element locally
- [ ] GST-12 Remove process-global plugin-rank mutations from per-pipeline
      routing. Express exclusions such as the AC-3 parser workaround through
      scoped autoplug decisions; keep deliberate process-wide operator
      demotions explicit in the worker spec and diagnostics
- [ ] GST-13 Replace `seekable_appsrc`'s detached reader/feeder threads with an
      owned `SourcePump` carrying cancellation, bounded request/data channels
      and join handles. EOF, cancellation and read failure are distinct states;
      drop/seek/error wakes every waiter and releases the source/socket
- [ ] GST-14 Propagate a source read failure as a GStreamer/resource error and a
      failed session, never as clean EOS. Apply explicit read/write deadlines to
      worker sockets and test partial, short, stalled and failed lease reads
- [ ] GST-15 Give multipart playback one job-wide prefetch/memory budget rather
      than a complete 16 MiB ring and detached thread pair per part. Later parts
      do no source I/O until demanded by the concat path. Select the budget from
      measured latency and memory costs; this bounds transient buffering and
      does not evict reusable media caches
- [ ] GST-16 Give one runtime owner the pipeline bus, source pumps, probes,
      appsrc producer threads and completion channel. Stop flushes the bus,
      transitions to NULL with a deadline, joins owned tasks and sets a terminal
      result; dropping or failing during construction cannot strand threads,
      pipelines, sockets or leases. `PipelineRuntime` is a proposed type, not
      the acceptance criterion
- [ ] GST-17 Track every Unix listener/read bridge for every source part. Accept
      has a startup deadline and all handles are cancelled and joined on spawn
      failure, startup timeout, seek replacement, session end and link loss
- [ ] GST-18 Remove the in-process playback worker fallback from integration
      tests and normal runtime. Tests spawn the same supervised worker binary as
      production; unit tests exercise only pure planners, parsers and adapters.
      This isolates jobs and does not split the enabled all-in-one modules into
      separate module processes
- [ ] GST-19 Have TS and fMP4 paths share one playlist contract and one decided
      target duration. The on-disk playlist, readiness/pacing logic and bytes
      served to the client use the same value; validate target duration against
      actual EXTINF values, fMP4 init presence and monotonic segment numbering
      before declaring a session ready
- [~] GST-20 Segment and ordinary playlist write failures already become
      pipeline errors, and playlist replacement is atomic. Make missing fragment
      duration/header markers and a failed EOS/ENDLIST rewrite pipeline errors;
      write `init.mp4` and segment files atomically. Keep the documented
      callback buffer-list contract, but reverify it automatically against every
      supported gst-plugins-rs version when the pin changes
- [ ] GST-21 Stream local and remote playlists, segments, subtitle taps and
      diagnostic logs with backpressure and measured transient-memory limits.
      Do not read a long-GOP segment or repeatedly read an entire growing
      subtitle file into memory; use offset-aware artifact reads/tailing. These
      limits must not become eviction of reusable caches
- [ ] GST-22 Measure worker scratch, segment and log growth for representative
      copy/transcode workloads, including the rebuild cost and latency-at-use of
      every artifact. Keep lifecycle cleanup and check `remove_dir_all` failures
      instead of accepting stale sockets or playlists. If the maintainer adopts
      a disk cap, fail a session predictably with a typed error; do not add a
      background janitor or silently evict segments/caches needed for replay
- [ ] GST-23 Put checked arithmetic and explicit product-envelope limits around
      EBML/MP4 indexes, PGS/VobSub dimensions and object counts, zlib/zstd
      expansion, KBS1 blocks, cue/timeline counts and raster output. Decode large
      data incrementally instead of `decode_all` or accumulating a whole film's
      rendered frames and NDJSON in memory
- [ ] GST-24 Put the libass ABI behind versioned bindings and a small safe RAII
      wrapper for library, renderer and track ownership. Prefer generated or
      maintained upstream bindings where they satisfy the supported-platform
      criteria; otherwise pin and ABI-test the handwritten declarations.
      Validate dimensions and bitmap geometry before unsafe reads and exercise
      every supported libass version in CI
- [ ] GST-25 Report readiness through one structured, atomic worker control
      result rather than an append-only side channel. It names selected element
      factories/plugin versions, negotiated caps, performed transforms,
      degradations and terminal cause, and reaches the hub before it announces
      readiness. `PipelineActual` is a proposed representation
- [ ] GST-26 Keep one bounded diagnostic ring per worker and include GStreamer
      warnings/errors, state transitions and the pipeline actualisation in the
      session bundle without allowing GST_DEBUG output to exhaust scratch space
- [~] GST-27 The Dockerfile builds pinned GStreamer 1.28.6, gst-plugins-rs,
      libass and codec dependencies and now makes patch verification plus
      `KAHAWAI_MEDIA_TEST_STRICT=1 cargo test --locked --release --workspace` a
      mandatory ancestor of every release image. Required prerequisites panic
      in strict mode; distro CI retains best-effort execution but records every
      unavailable path. On 2026-08-08 the complete debug-profile strict suite
      against the host's Kahawai-only GStreamer 1.28.5 prefix exited 0. The same
      day the amd64 pinned-GStreamer 1.28.6 container gate passed all 11 patch
      reproducers and the complete strict release-profile workspace suite. Both
      hosted release architectures still need successful recorded runs
- [ ] GST-28 Add worker-process regression fixtures for every supported
      container and copy/encode/seek/subtitle route, including long/irregular
      GOPs, multiple same-kind tracks, multipart sources, missing PTS/DTS,
      uneven track ends and the real files represented by carried upstream
      patches
- [ ] GST-29 Add fault-injection tests for disk full/write denial, missing or
      incompatible plugin properties, link/state failure, source timeout/error,
      callback panic, worker crash/hang and teardown while pacing or appsrc is
      blocked. Repeated runs must return thread, file-descriptor, child-process
      and scratch usage to baseline
- [ ] GST-30 Add fuzz targets for the pure EBML/MP4 subtitle indexes, PGS,
      VobSub, KBS1/zstd and ASS parsing boundaries. Seed them with the corpus and
      run bounded PR smoke fuzzing plus longer sanitizer-enabled nightly jobs
- [~] GST-31 The active GStreamer patch loop fails when a patch stops applying,
      and every patch record now has an executable reproducer or wrapper. The
      verifier accepts explicit library/plugin prefixes, isolates the pinned
      container registry, distinguishes a crashed reproducer from a missing
      fix, and has a device-independent direct-parser proof for the ABI-changing
      H.264 patch. On 2026-08-08 all 11 records were LIVE against the host's
      Kahawai-only GStreamer 1.28.5 prefix, both with the exposed NVIDIA decoder
      and with the headless parser substitute (exit 0); all 11 also passed in
      the pinned GStreamer 1.28.6 amd64 container without NVIDIA hardware.
      Required hosted arm64 verification and recording current upstream issue,
      release and ABI claims remain

## Backup and filesystem safety (BKP)

- [ ] BKP-1 Introduce snapshot manifest v2 with SHA-256 and size metadata for
      every included artifact: configuration, database, PKI, JWT/credential
      keys, subtitles and other non-derivable state
- [ ] BKP-2 Add `restore --config-out`; default to the configuration path used
      by the command or `<data_dir>/kahawai.toml`, and rewrite the restored
      `hub.data_dir` to the selected restore destination. Add a companion script
      under `scripts/` that exercises the CLI-visible restore behaviour
- [ ] BKP-3 Hold an exclusive data-directory lock while the hub is running.
      Online backup may operate through SQLite's supported snapshot mechanism;
      restore refuses while a live process owns the lock
- [ ] BKP-4 Stage restore into a sibling directory, validate the manifest and
      SQLite integrity, and then swap complete trees with rollback. A failed or
      interrupted restore leaves the original installation usable
- [ ] BKP-5 Treat restored directories as authoritative: stale files in the
      current PKI, subtitle or state trees are removed rather than merged into
      the restored snapshot
- [~] BKP-6 JWT and satellite identity secrets, hub PKI keys and the database
      already receive mode 0600. Enforce mode 0700 on private state, session,
      backup, PKI, subtitle and temporary directories, and mode 0600 on the
      remaining configuration snapshots, backup outputs, logs and generated
      user artifacts on Unix; add regression checks for both existing and new
      permission guarantees
- [ ] BKP-7 Document exactly which caches are intentionally excluded and prove
      the restored hub accepts the original satellite certificates and resumes
      durable user/library state without an unplanned rescan

## Continuous integration and test coverage (CI)

- [x] CI-1 CI runs on direct pushes to `master` and on pull requests for
      third-party contributions, with separate Ubuntu 26.04 jobs for
      `cargo fmt --all --check`, locked workspace clippy with warnings denied,
      the complete locked workspace tests and the locked no-default build. The
      commands exited 0 locally and in the hosted run for `ad8e764` on
      2026-08-08
- [x] CI-2 Ubuntu 26.04 web CI runs on direct pushes to `master` and pull
      requests, installs from the lockfile, lints with warnings denied, runs
      unit tests and builds the ignored production `web/dist` from a clean
      checkout. Release source gates and the container build generate it before
      Cargo with `KAHAWAI_REQUIRE_WEB=1`; native bundler output is not committed
      or compared across developer platforms. The original gates completed in
      the hosted run for `ad8e764` on 2026-08-08; the generated-asset ownership
      change awaits its first hosted run
- [x] CI-3 Web lint is scoped to `src` and `test`, excluding generated output
      and dependencies, and `.oxlintrc.json` carries `"ignorePatterns":
      ["dist"]` so the exclusion holds for anything that does not go through
      the npm script — an editor lints the workspace, not `package.json`. The stale `capsRev` calculation and `switchBurn`
      callback capture were corrected rather than suppressed, and lint exited 0
      locally and in the hosted run for `ad8e764` on 2026-08-08
- [x] CI-4 Worker integration tests remove inherited `KAHAWAI_*` settings
      and give every spawned process an explicit temporary configuration, state
      directories and isolated XDG environment. The logs from both ordinary and
      strict full-workspace runs on 2026-08-08 show only temporary configuration
      paths, and all three worker tests also passed in the hosted run for
      `ad8e764`
- [~] CI-5 `auth_api` covers atomic concurrent refresh, family replay,
      family-isolated API logout, password-reset revocation of all refresh
      families across `Auth` restart, deletion cascade and migration-time
      invalidation of legacy refresh tokens. Access-token invalidation after
      deletion/demotion/reset, browser logout, cookie attributes and browser
      secret storage remain. Local setup now has foreign-Origin rejection,
      atomic concurrent-claim coverage and durable listener/socket closure.
      `cargo test --workspace`, formatting and clippy
      all exited 0 locally on 2026-08-09
- [x] CI-6 `direct_play_ranges_end_to_end` creates two users and sends the
      foreign account through stream, playlist, segment, subtitle, seek,
      progress and end routes. Every response is byte-for-byte the same 404 as
      an absent id, and the owner then reads and ends the still-live session
- [x] CI-7 Pin deterministic root-token test vectors and exercise identical
      relative paths in separate roots through scan, persistence, source
      selection, byte leases and playback. Prove root reordering changes
      nothing; duplicate/within-collection overlapping roots are rejected;
      cross-collection overlap remains valid; protocol 2 peers are rejected;
      protocol 3 exposes only the required exact-source shape; ambiguous legacy
      database rows are never guessed; and migration preserves source/item IDs,
      durable metadata, provider questions/answers and user state without
      changing scan generations, reconciling the catalogue or rematching media
- [ ] CI-8 Test backup configuration restoration, Unix permissions, corrupt and
      truncated manifests, invalid SQLite, stale-file removal, interruption
      rollback and refusal while the live hub owns the data lock
- [ ] CI-9 Run Playwright against Chromium and WebKit for setup, login/logout,
      user grants, administration, browse/search/deep links, direct/remux/
      transcode playback, seek/recovery, subtitles and native HLS behaviour;
      assert persisted and browser-visible outcomes rather than only command
      success
- [ ] CI-10 Run automated accessibility checks on the primary authentication,
      browse, detail, player, settings and administration screens
- [ ] CI-11 Add a modular Compose release test with separate hub, mediahost and
      transcoder processes; disconnect/restart satellites and workers and prove
      recovery without duplicate, leaked or permanently stuck sessions
- [ ] CI-12 Run the documented scale, concurrency, version-skew and performance
      suites on nightly/release jobs, keeping expensive corpus tests out of the
      normal pull-request path
- [ ] CI-13 Add RustSec/OSV/npm and license/source checks. Critical or high
      findings fail release unless a waiver records an owner, justification and
      expiry date

## Runtime, observability and delivery (OPS-RDY)

- [ ] OPS-RDY-1 Coordinate SIGTERM and Ctrl-C shutdown across the API,
      satellite listeners, scanners and playback workers. Stop accepting new
      sessions, drain for a measured and maintainer-approved bounded interval
      (30 seconds is the proposed default), close durable state and terminate
      remaining workers
- [ ] OPS-RDY-2 Treat unexpected termination of the API, satellite listener or
      another critical supervisor as a process failure instead of leaving a
      partially serving hub alive
- [ ] OPS-RDY-3 Add `/health/live` and `/health/ready`, retaining `/health` as
      the readiness alias. Database and critical supervisor failure makes
      readiness fail; optional offline satellites report degraded state
- [ ] OPS-RDY-4 Add request latency/error, session outcome, scan, provider and
      transcoder metrics using the existing fleet-scale bounded-label policy
- [~] OPS-RDY-4A Existing metrics already avoid labels containing user, item,
      session, file or unbounded provider-response values. Add a regression test
      over every exported metric family so future instrumentation cannot add
      unbounded-cardinality or sensitive labels unnoticed
- [ ] OPS-RDY-5 A metric query failure must surface as an observability/readiness
      error rather than silently becoming a zero-valued gauge
- [ ] OPS-RDY-6 Add request IDs to structured logs and API errors, configurable
      JSON logging for containers and a documented reload policy for log levels
- [ ] OPS-RDY-7 Build non-root OCI targets for all-in-one, hub, mediahost and
      transcoder using a fixed unprivileged UID/GID, an executable healthcheck,
      read-only media mount guidance and explicit writable state mounts. Every
      target uses the same Kahawai binary; the all-in-one target retains one
      parent process containing all three modules, with supervised job children
- [~] OPS-RDY-8 The tag-driven release workflow defines native Ubuntu 26.04
      amd64 and arm64 image builds, strict media gates, exact-digest smoke tests,
      OCI metadata, SBOM/provenance, a gated multi-architecture manifest and
      checksummed stamped source plus explicitly unsupported bare binaries. On
      2026-08-08 the local amd64 production image built from the gated stage and
      its smoke test scanned a generated clip and played five HLS segments. The
      first hosted two-architecture release must still be inspected; base
      image/action digest pinning and artifact signing remain
- [ ] OPS-RDY-9 Publish a modular Compose example and a release runbook covering
      reverse proxy TLS, trusted proxies, persistent paths, ownership, upgrade,
      rollback and backup/restore drills
- [ ] OPS-RDY-10 Add runnable release checks for coordinated shutdown, critical-
      supervisor failure, live/ready health transitions, metric-query failure
      and structured request IDs. Restart the service and inspect process,
      health, logs and durable state; command success alone is not evidence

## API contracts and maintainability (ENG)

- [ ] ENG-1 Establish one authoritative typed definition for public request,
      response and error DTOs and remove handler-local untyped JSON for stable
      resources. A shared API crate is the proposed boundary, subject to the
      documented dependency and compile-cost criteria
- [ ] ENG-2 Produce OpenAPI and TypeScript client/types from the same
      authoritative contract, snapshot the wire contract and fail CI on
      unexplained changes. Code generation direction and committed artifacts are
      implementation decisions justified by reproducibility and toolchain cost
- [ ] ENG-3 After characterization tests cover the existing wire behaviour,
      define API ownership boundaries that prevent auth, library, playback,
      admin, provider and observability changes from sharing one implementation
      unit merely by history. The exact router/file split follows measured
      coupling rather than a prescribed directory layout
- [ ] ENG-4 Define and enforce comparable ownership boundaries for session
      lifecycle/placement/artifacts/recovery, media planning/source/pipeline/
      segmenting/supervision and provider enrichment. Record the criteria before
      selecting crate/module splits; source size alone is not a cost model
- [ ] ENG-5 Keep the structural refactor behaviour-neutral. Contract snapshots
      and media fixtures prove unchanged JSON, manifests, timestamps,
      byte-range semantics and playback choices
- [ ] ENG-6 Establish the post-1.0 compatibility mechanism: an API baseline for
      current/previous-major serving and a protocol baseline used by automated
      breaking-change and version-skew checks

## Web experience and performance (UX; re-audited after the redesign)

**Status: re-audited.** The redesign landed on `ui-redesign`: the clickable
prototype in `web-mockup/` was ported screen by screen, and this section
replaces the postponed one rather than inheriting its gates, as the postponed
version instructed. Where the built UI is knowingly narrower than the design,
or where the design assumed data the API does not carry, the entry lives in
`docs/kahawai-ui-checklist.md` — that ledger is the design-vs-built record;
this section is the release gate.

Two of the five are now answered by measurement. Two were written as proposals
pending a live pass and are restated as what was actually observed rather than
promoted or ticked. One is unchanged.

- [x] UX-1 Replace silently swallowed request and save failures with an error
      boundary plus actionable inline/toast retry states; preserve useful errors
      across login refresh and playback recovery.
      An error boundary sits under every routed screen, keyed on the route so
      leaving clears it: a render throw used to take the app with it and leave
      a white page with nothing to report. It is the app's one class component,
      because React offers no other way to catch a render throw, and its doc
      names what it cannot catch — handler and promise rejections, which are
      the `Failed` path's job.
      A load that fails offers Try again and somewhere to go, on the item,
      season, home and settings screens. Try again re-runs the load that
      failed rather than a second code path.
      Login refresh: which screen you are on was decided once, at startup, so a
      session that died an hour in left the shell up with every panel showing
      its own 401 and a retry that could never work. Clearing the tokens now
      reaches the shell, which goes to sign-in and says why, and the route is
      untouched so signing back in returns you to the page you were reading.
      Playback recovery: fatal `hls.js` errors are kept and named, so the loop
      guard's refusal says WHY the restart produced nothing instead of
      shrugging — everything except 410 and 401 used to fall off the end of
      that handler. The unrecoverable case is a dialog with a retry, not a
      line of text.
      Deliberately NOT done, and the reasoning is in
      `docs/kahawai-ui-checklist.md`: toasts carry no actions. Auditing every
      site showed the test that discriminates is "is the control that caused
      this still on screen?", and for almost all of them it is — so a toast
      button would duplicate it five seconds before vanishing. The two cases
      where the affordance genuinely was missing were given INLINE retries
      instead, anchored to where the content is absent.
      **This item no longer quotes a count of swallowed failures.** Three
      attempts produced three different numbers — four, then twelve, then a
      paragraph whose own list disagreed with its total — and one of them named
      a site that does not exist. A total nobody can re-derive is worse than no
      total, so what follows is the list by name, and the recipe: grep `web/src`
      for `catch` and read each body, which is the only method that has been
      right.
      **Per-item tolerance, not request failures.** A malformed JSON line in a
      cue or overlay stream (`Player.tsx`, the live-text and overlay effects), an
      unparseable access token (`claims`), a malformed SSE hint (`openEvents`),
      and the two `localStorage` reads for the capability mask (`loadMask`,
      `saveMask` — these are storage, NOT codec probes, which is what an earlier
      draft of this paragraph claimed). The surrounding stream carries on and
      dropping one item is the design. Out of scope for this requirement.
      **Deliberately silent, with the reason in each case.** The four track- and
      subtitle-memory writes (`putPref`); the font list, because libass has its
      own default; the up-next lookup; the anime view preference; the header's
      library list; `refreshTokens`' transient failure, whose caller reports the
      401 it could not repair; `signOut`'s call to the hub, fire-and-forget
      because the browser's copies are already gone; `postProgress` and
      `endSession`, which run on unload where there is nowhere to put a message;
      and `startPlaybackSession`'s own prefs read WHEN its caller asks for
      quiet, which is the automatic recovery and the stand-by retry — the latter
      runs every five seconds for as long as a host is away, and a report there
      is a toast on a timer about weather its own dialog is already describing.
      Quiet is opt-in and the default reports: the first cut had it the other
      way round and took the report away from three deliberate presses — a Play
      in the season view, an Apply in the capability dialog, a Try again — that
      read prefs through this function and nowhere else.
      **Newly found and NOT closed:** `syncOrigin` gives up after three attempts
      with no report, which leaves every subtitle path computing against a wrong
      timeline origin. That is a request failure with a user-visible consequence,
      so it is listed as open rather than accepted.
      **Closed here.** Everything where a viewer lost something they had chosen
      with no way to know. Preferences were the bulk of it, and were five
      separate silent catches: now one `prefsOrNone` helper that keeps the
      empty-list fallback — a page rendering on source order beats one that does
      not render — and says what happened. What was being lost: the audio track
      last chosen, the remembered subtitle track, the per-media-type language
      wishlist, and the bandwidth cap typed into Settings. Beside those: the
      library page's own details, whose absence left a music library laid out as
      films under the title "Library"; and BOTH subtitle rendering paths, styled
      and image, where there is no `.vtt` fallback to inherit — the `<track>`
      renders only for `delivery === 'text'` — so a failed feed was simply no
      subtitles, indistinguishable from a track that never had any.
      **Known and not fixed, found while doing this:** when a styled-subtitle
      tap dies AFTER its header, the renderer already exists and the hub's copy
      is fetched on top of it, so the overlap is drawn twice. Gating the
      fallback on the renderer was tried and is worse — a canvas holding the few
      cues that arrived, a complete copy on the hub untouched, and silence. The
      fix is for the feed to be able to replace a renderer rather than only
      append to one.
      **A known limit on where the player can report.** It uses `showNote`
      rather than `notify` because the toast host is a sibling of the element
      that goes fullscreen, so a toast raised there is painted nowhere. But
      `showNote` is dropped while a stand-by or playback-stopped dialog is up,
      and lost if the player unmounts before it renders. Neither reporter covers
      every case; the fix is to paint the notice host inside the fullscreen
      subtree, which is a change to the shell and not to these call sites.
      Recorded rather than done.
- [ ] UX-2 Proposal pending live UI verification: exercise library, provider,
      satellite, scan and session views under loading, empty, degraded and
      offline conditions; record what is actually absent, then add only the
      missing intentional states accepted into MVP scope.
      Exercised, and what it found is fixed. The recurring fault was one shape,
      in five places: **a failure rendered as an empty success.** A shelf whose
      fetch failed became an empty shelf and empty shelves are dropped, so a
      library vanished from the home screen without a word. Continue-watching
      failing left no row, which reads as nothing on the go. A cross-library
      search turned every failure into an empty result, so a dead hub was
      indistinguishable from nothing matching. Settings rendered every control
      at its default, which reads as "these are your settings". Each of those
      now says what happened, and the search says it once, after every library
      has answered, so it can tell a bad connection from one bad library.
      Loading: the home screen shows skeletons per shelf the moment the library
      list is known and fills them in independently — it used to wait on the
      slowest library and show nothing, which a GPRS simulator made obvious and
      a LAN never could. Ghosts sit at half strength and do NOT show the
      missing-artwork mark, and a picture still in flight is dimmer than one
      that is genuinely absent; three states, three appearances.
      Degraded: an offline collection is marked on its library row; a mediahost
      that could not read files says so on the satellite itself (MH-8, count
      only — see below); a satellite lost mid-playback is handled and exercised
      against the live fleet (AR-6).
      Offline: `api()` throws `Offline` on a network failure, so a dead hub
      reads as "Could not reach the hub." rather than `TypeError: Failed to
      fetch`, which is what it used to show people.
      Left as accepted gaps rather than built: a home screen sitting idle says
      nothing, because nothing is wrong until you ask for something and a
      heartbeat would be machinery for a state you find the moment you act.
      The library grid — the biggest screen, and virtualised — has NOT been
      exercised under a slow link; its reserved scroll height comes from
      measuring one cell, and how that behaves when cells arrive slowly is
      unknown. A scan that cannot run at all has no signal to show: the
      protocol carries per-file errors only, and `FileError` is logged and
      never stored, so the chip can give a count but not which files.
      Unchecked for the grid and the scan-level signal, not for the rest
- [ ] UX-3 Proposal pending an interactive accessibility pass: verify keyboard
      navigation, focus restoration, fullscreen escape, labels for glyph-only
      controls, meaningful artwork alternatives where appropriate and
      responsive player behaviour; turn reproduced failures into named gates.
      Unaudited. Nothing in the redesign was verified with a keyboard-only run
      or a screen reader (`UI-17`). Keyboard reachability was preserved by
      construction where a pointer-only gesture was introduced — clicking a
      language pill still promotes it, the subtitle-fallback rows take the
      arrow keys, and a lane arrow at its limit is disabled rather than removed
      so it keeps its place in the tab order — and glyph-only controls carry
      titles. None of that is an audit. This item is unchanged in substance
- [~] UX-4 Lazy-load administration, settings, player and subtitle rendering;
      load JASSUB/WASM only for playback modes that need it.
      Done, in the shape the measurement justified rather than the shape
      proposed. The player is `lazy`, which takes `hls.js` and `jassub` with it
      — 164 KiB gzip that browsing never fetches. Inside the player, libass is
      imported only once a track turns out to be styled, so an audio-only or
      plain-text evening never pulls the worker or its wasm. Verified on a live
      hub: a first load fetches only the entry bundle and the runtime; the
      player and jassub chunks arrive on play; a styled track then renders
      through libass. Administration and settings were split too and put back
      — 9 KiB of an 83 KiB bundle, in exchange for a chunk request on opening
      either (`UI-18`). So two of the four named here are done and two were
      tried and deliberately reverted — which is a partial outcome, not a met
      gate, and it was ticked anyway. `[~]` until whoever owns the gate either
      accepts the narrower scope or asks for the split back
- [~] UX-5 Measure the initial application payload and approve a budget against
      startup latency on the supported client/network envelope. The proposed
      ceiling is 200 KiB gzip excluding documented player/WASM chunks.
      Measured, and under the proposed ceiling. **A first load is 100,512 bytes
      (98.2 KiB) of code and markup, against a 200 KiB ceiling** — 91,789 bytes
      gzip of entry JavaScript, a 361-byte preloaded runtime chunk, 8,014 bytes
      of CSS and the 349-byte document, read off the wire with
      `Accept-Encoding: gzip`. The lazy player chunk is a further 163,161 bytes
      gzip and is excluded by the ceiling's own terms.
      Re-take this rather than quote it. It has been wrong in this file three
      times, each time low, because the number was copied forward while the
      bundle grew: `for a in $(curl -s localhost:8420/app/ | grep -oE
      'assets/[^"]+'); do curl -s -H 'Accept-Encoding: gzip' -o /dev/null -w
      '%{size_download}\n' localhost:8420/app/$a; done`. The earlier figure — 245,213 bytes at `7c0a5f5` — was a build
      report, not a measurement: the hub ignored `Accept-Encoding` and served
      every asset uncompressed, so nothing had ever actually been sent gzipped
      and the budget was being compared against a number nobody served. With
      compression on the web routes and the player split out, the wire agrees
      with the build. Remaining before this can be a gate: accept the ceiling,
      and fail the production check on an unapproved regression. Neither has
      happened — nothing in `scripts/` measures the bundle and no CI job
      compares it against a budget — so a regression still ships silently and
      the item is `[~]`, not `[x]`. The measurement is the part that is done.
      Note for whoever accepts it: on the home screen, code is no longer the
      cost. That screen also fetches 59 artwork requests totalling 2.05 MB,
      because `card` artwork is generated at 480 px and displayed at 128 px on
      a dpr-1 display (`UI-16`). A 200 KiB code budget is worth having, and it
      governs about four per cent of what a first visit downloads

## MVP exit criteria

- [ ] EXIT-1 Every item classified `release-blocker` by the maintainer is checked;
      unresolved recommendations classified `post-MVP` or `not-planned` carry a
      rationale and are not silently redefined as complete
- [ ] EXIT-2 Every mandatory pull-request and release CI job is green from a
      clean checkout, including the no-default build and generated-web diff;
      their required runtime, restart, persisted-state and artifact postconditions
      have also been inspected successfully
- [ ] EXIT-3 The non-root all-in-one image passes fresh setup, representative
      direct/remux/transcode playback, graceful shutdown and backup/restore on
      every Linux architecture classified as supported in the platform matrix
- [ ] EXIT-4 The modular release test passes satellite/worker disconnect and
      restart scenarios without cross-user access, state loss or manual cleanup
- [ ] EXIT-5 Performance jobs satisfy the documented browse, direct/remux and
      transcode concurrency targets with accepted startup-latency, memory and
      CPU baselines
- [ ] EXIT-6 There are no unwaived critical/high security findings and every
      remaining waiver has an owner and expiry
- [ ] EXIT-7 The published feature, platform, API and operational support claims
      exactly match the evidence matrix and release artifacts
- [ ] EXIT-8 **Kahawai is ready to be labelled an internet-exposed MVP**
