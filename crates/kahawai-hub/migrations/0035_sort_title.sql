-- NFR-1: an indexed sort key for browse.
--
-- Sorting on a title resolved at read time cannot use an index, so every
-- page paid a full sort of the whole catalogue: 473 ms for the first page
-- of 50k items and 1271 ms for the last. Ordered by this column instead,
-- the same pages cost 0.9 ms and 7.8 ms.
--
-- This stores what a read could derive, which HUB-5 ruled out after the
-- merged-metadata table spent its life subtly out of date. The rule was
-- right and the reason it was right is staleness — so the answer here is
-- not "remember to update it" but TRIGGERS: the value is maintained by
-- the database on every write to anything it depends on, including a
-- write made by hand in sqlite3, which is precisely how the old merge
-- drifted. There is no code path that can forget.
--
-- It holds the ASSIGNED answer's title, falling back to the item's own.
-- That is what the browse list sorts by; the resolved view may side-fill
-- a title from another provider when the assigned record has none, which
-- costs such a row its place in the sort and nothing else.

ALTER TABLE items ADD COLUMN sort_title TEXT;

UPDATE items SET sort_title = COALESCE(
    (SELECT pm.title FROM item_match im
       JOIN provider_metadata pm
         ON pm.item_id = im.item_id AND pm.provider = im.provider
      WHERE im.item_id = items.id AND NULLIF(pm.title, '') IS NOT NULL),
    title);

-- Browse orders by (sort_title, year); the index carries both so a page
-- is a range scan rather than a sort, at any offset.
CREATE INDEX items_sort_title ON items (sort_title, year);

-- One definition, applied from every direction. SQLite has no way to
-- share an expression between triggers, so it is repeated verbatim —
-- if you edit one, edit all of them, and `sort_title_never_drifts`
-- (tests/sort_title.rs) will tell you if you did not.

CREATE TRIGGER items_sort_title_insert AFTER INSERT ON items BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = NEW.id AND NULLIF(pm.title, '') IS NOT NULL),
        NEW.title)
     WHERE id = NEW.id;
END;

-- A rescan can rename the item; the fallback has to follow it.
CREATE TRIGGER items_sort_title_retitle AFTER UPDATE OF title ON items BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = NEW.id AND NULLIF(pm.title, '') IS NOT NULL),
        NEW.title)
     WHERE id = NEW.id;
END;

-- The assignment moving to another provider changes which title wins.
CREATE TRIGGER item_match_sort_title_ins AFTER INSERT ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM provider_metadata pm
          WHERE pm.item_id = NEW.item_id AND pm.provider = NEW.provider
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id;
END;

CREATE TRIGGER item_match_sort_title_upd AFTER UPDATE ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM provider_metadata pm
          WHERE pm.item_id = NEW.item_id AND pm.provider = NEW.provider
            AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id;
END;

-- Unmatched again: back to the item's own title.
CREATE TRIGGER item_match_sort_title_del AFTER DELETE ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = OLD.item_id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = OLD.item_id;
END;

-- And the assigned record's own title changing, which is the case a
-- hand-written UPDATE would otherwise slip past.
CREATE TRIGGER provider_metadata_sort_title_ins AFTER INSERT ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = NEW.item_id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id;
END;

CREATE TRIGGER provider_metadata_sort_title_upd AFTER UPDATE ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = NEW.item_id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = NEW.item_id;
END;

CREATE TRIGGER provider_metadata_sort_title_del AFTER DELETE ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = OLD.item_id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id = OLD.item_id;
END;
