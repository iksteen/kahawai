-- Sealed credentials. Semantics: the module doc of kahawai-hub/src/secrets.rs
-- (house rule: schema meaning lives next to the code that enforces it).
CREATE TABLE credentials (
    -- users.id, or '' for one the hub holds itself. Empty string rather than
    -- NULL because `WHERE owner_id = NULL` matches nothing, so a NULL sentinel
    -- would make every hub-wide query silently return empty. No foreign key,
    -- because '' is not a user -- so deleting a user has to delete their
    -- credentials explicitly, which `secrets::delete_owner` does inside the
    -- same transaction.
    owner_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    field    TEXT NOT NULL,
    -- nonce(12) || ciphertext || tag(16), sealed under <data_dir>/credentials.secret
    -- with (owner_id, provider, field) as additional data, so a row moved to
    -- another owner or field fails to open. One column, so there is no
    -- representable state where a nonce exists without its ciphertext.
    secret   BLOB NOT NULL,
    PRIMARY KEY (owner_id, provider, field)
) WITHOUT ROWID;
