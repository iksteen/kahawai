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
owner-scoped, refresh rotation races, restore does not restore configuration and
multi-root collections can alias files. The CI implementation added after the
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
- [ ] REL-4 Record the compatibility decision for stable root identities, the
      satellite protocol and token invalidation. If the approved design is
      incompatible, assign the protocol version, rescan/reauthentication scope
      and support window deliberately and publish an operator migration path
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

- [ ] AUTH-1 Add an immutable auth migration with `users.auth_version`; old
      access and refresh tokens are intentionally invalid after this migration.
      Document the field's meaning in the Rust auth module, not as mutable
      commentary in the migration log
- [~] AUTH-2 Retain the existing HS256 allowlist, signature and expiry
      validation. Add and require the intended issuer, audience and token-type
      claims, then load the user by primary key for every authenticated HTTP
      request; construct username and administrator status from the database
      rather than trusting mutable claims in the token
- [ ] AUTH-3 Make deletion, administrator-role changes and password resets
      invalidate access immediately, including across hub restart and when the
      reset is performed by a separate CLI process
- [ ] AUTH-4 Consume refresh tokens atomically with a conditional database
      update; concurrent use has exactly one winner
- [ ] AUTH-5 Group rotating refresh tokens into families. Reuse of a consumed
      token revokes its family, logout revokes the active family, and password
      reset revokes every family for that user
- [ ] AUTH-6 Add `POST /api/v1/auth/logout` and support explicit
      `client: "browser" | "api"` modes on setup and login, defaulting to API
      mode for command-line and third-party clients
- [ ] AUTH-7 API auth mode returns access and refresh bearer tokens and sets no
      cookie. Browser mode returns only the access token and sets host-only,
      `HttpOnly`, `SameSite=Strict` refresh and media cookies
- [ ] AUTH-8 Browser access tokens live in memory only. A reload obtains a new
      access token through the refresh cookie; neither access nor refresh tokens
      are written to local storage or a JavaScript-readable cookie
- [ ] AUTH-9 Accept media-cookie authentication only for the documented
      `GET`/`HEAD` media, artwork, event and bootstrap resources. Every mutation
      requires an `Authorization` bearer token; refresh and logout additionally
      validate the request Origin
- [ ] AUTH-10 Add `hub.public_url`. Browser deployment on a non-loopback bind
      requires an HTTPS public URL, which determines the canonical Origin and
      enables `Secure` cookies; forwarded scheme/host values are trusted only
      from configured trusted proxies
- [ ] AUTH-11 Put one owner check in front of every user session resource:
      stream, playlist, segment, subtitle, seek, progress and end. A missing or
      foreign session returns the same 404; administrative session routes remain
      separately administrator-gated
- [~] AUTH-12 Setup already becomes inaccessible after the first administrator
      is created and compares token digests rather than plaintext. Replace the
      current 32-bit setup token with at least 128 random bits and apply explicit
      setup throttling by trusted client IP plus a global bound; cite the current
      primary security basis for the entropy and throttling requirements. The
      current remotely brute-forceable first-run state is an internet-exposure
      release blocker
- [~] AUTH-13 Retain existing Argon2id hashes and adopt a documented password
      policy from current primary guidance; the proposed remaining change is a
      minimum 12 characters without composition rules instead of the current
      eight-character minimum

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

- [ ] DATA-1 Give collections and roots stable identities independent of name,
      path and list order. Document the chosen config/wire representation, safe
      ID grammar and validated media-type domain; the proposed shape is
      `CollectionConfig { id, name, media_type, roots: RootConfig { id, path } }`
- [ ] DATA-2 Carry the stable root identity through file records, open/read
      requests, scan generations, database source keys and session sources. A
      read resolves one exact root and never searches roots for the first
      matching relative path; document schema meaning in the owning Rust module
- [ ] DATA-3 Reject duplicate, missing, non-canonical, nested or overlapping
      roots and unsafe collection/root IDs during configuration validation,
      before any watcher or scanner starts
- [ ] DATA-4 Implement the maintainer-approved compatibility decision from
      REL-4. Incompatible satellites receive an actionable upgrade/rescan error;
      remove compatibility backfills only if their support window has explicitly
      ended and all current fields exist in the replacement protocol
- [ ] DATA-5 Preserve item IDs, metadata, grants, provider matches and watch
      progress during the root-identity migration; clear only scan-derived
      source bindings and force a complete rescan
- [ ] DATA-6 Exercise two roots containing the same relative filename and prove
      they remain distinct through scan, deduplication, source selection, byte
      leases and playback
- [ ] DATA-7 Make the documented `--no-default-features` build compile. The
      currently reproduced failure is the unconditional `spawn_ocr_sweep` call;
      keep the complete build as the acceptance check so any later optional-
      feature failure is reported rather than guessed in advance
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
      all-in-one still embeds the hub, mediahost and transcoder modules in one
      parent process and uses the single Kahawai binary
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
      This isolates jobs and does not split the three all-in-one modules into
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
      unit tests and the production build, then compares generated `web/dist`
      with Git. All operations completed locally and in the hosted run for
      `ad8e764` on 2026-08-08
- [x] CI-3 Web lint is scoped to `src` and `test`, excluding generated output
      and dependencies. The stale `capsRev` calculation and `switchBurn`
      callback capture were corrected rather than suppressed, and lint exited 0
      locally and in the hosted run for `ad8e764` on 2026-08-08
- [x] CI-4 Worker integration tests remove inherited `KAHAWAI_*` settings
      and give every spawned process an explicit temporary configuration, state
      directories and isolated XDG environment. The logs from both ordinary and
      strict full-workspace runs on 2026-08-08 show only temporary configuration
      paths, and all three worker tests also passed in the hosted run for
      `ad8e764`
- [ ] CI-5 Test account deletion, demotion and password reset across immediate
      restart; atomic concurrent refresh, token-family replay, browser/API
      logout, cookie attributes, Origin checks and absence of browser-stored
      secrets. Prove setup-token entropy, per-IP/global throttling and permanent
      setup closure after the first administrator is committed
- [ ] CI-6 With two users, exercise every session-scoped endpoint using a
      foreign session ID and prove all denials are indistinguishable 404s
- [ ] CI-7 Test identical paths in separate roots, root reordering, overlapping
      root rejection, protocol-version rejection, the forced rescan and
      preservation of durable identities and user state
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

## Web experience and performance (UX; postponed pending redesign)

**Status: postponed.** A new design is in progress, so none of the current UX
items is an MVP release blocker or authorised implementation work. Retain them
as historical audit observations only. Once the new design lands, re-audit the
resulting interface and replace this section rather than carrying these gates
forward by assumption.

- [ ] UX-1 Replace silently swallowed request and save failures with an error
      boundary plus actionable inline/toast retry states; preserve useful errors
      across login refresh and playback recovery
- [ ] UX-2 Proposal pending live UI verification: exercise library, provider,
      satellite, scan and session views under loading, empty, degraded and
      offline conditions; record what is actually absent, then add only the
      missing intentional states accepted into MVP scope
- [ ] UX-3 Proposal pending an interactive accessibility pass: verify keyboard
      navigation, focus restoration, fullscreen escape, labels for glyph-only
      controls, meaningful artwork alternatives where appropriate and
      responsive player behaviour; turn reproduced failures into named gates
- [ ] UX-4 Lazy-load administration, settings, player and subtitle rendering;
      load JASSUB/WASM only for playback modes that need it
- [ ] UX-5 Measure the initial application payload and approve a budget against
      startup latency on the supported client/network envelope. The proposed
      ceiling is 200 KiB gzip excluding documented player/WASM chunks. At
      `7c0a5f5`, the initial JavaScript measured 245,213 bytes gzip plus roughly
      12 KiB CSS; that measurement is evidence for the decision, not a failure
      until the budget is accepted. Once accepted, fail the production check on
      an unapproved regression

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
