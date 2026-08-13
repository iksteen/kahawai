-- Watch restoration for multipart media follows the complete ordered rendition,
-- not whichever individual disc happened to disappear or return first.
CREATE TABLE watch_source_archive (
    user_id            TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    position_ms        INTEGER NOT NULL,
    duration_ms        INTEGER,
    played             INTEGER NOT NULL,
    play_count         INTEGER NOT NULL,
    archived_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY(user_id,source_fingerprint)
);
