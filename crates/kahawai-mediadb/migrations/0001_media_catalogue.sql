CREATE TABLE mediahosts (id TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE collections (
 id TEXT PRIMARY KEY, mediahost_id TEXT NOT NULL REFERENCES mediahosts(id),
 remote_id TEXT NOT NULL, media_type TEXT NOT NULL CHECK(media_type IN ('movies','series','anime','music')),
 epoch TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0),
 snapshot_active INTEGER NOT NULL DEFAULT 0 CHECK(snapshot_active IN (0,1)),
 generation INTEGER NOT NULL DEFAULT 0, snapshot_max INTEGER NOT NULL DEFAULT 0,
 UNIQUE(mediahost_id,remote_id), UNIQUE(id,media_type)
);
CREATE TABLE collection_roots (
 id TEXT PRIMARY KEY, collection_id TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
 token TEXT NOT NULL CHECK(length(token)>0), path TEXT NOT NULL,
 active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
 UNIQUE(collection_id,token), UNIQUE(collection_id,path), UNIQUE(id,collection_id)
);
CREATE TABLE files (
 id TEXT PRIMARY KEY, collection_id TEXT NOT NULL, root_id TEXT NOT NULL,
 path TEXT NOT NULL CHECK(length(path)>0), version INTEGER NOT NULL DEFAULT 0,
 seen INTEGER NOT NULL DEFAULT 0, size INTEGER, mtime INTEGER,
 head_hash BLOB, tail_hash BLOB, oshash BLOB, media_json TEXT, mapping_error TEXT,
 FOREIGN KEY(root_id,collection_id) REFERENCES collection_roots(id,collection_id) ON DELETE CASCADE,
 UNIQUE(root_id,path), UNIQUE(id,collection_id)
);
CREATE TABLE source_facts (
 file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 kind TEXT NOT NULL, version INTEGER NOT NULL, seen INTEGER NOT NULL,
 payload BLOB NOT NULL, PRIMARY KEY(file_id,kind)
);
CREATE TABLE library_items (
 id TEXT PRIMARY KEY,
 media_type TEXT NOT NULL CHECK(media_type IN ('movies','series','anime','music')),
 title TEXT NOT NULL, title_key TEXT NOT NULL, year INTEGER,
 singleton TEXT NOT NULL,
 CHECK(singleton<>'' OR (media_type<>'music' AND title_key<>'' AND year IS NOT NULL)),
 UNIQUE(media_type,title_key,year,singleton)
);
CREATE UNIQUE INDEX library_item_unknown_year ON library_items(media_type,title_key,singleton) WHERE year IS NULL;
CREATE INDEX library_item_browse ON library_items(media_type,title_key,year,id);
CREATE TABLE collection_items (
 id TEXT PRIMARY KEY, collection_id TEXT NOT NULL, root_id TEXT NOT NULL,
 occurrence TEXT NOT NULL, title TEXT NOT NULL, year INTEGER,
 library_item_id TEXT NOT NULL REFERENCES library_items(id),
 artist TEXT, description_json TEXT NOT NULL, manual INTEGER NOT NULL DEFAULT 0 CHECK(manual IN (0,1)),
 FOREIGN KEY(root_id,collection_id) REFERENCES collection_roots(id,collection_id) ON DELETE CASCADE,
 UNIQUE(root_id,occurrence), UNIQUE(id,collection_id)
);
CREATE INDEX collection_item_library ON collection_items(library_item_id,collection_id,id);
CREATE TABLE media_entries (
 id TEXT PRIMARY KEY, collection_id TEXT NOT NULL, item_id TEXT NOT NULL,
 occurrence TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('movie','episode','track')),
 title TEXT NOT NULL, artist TEXT, disc INTEGER CHECK(disc>0), track INTEGER CHECK(track>0),
 FOREIGN KEY(item_id,collection_id) REFERENCES collection_items(id,collection_id) ON DELETE CASCADE,
 UNIQUE(item_id,occurrence), UNIQUE(id,collection_id)
);
CREATE TABLE media_parts (
 entry_id TEXT NOT NULL, collection_id TEXT NOT NULL, ordinal INTEGER NOT NULL CHECK(ordinal>0),
 file_id TEXT NOT NULL UNIQUE,
 FOREIGN KEY(entry_id,collection_id) REFERENCES media_entries(id,collection_id) ON DELETE CASCADE,
 FOREIGN KEY(file_id,collection_id) REFERENCES files(id,collection_id) ON DELETE CASCADE,
 PRIMARY KEY(entry_id,ordinal)
);
CREATE TABLE entry_episodes (
 entry_id TEXT NOT NULL REFERENCES media_entries(id) ON DELETE CASCADE,
 ordinal INTEGER NOT NULL CHECK(ordinal>0), numbering TEXT NOT NULL CHECK(numbering IN ('season','absolute')),
 season INTEGER CHECK(season>=0), episode INTEGER NOT NULL CHECK(episode>=0),
 episode_end INTEGER CHECK(episode_end>=episode),
 CHECK((numbering='season' AND season IS NOT NULL) OR (numbering='absolute' AND season IS NULL)),
 PRIMARY KEY(entry_id,ordinal)
);
CREATE TABLE provider_records (
 id TEXT PRIMARY KEY, provider TEXT NOT NULL, namespace TEXT NOT NULL, external_id TEXT NOT NULL,
 language TEXT NOT NULL, media_type TEXT NOT NULL CHECK(media_type IN ('movies','series','anime','music')),
 title TEXT NOT NULL, year INTEGER, description_json TEXT NOT NULL,
 UNIQUE(provider,namespace,external_id,language), UNIQUE(id,media_type)
);
CREATE TABLE metadata_assignments (
 item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
 record_id TEXT NOT NULL REFERENCES provider_records(id), UNIQUE(item_id,record_id)
);
CREATE INDEX assignment_record ON metadata_assignments(record_id,item_id);
CREATE TABLE metadata_supplements (
 item_id TEXT NOT NULL, primary_record_id TEXT NOT NULL,
 record_id TEXT NOT NULL REFERENCES provider_records(id),
 FOREIGN KEY(item_id,primary_record_id) REFERENCES metadata_assignments(item_id,record_id) ON DELETE CASCADE,
 PRIMARY KEY(item_id,record_id), CHECK(record_id<>primary_record_id)
);
CREATE TABLE provider_order (
 media_type TEXT NOT NULL CHECK(media_type IN ('movies','series','anime','music')),
 provider TEXT NOT NULL, position INTEGER NOT NULL CHECK(position>=0),
 PRIMARY KEY(media_type,provider), UNIQUE(media_type,position)
);
INSERT INTO provider_order VALUES
 ('movies','tmdb',0),('movies','tvdb',1),('series','tmdb',0),('series','tvdb',1),
 ('anime','anidb',0),('anime','anilist',1),('anime','tmdb',2),('anime','tvdb',3),
 ('music','musicbrainz',0);
CREATE TABLE libraries (
 id TEXT PRIMARY KEY, name TEXT NOT NULL,
 media_type TEXT NOT NULL CHECK(media_type IN ('movies','series','anime','music')),
 UNIQUE(id,media_type)
);
CREATE TABLE library_collections (
 library_id TEXT NOT NULL, collection_id TEXT NOT NULL, media_type TEXT NOT NULL,
 position INTEGER NOT NULL CHECK(position>=0),
 FOREIGN KEY(library_id,media_type) REFERENCES libraries(id,media_type) ON DELETE CASCADE,
 FOREIGN KEY(collection_id,media_type) REFERENCES collections(id,media_type) ON DELETE CASCADE,
 PRIMARY KEY(library_id,collection_id), UNIQUE(library_id,position)
);
CREATE INDEX library_collection_membership ON library_collections(collection_id,library_id);
