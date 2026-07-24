-- Music (M4): albums and tracks live in items like shows and episodes
-- (season=disc, episode=track). The artist is real item data for both.
ALTER TABLE items ADD COLUMN artist TEXT;
