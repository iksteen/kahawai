-- HUB-31: season-view projection for absolute-numbered (anime) shows.
-- The native identity stays items.season/episode (absolute); the
-- TVDB-style projection is provider-derived metadata.
ALTER TABLE item_metadata ADD COLUMN proj_season INTEGER;
ALTER TABLE item_metadata ADD COLUMN proj_episode INTEGER;
-- Per-library presentation choice: 'seasons' (projected, default) or
-- 'native' (flat absolute order).
ALTER TABLE libraries ADD COLUMN anime_view TEXT NOT NULL DEFAULT 'seasons';
