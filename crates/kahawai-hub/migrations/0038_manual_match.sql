-- HUB-5: human pins move out of `item_match` and into their own table.
--
-- `item_match` is derived state — a pure function of stored answers, chain
-- order and refusals — except for one column that was an INPUT: `manual`.
-- That mixture is why the pick had to carry three `manual = 0` predicates
-- guarding rows it must not touch, and why every recompute was a partial
-- one. Separating the input lets the pick recompute EVERY row from
-- scratch, with a pin winning inside the derivation rather than by being
-- skipped.
--
-- See the `providers` module doc for what each column means and which
-- side of the input/derived line every table sits on.

CREATE TABLE manual_match (
    item_id     TEXT PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    pinned_at   INTEGER NOT NULL
);

INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
SELECT item_id, provider, provider_id, updated_at
  FROM item_match
 WHERE manual = 1 AND provider_id <> '';

-- A pin and a refusal of the same record contradict each other.
-- `assign_manual` clears the refusal when it pins, but 0030 built the
-- pins it migrated without that step, so the contradiction can be on
-- disk already — and once the pick starts reading `manual_match`, such a
-- row would be a pin that never wins.
DELETE FROM rejected_matches
 WHERE (item_id, provider, provider_id) IN
       (SELECT item_id, provider, provider_id FROM manual_match);
