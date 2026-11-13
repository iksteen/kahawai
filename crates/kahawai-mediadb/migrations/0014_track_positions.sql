-- Song search (Store::search_tracks). Numbered track positions, to find the
-- few positions holding more than one entry without grouping every track; and
-- tracks in title order, covering the search's filter and sort, so a page
-- walks the index rather than sorting every match ahead of its offset.
CREATE INDEX track_positions ON media_entries(item_id,disc,track) WHERE kind='track' AND track IS NOT NULL;
CREATE INDEX track_titles ON media_entries(title COLLATE NOCASE,item_id,disc,track,id,collection_id) WHERE kind='track';
