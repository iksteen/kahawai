ALTER TABLE catalog_collections
    ADD COLUMN completed_generation INTEGER NOT NULL DEFAULT 0;
