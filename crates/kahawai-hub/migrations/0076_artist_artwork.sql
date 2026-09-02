-- Artist artwork is a provider answer for a synthetic Album Artist group,
-- not item metadata: one artist may own albums in several collections and
-- libraries. The Rust module beside the resolver defines the lifecycle and
-- ambiguity rules; this migration only records their durable shape.
ALTER TABLE provider_metadata ADD COLUMN provider_artist_id TEXT;

CREATE TABLE artist_artwork (
    artist_key       TEXT PRIMARY KEY,
    artist_name      TEXT NOT NULL,
    musicbrainz_id   TEXT,
    image_id         TEXT,
    image_url        TEXT,
    outcome          TEXT NOT NULL,
    source_revision  TEXT NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE INDEX artist_artwork_mbid ON artist_artwork (musicbrainz_id);
