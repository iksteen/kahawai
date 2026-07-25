-- Per-user preferences (HUB-33 dual-audio; later per-user knobs like
-- the OCR text tier reuse the same table). scope = library id, or ''
-- for user-global keys.
CREATE TABLE user_prefs (
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    scope   TEXT NOT NULL DEFAULT '',
    key     TEXT NOT NULL,
    value   TEXT NOT NULL,
    PRIMARY KEY (user_id, scope, key)
);
