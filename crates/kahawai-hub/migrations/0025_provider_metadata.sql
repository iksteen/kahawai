-- HUB-5: what each provider said is the stored truth; the merged row is
-- derived from it. Precedence is per FIELD and per MEDIA TYPE, so the
-- provider that identifies an item need not be the one that describes
-- it best, and reordering providers re-decides ownership from local
-- rows — no provider is asked again.
CREATE TABLE provider_metadata (
    item_id           TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider          TEXT NOT NULL,   -- 'tmdb' | 'tvdb' | 'anilist' | 'musicbrainz'
    -- This provider's own record id, whether it identified the item or
    -- only described it. EMPTY means it looked and found nothing —
    -- always paired with confidence = 'miss', and that pair is how
    -- never-ask-twice is remembered.
    provider_id       TEXT NOT NULL,
    title             TEXT,
    overview          TEXT,
    poster_path       TEXT,
    rating            REAL,
    premiered         TEXT,
    original_language TEXT,
    genres            TEXT,            -- JSON array
    confidence        TEXT NOT NULL,   -- auto | weak | manual | miss
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (item_id, provider)
);

CREATE INDEX provider_metadata_item ON provider_metadata (item_id);

-- Provider precedence as data, one row per chain position. Editable at
-- runtime; a reorder re-merges and costs nothing but local work.
CREATE TABLE provider_ranks (
    media_type TEXT NOT NULL,
    provider   TEXT NOT NULL,
    rank       INTEGER NOT NULL,
    PRIMARY KEY (media_type, provider)
);

INSERT INTO provider_ranks (media_type, provider, rank) VALUES
    ('anime',  'anime',       0),
    ('anime',  'tmdb',        1),
    ('anime',  'tvdb',        2),
    ('movies', 'tmdb',        0),
    ('movies', 'tvdb',        1),
    ('music',  'musicbrainz', 0);

-- Work the chain still owes: an item/provider pair that has not been
-- asked yet, or could not be asked because the provider is banned or
-- backed off. `due_at` is when it may be tried; the enrichment run
-- drains what is due. Nothing is ever silently dropped — a provider we
-- cannot reach now becomes a row here, not a permanent hole.
CREATE TABLE enrichment_queue (
    item_id  TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    due_at   INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    reason   TEXT,
    PRIMARY KEY (item_id, provider)
);

CREATE INDEX enrichment_queue_due ON enrichment_queue (due_at);

-- What is already stored records a real fact — which provider supplied
-- which value — so it moves across as that provider's answer. Discarding
-- it would mean re-asking every provider for every item, and re-asking
-- AniDB 12k times is how bans happen (§4.3).
INSERT INTO provider_metadata
    (item_id, provider, provider_id, title, overview, poster_path, rating,
     premiered, original_language, genres, confidence, updated_at)
SELECT item_id, provider, provider_id, title, overview, poster_path, rating,
       premiered, original_language, genres, confidence, updated_at
FROM item_metadata;

-- Anything genuinely unknown is QUEUED, not invented: every item with a
-- missing field gets the providers that have never answered for it,
-- due immediately. The run drains that queue at the pace §4.3 allows,
-- and a provider that refuses is rescheduled rather than dropped.
INSERT INTO enrichment_queue (item_id, provider, due_at, reason)
SELECT m.item_id, r.provider, unixepoch(), 'field-level backfill'
FROM item_metadata m
JOIN items i ON i.id = m.item_id
JOIN item_sources s ON s.item_id = i.id
                    OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
JOIN collections c ON (c.module_id, c.collection_id) = (s.module_id, s.collection_id)
JOIN provider_ranks r ON r.media_type = CASE
        WHEN c.media_type = 'anime' THEN 'anime'
        WHEN c.media_type = 'music' THEN 'music'
        ELSE 'movies' END
WHERE (m.title IS NULL OR m.overview IS NULL OR m.poster_path IS NULL
       OR m.premiered IS NULL OR m.rating IS NULL)
  AND NOT EXISTS (
      SELECT 1 FROM provider_metadata pm
      WHERE pm.item_id = m.item_id
        AND (pm.provider = r.provider
             OR (r.provider = 'anime' AND pm.provider = 'anilist')))
GROUP BY m.item_id, r.provider;

-- Finally, call the served row what it is. It is not "the item's
-- metadata" any more — it is the MERGE of the rows above, rebuilt by
-- rank whenever an answer lands or the order changes. The name is the
-- warning: writing to it directly does not persist, because the next
-- merge overwrites it. Providers write provider_metadata; everything
-- reads merged_metadata.
ALTER TABLE item_metadata RENAME TO merged_metadata;
