-- Libraries (HUB-8): user-facing groupings of collections. A library has
-- exactly one media type and only accepts collections of that type;
-- grants (later) attach to libraries, never to collections.
CREATE TABLE libraries (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    media_type TEXT NOT NULL
);
CREATE TABLE library_collections (
    library_id    TEXT NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    PRIMARY KEY (library_id, module_id, collection_id)
);
CREATE INDEX library_collections_by_collection
    ON library_collections (module_id, collection_id);
