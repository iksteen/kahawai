-- The source catalogue is authoritative local state. Schema meaning and the
-- version/epoch contract live in `src/catalog.rs`; this file is only the
-- immutable engine change log.
CREATE TABLE catalog_collections (
    id                        TEXT PRIMARY KEY,
    media_type                TEXT NOT NULL,
    epoch                     TEXT NOT NULL,
    current_version           INTEGER NOT NULL DEFAULT 0,
    oldest_replayable_version INTEGER NOT NULL DEFAULT 0,
    scan_generation           INTEGER NOT NULL DEFAULT 0,
    scanning                  INTEGER NOT NULL DEFAULT 0,
    scanned                   INTEGER NOT NULL DEFAULT 0,
    failed                    INTEGER NOT NULL DEFAULT 0,
    skipped                   INTEGER NOT NULL DEFAULT 0,
    retired                   INTEGER NOT NULL DEFAULT 0
);

-- One current value per replicated entity. Deleted rows are tombstones until
-- every hub currently receiving this collection has acknowledged them.
CREATE TABLE catalog_records (
    collection_id TEXT NOT NULL REFERENCES catalog_collections(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    record_key    BLOB NOT NULL,
    version       INTEGER NOT NULL,
    payload       BLOB NOT NULL,
    deleted       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (collection_id, kind, record_key)
) WITHOUT ROWID;
CREATE INDEX catalog_records_version
    ON catalog_records(collection_id, version);

-- Scanner fast path and durable seen-generation journal. Derived records are
-- revision-bound by the size/mtime stored beside their source.
CREATE TABLE catalog_files (
    collection_id TEXT NOT NULL REFERENCES catalog_collections(id) ON DELETE CASCADE,
    root_token    TEXT NOT NULL,
    path_rel      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    mtime_unix    INTEGER NOT NULL,
    head_xxh3     INTEGER NOT NULL,
    tail_xxh3     INTEGER NOT NULL,
    oshash        INTEGER NOT NULL,
    streams_json  TEXT NOT NULL,
    seen_generation INTEGER NOT NULL,
    version       INTEGER NOT NULL,
    error         TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (collection_id, root_token, path_rel)
) WITHOUT ROWID;

-- ACK observations are not sync authority (the hub supplies its cursor on
-- every connection); they exist only to prove tombstone compaction is safe.
CREATE TABLE catalog_hub_acks (
    hub_id        TEXT NOT NULL,
    collection_id TEXT NOT NULL REFERENCES catalog_collections(id) ON DELETE CASCADE,
    epoch         TEXT NOT NULL,
    version       INTEGER NOT NULL,
    PRIMARY KEY (hub_id, collection_id)
) WITHOUT ROWID;

-- Durable local discovery queue. `running` becomes `pending` at startup;
-- analyzer generation plus source revision makes completion/failure reusable.
CREATE TABLE catalog_jobs (
    collection_id TEXT NOT NULL REFERENCES catalog_collections(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    job_key       BLOB NOT NULL,
    root_token    TEXT NOT NULL DEFAULT '',
    path_rel      TEXT NOT NULL DEFAULT '',
    size          INTEGER NOT NULL DEFAULT 0,
    mtime_unix    INTEGER NOT NULL DEFAULT 0,
    generation    INTEGER NOT NULL DEFAULT 0,
    priority      INTEGER NOT NULL DEFAULT 0,
    state         TEXT NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','running','done','failed')),
    error         TEXT NOT NULL DEFAULT '',
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (collection_id, kind, job_key)
) WITHOUT ROWID;
CREATE INDEX catalog_jobs_pending
    ON catalog_jobs(state, priority DESC, updated_at DESC);
