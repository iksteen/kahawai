-- Portability spike: kahawai's derived-state crown jewel (item_match,
-- trigger-maintained) translated to PostgreSQL. Schema subset + the
-- repick as a plpgsql function + input triggers, then the drift-test
-- scenario with hard ASSERTs. Applied to a scratch schema; DROP at will.
\set ON_ERROR_STOP on
DROP SCHEMA IF EXISTS spike CASCADE;
CREATE SCHEMA spike;
SET search_path TO spike;

CREATE TABLE items (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    title      TEXT NOT NULL,
    norm_title TEXT NOT NULL,
    parent_id  TEXT REFERENCES items (id) ON DELETE CASCADE,
    year       BIGINT,
    sort_title TEXT
);
CREATE TABLE collections (
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    media_type    TEXT NOT NULL,
    PRIMARY KEY (module_id, collection_id)
);
CREATE TABLE item_sources (
    item_id       TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    path_rel      TEXT NOT NULL,
    PRIMARY KEY (item_id, module_id, collection_id, path_rel)
);
CREATE TABLE provider_metadata (
    item_id     TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL DEFAULT '',
    title       TEXT,
    confidence  TEXT NOT NULL,
    updated_at  BIGINT NOT NULL DEFAULT extract(epoch from now())::bigint,
    PRIMARY KEY (item_id, provider)
);
CREATE TABLE provider_ranks (
    media_type TEXT NOT NULL,
    provider   TEXT NOT NULL,
    rank       BIGINT NOT NULL,
    PRIMARY KEY (media_type, provider)
);
CREATE TABLE manual_match (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    pinned_at   BIGINT NOT NULL DEFAULT extract(epoch from now())::bigint
);
CREATE TABLE rejected_matches (
    item_id     TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    rejected_at BIGINT NOT NULL DEFAULT extract(epoch from now())::bigint,
    PRIMARY KEY (item_id, provider, provider_id)
);
CREATE TABLE item_match (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    media_type  TEXT NOT NULL,
    manual      BIGINT NOT NULL DEFAULT 0,
    updated_at  BIGINT NOT NULL
);

-- The repick: DROP_STALE + PICK_ASSIGNMENT, translated. Differences
-- from the SQLite original, each mechanical:
--   unixepoch()               -> extract(epoch from now())::bigint
--   x IS NOT y (upsert guard) -> x IS DISTINCT FROM y
--   boolean-as-int (pinned)   -> (...)::int
-- Everything else — ROW_NUMBER window, subquery-in-FROM, boolean sort
-- keys (false < true matches 0 < 1), ON CONFLICT DO UPDATE ... WHERE —
-- is shared syntax.
CREATE FUNCTION repick(p_item TEXT, p_media TEXT) RETURNS void
LANGUAGE plpgsql AS $fn$
BEGIN
  DELETE FROM item_match
   WHERE (p_item IS NULL OR item_id = p_item)
     AND (p_media IS NULL OR media_type = p_media)
     AND (NOT EXISTS (
            SELECT 1 FROM provider_metadata pm
             WHERE pm.item_id = item_match.item_id
               AND pm.provider = item_match.provider
               AND pm.provider_id <> ''
               AND pm.confidence IN ('auto', 'weak'))
          OR EXISTS (
            SELECT 1 FROM rejected_matches rj
             WHERE rj.item_id = item_match.item_id
               AND rj.provider = item_match.provider
               AND rj.provider_id = item_match.provider_id));

  INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
  SELECT item_id, provider, provider_id, media_type, pinned,
         extract(epoch from now())::bigint
  FROM (
    SELECT t.item_id, t.media_type, pm.provider, pm.provider_id,
           (mm.item_id IS NOT NULL)::int AS pinned,
           ROW_NUMBER() OVER (PARTITION BY t.item_id ORDER BY
               mm.item_id IS NULL,
               CASE pm.confidence WHEN 'auto' THEN 0 WHEN 'weak' THEN 1 ELSE 2 END,
               pm.provider <> 'local',
               COALESCE(r.rank, 99),
               pm.provider) AS n
      FROM (
        SELECT i.id AS item_id,
               COALESCE((SELECT CASE WHEN c.media_type IN ('movies','series','anime','music')
                                     THEN c.media_type ELSE 'movies' END
                           FROM item_sources s
                           JOIN collections c ON (c.module_id, c.collection_id)
                                               = (s.module_id, s.collection_id)
                          WHERE s.item_id = i.id
                             OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
                          LIMIT 1), 'movies') AS media_type
          FROM items i
         WHERE i.kind IN ('movie', 'show', 'album')
           AND (p_item IS NULL OR i.id = p_item)
      ) t
      JOIN provider_metadata pm ON pm.item_id = t.item_id
      LEFT JOIN manual_match mm ON mm.item_id = pm.item_id
                               AND mm.provider = pm.provider
                               AND mm.provider_id = pm.provider_id
      LEFT JOIN provider_ranks r
             ON r.media_type = t.media_type
            AND r.provider = CASE pm.provider WHEN 'anilist' THEN 'anime' ELSE pm.provider END
     WHERE pm.confidence IN ('auto', 'weak') AND pm.provider_id <> ''
       AND (p_media IS NULL OR t.media_type = p_media)
       AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                        WHERE rj.item_id = pm.item_id
                          AND rj.provider = pm.provider
                          AND rj.provider_id = pm.provider_id)
  ) ranked WHERE n = 1
  ON CONFLICT (item_id) DO UPDATE SET
    provider = excluded.provider,
    provider_id = excluded.provider_id,
    media_type = excluded.media_type,
    manual = excluded.manual,
    updated_at = extract(epoch from now())::bigint
  WHERE item_match.provider    IS DISTINCT FROM excluded.provider
     OR item_match.provider_id IS DISTINCT FROM excluded.provider_id
     OR item_match.media_type  IS DISTINCT FROM excluded.media_type
     OR item_match.manual      IS DISTINCT FROM excluded.manual;
END $fn$;

-- Input triggers (the SQLite build generates 16; three representatives
-- prove the shape — the rest are the same pattern with other columns).
CREATE FUNCTION trg_repick_row() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
  PERFORM repick(COALESCE(NEW.item_id, OLD.item_id), NULL);
  RETURN NULL;
END $fn$;
CREATE TRIGGER pm_repick
AFTER INSERT OR DELETE OR UPDATE OF provider_id, confidence ON provider_metadata
FOR EACH ROW EXECUTE FUNCTION trg_repick_row();
CREATE TRIGGER mm_repick
AFTER INSERT OR DELETE OR UPDATE ON manual_match
FOR EACH ROW EXECUTE FUNCTION trg_repick_row();
CREATE TRIGGER rj_repick
AFTER INSERT OR DELETE ON rejected_matches
FOR EACH ROW EXECUTE FUNCTION trg_repick_row();

-- Cross-table chaining (the 0035 shape): item_match drives sort_title.
CREATE FUNCTION trg_sort_title() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
  UPDATE items SET sort_title = lower(COALESCE(
      (SELECT pm.title FROM provider_metadata pm
        JOIN item_match m ON m.item_id = pm.item_id AND m.provider = pm.provider
       WHERE pm.item_id = items.id AND pm.title IS NOT NULL), norm_title))
   WHERE id = COALESCE(NEW.item_id, OLD.item_id);
  RETURN NULL;
END $fn$;
CREATE TRIGGER im_sort_title
AFTER INSERT OR DELETE OR UPDATE ON item_match
FOR EACH ROW EXECUTE FUNCTION trg_sort_title();

-- ---- The drift scenario, asserted hard (any failure aborts psql). ----
INSERT INTO collections VALUES ('m', 'c', 'movies');
INSERT INTO items (id, kind, title, norm_title) VALUES ('i1', 'movie', 'Raw Name', 'raw name');
INSERT INTO item_sources VALUES ('i1', 'm', 'c', 'f.mkv');
INSERT INTO provider_ranks VALUES ('movies', 'tmdb', 0), ('movies', 'tvdb', 1);

-- 1. A weak tvdb answer arrives: it wins by default.
INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence)
VALUES ('i1', 'tvdb', '42', 'TVDB Title', 'weak');
DO $$ BEGIN
  ASSERT (SELECT provider FROM item_match WHERE item_id = 'i1') = 'tvdb', 'weak tvdb should win alone';
  ASSERT (SELECT sort_title FROM items WHERE id = 'i1') = 'tvdb title', 'sort_title must chain from the pick';
END $$;

-- 2. A strong tmdb answer displaces it (confidence beats rank position).
INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence)
VALUES ('i1', 'tmdb', '7', 'TMDB Title', 'auto');
DO $$ BEGIN
  ASSERT (SELECT provider FROM item_match WHERE item_id = 'i1') = 'tmdb', 'auto tmdb should displace weak tvdb';
  ASSERT (SELECT sort_title FROM items WHERE id = 'i1') = 'tmdb title', 'sort_title follows the flip';
END $$;

-- 3. A human pins the tvdb record: the pin is the first sort key.
INSERT INTO manual_match (item_id, provider, provider_id) VALUES ('i1', 'tvdb', '42');
DO $$ BEGIN
  ASSERT (SELECT provider FROM item_match WHERE item_id = 'i1') = 'tvdb', 'pin must beat auto';
  ASSERT (SELECT manual FROM item_match WHERE item_id = 'i1') = 1, 'winner IS the pin';
END $$;

-- 4. The pinned answer is deleted: automatic pick takes over, pin dormant.
DELETE FROM provider_metadata WHERE item_id = 'i1' AND provider = 'tvdb';
DO $$ BEGIN
  ASSERT (SELECT provider FROM item_match WHERE item_id = 'i1') = 'tmdb', 'orphaned pin yields to auto';
  ASSERT (SELECT manual FROM item_match WHERE item_id = 'i1') = 0, 'manual reads 0 without its row';
END $$;

-- 5. The human rejects the tmdb record: nothing is left -> NO ROW.
INSERT INTO rejected_matches (item_id, provider, provider_id) VALUES ('i1', 'tmdb', '7');
DO $$ BEGIN
  ASSERT NOT EXISTS (SELECT 1 FROM item_match WHERE item_id = 'i1'), 'all refused = absent, not a placeholder';
END $$;

-- 6. The no-op guard: re-inserting identical state must not churn updated_at.
DELETE FROM rejected_matches WHERE item_id = 'i1';
DO $$
DECLARE t0 BIGINT; t1 BIGINT;
BEGIN
  ASSERT (SELECT provider FROM item_match WHERE item_id = 'i1') = 'tmdb';
  t0 := (SELECT updated_at FROM item_match WHERE item_id = 'i1');
  PERFORM pg_sleep(1.1);
  UPDATE provider_metadata SET confidence = 'auto' WHERE item_id = 'i1'; -- same value
  t1 := (SELECT updated_at FROM item_match WHERE item_id = 'i1');
  ASSERT t0 = t1, 'a pick that re-decides the same thing must be a no-op';
END $$;

SELECT 'SPIKE PASSED: derived item_match + chained sort_title work on PostgreSQL' AS verdict;
