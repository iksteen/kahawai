-- Every media type gets its OWN provider order. 0025 seeded ranks for
-- movies, anime and music only, because the requirement lists movies and
-- series together — but that grouping is about their DEFAULT order being
-- identical, not about sharing one chain. Ranking TVDB above TMDB for
-- series while leaving films alone is a reasonable thing to want, and it
-- needs a row of its own to be expressible.
INSERT INTO provider_ranks (media_type, provider, rank) VALUES
    ('series', 'tmdb', 0),
    ('series', 'tvdb', 1)
ON CONFLICT (media_type, provider) DO NOTHING;
