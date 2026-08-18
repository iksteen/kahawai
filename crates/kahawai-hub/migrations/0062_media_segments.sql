-- Intro / recap / credits boundaries, and the record of having looked.
-- Semantics: the module doc of kahawai-hub/src/segments.rs (house rule:
-- schema meaning lives next to the code that enforces it).
CREATE TABLE media_segments (
    item_id  TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL CHECK (kind IN ('recap', 'intro', 'credits')),
    start_ms INTEGER NOT NULL,
    end_ms   INTEGER NOT NULL CHECK (end_ms > start_ms),
    -- Which analyzer answered: 'chromaprint' or 'blackframe'.
    source   TEXT NOT NULL,
    PRIMARY KEY (item_id, kind)
);

-- One row per episode the detector has finished with, whether or not it
-- found anything. Without it, a season with no shared opening is a
-- season the sweep re-analyzes forever.
CREATE TABLE media_segment_scans (
    item_id    TEXT PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
    scanned_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- The detector generation that produced it. Bumping the constant in
    -- segments.rs is how a changed algorithm asks for the season again.
    detector   INTEGER NOT NULL
);
