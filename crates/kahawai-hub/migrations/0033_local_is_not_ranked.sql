-- HUB-9, corrected: `local` is not a chain member.
--
-- 0032 put it at rank 0, which made "the file on your disk" a position in
-- a list — implying some order where a search result should beat it, and
-- forcing the owner to maintain a knob that has one sensible setting. It
-- is now asked before the chain and sorts ahead of it, in code.
--
-- Anything ranked below it moves back up; the stored order is otherwise
-- untouched, so a hand-made order keeps its relative sequence.
DELETE FROM provider_ranks WHERE provider = 'local';
UPDATE provider_ranks SET rank = rank - 1 WHERE rank > 0;
