-- AniDB never-ask-twice cache (HUB-30): FILE-by-ED2K answers, hits and
-- misses alike, keyed by content hash. aid NULL = AniDB doesn't know it.
CREATE TABLE ed2k_aid (
    ed2k       TEXT PRIMARY KEY,
    aid        INTEGER,
    updated_at INTEGER NOT NULL
);
