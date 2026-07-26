-- The merge is gone: nothing is stored that a read can derive.
--
-- What an item IS lives in item_match; what each provider said lives in
-- provider_metadata; the row the API serves is resolved from those two by
-- the `resolved_metadata` view, which hub/db.rs installs on open. A view
-- rather than a table because the resolution rule is the thing being
-- experimented with, and because a stored merge is what produced a day of
-- bugs: identity flipping to a weak match, a decline erasing a human's
-- correction, two manual rows tying on insertion order.
DROP TABLE merged_metadata;

-- Redundant since 0025: the PRIMARY KEY (item_id, provider) already serves
-- every lookup by item_id.
DROP INDEX IF EXISTS provider_metadata_item;
