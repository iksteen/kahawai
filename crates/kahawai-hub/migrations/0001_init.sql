-- Hub state (HUB-13): everything the hub owns lives in this embedded DB.

CREATE TABLE satellites (
    module_id        TEXT PRIMARY KEY,
    module_type      TEXT NOT NULL,
    name             TEXT NOT NULL,
    cert_fingerprint TEXT NOT NULL,
    enrolled_at      INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE revoked_certs (
    fingerprint TEXT PRIMARY KEY,
    revoked_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE collections (
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    media_type    TEXT NOT NULL,
    roots_json    TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (module_id, collection_id)
);

CREATE TABLE files (
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    path_rel      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    mtime_unix    INTEGER NOT NULL,
    -- u64 hashes stored as their i64 bit patterns
    head_xxh3     INTEGER NOT NULL,
    tail_xxh3     INTEGER NOT NULL,
    oshash        INTEGER NOT NULL,
    streams_json  TEXT NOT NULL,
    PRIMARY KEY (module_id, collection_id, path_rel)
);

CREATE TABLE items (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    title      TEXT NOT NULL,
    norm_title TEXT NOT NULL,
    year       INTEGER
);
CREATE INDEX items_kind_title ON items (kind, norm_title, year);

CREATE TABLE item_sources (
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    path_rel      TEXT NOT NULL,
    item_id       TEXT NOT NULL REFERENCES items (id),
    PRIMARY KEY (module_id, collection_id, path_rel)
);
CREATE INDEX item_sources_item ON item_sources (item_id);
