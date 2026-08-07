CREATE TABLE IF NOT EXISTS image_set_failures (
    module_id     TEXT    NOT NULL,
    collection_id TEXT    NOT NULL,
    path_rel      TEXT    NOT NULL,
    sub_index     INTEGER NOT NULL,
    mtime_unix    INTEGER,
    error         TEXT    NOT NULL,
    at            INTEGER NOT NULL,
    PRIMARY KEY (module_id, collection_id, path_rel, sub_index)
);
