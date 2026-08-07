-- HUB-10: per-library access grants. What the flag and the list mean is
-- in the hub/grants.rs module doc, next to the code that enforces them.

ALTER TABLE users ADD COLUMN all_libraries INTEGER NOT NULL DEFAULT 1;

CREATE TABLE user_libraries (
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    library_id TEXT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, library_id)
) WITHOUT ROWID;
