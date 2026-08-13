#!/usr/bin/env python3
"""End-to-end self-test for kahawai-collection-replay.py."""

from __future__ import annotations

import pathlib
import sqlite3
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
TOOL = HERE / "kahawai-collection-replay.py"

GLOBAL_SCHEMAS = {
    "satellites": "module_id TEXT PRIMARY KEY,module_type TEXT,name TEXT,cert_fingerprint TEXT,enrolled_at INTEGER,disabled INTEGER,pending_fingerprint TEXT,pending_issued_at INTEGER",
    "satellite_audit": "id INTEGER PRIMARY KEY,module_id TEXT,fingerprint TEXT,action TEXT,at INTEGER",
    "libraries": "id TEXT PRIMARY KEY,name TEXT,media_type TEXT",
    "library_collections": "library_id TEXT,module_id TEXT,collection_id TEXT,PRIMARY KEY(library_id,module_id,collection_id)",
    "users": "id TEXT PRIMARY KEY,username TEXT,password_hash TEXT,is_admin INTEGER,created_at INTEGER,all_libraries INTEGER,auth_version INTEGER",
    "refresh_families": "id TEXT PRIMARY KEY,user_id TEXT,current_token_hash TEXT,expires_at INTEGER,revoked_at INTEGER,created_at INTEGER,rotated_at INTEGER",
    "user_libraries": "user_id TEXT,library_id TEXT,PRIMARY KEY(user_id,library_id)",
    "user_prefs": "user_id TEXT,scope TEXT,key TEXT,value TEXT,PRIMARY KEY(user_id,scope,key)",
    "watch_state_archive": "user_id TEXT,size INTEGER,head_xxh3 INTEGER,tail_xxh3 INTEGER,position_ms INTEGER,duration_ms INTEGER,played INTEGER,play_count INTEGER,archived_at INTEGER,PRIMARY KEY(user_id,size,head_xxh3,tail_xxh3)",
    "ed2k_aid": "ed2k TEXT PRIMARY KEY,aid INTEGER,updated_at INTEGER,eid INTEGER,epno TEXT,gid INTEGER,group_name TEXT",
    "provider_ranks": "media_type TEXT,provider TEXT,rank INTEGER,PRIMARY KEY(media_type,provider)",
    "settings": "key TEXT PRIMARY KEY,value TEXT",
    "transcoder_pace": "module_id TEXT,work_class TEXT,multiple REAL,samples INTEGER,updated_at INTEGER,PRIMARY KEY(module_id,work_class)",
}

ITEM_SCHEMAS = {
    "provider_metadata": "item_id TEXT,provider TEXT,provider_id TEXT,title TEXT,overview TEXT,poster_path TEXT,rating REAL,premiered TEXT,original_language TEXT,genres TEXT,confidence TEXT,updated_at INTEGER,proj_season INTEGER,proj_episode INTEGER,cast_json TEXT,PRIMARY KEY(item_id,provider)",
    "provider_queries": "item_id TEXT,provider TEXT,query_type TEXT,query TEXT,rev INTEGER,asked_at INTEGER,PRIMARY KEY(item_id,provider,query_type,query)",
    "rejected_matches": "item_id TEXT,provider TEXT,provider_id TEXT,rejected_at INTEGER,PRIMARY KEY(item_id,provider,provider_id)",
    "manual_match": "item_id TEXT PRIMARY KEY,provider TEXT,provider_id TEXT,pinned_at INTEGER",
    "anime_ids": "item_id TEXT PRIMARY KEY,anidb_id INTEGER,anilist_id INTEGER,mapped_tvdb INTEGER,mapped_tmdb INTEGER",
    "enrichment_queue": "item_id TEXT,provider TEXT,due_at INTEGER,attempts INTEGER,reason TEXT,PRIMARY KEY(item_id,provider)",
    "item_relations": "from_item TEXT,kind TEXT,target_anilist INTEGER,target_title TEXT,PRIMARY KEY(from_item,kind,target_anilist)",
    "watch_state": "user_id TEXT,item_id TEXT,position_ms INTEGER,duration_ms INTEGER,played INTEGER,play_count INTEGER,updated_at INTEGER,PRIMARY KEY(user_id,item_id)",
}


def common(db: sqlite3.Connection, version: int) -> None:
    db.execute("CREATE TABLE _sqlx_migrations(version INTEGER PRIMARY KEY)")
    db.execute("INSERT INTO _sqlx_migrations VALUES(?)", (version,))
    for table, schema in GLOBAL_SCHEMAS.items():
        db.execute(f"CREATE TABLE {table}({schema})")
    for table, schema in ITEM_SCHEMAS.items():
        db.execute(f"CREATE TABLE {table}({schema})")
    db.execute("INSERT INTO satellites VALUES('m','mediahost','m','fp',1,0,NULL,NULL)")
    db.execute("INSERT INTO libraries VALUES('l1','One','movies')")
    db.execute("INSERT INTO libraries VALUES('l2','Two','movies')")
    db.execute("INSERT INTO users VALUES('u','u','x',0,1,1,1)")
    db.execute("INSERT INTO library_collections VALUES('l1','m','c')")
    db.execute("INSERT INTO library_collections VALUES('l2','m','c')")


def make_source(path: pathlib.Path, conflict: bool = False) -> None:
    db = sqlite3.connect(path)
    common(db, 56)
    db.executescript(
        """
        CREATE TABLE collections(module_id TEXT,collection_id TEXT,media_type TEXT,roots_json TEXT,
          sync_version INTEGER,exact_roots_json TEXT,root_adoption_pending INTEGER,
          PRIMARY KEY(module_id,collection_id));
        CREATE TABLE items(id TEXT PRIMARY KEY,kind TEXT,title TEXT,norm_title TEXT,year INTEGER,
          parent_id TEXT,season INTEGER,episode INTEGER,artist TEXT,sort_title TEXT,norm_artist TEXT,
          episode_end INTEGER,library_id TEXT,media_key TEXT);
        CREATE TABLE files(module_id TEXT,collection_id TEXT,path_rel TEXT,size INTEGER,mtime_unix INTEGER,
          head_xxh3 INTEGER,tail_xxh3 INTEGER,oshash INTEGER,streams_json TEXT,ed2k TEXT,
          subs_extracted INTEGER,revision INTEGER,root_token TEXT,source_path TEXT,
          PRIMARY KEY(module_id,collection_id,path_rel));
        CREATE TABLE item_sources(module_id TEXT,collection_id TEXT,path_rel TEXT,item_id TEXT,part INTEGER,
          root_token TEXT,source_path TEXT,PRIMARY KEY(module_id,collection_id,path_rel));
        CREATE TABLE library_item_sources(library_id TEXT,module_id TEXT,collection_id TEXT,path_rel TEXT,
          root_token TEXT,source_path TEXT,item_id TEXT,part INTEGER,
          PRIMARY KEY(library_id,module_id,collection_id,path_rel));
        CREATE TABLE subtitle_tracks(id INTEGER PRIMARY KEY,item_id TEXT,origin TEXT,module_id TEXT,
          collection_id TEXT,path_rel TEXT,stream_index INTEGER,format TEXT,language TEXT,label TEXT,
          provider TEXT,machine INTEGER,created_by TEXT,created_at INTEGER,derived_from INTEGER,
          root_token TEXT,source_path TEXT);
        CREATE TABLE item_subtitle_tracks(item_id TEXT,track_id INTEGER,PRIMARY KEY(item_id,track_id));
        CREATE TABLE image_set_failures(module_id TEXT,collection_id TEXT,path_rel TEXT,sub_index INTEGER,
          mtime_unix INTEGER,error TEXT,at INTEGER,root_token TEXT,source_path TEXT,
          PRIMARY KEY(module_id,collection_id,path_rel,sub_index));
        INSERT INTO collections VALUES('m','c','movies','["/media"]',9,
          '[{"token":"root","path":"/media"}]',0);
        INSERT INTO items VALUES('film','movie','Film','film',2020,NULL,NULL,NULL,NULL,'Film',NULL,NULL,'l1',NULL);
        INSERT INTO items VALUES('film:l2','movie','Film','film',2020,NULL,NULL,NULL,NULL,'Film',NULL,NULL,'l2',NULL);
        INSERT INTO files VALUES('m','c','',10,1,2,3,4,'{}',NULL,1,7,'root','Film.mkv');
        INSERT INTO library_item_sources VALUES('l1','m','c','','root','Film.mkv','film',2);
        INSERT INTO library_item_sources VALUES('l2','m','c','','root','Film.mkv','film:l2',2);
        INSERT INTO provider_metadata VALUES('film','tmdb','42','Film',NULL,NULL,NULL,NULL,NULL,NULL,'auto',1,NULL,NULL,NULL);
        INSERT INTO provider_metadata VALUES('film:l2','tmdb','42','Film',NULL,NULL,NULL,NULL,NULL,NULL,'auto',1,NULL,NULL,NULL);
        INSERT INTO watch_state VALUES('u','film',100,1000,0,1,1);
        INSERT INTO watch_state VALUES('u','film:l2',100,1000,0,1,1);
        INSERT INTO subtitle_tracks VALUES(5,'film','embedded','m','c','',0,'srt','en',NULL,NULL,0,NULL,1,NULL,'root','Film.mkv');
        INSERT INTO subtitle_tracks VALUES(9,'film','embedded','m','c','',1,'pgs','en',NULL,NULL,1,NULL,1,NULL,'root','Film.mkv');
        INSERT INTO subtitle_tracks VALUES(6,'film','ocr',NULL,NULL,NULL,NULL,'srt','en',NULL,NULL,1,NULL,1,9,NULL,NULL);
        INSERT INTO item_subtitle_tracks VALUES('film',5);
        INSERT INTO item_subtitle_tracks VALUES('film:l2',5);
        INSERT INTO item_subtitle_tracks VALUES('film',9);
        INSERT INTO item_subtitle_tracks VALUES('film:l2',9);
        INSERT INTO item_subtitle_tracks VALUES('film',6);
        INSERT INTO item_subtitle_tracks VALUES('film:l2',6);
        INSERT INTO image_set_failures VALUES('m','c','',0,1,'bad',1,'root','Film.mkv');
        """
    )
    if conflict:
        db.execute("UPDATE watch_state SET position_ms=200 WHERE item_id='film:l2'")
    db.commit()


def make_target(path: pathlib.Path) -> None:
    db = sqlite3.connect(path)
    common(db, 53)
    db.executescript(
        """
        CREATE TABLE collections(module_id TEXT,collection_id TEXT,media_type TEXT,roots_json TEXT,
          sync_version INTEGER,root_adoption_pending INTEGER,PRIMARY KEY(module_id,collection_id));
        CREATE TABLE collection_roots(id INTEGER PRIMARY KEY,module_id TEXT,collection_id TEXT,
          root_token TEXT,normalized_path TEXT,configured INTEGER,UNIQUE(module_id,collection_id,root_token));
        CREATE TABLE items(id TEXT PRIMARY KEY,kind TEXT,title TEXT,norm_title TEXT,year INTEGER,
          parent_id TEXT,season INTEGER,episode INTEGER,artist TEXT,sort_title TEXT,norm_artist TEXT,
          episode_end INTEGER,module_id TEXT,collection_id TEXT);
        CREATE TABLE files(id INTEGER PRIMARY KEY,module_id TEXT,collection_id TEXT,root_id INTEGER,
          path_rel TEXT,item_id TEXT,part INTEGER,size INTEGER,mtime_unix INTEGER,head_xxh3 INTEGER,
          tail_xxh3 INTEGER,oshash INTEGER,streams_json TEXT,ed2k TEXT,subs_extracted INTEGER,revision INTEGER);
        CREATE TABLE subtitle_tracks(id INTEGER PRIMARY KEY,item_id TEXT,source_id INTEGER,origin TEXT,
          stream_index INTEGER,format TEXT,language TEXT,label TEXT,provider TEXT,machine INTEGER,
          created_by TEXT,created_at INTEGER,derived_from INTEGER,payload_id INTEGER);
        CREATE TABLE image_set_failures(source_id INTEGER,sub_index INTEGER,mtime_unix INTEGER,
          error TEXT,at INTEGER,PRIMARY KEY(source_id,sub_index));
        INSERT INTO collections VALUES('m','c','movies','["/media"]',8,0);
        INSERT INTO items VALUES('film','movie','Film','film',2020,NULL,NULL,NULL,NULL,'Film',NULL,NULL,'m','c');
        INSERT INTO files VALUES(1,'m','c',NULL,'Film.mkv','film',NULL,10,1,2,3,4,'{}',NULL,1,7);
        """
    )
    db.commit()


def run(*args: object, ok: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run([sys.executable, str(TOOL), *(str(a) for a in args)], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if (result.returncode == 0) != ok:
        raise AssertionError(f"command {args} returned {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="kahawai-replay-selftest-") as raw:
        root = pathlib.Path(raw)
        source, target, bundle = root / "source.db", root / "target.db", root / "bundle.db"
        make_source(source)
        make_target(target)
        run("export", source, bundle)
        run("import", bundle, target)
        db = sqlite3.connect(target)
        assert db.execute("SELECT count(*) FROM items").fetchone()[0] == 1
        assert db.execute("SELECT sync_version FROM collections").fetchone()[0] == 9
        assert db.execute("SELECT id,item_id,part FROM files").fetchone() == (1, "film", 2)
        assert db.execute("SELECT count(*) FROM subtitle_tracks").fetchone()[0] == 3
        assert db.execute("SELECT derived_from FROM subtitle_tracks WHERE id=6").fetchone()[0] == 9
        assert db.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()[0] == 0
        assert db.execute("PRAGMA quick_check").fetchone()[0] == "ok"

        bad_source, bad_bundle, untouched = root / "bad.db", root / "bad-bundle.db", root / "untouched.db"
        make_source(bad_source, conflict=True)
        make_target(untouched)
        before = untouched.read_bytes()
        refused = run("export", bad_source, bad_bundle, ok=False)
        assert "watch_state conflict" in refused.stderr
        assert not bad_bundle.exists() or sqlite3.connect(bad_bundle).execute(
            "SELECT count(*) FROM components"
        ).fetchone()[0] == 0
        assert untouched.read_bytes() == before
    print("collection replay self-test: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
