ALTER TABLE collections ADD COLUMN root_adoption_pending INTEGER NOT NULL DEFAULT 0
    CHECK (root_adoption_pending IN (0, 1));

UPDATE collections SET root_adoption_pending = 1
WHERE exact_roots_json <> '[]';
