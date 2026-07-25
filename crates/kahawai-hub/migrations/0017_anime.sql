-- Anime identity (HUB-29): AniDB is the identity authority, AniList the
-- description source; TVDB/TMDB ids arrive via the community
-- anime-lists mapping and keep episode enrichment working.
ALTER TABLE item_metadata ADD COLUMN anidb_id INTEGER;
ALTER TABLE item_metadata ADD COLUMN anilist_id INTEGER;
ALTER TABLE item_metadata ADD COLUMN mapped_tvdb INTEGER;
ALTER TABLE item_metadata ADD COLUMN mapped_tmdb INTEGER;

-- Relations graph (SEQUEL/PREQUEL/…) from AniList; targets referenced
-- by AniList id so out-of-library entries still render by name.
CREATE TABLE item_relations (
    from_item      TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    target_anilist INTEGER NOT NULL,
    target_title   TEXT,
    PRIMARY KEY (from_item, kind, target_anilist)
);
