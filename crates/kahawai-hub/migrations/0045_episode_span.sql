-- HUB-30 batch markers: one file spanning several episodes ("OVA 1-2",
-- "S01E01-E02"). item_sources deliberately binds a file to exactly ONE
-- item (its PK), and we hold no per-episode byte offsets — so the
-- honest model is a SPAN: a single episode item covering
-- episode..episode_end, played as one. NULL = ordinary single episode.
-- The hash binder leaves span items alone (a single-epno answer must
-- not collapse a span), and the episode-title projection skips them.
ALTER TABLE items ADD COLUMN episode_end INTEGER;
