-- Per-user watch state (HUB-10). Rows die with their item (file removal
-- reconciliation); the content-identity archive that survives mediahost
-- deletion arrives with HUB-20.

CREATE TABLE watch_state (
    user_id     TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    item_id     TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    played      INTEGER NOT NULL DEFAULT 0,
    play_count  INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (user_id, item_id)
);
