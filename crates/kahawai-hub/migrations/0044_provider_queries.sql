-- HUB-5: never-ask-twice gates on the QUESTION, not the outcome.
--
-- A recorded miss used to be the thing that prevented re-asking, which
-- made it permanent even when the question it answered no longer
-- existed: a repaired title, a hash that arrived after the first walk,
-- a code fix to how the search string is derived. This table records
-- what was actually asked; when the current question differs from every
-- recorded one, the provider is due again — automatically, one paced
-- request per changed question, ever.
--
-- Semantics (authoritative doc: the providers.rs module doc):
--   query_type 'title'     — a title search; query is the canonical
--                            anchor (norm_title|year), not the ladder's
--                            individual variants.
--   query_type 'mapped_id' — a fetch by bridge id (anime-lists mapping).
--   rev                    — bumped in code (providers::QUERY_REV) when
--                            query DERIVATION changes, re-opening every
--                            recorded question once.
-- ED2K hash questions stay in ed2k_aid: content-keyed, shared across
-- items, which is the stronger dedup.
CREATE TABLE provider_queries (
    item_id    TEXT    NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider   TEXT    NOT NULL,
    query_type TEXT    NOT NULL,
    query      TEXT    NOT NULL,
    rev        INTEGER NOT NULL,
    asked_at   INTEGER NOT NULL,
    PRIMARY KEY (item_id, provider, query_type, query)
);
