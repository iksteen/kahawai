ALTER TABLE catalog_jobs
    ADD COLUMN source_version INTEGER NOT NULL DEFAULT 0;
