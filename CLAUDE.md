# Kahawai

Self-hosted media streaming server in Rust: a hub (clients talk only to it),
plus mediahost and transcoder satellites that dial in over mTLS. Single
binary, subcommands per module, `all-in-one` runs the lot in one process.

Requirements, design and status live in `docs/`:
`kahawai-technical-requirements.md` (numbered requirements — HUB-n, MH-n,
OPS-n …), `kahawai-implementation.md` (how it works and why),
`kahawai-status-checklist.md` (what is actually built).

## Working agreements

1. **Criteria before code.** For any change with a cost trade-off, state
   the decision criteria and the cost model first, and get them
   confirmed. Anything about caches, quotas or eviction needs *both*
   axes named explicitly: cost to rebuild the data, and latency at the
   moment it is needed. "It looks big" is not a cost model.

2. **Third-party facts come from primary docs, fetched now.** Rate
   limits, quotas, auth rules, ban behaviour: read the provider's own
   documentation in-session and quote the rule in a comment beside the
   constant. Never from recall — recall got three of four provider
   limits wrong, all in our favour, and earned an AniDB ban.

3. **Verify against reality, not the compiler.** Green tests are
   necessary, not sufficient. Gate on exit codes (never grep counts),
   then check the thing itself: query the live database, restart the
   service, read what actually landed. The episode-queue bug of
   2026-07-26 passed every test and surfaced only from restarting the hub
   and reading the queue afterwards.

4. **Decide what is yours to decide.** A design question I authored is
   mine to answer, with a recommendation and reasons. Ask when a
   *criterion* is missing — not for permission to proceed. Both failures
   cost the same: guessing, and stalling.

5. **Scope moves are the user's call.** If a requirement looks wrong,
   say so in a sentence and deliver it anyway under stated assumptions.
   Do not quietly narrow it, and do not mark it done by redefining it.

6. **No cosmetic surgery on data.** Schema *meaning* belongs in the Rust
   module doc next to the code that enforces it; migrations are an
   immutable log of changes and their comments go stale. Never rebuild a
   table or touch `_sqlx_migrations` to fix wording. Engine-specific
   tricks (SQLite `sqlite_master`, `PRAGMA`) are a smell: the storage
   engine is an implementation detail.

7. **If a minimal-diff mode is active** — ponytail or similar, enabled
   per developer, not by this project — note that it pushes for the
   shortest change that works. That is a good default for feature work in
   settled code and the wrong one for architecture, data models and cost
   models. In those, prefer the correct design over the small one, and
   say which you chose.

## Build, test, verify

```sh
cargo build                     # default-members = the kahawai binary
cargo test --workspace          # 31 test binaries; gate on the exit code
cargo clippy --workspace --all-targets
```

Requirement-status changes update `docs/kahawai-status-checklist.md` in
the *same* commit. Non-trivial logic leaves one runnable check behind.

## Running the local hub

A dev hub runs unsupervised from `target/debug/kahawai hub` — nothing
restarts it for you. State lives under the XDG data dir
(`~/.local/share/kahawai/` by default, `[hub] data_dir` to override).
Restarting it after a fix is routine work; do it rather than asking:

```sh
cargo build                     # FIRST: sqlx::migrate! embeds migration
                                # SQL at compile time, so an un-rebuilt
                                # binary re-applies the OLD sql
pkill -f '^\./target/debug/kahawai hub'
nohup ./target/debug/kahawai hub >> ~/.local/share/kahawai/hub.log 2>&1 &
```

Then verify: `_sqlx_migrations` version, process up, log tail. Migrations
apply only at hub startup, so an un-restarted hub is a schema behind.

Deployment topology, cross-compilation and the NAS/macOS satellites:
`docs/kahawai-deployment.md`.

## Where the answers live

- **Provider traffic** — `hub/gate.rs` is the only way out: one queue per
  provider host, spaced at that provider's documented rate, 429/503
  parks that provider alone. AniDB additionally has a two-tier flood
  rule (short 2 s *and* sustained 4 s) and a ban recorded on disk that is
  checked before a socket opens. Adding a provider means adding its
  spacing there, with the source quoted.
- **Enrichment schema** — the `hub/providers.rs` module doc is the
  reference for `provider_metadata`, `merged_metadata`, `provider_ranks`
  and `enrichment_queue`. Providers write their own answers; nothing
  writes the merged row.
- **Caches are not evicted**, by decision (OPS-6): every one is either
  expensive to rebuild (subtitle extractions re-demux a whole file) or
  latency-critical at point of use (artwork during a grid scroll). Don't
  propose a janitor.

## House rules

- **Never `git push`.** Commit locally; the maintainer pushes.
- Bundle IDs, launchd labels and service names: `org.thegraveyard.*`.
- Every CLI-testable feature gets a companion script in `scripts/`
  (see `kahawai-play.sh`, `kahawai-list.sh`).
- Media is never written to. User state survives crashes.
