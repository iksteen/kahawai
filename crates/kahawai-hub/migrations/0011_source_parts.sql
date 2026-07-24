-- Multi-part sources (CD1/CD2-era rips): parts of one movie are ordered
-- sources of a single item; NULL part = a complete single-file source.
ALTER TABLE item_sources ADD COLUMN part INTEGER;
