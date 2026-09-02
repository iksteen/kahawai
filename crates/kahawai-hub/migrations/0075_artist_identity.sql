ALTER TABLE items ADD COLUMN artist_key TEXT;
CREATE INDEX items_album_artist_key ON items (artist_key)
    WHERE kind='album' AND artist_key IS NOT NULL;
