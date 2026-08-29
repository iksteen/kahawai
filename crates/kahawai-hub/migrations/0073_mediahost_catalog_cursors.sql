-- Durable protocol-4 projection cursors. Semantics live in registry.rs beside
-- the transactional apply/reset code; this migration is only the engine log.
CREATE TABLE mediahost_catalog_cursors (
    module_id     TEXT NOT NULL REFERENCES satellites(module_id) ON DELETE CASCADE,
    collection_id TEXT NOT NULL,
    epoch         TEXT NOT NULL,
    version       INTEGER NOT NULL,
    PRIMARY KEY (module_id, collection_id),
    FOREIGN KEY (module_id, collection_id)
        REFERENCES collections(module_id, collection_id) ON DELETE CASCADE
) WITHOUT ROWID;
