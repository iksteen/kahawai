-- A subtitle row has one authoritative owner. Physical streams and every
-- derivative of them follow the stable file id; independently acquired tracks
-- follow the collection item. Rebinding a file therefore needs no subtitle
-- rewrite, and deleting a source evicts its reproducible OCR/raster rows.

CREATE TABLE subtitle_tracks_v54 (
    id            INTEGER PRIMARY KEY,
    item_id       TEXT REFERENCES items(id) ON DELETE CASCADE,
    source_id     INTEGER REFERENCES files(id) ON DELETE CASCADE,
    origin        TEXT NOT NULL,
    stream_index  INTEGER,
    format        TEXT NOT NULL,
    language      TEXT,
    label         TEXT,
    provider      TEXT,
    machine       INTEGER NOT NULL DEFAULT 0,
    created_by    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    derived_from  INTEGER REFERENCES subtitle_tracks_v54(id) ON DELETE CASCADE,
    payload_id    INTEGER,
    CHECK((item_id IS NULL) <> (source_id IS NULL)),
    CHECK(origin NOT IN ('embedded','sidecar') OR source_id IS NOT NULL),
    CHECK(origin <> 'downloaded' OR item_id IS NOT NULL)
);

-- Follow lineage recursively rather than assuming derivatives are only one
-- level deep. Existing OCR/raster rows stored the item's id, but their parent
-- names the physical source that actually owns the generated payload.
WITH RECURSIVE owner(id,item_id,source_id) AS (
    SELECT id,CASE WHEN source_id IS NOT NULL THEN NULL ELSE item_id END,source_id
      FROM subtitle_tracks WHERE derived_from IS NULL
    UNION ALL
    SELECT child.id,
           CASE WHEN COALESCE(child.source_id,parent.source_id) IS NOT NULL
                THEN NULL ELSE COALESCE(child.item_id,parent.item_id) END,
           COALESCE(child.source_id,parent.source_id)
      FROM subtitle_tracks child JOIN owner parent ON parent.id=child.derived_from
)
INSERT INTO subtitle_tracks_v54
  (id,item_id,source_id,origin,stream_index,format,language,label,provider,
   machine,created_by,created_at,derived_from,payload_id)
SELECT t.id,o.item_id,o.source_id,t.origin,t.stream_index,t.format,t.language,
       t.label,t.provider,t.machine,t.created_by,t.created_at,NULL,t.payload_id
  FROM subtitle_tracks t JOIN owner o ON o.id=t.id;

-- Refuse a corrupt lineage cycle instead of silently omitting its rows.
CREATE TEMP TABLE subtitle_track_copy_assert (
    missing INTEGER NOT NULL CHECK(missing=0)
);
INSERT INTO subtitle_track_copy_assert
SELECT (SELECT count(*) FROM subtitle_tracks)
     - (SELECT count(*) FROM subtitle_tracks_v54);

-- All parents now exist, so restore lineage without a per-row self-FK lookup
-- during the bulk copy.
UPDATE subtitle_tracks_v54
   SET derived_from=(SELECT old.derived_from FROM subtitle_tracks old
                      WHERE old.id=subtitle_tracks_v54.id)
 WHERE id IN(SELECT id FROM subtitle_tracks WHERE derived_from IS NOT NULL);

CREATE INDEX subtitle_tracks_v54_item ON subtitle_tracks_v54(item_id);
CREATE INDEX subtitle_tracks_v54_source ON subtitle_tracks_v54(source_id);
CREATE INDEX subtitle_tracks_v54_derived ON subtitle_tracks_v54(derived_from);
CREATE UNIQUE INDEX subtitle_tracks_v54_stream
    ON subtitle_tracks_v54(source_id,origin,stream_index)
    WHERE origin IN ('embedded','sidecar');

-- These maintained the now-removed duplicate item_id on source rows.
DROP TRIGGER files_source_tracks_bound;
DROP TRIGGER files_source_tracks_rebind;
DROP TRIGGER subtitle_source_item_ins;
DROP TRIGGER subtitle_source_item_upd;

-- DROP TABLE applies the old self-FK action. Bound its child lookups on real
-- catalogues before replacing the table.
CREATE INDEX subtitle_tracks_drop_derived_v54 ON subtitle_tracks(derived_from);
DROP TABLE subtitle_tracks;
ALTER TABLE subtitle_tracks_v54 RENAME TO subtitle_tracks;

-- A derivative inherits exactly the same direct owner as its parent. `IS` is
-- intentional: it compares NULLs as equal.
CREATE TRIGGER subtitle_derived_owner_ins BEFORE INSERT ON subtitle_tracks
WHEN NEW.derived_from IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM subtitle_tracks p WHERE p.id=NEW.derived_from
      AND p.item_id IS NEW.item_id AND p.source_id IS NEW.source_id)
BEGIN
    SELECT RAISE(ABORT,'subtitle derivative has another owner');
END;
CREATE TRIGGER subtitle_derived_owner_upd
BEFORE UPDATE OF item_id,source_id,derived_from ON subtitle_tracks
WHEN NEW.derived_from IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM subtitle_tracks p WHERE p.id=NEW.derived_from
      AND p.item_id IS NEW.item_id AND p.source_id IS NEW.source_id)
BEGIN
    SELECT RAISE(ABORT,'subtitle derivative has another owner');
END;

DROP TABLE subtitle_track_copy_assert;
ANALYZE;
