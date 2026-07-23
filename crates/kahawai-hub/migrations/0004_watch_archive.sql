-- Watch-state archive (HUB-20): keyed to content identity (MH-5), so watch
-- state survives mediahost deletion and file moves — restored when the same
-- bytes reappear under any path on any host. binding_archive (manual match
-- bindings) arrives with enrichment.

CREATE TABLE watch_state_archive (
    user_id     TEXT NOT NULL,
    size        INTEGER NOT NULL,
    head_xxh3   INTEGER NOT NULL,
    tail_xxh3   INTEGER NOT NULL,
    position_ms INTEGER NOT NULL,
    duration_ms INTEGER,
    played      INTEGER NOT NULL,
    play_count  INTEGER NOT NULL,
    archived_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (user_id, size, head_xxh3, tail_xxh3)
);
