CREATE TABLE catalogue_watch_state (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    position_ms INTEGER NOT NULL DEFAULT 0 CHECK(position_ms >= 0),
    duration_ms INTEGER CHECK(duration_ms >= 0),
    played INTEGER NOT NULL DEFAULT 0 CHECK(played IN (0,1)),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY(user_id, item_id)
);
CREATE INDEX catalogue_watch_parent ON catalogue_watch_state(user_id,parent_id);
