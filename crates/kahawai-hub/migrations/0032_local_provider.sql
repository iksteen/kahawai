-- HUB-9: the .nfo beside the media file is a provider, and by default the
-- first one in every chain — a human wrote it, so it outranks a search
-- result. Existing orders get it at the front; anything already ranked
-- shifts down one and keeps its relative order.
UPDATE provider_ranks SET rank = rank + 1;
INSERT INTO provider_ranks (media_type, provider, rank)
-- `WHERE true` is not decoration: without it SQLite cannot tell whether
-- the ON belongs to a join or to the conflict clause, and refuses to parse.
SELECT DISTINCT media_type, 'local', 0 FROM provider_ranks WHERE true
ON CONFLICT (media_type, provider) DO NOTHING;
