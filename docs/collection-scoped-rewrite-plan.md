# Collection-scoped catalogue rewrite plan

This file is the durable hand-off for replacing the unpublished exact-root and
library-presentation work with a simpler collection-scoped catalogue. Follow it
after a context compaction rather than reconstructing the plan from memory.

## Current state and safety anchors

At the time this plan was written:

- `master` is `6552957 Scope media presentations to libraries`.
- `origin/master` is `239a31e Generate web assets only in artifact builds`.
- Local `master` is seven commits ahead of `origin/master`; none of migrations
  53–56 has been pushed.
- The local AIO has applied migration 56 and is healthy.
- Migration 54's immutable live SHA-384 is:
  `3afd3f902ca273e249bff9b56cfded3aa6f32c4103f5e9cf324e17e71c491443b71e575db5aeb81013c4af0ec19ae4fe`.
- Current migration 56's applied SHA-384 is:
  `bb596f5e26418a0dc7956bdca358d9f2f31cc811defd94de47f8351cf47e4878e3aa9a90b6437f9b0bf0ea0e24769344`.
- The verified pre-migration-56 snapshot is:
  `~/.local/share/kahawai/backups/pre-library-scope-20260813-161210`.
- The confirmed level-52 catalogue backup is:
  `~/.local/share/kahawai/backups/pre-library-verify-20260812-214614/hub.db`.
- Another useful pre-protocol-3 complete backup is:
  `~/.local/share/kahawai/backups/pre-protocol3-live-20260813-132149`.
- Existing safety branches include `backup/batch-rematch-upgrade` and
  `backup/split-30deace`; do not delete them.
- Never start a rewritten migration set against the current migration-56
  database. Its checksums are expected to differ.
- Never push unless explicitly requested.

Before rewriting history, create this additional branch:

```bash
git branch backup/pre-collection-model-rewrite-6552957 6552957
```

Do not edit or remove the safety branch until the rewritten deployment has aged
and been explicitly accepted.

## 1. Freeze the intended semantics

The final rules are:

1. Every item belongs to exactly one collection.
2. Every playable source belongs to exactly one item in that collection.
3. A library only contains collections; it does not own or clone items.
4. Putting one collection in several libraries exposes the same item IDs and
   watch state.
5. Items in different collections are always independent:
   - no title-based merging;
   - no provider-ID-based merging;
   - no shared watch state;
   - no shared provider/manual/query state.
6. A library containing both `anime` and `animore` shows two Hellsing Ultimate
   series.
7. Alternate sources may deduplicate only within one collection item.
8. Exact-root addressing remains mandatory.
9. Provider matches may later be corrected independently for collection items;
   the current wrong Hellsing provider match must not force identity sharing.

Update HUB-3 and the implementation/status documentation to state these rules.

## 2. Decision criteria and cost model

The rewrite optimizes for:

1. **Losslessness:** retain item IDs where possible, every physical source,
   provider/manual/query state, watch state, subtitle/cache data, relations and
   collection generations.
2. **One authoritative representation:** no permanent compatibility views or
   duplicate source ownership models.
3. **Simple steady state:** collection ownership should be explicit; libraries
   should require no item synchronization machinery.
4. **One-time migration cost:** a bounded transformation of roughly 40k items,
   38k files and 53k subtitle tracks is acceptable if measured and proven.
5. **No rebuild latency:** migration must not rescan, contact providers,
   re-extract subtitles or reset generations.

For caches, preserve both named axes: extraction/artwork caches are expensive to
rebuild, and they are latency-critical when requested. Do not introduce
janitorial eviction.

## 3. Preserve the current branch and deployment

1. Create `backup/pre-collection-model-rewrite-6552957`.
2. Verify it points exactly at `6552957`.
3. Keep the current migration-56 live database and all backup snapshots.
4. Take another complete online snapshot immediately before the final cutover.
5. Record current health, counts, generations, checksums and process IDs.
6. Do not develop rewritten migrations in the checkout/target directory used by
   the running release binary.

## 4. Select and verify the level-52 source database

Inventory all candidate level-52 backups. Prefer the newest complete and
verified snapshot, but use the known catalogue database above if needed.

Work only on copies. Verify:

```sql
SELECT max(version) FROM _sqlx_migrations; -- must be 52
PRAGMA foreign_key_check;
PRAGMA quick_check;
```

Record baseline counts and complete key inventories for:

- users, auth versions and refresh families;
- items and parent/child hierarchy;
- files and source bindings;
- collections and generations;
- provider metadata and provider questions;
- manual and rejected matches;
- anime IDs and relations;
- enrichment queue;
- watch state and archives;
- subtitle tracks and cache inventory;
- image failures;
- recorded misses and all other durable provider inputs.

Never modify the backup itself.

## 5. Develop in a separate worktree

Preserve commits through `9b47931 Keep lightweight media work in the hub` and
replace the unpublished schema/protocol commits after it:

- `c8923e9 Use deterministic exact-root source identity`;
- `38c13b7 Require protocol 3 exact source identity`;
- `6552957 Scope media presentations to libraries`.

Create a replacement branch and worktree:

```bash
git worktree add \
  -b rewrite/collection-scoped-v53 \
  ../kahawai-collection-scoped \
  9b47931
```

Use a separate target directory:

```bash
export CARGO_TARGET_DIR="$(realpath -m ../kahawai-collection-scoped-target)"
```

Keep this target on the repository filesystem rather than a size-limited
`/tmp`, and separate from the live checkout's `target`.

Do not reset `master` until the replacement branch has passed every migration,
test and live-shaped proof.

## 6. Define the simplified final schema

Exact table names may be refined during design, but each concept must have one
authoritative representation.

### 6.1 Collections and roots

```text
collections
collection_roots(
    root_id,
    module_id,
    collection_id,
    root_token,
    normalized_path
)
```

A root token/path binding exists once in `collection_roots`.

Required constraints:

- root token is non-empty and unique in the appropriate mediahost identity
  space;
- `(module_id, collection_id, normalized_path)` is unique;
- a token can never map to two normalized path byte strings;
- duplicate/nested/overlapping roots remain rejected within one collection.

### 6.2 Items

```text
items(
    id,
    module_id,
    collection_id,
    parent_id,
    kind,
    title,
    ...
)
```

Required invariants:

- every item belongs to exactly one collection;
- parent and child belong to the same collection;
- title/year/provider identity never crosses collection boundaries;
- item IDs remain stable wherever one old item maps to one collection.

Prefer a relational collection FK over duplicated free-form identity. If SQLite
cannot express a same-collection parent constraint directly, enforce it with a
small trigger and a runnable invariant test.

### 6.3 Sources

Replace split `files`/`item_sources` ownership with one authoritative source
model, for example:

```text
sources(
    source_id,
    module_id,
    collection_id,
    root_id NULL,
    path_rel,
    item_id NULL,
    part,
    size,
    mtime_unix,
    fingerprints,
    oshash,
    ed2k,
    streams_json,
    extraction state,
    revision,
    ...
)
```

Properties:

- `item_id` may be null only for unresolved/bare files;
- `root_id` may be null only during legacy level-52 adoption;
- exact source identity is unique by collection, root and relative path;
- adoption assigns `root_id` but does not rewrite a synthetic path key;
- playback and source-bound cache records use `source_id`;
- no `media_sources` compatibility view;
- no `library_item_sources` table;
- no NUL-prefixed composite path encoding.

If retaining separate `files` and source-binding tables is materially simpler,
the same rule applies: there must be one authoritative collection-scoped binding
and one stable source ID. Do not recreate parallel physical and presentation
source representations.

### 6.4 Provider and user state

These remain attached directly to collection items:

```text
provider_metadata(item_id, ...)
provider_queries(item_id, ...)
manual_match(item_id, ...)
rejected_matches(item_id, ...)
anime_ids(item_id, ...)
item_relations(item_id, ...)
watch_state(user_id, item_id, ...)
```

There is no watch synchronization trigger. The same collection in two libraries
naturally exposes the same item/watch key. Different collections remain
independent.

### 6.5 Libraries

```text
library_collections(library_id, module_id, collection_id)
```

Browse follows:

```text
library -> library_collections -> collection items
```

Remove `item_libraries`. Deleting or detaching a library must not delete
collection items, provider state, watch state, sources or subtitle assets.

### 6.6 Subtitles

Use two explicit ownership modes:

- source-bound embedded/sidecar/OCR/raster tracks reference `source_id`;
- downloaded/manual item tracks reference `item_id`.

Derived tracks reference their physical parent track or source. They must not
need library-presentation projection, item-owner rehoming or duplicated cache
payloads.

## 7. Replace migrations 53–56 with one direct migration 53

Starting from `9b47931`, add one migration, for example:

```text
0053_collection_scoped_exact_sources.sql
```

Do not retain the unpublished current migrations 53–56 in rewritten history.
The new migration must transform the level-52 schema directly into the final
collection-scoped, exact-source schema.

### 7.1 Migration algorithm

1. Create final `collection_roots`, authoritative source storage and replacement
   collection ownership structures.
2. Create a temporary mapping:

   ```text
   old_item x collection -> new_item
   ```

3. Discover collection membership from every old item source.
4. Propagate child collection membership to shows/albums.
5. Abort if a sourced item cannot be assigned to a collection.
6. For an old item appearing in one collection, preserve its original ID.
7. For an old item appearing in several collections:
   - preserve the original ID for a deterministic collection;
   - generate deterministic collision-safe IDs for the other collections.
8. Clone parent items before children.
9. Bind every child to the parent clone in the same collection.
10. Convert every level-52 file/source pair into exactly one authoritative source
    record with a stable source ID.
11. Preserve bare files with `item_id = NULL`.
12. Initially leave legacy `root_id = NULL`; do not guess or encode roots into
    path strings.
13. Copy provider metadata, questions, manual/rejected matches, anime IDs,
    relations and queued work to every collection-specific item produced from
    an old shared item.
14. Copy watch state to every collection-specific item produced from an old
    shared item. Those copies become independent immediately after migration.
15. Rebind embedded/sidecar subtitle rows to `source_id`.
16. Clone genuinely item-level downloaded subtitle records where an old item
    split across collections, while preserving payload/cache bytes.
17. Rebuild derived metadata and match views from preserved inputs; do not call
    providers.
18. Preserve collection generations exactly.
19. Drop temporary mapping tables and obsolete source/membership triggers.
20. Leave no permanent compatibility view or library-presentation table.

### 7.2 Migration prohibitions

Migration 53 must not:

- scan media;
- contact providers;
- advance collection generations;
- clear/replay enrichment work without cause;
- delete subtitle, artwork or provider cache files;
- infer equivalence across collections;
- use title/provider identity to merge collection items;
- edit `_sqlx_migrations` manually.

## 8. Port only the necessary root/protocol work

Reimplement the useful parts of `c8923e9` and `38c13b7` against the simplified
source model.

Retain:

- protocol 3 and rejection of protocol 2 satellites;
- exact `SourcePath { root_token, path_rel }` messages;
- deterministic full-width root tokens;
- lexical path normalization without filesystem canonicalization;
- token/path collision detection;
- duplicate/overlapping-root validation;
- exact-root reads with no root-order fallback;
- unavailable-root manifest preservation;
- targeted ambiguous multi-root resolution;
- crash-safe adoption acknowledgement;
- immediate retry of the consumed startup trigger after acknowledgement.

Simplify adoption:

- validated announcements populate `collection_roots`;
- single-root adoption sets `sources.root_id` transactionally;
- multi-root adoption resolves individual source rows using path, size,
  head/tail fingerprints and oshash;
- adoption never rewrites source path keys;
- source-bound cache records remain attached through stable `source_id`;
- use one minimal durable adoption-pending marker only if still required for
  crash safety.

## 9. Rewrite runtime resolution around collections

Resolution must operate per collection:

```text
for each source:
    resolve candidate only among items in source.collection
```

Constrain collection identity in:

- movie parsing and multipart grouping;
- shows and episodes;
- albums and tracks;
- anime hash rebinding;
- bare-file hash binding;
- provider matching and assignment;
- orphan cleanup;
- subtitle/source worklists;
- artwork and NFO reads;
- playback/session source selection.

No query may resolve by title/year/provider ID without also constraining the
collection.

When a collection belongs to several libraries, ingest and resolve it once. Do
not create library-specific items.

## 10. Replace library-presentation regressions

Required tests:

1. `anime` and `animore` each contain Hellsing Ultimate with:
   - separate show IDs;
   - separate episode IDs;
   - separate sources;
   - separate provider/manual/query state;
   - separate watch state.
2. One library containing both collections shows both series.
3. One collection attached to two libraries exposes the same item IDs in both.
4. Updating watch state through either library reads/writes the same item state
   because both reference the same item.
5. Identical provider IDs in different collections never merge items.
6. Identical normalized title/year identities in different collections never
   merge items.
7. Multiple physical copies within one collection may remain alternate sources
   of one item.
8. Hash rebinding affects only the source's collection.
9. Deleting one library does not delete collection items or state.
10. Detaching a collection changes only library membership.
11. Subtitle assets remain accessible after library membership changes.
12. Single-root and ambiguous multi-root level-52 upgrades remain lossless.
13. Root adoption changes no item ID, source ID, watch state, provider state or
    generation.
14. Equal relative paths under two roots remain distinct exact sources.
15. An unavailable root preserves its old manifest while other roots scan.

Remove tests specific to:

- library item cloning;
- `library_item_sources`;
- `media_sources`;
- watch synchronization triggers;
- subtitle presentation projection;
- subtitle-owner rehoming.

## 11. Prove migration 52 -> 53 on disposable databases

Run the migration against:

1. small synthetic fixtures;
2. the selected real level-52 catalogue copy;
3. a synthetic ambiguous multi-root database.

Required assertions:

- every old physical source maps to exactly one new source;
- every source-bound item maps to the correct collection;
- every old item ID survives as a canonical item or has a recorded deterministic
  split mapping;
- expected clones account exactly for item-count increases;
- every parent/child hierarchy remains collection-local;
- provider/manual/query/relation/watch records are preserved for every split
  collection item;
- every subtitle payload/cache record remains reachable;
- collection generations are unchanged;
- enrichment queue semantics are unchanged;
- zero FK violations;
- `quick_check = ok`;
- no provider calls;
- no scan;
- no permanent temporary tables or compatibility views.

Specifically verify Hellsing becomes:

- `anime`: 10 episodes;
- `animore`: 4 episodes;
- two independent shows even when one library contains both collections.

Record migration runtime and query plans for any expensive join. Use temporary
migration-local indexes where rebuild cost is low and steady-state use does not
justify permanent write amplification.

## 12. Handle changes made after the level-52 backup

The level-52 backup predates the current live database. A plain restore would
lose newer media and user state.

Build a one-shot, tested logical exporter/importer:

1. Read the frozen current migration-56 database.
2. Export current data in collection-scoped terms.
3. Treat different collections as independent.
4. Consolidate temporary per-library clones only when they represent the same
   collection item.
5. Preflight for conflicting temporary clones:
   - different manual matches;
   - divergent provider answers under the same key;
   - divergent watch states;
   - conflicting hierarchy or titles;
   - conflicting source assignments.
6. Abort and report every conflict rather than silently choosing a winner.
7. Replay additions and updates into the freshly migrated level-53 target.
8. Overlay current exact-root bindings and current collection generations.
9. Preserve current item IDs where compatible with the deterministic mapping.
10. Verify every post-level-52 source and durable user-state key appears in the
    staged target.

This importer is a one-shot cutover tool, not permanent runtime compatibility
logic. Leave it as a runnable verification/recovery script if useful, but do not
route normal ingestion through it.

## 13. Run full gates before changing master

From the rewrite worktree:

```bash
cargo fmt --all -- --check
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Also run the real-catalogue migration and logical-comparison scripts. Gate on
exit codes, then inspect the staged database directly.

Commit the replacement schema/protocol as one atomic commit if intermediate
states are not independently releasable:

```text
Use collection-scoped exact source identity
```

## 14. Replace unpublished history on master

After all gates pass:

```bash
git status --short
git log --oneline --decorate --graph --all
```

Verify `backup/pre-collection-model-rewrite-6552957` still points at `6552957`.

In the main checkout:

```bash
git reset --hard rewrite/collection-scoped-v53
```

This is safer than interactively rebasing `master`: development occurs on a
replacement branch and `master` moves only after proof.

Expected final history:

```text
<new collection-scoped commit>
9b47931 Keep lightweight media work in the hub
42c8bea Resume enrichment after completed scans
1c1b25d Stop deployed satellites by executable identity
4e2962c Scope playback sessions to their owner
239a31e origin/master
```

The old history remains reachable through the safety branch. Because
`origin/master` is an ancestor, a future push should remain fast-forward; no
force push should be needed. Do not push without explicit instruction.

## 15. Perform live cutover through a staged database

1. Take a fresh complete migration-56 backup using `kahawai hub backup`.
2. Verify its manifest, DB size, subtitle file/byte counts, PKI/config/JWT,
   migration 56 checksum, FK check and `quick_check`.
3. Record current counts, generations, health and process IDs.
4. Stop AIO safely:

   ```bash
   scripts/kahawai-restart.sh all-in-one --stop-only
   ```

5. Verify the process is dead.
6. Export the now-frozen migration-56 logical state.
7. Create a staging data directory on the same filesystem.
8. Restore an immutable copy of the level-52 database into staging.
9. Apply rewritten migration 53 offline using the rewritten binary's embedded
   migrator.
10. Replay the frozen logical export.
11. Verify the staged database completely.
12. Preserve the displaced live database plus WAL/SHM in a timestamped rollback
    directory.
13. Install the staged database atomically.
14. Keep current PKI, JWT secret, config and subtitle/cache trees.
15. Build the rewritten release binary with:

    ```bash
    RUST_MIN_STACK=16777216 cargo build --release --bin kahawai
    ```

16. Start AIO through `scripts/kahawai-restart.sh all-in-one`.
17. Do not deploy or start disabled Silence/Mac mini transcoders.
18. Deploy Silence mediahost only if protocol wire bytes changed; protocol 3
    semantics alone do not justify touching it.

## 16. Verify the live result

Query the real database, not only logs:

- rewritten migration 53 applied with expected checksum;
- zero failed migrations;
- zero FK errors;
- `quick_check = ok`;
- every user/auth key preserved;
- every physical source preserved;
- every provider/manual/query/relation/watch record preserved;
- subtitle/cache inventories unchanged;
- collection generations unchanged;
- no unresolved roots for mounted single-root collections;
- queue unchanged except independently owed work;
- no compatibility views/tables from the abandoned presentation model.

Behavioral verification:

- `/health` reports `ok`;
- Silence reconnects over protocol 3;
- all collections report in sync and skip scans;
- one library with both `anime` and `animore` shows two Hellsing series;
- those series have independent watch state and independently correctable
  provider matches;
- one collection in two libraries exposes the same item/watch state;
- direct play reads the exact root/source;
- subtitles remain available;
- disabled transcoders remain disabled and stopped.

Keep the old branch and all snapshots until the rewritten deployment has aged
and been explicitly accepted.
