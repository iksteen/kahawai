-- HUB-24: subtitles fetched from external providers (OpenSubtitles),
-- keyed to the item. The cues/ASS live in the subtitle cache under
-- key "d{id}"; this table is what list() enumerates.
CREATE TABLE downloaded_subtitles (
    id           INTEGER PRIMARY KEY,
    item_id      TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    provider     TEXT NOT NULL,
    language     TEXT,
    format       TEXT NOT NULL,       -- 'srt' | 'ass'
    release_name TEXT,                -- provider's file/release label
    created_at   INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX downloaded_subtitles_item ON downloaded_subtitles (item_id);
