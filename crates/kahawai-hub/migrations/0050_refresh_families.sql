-- Atomic rotating refresh-token families (AUTH-4/5).

DROP TABLE refresh_tokens;

CREATE TABLE refresh_families (
    id                 TEXT PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    current_token_hash TEXT NOT NULL UNIQUE,
    expires_at         INTEGER NOT NULL,
    revoked_at         INTEGER,
    created_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    rotated_at         INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX refresh_families_user ON refresh_families (user_id);
