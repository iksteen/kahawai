-- TMDB reports vote_average = 0 for a title nobody has rated. Storing
-- that as a score put a literal "0" on 200 items — a rating of zero,
-- rather than no rating. Absent is absent.
UPDATE provider_metadata SET rating = NULL WHERE rating = 0;
UPDATE merged_metadata SET rating = NULL WHERE rating = 0;
