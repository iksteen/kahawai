-- Fill the new model from what is already stored. No provider is contacted:
-- every answer needed is on disk, which is the whole reason this redesign is
-- affordable.

-- `confidence` becomes a pure match STRENGTH. 'manual' was never a strength —
-- it is who decided — and it moves to item_match.manual. Per the owner's
-- request, existing manual assignments are wiped so the automatic behaviour
-- can be felt: the answers survive, the pins do not.
UPDATE provider_metadata SET confidence = 'auto' WHERE confidence = 'manual';

-- Bridge ids out of the metadata row.
INSERT INTO anime_ids (item_id, anidb_id, anilist_id, mapped_tvdb, mapped_tmdb)
SELECT item_id, anidb_id, anilist_id, mapped_tvdb, mapped_tmdb
FROM merged_metadata
WHERE anidb_id IS NOT NULL OR anilist_id IS NOT NULL
   OR mapped_tvdb IS NOT NULL OR mapped_tmdb IS NOT NULL;

-- The projection onto the episode's provider answers. An episode's rows all
-- describe the same episode, so they all carry the same projection.
UPDATE provider_metadata SET
    proj_season  = (SELECT m.proj_season  FROM merged_metadata m
                     WHERE m.item_id = provider_metadata.item_id),
    proj_episode = (SELECT m.proj_episode FROM merged_metadata m
                     WHERE m.item_id = provider_metadata.item_id)
WHERE EXISTS (SELECT 1 FROM merged_metadata m
               WHERE m.item_id = provider_metadata.item_id
                 AND m.proj_episode IS NOT NULL);

-- A rejected item's refused records. Today's reject deletes the provider rows
-- outright, so this usually finds nothing — it exists so a database that was
-- rejected-then-re-enriched does not silently re-assign what a human refused.
INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
SELECT pm.item_id, pm.provider, pm.provider_id, unixepoch()
FROM provider_metadata pm
JOIN merged_metadata m ON m.item_id = pm.item_id
WHERE m.confidence = 'rejected' AND pm.provider_id <> '';

-- The assignments themselves, by the rule: strength first (a strong match
-- beats a weak one whatever the order), then the media type's preference
-- order, then the provider name so the result is deterministic. Anything
-- refused is not a candidate. manual = 0 throughout.
INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
SELECT item_id, provider, provider_id, media_type, 0, unixepoch() FROM (
    SELECT t.item_id, t.media_type, pm.provider, pm.provider_id,
           ROW_NUMBER() OVER (PARTITION BY t.item_id ORDER BY
               CASE pm.confidence WHEN 'auto' THEN 0 WHEN 'weak' THEN 1 ELSE 2 END,
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
    ) t
    JOIN provider_metadata pm ON pm.item_id = t.item_id
    LEFT JOIN provider_ranks r
           ON r.media_type = t.media_type
          AND r.provider = CASE pm.provider WHEN 'anilist' THEN 'anime' ELSE pm.provider END
    WHERE pm.confidence IN ('auto', 'weak') AND pm.provider_id <> ''
      AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                       WHERE rj.item_id = pm.item_id
                         AND rj.provider = pm.provider
                         AND rj.provider_id = pm.provider_id)
) WHERE n = 1;
