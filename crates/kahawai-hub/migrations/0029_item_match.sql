-- HUB-5, take three: one row says what an item IS, and everything
-- descriptive is derived at read time from the providers' own answers.
--
-- The materialised merge this replaces was where the bugs lived: identity
-- flipping to a weak match, a decline erasing a human's correction, a weak
-- stranger donating fields, two manual rows tying on insertion order. Each
-- fix added a rule to the merge. An assignment plus read-time side-fill has
-- no merge to get wrong, and re-deciding costs nothing because every
-- provider's answer is already on disk.

-- What this item IS. Top-level only: movies, shows, albums — never
-- episodes or tracks, which follow their parent's assignment.
CREATE TABLE item_match (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    -- Which chain this item resolves against. Denormalised on purpose:
    -- deriving it per read (items -> item_sources -> collections) cost 4x
    -- on the browse list, measured 314 ms against 80 ms for 3,372 items.
    -- The write path knows it when it assigns; reads must never compute it.
    media_type  TEXT NOT NULL,
    -- A human decided this one. Automatic re-picking never touches it.
    manual      INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);

-- Records a human refused. "There is currently no correct record" is the
-- state where every candidate an item has is in here; a record that is NOT
-- in here may be assigned automatically, which is how the item recovers by
-- itself the moment a provider offers something new.
CREATE TABLE rejected_matches (
    item_id     TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    rejected_at INTEGER NOT NULL,
    PRIMARY KEY (item_id, provider, provider_id)
);

-- Bridge identity, not description: these say which record in another
-- service is the same work, and they never participate in side-fill.
CREATE TABLE anime_ids (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    anidb_id    INTEGER,
    anilist_id  INTEGER,
    mapped_tvdb INTEGER,
    mapped_tmdb INTEGER
);

-- The relations view resolves an AniList id back to a local item; that is a
-- full table scan today.
CREATE INDEX anime_ids_anilist ON anime_ids (anilist_id);

-- HUB-31's projection of absolute numbering onto a seasoned view belongs to
-- the provider that curated it, not to the item.
ALTER TABLE provider_metadata ADD COLUMN proj_season INTEGER;
ALTER TABLE provider_metadata ADD COLUMN proj_episode INTEGER;
