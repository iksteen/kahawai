-- Series hierarchy (HUB-12, M4): episodes point at their show; season
-- items are a projection, not rows (ponytail: add if artwork needs them).
ALTER TABLE items ADD COLUMN parent_id TEXT REFERENCES items(id);
ALTER TABLE items ADD COLUMN season INTEGER;
ALTER TABLE items ADD COLUMN episode INTEGER;
CREATE INDEX items_children ON items (parent_id, season, episode);
