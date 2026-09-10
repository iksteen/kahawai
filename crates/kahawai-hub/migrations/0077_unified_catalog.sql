ALTER TABLE items RENAME TO collection_items;
CREATE INDEX catalog_copy_membership ON collection_items(id,module_id,collection_id);
ALTER TABLE watch_state RENAME TO state_imports;
ALTER TABLE state_imports ADD COLUMN expected_catalog_id TEXT;
ALTER TABLE state_imports ADD COLUMN resume_source_fingerprint TEXT;

CREATE TABLE catalog_items (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('movie','series','episode','album','song')),
    title TEXT NOT NULL,
    norm_title TEXT NOT NULL,
    sort_title TEXT NOT NULL,
    year INTEGER,
    artist TEXT,
    artist_key TEXT,
    norm_artist TEXT,
    anime INTEGER NOT NULL DEFAULT 0,
    provisional INTEGER NOT NULL DEFAULT 1,
    merged_into TEXT REFERENCES catalog_items(id),
    added_id TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX catalog_title ON catalog_items(sort_title,id);
CREATE INDEX catalog_year ON catalog_items(year,sort_title,id);
CREATE INDEX catalog_added ON catalog_items(added_id,id);
CREATE INDEX catalog_kind ON catalog_items(kind,sort_title,id);
CREATE INDEX catalog_artist ON catalog_items(artist_key,kind,sort_title,id);

CREATE TABLE catalog_keys (
    catalog_id TEXT NOT NULL REFERENCES catalog_items(id),
    key TEXT NOT NULL,
    PRIMARY KEY(catalog_id,key)
);
CREATE INDEX catalog_key_lookup ON catalog_keys(key,catalog_id);

CREATE TABLE item_assignments (
    collection_item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL DEFAULT 0,
    mode TEXT NOT NULL DEFAULT 'automatic' CHECK(mode IN ('automatic','manual','unmatched')),
    evidence TEXT,
    metadata_eligible INTEGER NOT NULL DEFAULT 1,
    conflict TEXT
);
CREATE TABLE assignment_members (
    collection_item_id TEXT NOT NULL REFERENCES item_assignments(collection_item_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    catalog_id TEXT NOT NULL REFERENCES catalog_items(id),
    PRIMARY KEY(collection_item_id,ordinal),
    UNIQUE(collection_item_id,catalog_id)
);
CREATE INDEX assignment_catalog ON assignment_members(catalog_id,collection_item_id);

CREATE TABLE assignment_pins (
    collection_item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
    catalog_ids TEXT NOT NULL
);
CREATE TABLE rejected_catalog_matches (
    collection_item_id TEXT NOT NULL REFERENCES collection_items(id) ON DELETE CASCADE,
    catalog_id TEXT NOT NULL REFERENCES catalog_items(id),
    PRIMARY KEY(collection_item_id,catalog_id)
);
CREATE TABLE catalog_overrides (
    catalog_id TEXT PRIMARY KEY REFERENCES catalog_items(id),
    fields TEXT NOT NULL
);

CREATE TABLE episode_details (
    item_id TEXT PRIMARY KEY REFERENCES catalog_items(id),
    series_id TEXT NOT NULL REFERENCES catalog_items(id),
    season INTEGER,
    episode INTEGER
);
CREATE INDEX episodes_series ON episode_details(series_id,season,episode,item_id);
CREATE TABLE episode_numberings (
    episode_id TEXT NOT NULL REFERENCES episode_details(item_id),
    series_id TEXT NOT NULL REFERENCES catalog_items(id),
    scheme TEXT NOT NULL,
    season INTEGER NOT NULL,
    episode INTEGER NOT NULL,
    PRIMARY KEY(episode_id,scheme,season,episode)
);
CREATE INDEX episode_number_lookup ON episode_numberings(series_id,scheme,season,episode);

CREATE TABLE album_tracks (
    collection_item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
    album_id TEXT NOT NULL REFERENCES catalog_items(id),
    song_id TEXT NOT NULL REFERENCES catalog_items(id),
    disc_number INTEGER NOT NULL,
    track_number INTEGER NOT NULL
);
CREATE INDEX album_positions ON album_tracks(album_id,disc_number,track_number,song_id);
CREATE INDEX album_songs ON album_tracks(song_id,album_id);
CREATE TABLE source_boundaries (
    playable_source_id INTEGER NOT NULL REFERENCES playable_sources(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    source_fingerprint TEXT NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL CHECK(end_ms>start_ms),
    PRIMARY KEY(playable_source_id,ordinal)
);

CREATE TABLE user_item_state (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES catalog_items(id),
    position_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    played INTEGER NOT NULL DEFAULT 0,
    play_count INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    resume_source_fingerprint TEXT,
    PRIMARY KEY(user_id,item_id)
);
CREATE INDEX user_progress ON user_item_state(user_id,updated_at DESC,item_id);
ALTER TABLE watch_state_archive ADD COLUMN catalog_item_id TEXT;
ALTER TABLE watch_state_archive ADD COLUMN state_updated_at INTEGER;
ALTER TABLE watch_state_archive ADD COLUMN resume_source_fingerprint TEXT;
ALTER TABLE watch_source_archive ADD COLUMN catalog_item_id TEXT;
ALTER TABLE watch_source_archive ADD COLUMN state_updated_at INTEGER;
ALTER TABLE watch_source_archive ADD COLUMN resume_source_fingerprint TEXT;

CREATE TABLE catalog_pending (
    collection_item_id TEXT PRIMARY KEY
);
INSERT INTO catalog_pending SELECT id FROM collection_items;

CREATE VIEW collection_watch_state AS
SELECT w.user_id,a.collection_item_id AS item_id,w.position_ms,w.duration_ms,
       w.played,w.play_count,w.updated_at,w.resume_source_fingerprint,w.item_id AS catalog_item_id
FROM assignment_members a JOIN user_item_state w ON w.item_id=a.catalog_id
WHERE a.ordinal=1;
