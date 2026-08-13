-- Collection ownership is item identity. Libraries compose collections and do
-- not own, clone, merge, or synchronize items. Exact roots belong to physical
-- files once; dependent records refer to the stable file id.

-- The historical membership cache inferred identity through library
-- composition. Drop its maintenance before splitting identities.
DROP TRIGGER item_sources_libraries_ins;
DROP TRIGGER item_sources_libraries_del;
DROP TRIGGER item_sources_libraries_upd;
DROP TRIGGER library_collections_libraries_ins;
DROP TRIGGER library_collections_libraries_del;
DROP TRIGGER library_collections_libraries_upd;
DROP TRIGGER items_libraries_parent;
DROP TRIGGER item_libraries_sort_keys;

CREATE TEMP TABLE item_collection_migration (
    old_item      TEXT NOT NULL,
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    item_id       TEXT NOT NULL,
    PRIMARY KEY (old_item, module_id, collection_id),
    UNIQUE (item_id)
) WITHOUT ROWID;

-- Directly sourced items and their parents have the same collection evidence.
-- The deterministic first collection retains the old id. This preserves every
-- id where the old row already belonged to one collection and records every
-- formerly cross-collection split explicitly for the rest of this migration.
INSERT INTO item_collection_migration
       (old_item, module_id, collection_id, item_id)
SELECT old_item, module_id, collection_id,
       CASE WHEN row_number() OVER (
                       PARTITION BY old_item ORDER BY module_id, collection_id) = 1
            THEN old_item
            ELSE old_item || ':collection:' || module_id || ':' || collection_id END
  FROM (
    SELECT s.item_id AS old_item, s.module_id, s.collection_id
      FROM item_sources s
    UNION
    SELECT i.parent_id, s.module_id, s.collection_id
      FROM item_sources s JOIN items i ON i.id = s.item_id
     WHERE i.parent_id IS NOT NULL
  );

ALTER TABLE items ADD COLUMN module_id TEXT;
ALTER TABLE items ADD COLUMN collection_id TEXT;

UPDATE items
   SET (module_id, collection_id) = (
       SELECT m.module_id, m.collection_id
         FROM item_collection_migration m
        WHERE m.old_item = items.id AND m.item_id = m.old_item)
 WHERE EXISTS (
       SELECT 1 FROM item_collection_migration m
        WHERE m.old_item = items.id AND m.item_id = m.old_item);

-- Clone top-level identities first, then children against the parent clone in
-- the same collection.
INSERT INTO items
  (id, kind, title, norm_title, year, parent_id, season, episode, artist,
   sort_title, norm_artist, episode_end, module_id, collection_id)
SELECT m.item_id, i.kind, i.title, i.norm_title, i.year, NULL,
       i.season, i.episode, i.artist, i.sort_title, i.norm_artist, i.episode_end,
       m.module_id, m.collection_id
  FROM item_collection_migration m JOIN items i ON i.id = m.old_item
 WHERE m.item_id <> m.old_item AND i.parent_id IS NULL;

INSERT INTO items
  (id, kind, title, norm_title, year, parent_id, season, episode, artist,
   sort_title, norm_artist, episode_end, module_id, collection_id)
SELECT m.item_id, i.kind, i.title, i.norm_title, i.year, pm.item_id,
       i.season, i.episode, i.artist, i.sort_title, i.norm_artist, i.episode_end,
       m.module_id, m.collection_id
  FROM item_collection_migration m
  JOIN items i ON i.id = m.old_item
  JOIN item_collection_migration pm
    ON pm.old_item = i.parent_id
   AND pm.module_id = m.module_id
   AND pm.collection_id = m.collection_id
 WHERE m.item_id <> m.old_item AND i.parent_id IS NOT NULL;

UPDATE items
   SET parent_id = (
       SELECT pm.item_id
         FROM item_collection_migration own
         JOIN item_collection_migration pm
           ON pm.old_item = items.parent_id
          AND pm.module_id = own.module_id
          AND pm.collection_id = own.collection_id
        WHERE own.old_item = items.id AND own.item_id = own.old_item)
 WHERE parent_id IS NOT NULL
   AND EXISTS (
       SELECT 1 FROM item_collection_migration own
        WHERE own.old_item = items.id AND own.item_id = own.old_item);

-- Durable provider/user inputs follow each collection identity. Derived
-- item_match rows are rebuilt by the hub from these inputs after migrations.
INSERT INTO provider_metadata
  (item_id, provider, provider_id, title, overview, poster_path, rating,
   premiered, original_language, genres, confidence, updated_at,
   proj_season, proj_episode, cast_json)
SELECT m.item_id, p.provider, p.provider_id, p.title, p.overview, p.poster_path,
       p.rating, p.premiered, p.original_language, p.genres, p.confidence,
       p.updated_at, p.proj_season, p.proj_episode, p.cast_json
  FROM item_collection_migration m JOIN provider_metadata p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO provider_queries
  (item_id, provider, query_type, query, rev, asked_at)
SELECT m.item_id, p.provider, p.query_type, p.query, p.rev, p.asked_at
  FROM item_collection_migration m JOIN provider_queries p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
SELECT m.item_id, p.provider, p.provider_id, p.rejected_at
  FROM item_collection_migration m JOIN rejected_matches p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
SELECT m.item_id, p.provider, p.provider_id, p.pinned_at
  FROM item_collection_migration m JOIN manual_match p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO anime_ids (item_id, anidb_id, anilist_id, mapped_tvdb, mapped_tmdb)
SELECT m.item_id, p.anidb_id, p.anilist_id, p.mapped_tvdb, p.mapped_tmdb
  FROM item_collection_migration m JOIN anime_ids p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO enrichment_queue (item_id, provider, due_at, attempts, reason)
SELECT m.item_id, p.provider, p.due_at, p.attempts, p.reason
  FROM item_collection_migration m JOIN enrichment_queue p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO item_relations (from_item, kind, target_anilist, target_title)
SELECT m.item_id, p.kind, p.target_anilist, p.target_title
  FROM item_collection_migration m JOIN item_relations p ON p.from_item=m.old_item
 WHERE m.item_id <> m.old_item;

INSERT INTO watch_state
  (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at)
SELECT p.user_id, m.item_id, p.position_ms, p.duration_ms,
       p.played, p.play_count, p.updated_at
  FROM item_collection_migration m JOIN watch_state p ON p.item_id=m.old_item
 WHERE m.item_id <> m.old_item;

-- Move source-bound tracks to the item in their own collection. Derived tracks
-- follow their physical parent. Item-level downloaded rows are cloned below.
UPDATE subtitle_tracks
   SET item_id = (
       SELECT m.item_id FROM item_collection_migration m
        WHERE m.old_item = subtitle_tracks.item_id
          AND m.module_id = subtitle_tracks.module_id
          AND m.collection_id = subtitle_tracks.collection_id)
 WHERE module_id IS NOT NULL;

UPDATE subtitle_tracks
   SET item_id = (SELECT p.item_id FROM subtitle_tracks p
                   WHERE p.id = subtitle_tracks.derived_from)
 WHERE derived_from IS NOT NULL;

-- Cache files for hub-stored tracks are named by the original row id. A clone
-- therefore stores that immutable payload id rather than copying cache bytes.
ALTER TABLE subtitle_tracks ADD COLUMN payload_id INTEGER;
UPDATE subtitle_tracks SET payload_id = id WHERE module_id IS NULL;

INSERT INTO subtitle_tracks
  (item_id, origin, stream_index, format, language, label, provider, machine,
   created_by, created_at, derived_from, payload_id)
SELECT m.item_id, t.origin, t.stream_index, t.format, t.language, t.label,
       t.provider, t.machine, t.created_by, t.created_at, NULL, t.payload_id
  FROM item_collection_migration m JOIN subtitle_tracks t ON t.item_id=m.old_item
 WHERE m.item_id <> m.old_item AND t.module_id IS NULL AND t.derived_from IS NULL;

-- Source bindings now point at the collection-specific item.
UPDATE item_sources
   SET item_id = (
       SELECT m.item_id FROM item_collection_migration m
        WHERE m.old_item = item_sources.item_id
          AND m.module_id = item_sources.module_id
          AND m.collection_id = item_sources.collection_id);

-- The old membership cache is no longer identity. A covering collection index
-- on top-level items gives browse the same cheap range scan without triggers.
DROP TABLE item_libraries;
CREATE INDEX items_collection_identity
    ON items (module_id, collection_id, kind, norm_title, year);
CREATE INDEX items_collection_browse
    ON items (module_id, collection_id, parent_id, sort_title, year, id);

CREATE TRIGGER items_collection_required_ins
BEFORE INSERT ON items
WHEN NEW.module_id IS NULL OR NEW.collection_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'item has no collection');
END;
CREATE TRIGGER items_collection_required_upd
BEFORE UPDATE OF module_id, collection_id ON items
WHEN NEW.module_id IS NULL OR NEW.collection_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'item has no collection');
END;
CREATE TRIGGER items_collection_exists_ins
BEFORE INSERT ON items
WHEN NOT EXISTS(SELECT 1 FROM collections c
                 WHERE (c.module_id,c.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT, 'item names unknown collection');
END;
CREATE TRIGGER items_collection_exists_upd
BEFORE UPDATE OF module_id,collection_id ON items
WHEN NOT EXISTS(SELECT 1 FROM collections c
                 WHERE (c.module_id,c.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT, 'item names unknown collection');
END;
CREATE TRIGGER items_parent_same_collection_ins
BEFORE INSERT ON items
WHEN NEW.parent_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM items p WHERE p.id=NEW.parent_id
      AND (p.module_id,p.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT, 'item parent belongs to another collection');
END;
CREATE TRIGGER items_parent_same_collection_upd
BEFORE UPDATE OF parent_id, module_id, collection_id ON items
WHEN NEW.parent_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM items p WHERE p.id=NEW.parent_id
      AND (p.module_id,p.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT, 'item parent belongs to another collection');
END;
-- Fail the migration rather than leave an old source-less identity outside the
-- final model. One linear assertion is materially cheaper than firing ownership
-- triggers for all 40k rows (measured 251 s versus about 1 s).
CREATE TEMP TABLE collection_scope_assert (
    invalid INTEGER NOT NULL CHECK(invalid=0)
);
INSERT INTO collection_scope_assert
SELECT COUNT(*) FROM items i
 WHERE i.module_id IS NULL OR i.collection_id IS NULL
    OR NOT EXISTS(SELECT 1 FROM collections c
                   WHERE (c.module_id,c.collection_id)=(i.module_id,i.collection_id))
    OR (i.parent_id IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM items p WHERE p.id=i.parent_id
          AND (p.module_id,p.collection_id)=(i.module_id,i.collection_id)));
DROP TABLE collection_scope_assert;

-- One normalized root binding, shared by every physical source in it.
CREATE TABLE collection_roots (
    id              INTEGER PRIMARY KEY,
    module_id       TEXT NOT NULL,
    collection_id   TEXT NOT NULL,
    root_token      TEXT NOT NULL,
    normalized_path TEXT NOT NULL,
    configured      INTEGER NOT NULL DEFAULT 1 CHECK(configured IN (0,1)),
    FOREIGN KEY (module_id, collection_id)
      REFERENCES collections(module_id, collection_id) ON DELETE CASCADE,
    UNIQUE (module_id, collection_id, root_token),
    UNIQUE (module_id, collection_id, normalized_path)
);
CREATE INDEX collection_roots_token ON collection_roots(root_token);
CREATE TRIGGER collection_roots_token_ins BEFORE INSERT ON collection_roots
WHEN EXISTS (SELECT 1 FROM collection_roots r
              WHERE r.root_token=NEW.root_token
                AND r.normalized_path<>NEW.normalized_path)
BEGIN
    SELECT RAISE(ABORT, 'root token maps to different normalized path');
END;
CREATE TRIGGER collection_roots_token_upd
BEFORE UPDATE OF root_token,normalized_path ON collection_roots
WHEN EXISTS (SELECT 1 FROM collection_roots r
              WHERE r.id<>OLD.id AND r.root_token=NEW.root_token
                AND r.normalized_path<>NEW.normalized_path)
BEGIN
    SELECT RAISE(ABORT, 'root token maps to different normalized path');
END;
ALTER TABLE collections ADD COLUMN root_adoption_pending INTEGER NOT NULL DEFAULT 0
    CHECK (root_adoption_pending IN (0,1));

-- Stable physical source ids replace every repeated compound path relation.
CREATE TABLE files_v53 (
    id            INTEGER PRIMARY KEY,
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    root_id       INTEGER REFERENCES collection_roots(id),
    path_rel      TEXT NOT NULL,
    item_id       TEXT REFERENCES items(id) ON DELETE SET NULL,
    part          INTEGER,
    size          INTEGER NOT NULL,
    mtime_unix    INTEGER NOT NULL,
    head_xxh3     INTEGER NOT NULL,
    tail_xxh3     INTEGER NOT NULL,
    oshash        INTEGER NOT NULL,
    streams_json  TEXT NOT NULL,
    ed2k          TEXT,
    subs_extracted INTEGER NOT NULL DEFAULT 0,
    revision      INTEGER,
    FOREIGN KEY (module_id, collection_id)
      REFERENCES collections(module_id, collection_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX files_v53_exact
    ON files_v53(module_id, collection_id, root_id, path_rel)
    WHERE root_id IS NOT NULL;
CREATE UNIQUE INDEX files_v53_legacy
    ON files_v53(module_id, collection_id, path_rel)
    WHERE root_id IS NULL;
INSERT INTO files_v53
  (module_id, collection_id, path_rel, item_id, part, size, mtime_unix,
   head_xxh3, tail_xxh3, oshash, streams_json, ed2k, subs_extracted, revision)
SELECT f.module_id, f.collection_id, f.path_rel, s.item_id, s.part, f.size,
       f.mtime_unix, f.head_xxh3, f.tail_xxh3, f.oshash, f.streams_json,
       f.ed2k, f.subs_extracted, f.revision
  FROM files f LEFT JOIN item_sources s
    ON (s.module_id,s.collection_id,s.path_rel)
     = (f.module_id,f.collection_id,f.path_rel)
 ORDER BY f.module_id,f.collection_id,f.path_rel;
CREATE INDEX files_v53_item ON files_v53(item_id);
-- Migration-only full source lookup. The permanent exact/legacy partial indexes
-- are ideal for runtime predicates but SQLite did not choose either for the
-- subtitle LEFT JOIN (measured ~236 s). This O(files) map makes both dependent
-- backfills indexed and is dropped before commit.
CREATE TEMP TABLE source_id_migration (
    module_id TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    path_rel TEXT NOT NULL,
    source_id INTEGER NOT NULL,
    item_id TEXT,
    PRIMARY KEY(module_id,collection_id,path_rel)
) WITHOUT ROWID;
INSERT INTO source_id_migration
SELECT module_id,collection_id,path_rel,id,item_id FROM files_v53;

CREATE TABLE subtitle_tracks_v53 (
    id            INTEGER PRIMARY KEY,
    item_id       TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    source_id     INTEGER REFERENCES files_v53(id) ON DELETE CASCADE,
    origin        TEXT NOT NULL,
    stream_index  INTEGER,
    format        TEXT NOT NULL,
    language      TEXT,
    label         TEXT,
    provider      TEXT,
    machine       INTEGER NOT NULL DEFAULT 0,
    created_by    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    derived_from  INTEGER REFERENCES subtitle_tracks_v53(id) ON DELETE SET NULL,
    payload_id    INTEGER
);
INSERT INTO subtitle_tracks_v53
  (id,item_id,source_id,origin,stream_index,format,language,label,provider,
   machine,created_by,created_at,derived_from,payload_id)
SELECT t.id,COALESCE(f.item_id,t.item_id),f.source_id,t.origin,t.stream_index,t.format,t.language,t.label,
       t.provider,t.machine,t.created_by,t.created_at,NULL,t.payload_id
  FROM subtitle_tracks t LEFT JOIN source_id_migration f
    ON (f.module_id,f.collection_id,f.path_rel)
     = (t.module_id,t.collection_id,t.path_rel);
-- A self-FK check per row made the bulk insert cost ~42 s. All rows now exist,
-- so restoring only the 2.8k actual lineage links is both exact and ~0.08 s.
UPDATE subtitle_tracks_v53
   SET derived_from=(SELECT t.derived_from FROM subtitle_tracks t
                      WHERE t.id=subtitle_tracks_v53.id)
 WHERE id IN(SELECT id FROM subtitle_tracks WHERE derived_from IS NOT NULL);
CREATE INDEX subtitle_tracks_v53_item ON subtitle_tracks_v53(item_id);
CREATE INDEX subtitle_tracks_v53_source ON subtitle_tracks_v53(source_id);
CREATE UNIQUE INDEX subtitle_tracks_v53_stream
    ON subtitle_tracks_v53(source_id,origin,stream_index)
    WHERE origin IN ('embedded','sidecar');

CREATE TABLE image_set_failures_v53 (
    source_id  INTEGER NOT NULL REFERENCES files_v53(id) ON DELETE CASCADE,
    sub_index  INTEGER NOT NULL,
    mtime_unix INTEGER,
    error      TEXT NOT NULL,
    at         INTEGER NOT NULL,
    PRIMARY KEY(source_id,sub_index)
) WITHOUT ROWID;
INSERT INTO image_set_failures_v53
SELECT f.source_id,x.sub_index,x.mtime_unix,x.error,x.at
  FROM image_set_failures x JOIN source_id_migration f
    ON (f.module_id,f.collection_id,f.path_rel)
     = (x.module_id,x.collection_id,x.path_rel);

-- Dynamic assignment triggers are recreated by db::open against the final
-- schema. Remove every old body before replacing the source table: SQLite
-- recompiles all dependent triggers during ALTER TABLE RENAME.
DROP TRIGGER IF EXISTS repick_answer_del;
DROP TRIGGER IF EXISTS repick_answer_ins;
DROP TRIGGER IF EXISTS repick_answer_upd;
DROP TRIGGER IF EXISTS repick_collection_upd;
DROP TRIGGER IF EXISTS repick_pin_del;
DROP TRIGGER IF EXISTS repick_pin_ins;
DROP TRIGGER IF EXISTS repick_pin_upd;
DROP TRIGGER IF EXISTS repick_rank_del;
DROP TRIGGER IF EXISTS repick_rank_ins;
DROP TRIGGER IF EXISTS repick_rank_upd;
DROP TRIGGER IF EXISTS repick_reject_del;
DROP TRIGGER IF EXISTS repick_reject_ins;
DROP TRIGGER IF EXISTS repick_source_del;
DROP TRIGGER IF EXISTS repick_source_ins;
DROP TRIGGER IF EXISTS repick_source_upd;
DROP TABLE image_set_failures;
-- DROP TABLE applies the old self-FK action once per row. Its child lookup had
-- no index and measured ~184 s at 53k tracks; this migration-only index makes
-- those lookups bounded. The copied table already holds the lineage.
CREATE INDEX subtitle_tracks_drop_derived ON subtitle_tracks(derived_from);
DROP TABLE subtitle_tracks;
DROP TABLE item_sources;
DROP TABLE files;
ALTER TABLE files_v53 RENAME TO files;
ALTER TABLE subtitle_tracks_v53 RENAME TO subtitle_tracks;
ALTER TABLE image_set_failures_v53 RENAME TO image_set_failures;

CREATE TRIGGER files_collection_refs_ins BEFORE INSERT ON files
WHEN (NEW.root_id IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM collection_roots r WHERE r.id=NEW.root_id
          AND (r.module_id,r.collection_id)=(NEW.module_id,NEW.collection_id)))
  OR (NEW.item_id IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM items i WHERE i.id=NEW.item_id
          AND (i.module_id,i.collection_id)=(NEW.module_id,NEW.collection_id)))
BEGIN
    SELECT RAISE(ABORT,'file references another collection');
END;
CREATE TRIGGER files_collection_refs_upd
BEFORE UPDATE OF module_id,collection_id,root_id,item_id ON files
WHEN (NEW.root_id IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM collection_roots r WHERE r.id=NEW.root_id
          AND (r.module_id,r.collection_id)=(NEW.module_id,NEW.collection_id)))
  OR (NEW.item_id IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM items i WHERE i.id=NEW.item_id
          AND (i.module_id,i.collection_id)=(NEW.module_id,NEW.collection_id)))
BEGIN
    SELECT RAISE(ABORT,'file references another collection');
END;
CREATE TRIGGER files_source_tracks_bound
BEFORE UPDATE OF item_id ON files
WHEN NEW.item_id IS NULL AND EXISTS(SELECT 1 FROM subtitle_tracks t WHERE t.source_id=OLD.id)
BEGIN
    SELECT RAISE(ABORT,'cannot unbind a source that owns subtitle tracks');
END;
CREATE TRIGGER files_source_tracks_rebind
AFTER UPDATE OF item_id ON files
WHEN NEW.item_id IS NOT NULL AND NEW.item_id IS NOT OLD.item_id
BEGIN
    UPDATE subtitle_tracks SET item_id=NEW.item_id WHERE source_id=NEW.id;
END;
CREATE TRIGGER subtitle_source_item_ins BEFORE INSERT ON subtitle_tracks
WHEN NEW.source_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM files f WHERE f.id=NEW.source_id AND f.item_id=NEW.item_id)
BEGIN
    SELECT RAISE(ABORT,'subtitle source belongs to another item');
END;
CREATE TRIGGER subtitle_source_item_upd
BEFORE UPDATE OF source_id,item_id ON subtitle_tracks
WHEN NEW.source_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM files f WHERE f.id=NEW.source_id AND f.item_id=NEW.item_id)
BEGIN
    SELECT RAISE(ABORT,'subtitle source belongs to another item');
END;

DROP TABLE source_id_migration;
DROP TABLE item_collection_migration;
ANALYZE;
