-- Optimistic concurrency for an account's library grants (UI-25).
-- Meaning lives in the `grants` module doc ("What is stored"), beside the
-- code that enforces it.
ALTER TABLE users ADD COLUMN grants_version INTEGER NOT NULL DEFAULT 0;
