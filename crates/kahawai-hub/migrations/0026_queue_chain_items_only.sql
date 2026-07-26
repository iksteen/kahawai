-- The 0025 backfill queued any item with a missing field, episodes
-- included. Episodes are not chain-walked: their description comes from
-- the show's episode pass (enrich_show_episodes), so a queue row for one
-- is work nothing will ever pick up — 353 rows sat permanently due on
-- the first real run.
--
-- Only the kinds the chain walks may be queued. Kept as its own
-- migration rather than an edit to 0025, because 0025 is already applied
-- and sqlx validates migration checksums.
DELETE FROM enrichment_queue
WHERE item_id IN (SELECT id FROM items WHERE kind NOT IN ('movie', 'show', 'album'));
