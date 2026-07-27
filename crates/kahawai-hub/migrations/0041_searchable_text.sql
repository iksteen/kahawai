-- HUB-12: albums are findable by artist, episodes by their resolved
-- titles.
--
-- Search compares a FOLDED needle (case, diacritics, numerals — see
-- enrich::fold) against stored text, so anything searchable needs a copy
-- folded the same way or the comparison is asymmetric: "motorhead" could
-- never match an artist stored as "Motörhead", and neither could
-- "motörhead", because the needle loses its accents before the LIKE.
-- `norm_title` is that copy for filenames; `norm_artist` is the same
-- thing for the artist. Folding happens in Rust, so the column is
-- backfilled by db::open, not here, and written by the two places the
-- registry writes `artist`.
ALTER TABLE items ADD COLUMN norm_artist TEXT;

-- Episode titles: an episode's sort_title was always its own filename
-- title, because the 0035 derivation looked for an item_match on the
-- episode itself and episodes never carry one — they render through
-- their parent's assignment. On the live library 19% of episodes have a
-- resolved title that appears nowhere in the filename, so those were
-- unfindable.
--
-- The derivation becomes parent-aware: an item's sort_title is the title
-- its ASSIGNED provider gave it, where "assigned" means the item's own
-- match for a top-level item and the PARENT's match for an episode or
-- track. For top-level items `COALESCE(parent_id, id)` degenerates to
-- the old expression exactly. Side-fill still costs a row its sort/search
-- place, same as for top-level items (the documented rule).
--
-- One definition, applied from every direction — repeated verbatim, as
-- 0035 warned: if you edit one, edit all, and `sort_title_never_drifts`
-- will tell you if you did not. The item_match triggers now also
-- recompute the matched item's CHILDREN, because their sort_titles
-- follow the parent's assignment.

DROP TRIGGER items_sort_title_insert;
DROP TRIGGER items_sort_title_retitle;
DROP TRIGGER item_match_sort_title_ins;
DROP TRIGGER item_match_sort_title_upd;
DROP TRIGGER item_match_sort_title_del;
DROP TRIGGER provider_metadata_sort_title_ins;
DROP TRIGGER provider_metadata_sort_title_upd;
DROP TRIGGER provider_metadata_sort_title_del;

CREATE TRIGGER items_sort_title_insert AFTER INSERT ON items BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.id;
END;

-- A rescan can rename the item; the fallback has to follow it.
CREATE TRIGGER items_sort_title_retitle AFTER UPDATE OF title ON items BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.id;
END;

-- The assignment moving changes which provider's titles win — for the
-- matched item AND for every child rendering through it.
CREATE TRIGGER item_match_sort_title_ins AFTER INSERT ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id OR parent_id = NEW.item_id;
END;

-- BOTH ends on UPDATE, as 0037 established: a row moved between items
-- leaves the old item stale otherwise.
CREATE TRIGGER item_match_sort_title_upd AFTER UPDATE ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id IN (OLD.item_id, NEW.item_id)
        OR parent_id IN (OLD.item_id, NEW.item_id);
END;

-- Unmatched again: back to their own titles, children included.
CREATE TRIGGER item_match_sort_title_del AFTER DELETE ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = OLD.item_id OR parent_id = OLD.item_id;
END;

-- And the answer's own title changing — the episode projection writes
-- per-episode rows, so NEW.item_id here IS the episode when one retitles.
CREATE TRIGGER provider_metadata_sort_title_ins AFTER INSERT ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id;
END;

CREATE TRIGGER provider_metadata_sort_title_upd AFTER UPDATE ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id IN (OLD.item_id, NEW.item_id);
END;

CREATE TRIGGER provider_metadata_sort_title_del AFTER DELETE ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = items.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(items.parent_id, items.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = OLD.item_id;
END;

-- 0040 gave the membership INSERTs their sort keys but missed the two
-- UPDATE-shaped triggers 0037 added, so a source moving between
-- collections (or a collection between libraries) re-created membership
-- rows with NULL keys — items that vanish to the top of a sorted browse.
-- Same statements, keys added.

DROP TRIGGER item_sources_libraries_upd;
DROP TRIGGER library_collections_libraries_upd;

CREATE TRIGGER item_sources_libraries_upd AFTER UPDATE ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id) IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
END;

CREATE TRIGGER library_collections_libraries_upd AFTER UPDATE ON library_collections BEGIN
    DELETE FROM item_libraries WHERE library_id IN (OLD.library_id, NEW.library_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE lc.library_id IN (OLD.library_id, NEW.library_id);
END;

-- Repair any keyless membership rows those triggers already produced.
UPDATE item_libraries SET
    sort_title = (SELECT i.sort_title FROM items i WHERE i.id = item_id),
    year       = (SELECT i.year FROM items i WHERE i.id = item_id)
 WHERE sort_title IS NULL OR year IS NULL;

-- Re-derive every child's sort_title under the new rule. Top-level items
-- compute to their current value, so only children can change; the 0040
-- membership sync ignores them (children are never members) and its WHEN
-- guard skips the no-ops.
UPDATE items SET sort_title = COALESCE(
    (SELECT pm.title FROM item_match im
       JOIN provider_metadata pm
         ON pm.item_id = items.id AND pm.provider = im.provider
      WHERE im.item_id = COALESCE(items.parent_id, items.id)
        AND NULLIF(pm.title, '') IS NOT NULL),
    title)
 WHERE parent_id IS NOT NULL;
