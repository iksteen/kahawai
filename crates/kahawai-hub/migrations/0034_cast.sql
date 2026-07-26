-- HUB-6: cast, alongside the genres column that has been there since
-- 0025 but which only anilist/musicbrainz/local ever filled.
--
-- Stored as a JSON array of {name, character} in billing order, capped
-- at the leading players: TMDB returns 68 for a 1995 film and nothing
-- renders a cast of 68. It is a description, not an index — nothing
-- joins on it, so JSON in one column beats a table nobody queries.
ALTER TABLE provider_metadata ADD COLUMN cast_json TEXT;
