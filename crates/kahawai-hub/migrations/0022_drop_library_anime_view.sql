-- HUB-31 presentation is a user preference (user_prefs key anime_view,
-- default seasons); the per-library default was needless bookkeeping.
ALTER TABLE libraries DROP COLUMN anime_view;
