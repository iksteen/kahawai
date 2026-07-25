-- Original language of the matched title (ISO 639-1 from TMDB, AniList
-- country mapping for anime). '' = provider asked but had none;
-- NULL = not captured yet (backfill sweeps these).
ALTER TABLE item_metadata ADD COLUMN original_language TEXT;
