-- Explicit playable renditions. Physical `files` remain stable source facts;
-- these rows say which logical item plays them and in what ordered set.
-- Runtime fills family_key with the filename-derived release family after this
-- direct migration; migration never reparses paths or infers new identity.
CREATE UNIQUE INDEX items_collection_ref
    ON items(id,module_id,collection_id);
CREATE UNIQUE INDEX files_collection_ref
    ON files(id,module_id,collection_id);

CREATE TABLE playable_sources (
    id              INTEGER PRIMARY KEY,
    module_id       TEXT NOT NULL,
    collection_id   TEXT NOT NULL,
    item_id         TEXT NOT NULL,
    root_id         INTEGER,
    family_key      TEXT NOT NULL,
    expected_parts  INTEGER NOT NULL CHECK(expected_parts > 0),
    FOREIGN KEY(item_id,module_id,collection_id)
      REFERENCES items(id,module_id,collection_id) ON DELETE CASCADE,
    FOREIGN KEY(root_id) REFERENCES collection_roots(id) ON DELETE CASCADE,
    UNIQUE(module_id,collection_id,root_id,family_key)
);
CREATE UNIQUE INDEX playable_sources_legacy_family
    ON playable_sources(module_id,collection_id,family_key)
    WHERE root_id IS NULL;
CREATE INDEX playable_sources_item ON playable_sources(item_id);

CREATE TABLE playable_source_parts (
    playable_source_id INTEGER NOT NULL,
    module_id           TEXT NOT NULL,
    collection_id       TEXT NOT NULL,
    ordinal             INTEGER NOT NULL CHECK(ordinal > 0),
    file_id             INTEGER NOT NULL UNIQUE,
    PRIMARY KEY(playable_source_id,file_id),
    FOREIGN KEY(playable_source_id) REFERENCES playable_sources(id) ON DELETE CASCADE,
    FOREIGN KEY(file_id,module_id,collection_id)
      REFERENCES files(id,module_id,collection_id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX playable_source_parts_file ON playable_source_parts(file_id);
CREATE INDEX playable_source_parts_ordinal
    ON playable_source_parts(playable_source_id,ordinal);

-- Keep presentation ownership aligned while legacy callers are moved from
-- files.item_id to the explicit table. This is bounded to the one source row.
CREATE TRIGGER files_playable_rebind
AFTER UPDATE OF item_id ON files
WHEN NEW.item_id IS NOT OLD.item_id
BEGIN
    UPDATE playable_sources SET item_id=NEW.item_id
     WHERE id IN(SELECT playable_source_id FROM playable_source_parts
                  WHERE file_id=NEW.id)
       AND NEW.item_id IS NOT NULL;
    DELETE FROM playable_source_parts WHERE file_id=NEW.id AND NEW.item_id IS NULL;
    DELETE FROM playable_sources
     WHERE NOT EXISTS(SELECT 1 FROM playable_source_parts p
                       WHERE p.playable_source_id=playable_sources.id);
END;

-- Preserve exactly the deployed source binding. Part families receive a
-- temporary per-item key, ordinary files a per-file key; startup replaces only
-- the temporary multipart keys with the deterministic release family.
INSERT INTO playable_sources
  (module_id,collection_id,item_id,root_id,family_key,expected_parts)
SELECT module_id,collection_id,item_id,root_id,'file:'||id,1
  FROM files WHERE item_id IS NOT NULL AND part IS NULL
UNION ALL
SELECT module_id,collection_id,item_id,root_id,
       'legacy-item:'||item_id,max(part)
  FROM files WHERE item_id IS NOT NULL AND part IS NOT NULL
 GROUP BY module_id,collection_id,item_id,root_id;

INSERT INTO playable_source_parts
  (playable_source_id,module_id,collection_id,ordinal,file_id)
SELECT ps.id,f.module_id,f.collection_id,COALESCE(f.part,1),f.id
  FROM files f JOIN playable_sources ps
    ON ps.module_id=f.module_id AND ps.collection_id=f.collection_id
   AND ps.root_id IS f.root_id AND ps.item_id=f.item_id
   AND ps.family_key=CASE WHEN f.part IS NULL THEN 'file:'||f.id
                          ELSE 'legacy-item:'||f.item_id END
 WHERE f.item_id IS NOT NULL;

ANALYZE;
