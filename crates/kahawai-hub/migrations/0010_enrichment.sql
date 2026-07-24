-- Enrichment (M4/HUB-7): provider metadata is a separate table — items
-- are identity, metadata is description. settings is a tiny KV store
-- (provider keys, prefs).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE item_metadata (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,             -- 'tmdb'
    provider_id TEXT NOT NULL,             -- '' when the search missed
    title       TEXT,
    overview    TEXT,
    poster_path TEXT,                      -- provider-relative image path
    rating      REAL,
    premiered   TEXT,
    genres      TEXT,                      -- JSON array of names
    confidence  TEXT NOT NULL,             -- auto | weak | miss
    updated_at  INTEGER NOT NULL
);
