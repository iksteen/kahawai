# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

@AGENTS.md

`AGENTS.md` above carries the working agreements, house rules (never `git push`
autonomously; use `scripts/kahawai-restart.sh`, never a hand-written kill) and
the "where the answers live" map. Everything below adds to it rather than
repeating it. Design docs: `docs/kahawai-implementation.md` (how and why),
`docs/kahawai-technical-requirements.md` (numbered requirements such as HUB-n,
MH-n, SEC-n, OPS-n that code comments cite), `docs/kahawai-status-checklist.md`
(what is built; update in the same commit as a status change).

## Commands

Rust toolchain is pinned in `rust-toolchain.toml`; Node in `.node-version`.
System deps (GStreamer dev + plugins, libass, libva, leptonica/tesseract,
protobuf-compiler, clang, cmake) are listed in `.github/workflows/ci.yml`.

```sh
cargo build                                  # kahawai binary only (default-members)
cargo build --workspace                      # everything, incl. the lean daemons
cargo test --workspace                       # all crates; CI runs with --locked
cargo test -p kahawai-hub --test remux_play  # one integration test binary
cargo test -p kahawai-hub --test remux_play some_test_name   # one test
cargo test -p kahawai-core negotiate         # unit tests matching a substring
cargo clippy --workspace --all-targets -- -D warnings   # CI sets RUSTFLAGS=-D warnings
cargo fmt --all
cargo build --workspace --no-default-features            # CI gate: builds without OCR/Tesseract
```

`KAHAWAI_SKIP_WEB_BUILD=1` makes the hub crate's `build.rs` skip `npm ci && npm
run build`, which it otherwise runs on every hub build to embed `web/dist` via
`rust-embed`. Use it for Rust-only iteration and clippy. `KAHAWAI_REQUIRE_WEB=1`
turns a missing bundle into a build error (release builds).

Web UI (`web/`, Vue 3 + TypeScript + Vite + Tailwind, tested with vitest and
Playwright):

```sh
cd web
npm run dev          # Vite on :5173, /api and /admin proxied to a hub on :8420
npm test             # vitest
npx vitest run test/admin.test.ts        # one test file
npm run lint         # oxlint, warnings are errors
npm run fmt          # oxfmt; CI gates on fmt:check
npm run typecheck    # vue-tsc
npm run test:csp     # Playwright CSP contract (needs `npm run build` first)
npm run test:product # Playwright product journey against a real all-in-one
npm run api:export   # regenerate openapi.json + client after changing hub API
```

`scripts/kahawai-web-dev.sh` wraps the dev loop. Install the local pre-commit
hook with `git config core.hooksPath .githooks`; it runs fmt, clippy, web
fmt/lint and the OpenAPI fingerprint check.

### The OpenAPI contract is generated and fingerprinted

`web/openapi.json` is exported from the hub (`cargo run -p kahawai-hub --example
export_openapi`) and stamped with a fingerprint of the Rust files it came from.
`src/api/generated/` is Orval output from it and is not committed. Every web
install/build/test/typecheck runs `api:check` first, so after touching hub API
types or routes you must run `npm run api:export` or the web gates fail. Never
hand-edit either file.

## Architecture

One binary, `kahawai`, with subcommands `hub`, `mediahost`, `transcoder`,
`all-in-one`, `doctor`. Clients (web UI, Android app) talk only to the hub's
`/api/v1/*` HTTP surface; satellites dial the hub over tonic/gRPC with mTLS and
never accept inbound traffic. The hub is its own CA and issues satellite
certificates via an enrollment-code flow.

Crate dependency boundaries are deliberate and enforced by packaging:

- `kahawai-core`: pure shared types and logic, no I/O. Capability model,
  stream model, content identity, and the playback negotiation engine
  (direct play vs. remux vs. transcode).
- `kahawai-proto`: `.proto` files + tonic/prost codegen for hub<->satellite
  messages.
- `kahawai-transport`: mTLS, enrollment identity and certificate renewal
  shared by networked satellites.
- `kahawai-media`: GStreamer wrappers (discovery, pipeline builder, encoder
  probing). Blocking; call from `spawn_blocking`.
- `kahawai-playback`: the pipeline job and its argv/`StartSession` codecs,
  the supervised executor (run directory, worker sockets, readiness, log
  bundle) and the pure placement ranker, shared by the hub, the transcoder
  and the runtime's worker entry. Tokio, but never hub dependencies: the
  lean transcoder daemon links it.
- `kahawai-runtime`: config (figment: TOML + `KAHAWAI_<SECTION>__<KEY>` env
  overrides), logging, the doctor, worker plumbing. Knows nothing about roles.
- `kahawai-sqlite`: one serialized writer + read-only WAL reader pool. Every
  SQLite user goes through it.
- `kahawai-mediadb`: the catalogue/metadata/library-identity model and its
  migrations, separated from the hub's runtime concerns.
- `kahawai-hub`: hub library. axum API (`api/`), auth, sessions, enrichment
  providers (`providers.rs`, `enrich/`, `anidb.rs`, `opensubtitles.rs`),
  the provider rate gate (`gate.rs`), subtitles/OCR, segment (intro/credits)
  detection, PKI/enrollment, backup, embedded web UI (`web.rs`).
- `kahawai-mediahost`, `kahawai-transcoder`: satellite libraries (enroll,
  keep a control link, scan/serve or run encode sessions).
- `kahawai-mediahostd`, `kahawai-transcoderd`: lean standalone daemons that
  do not depend on the hub crate, so cargo cannot pull SQLite/axum/Tesseract
  into them.
- `kahawai`: the binary crate; also hosts the all-in-one wiring.
- `kahawai-intro`: intro/end-credits fingerprint detection (a port of
  Jellyfin's intro-skipper; `scripts/introref/` is the .NET reference rig).

All-in-one does not run gRPC over loopback: it hands the same generated
`HostToHub`/`HubToHost` values between hub and in-process mediahost over
bounded Tokio queues, drained by the same handler as the network link, and
serves bytes from local files directly. Keep that path and the wire path
behaviourally identical.

Migrations live in `crates/kahawai-hub/migrations`,
`crates/kahawai-mediadb/migrations` and `crates/kahawai-mediahost/migrations`,
embedded with `sqlx::migrate!` at compile time and applied only at startup.
That is why a fix touching schema needs a rebuild and a restart before it is
visible (see AGENTS.md on the restart script).

Background work on the hub is durable rows plus one shared driver (`crates/kahawai-hub/src/queue.rs`):
enrichment and subtitle work are claimed from mediadb tables that the catalogue commit populates, and
woken by commits, landings and reconnects. The mediahost selects its own discovery work from missing
local facts. Do not add periodic catalogue walks; add a row kind and a wake.

Feature `ocr` (default on) links Tesseract/Leptonica for bitmap-subtitle OCR;
CI builds `--no-default-features` too, so gate OCR code behind the feature.

Integration tests are in `crates/*/tests/`, one binary per file, with shared
fixtures under `tests/common`. Media-dependent tests skip when the local
GStreamer stack lacks a path and record it in `KAHAWAI_MEDIA_SKIP_FILE` if set.

## Related repository

`../kahawai-android` (sibling checkout) is the Kotlin/Compose/Media3 Android
and Google TV client. It consumes only the hub's `/api/v1/*` contract, so API
changes here can break it; it has its own README with build and run steps.
