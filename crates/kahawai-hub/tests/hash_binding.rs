//! HUB-30/30a: a file's ED2K hash states which episode it IS, and on
//! disagreement with the filename, the hash wins.
//!
//! The AniDB lookups are network and tested live; the BINDER is pure
//! database work, so everything it may move — sources, watch state,
//! ghost rows — is pinned here, hashes seeded as the resolver would
//! have cached them.

use kahawai_hub::enrich::Enricher;
use sqlx::SqlitePool;

const AID: u32 = 1234;

async fn harness() -> (Enricher, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let enricher = Enricher::new(dir.path().to_path_buf());
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
         VALUES ('m','mediahost','m','',unixepoch(),0)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
         VALUES ('m','c','anime','[\"/m\"]',1)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES('show','show','X','x','m','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    (enricher, db, dir)
}

/// An episode item with one hashed file bound to it, and the cached
/// AniDB answer for that hash.
async fn episode(
    db: &SqlitePool,
    id: &str,
    season: Option<i64>,
    ep: i64,
    file_aid: u32,
    epno: &str,
) {
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
         VALUES(?,'episode',?,?,'show',?,?,'m','c')",
    )
    .bind(id)
    .bind(format!("title {id}"))
    .bind(format!("title {id}"))
    .bind(season)
    .bind(ep)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json,subs_extracted,ed2k)
         VALUES('m','c',?||'.mkv',?,700,1,0,0,0,'{}',0,'hash-'||?)",
    )
    .bind(id)
    .bind(id)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR REPLACE INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
         VALUES ('hash-' || ?, ?, 9, ?, 7, 'Grp', unixepoch())",
    )
    .bind(id)
    .bind(file_aid)
    .bind(epno)
    .execute(db)
    .await
    .unwrap();
}

async fn slot_of(db: &SqlitePool, path: &str) -> (Option<i64>, i64, String) {
    sqlx::query_as::<_, (Option<i64>, i64, String)>(
        "SELECT i.season,i.episode,i.id FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items i ON i.id=fb.item_id
          WHERE f.path_rel=?",
    )
    .bind(path)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A file bound to NOTHING — the NCOP/NCED shape — is identified by its
/// cached hash answer and bound under whatever its aid names: a season-0
/// slot for a show, the movie item itself for a movie.
#[tokio::test]
async fn bare_files_bind_to_what_their_hash_names() {
    let (enricher, db, _dir) = harness().await;
    let q = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql)
                .execute(&db)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
        }
    };
    q("INSERT INTO anime_ids (item_id, anidb_id) VALUES ('show', 1234)").await;
    q("INSERT INTO items(id,kind,title,norm_title,module_id,collection_id) VALUES('film','movie','A Film','a film','m','c')")
        .await;
    q("INSERT INTO anime_ids (item_id, anidb_id) VALUES ('film', 9999)").await;

    // Three bare files: a creditless opening (C2) of the show, the
    // movie's own bytes ("1"), and one whose anime is not on the shelf.
    for (path, hash) in [
        ("ncop.mkv", "h-nc"),
        ("film.mkv", "h-film"),
        ("stray.mkv", "h-stray"),
    ] {
        sqlx::query(
            "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                                head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted, ed2k)
             VALUES ('m','c', ?, 700, 1, 0, 0, 0, '{}', 0, ?)",
        )
        .bind(path)
        .bind(hash)
        .execute(&db)
        .await
        .unwrap();
    }
    q(
        "INSERT INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
       VALUES ('h-nc', 1234, 1, 'C2', 7, 'Grp', unixepoch())",
    )
    .await;
    q(
        "INSERT INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
       VALUES ('h-film', 9999, 2, '1', 7, 'Grp', unixepoch())",
    )
    .await;
    q(
        "INSERT INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
       VALUES ('h-stray', 5555, 3, '1', 7, 'Grp', unixepoch())",
    )
    .await;

    let bound = enricher.bind_bare_files(&db).await.unwrap();
    assert_eq!(bound, 2);
    assert_eq!(slot_of(&db, "ncop.mkv").await.0, Some(0));
    assert_eq!(
        slot_of(&db, "ncop.mkv").await.1,
        102,
        "C2 lands in the credits band"
    );
    let film: String = sqlx::query_scalar("SELECT fb.item_id FROM files f JOIN file_bindings fb ON fb.file_id=f.id WHERE f.path_rel='film.mkv'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(film, "film", "a movie file becomes the movie's own source");
    let stray: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM files f JOIN file_bindings fb ON fb.file_id=f.id WHERE f.path_rel='stray.mkv'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stray, 0, "an anime not in the catalogue binds nothing");

    // Idempotent.
    assert_eq!(enricher.bind_bare_files(&db).await.unwrap(), 0);
}

/// A bare file whose aid nothing owns MINTS a movie item from AniDB's
/// answer — or adopts an aid-less twin — while a series-type aid stays
/// bare. The XML is a fixture in the cache location, so no network.
#[tokio::test]
async fn ownerless_movies_are_minted_or_adopted_from_the_hash() {
    let (enricher, db, dir) = harness().await;
    let xmldir = dir.path().join("anime/httpapi");
    std::fs::create_dir_all(&xmldir).unwrap();
    let xml = |aid: u32, kind: &str, eps: u32, title: &str, date: &str| {
        std::fs::write(
            xmldir.join(format!("{aid}.xml")),
            format!(
                "<anime id=\"{aid}\"><type>{kind}</type><episodecount>{eps}</episodecount>\
                 <startdate>{date}</startdate>\
                 <titles><title xml:lang=\"x-jat\" type=\"main\">{title} Romaji</title>\
                 <title xml:lang=\"en\" type=\"official\">{title}</title></titles></anime>"
            ),
        )
        .unwrap();
    };
    xml(979, "Movie", 1, "Akira", "1988-07-16");
    xml(500, "TV Series", 26, "Some Show", "1999-01-01");
    xml(600, "Movie", 1, "Adopted Film", "2001-06-01");
    // Movie-shaped despite the type string: a single-episode OVA
    // (Kite Liberator's shape) mints; a multi-episode OVA stays bare.
    xml(700, "OVA", 1, "Lone OVA", "2008-03-01");
    xml(800, "OVA", 4, "Serial OVA", "2005-01-01");
    // The adoptable twin: same normalized title and year, no anime_ids.
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,year,module_id,collection_id)
         VALUES('twin','movie','Adopted Film','adopted film',2001,'m','c')",
    )
    .execute(&db)
    .await
    .unwrap();

    for (path, hash, aid) in [
        ("akira.mkv", "h-akira", 979),
        ("stray-ep.mkv", "h-se", 500),
        ("adopt.mkv", "h-ad", 600),
        ("lone-ova.mkv", "h-lo", 700),
        ("serial-ova.mkv", "h-so", 800),
    ] {
        sqlx::query(
            "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                                head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted, ed2k)
             VALUES ('m','c', ?, 700, 1, 0, 0, 0, '{}', 0, ?)",
        )
        .bind(path)
        .bind(hash)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
             VALUES (?, ?, 1, '1', 7, 'Grp', unixepoch())",
        )
        .bind(hash)
        .bind(aid)
        .execute(&db)
        .await
        .unwrap();
    }

    let bound = enricher.bind_bare_files(&db).await.unwrap();
    assert_eq!(
        bound, 3,
        "movie, adoption and single-episode OVA bind; stray episode and serial OVA do not"
    );
    let lone: Option<String> = sqlx::query_scalar(
        "SELECT i.title FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items i ON i.id=fb.item_id WHERE f.path_rel='lone-ova.mkv'",
    )
    .fetch_optional(&db)
    .await
    .unwrap();
    assert_eq!(
        lone.as_deref(),
        Some("Lone OVA"),
        "single-episode OVA minted as a movie"
    );
    let serial: Option<String> =
        sqlx::query_scalar("SELECT fb.item_id FROM files f LEFT JOIN file_bindings fb ON fb.file_id=f.id WHERE f.path_rel='serial-ova.mkv'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(serial.is_none(), "multi-episode OVA stays bare");

    let akira: (String, Option<i64>, String) = sqlx::query_as(
        "SELECT i.title,i.year,i.id FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items i ON i.id=fb.item_id
          WHERE f.path_rel='akira.mkv'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (akira.0.as_str(), akira.1),
        ("Akira", Some(1988)),
        "minted from the XML"
    );
    let aid_of: i64 = sqlx::query_scalar("SELECT anidb_id FROM anime_ids WHERE item_id = ?")
        .bind(&akira.2)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(aid_of, 979);

    let adopted: String =
        sqlx::query_scalar("SELECT fb.item_id FROM files f JOIN file_bindings fb ON fb.file_id=f.id WHERE f.path_rel='adopt.mkv'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        adopted, "twin",
        "an aid-less twin is adopted, not duplicated"
    );

    let stray: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM files f JOIN file_bindings fb ON fb.file_id=f.id WHERE f.path_rel='stray-ep.mkv'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stray, 0, "a series-type aid must not scaffold a show");
    // And no phantom show was created for it.
    let shows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE kind IN ('show','movie')")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        shows, 4,
        "show harness + akira + twin + lone OVA, nothing else"
    );
}

/// A file WE parked in season 0 on a name guess is reclaimed by a
/// regular hash number — season 0 is speculation, not identity. A real
/// SxxEyy key still is.
#[tokio::test]
async fn a_regular_hash_number_reclaims_a_season_zero_parking() {
    let (enricher, db, _dir) = harness().await;
    episode(&db, "parked", Some(0), 1, AID, "3").await;
    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!((moves[0].from, moves[0].to), ((Some(0), 1), (None, 3)));
    assert_eq!(slot_of(&db, "parked.mkv").await.0, None);
}

#[tokio::test]
async fn the_hash_wins_over_the_filename() {
    let (enricher, db, _dir) = harness().await;
    // Filename said 6; AniDB says the file IS episode 5, which exists.
    episode(&db, "e5", None, 5, AID, "05").await;
    episode(&db, "e6", None, 6, AID, "5").await; // misnumbered rip of ep 5
    // The user watched it under the wrong number.
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin, created_at)
                 VALUES ('u','u','x',0,unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at)
         VALUES ('u','e6',120000,1200000,1,1,unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();

    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!((moves[0].from, moves[0].to), ((None, 6), (None, 5)));

    // Both files now back episode 5 — a second source, exactly HUB-3.
    assert_eq!(slot_of(&db, "e5.mkv").await.0, None);
    assert_eq!(slot_of(&db, "e6.mkv").await, (None, 5, "e5".into()));
    // The misnumbered item is a ghost and is gone; the watch state moved
    // with the content.
    let ghost: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id='e6'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(ghost, 0, "a sourceless misnumbered episode must not linger");
    let watched: String = sqlx::query_scalar("SELECT item_id FROM watch_state WHERE user_id='u'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(watched, "e5");

    // Idempotent: nothing left to move.
    let again = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();
    assert!(again.is_empty(), "{again:?}");
}

#[tokio::test]
async fn specials_land_in_season_zero_and_the_rest_is_left_alone() {
    let (enricher, db, _dir) = harness().await;
    // Parsed as absolute episode 0 — the classic misfiled special.
    episode(&db, "sp", None, 0, AID, "S2").await;
    // A credits file squatting on an episode slot: an artifact of the
    // numbering, moved into season 0's credits band.
    episode(&db, "op", None, 90, AID, "C1").await;
    // A file from a DIFFERENT AniDB entry (per-season split): left alone.
    episode(&db, "other", None, 40, AID + 1, "3").await;
    // Season-keyed episode: AniDB numbering is not this space; left.
    episode(&db, "skeyed", Some(2), 3, AID, "4").await;

    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();
    assert_eq!(moves.len(), 2, "{moves:?}");

    let sp = slot_of(&db, "sp.mkv").await;
    assert_eq!((sp.0, sp.1), (Some(0), 2), "special bound into season 0");
    let op = slot_of(&db, "op.mkv").await;
    assert_eq!(
        (op.0, op.1),
        (Some(0), 101),
        "credits into season 0's C band"
    );
    assert_eq!(
        slot_of(&db, "other.mkv").await,
        (None, 40, "other".into()),
        "cross-aid stays put"
    );
    assert_eq!(
        slot_of(&db, "skeyed.mkv").await,
        (Some(2), 3, "skeyed".into())
    );

    // The created season-0 item carried the file's own title.
    let title: String = sqlx::query_scalar(
        "SELECT title FROM items WHERE parent_id='show' AND season=0 AND episode=2",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(title, "title sp");
}

/// HUB-5 question-gated selection: an item is due while its CURRENT
/// question has no provider_queries row — a miss never gates, a rename
/// re-opens automatically, a real answer is terminal. (The Doomed
/// Megalopolis failure, 2026-07-28: a name-based miss permanently
/// blocked files whose hashes arrived minutes later.)
#[tokio::test]
async fn selection_follows_the_question_not_the_miss() {
    let (enricher, db, _dir) = harness().await;
    let registry = kahawai_hub::registry::Registry::new(db.clone(), Default::default());
    async fn selected(registry: &kahawai_hub::registry::Registry, enricher: &Enricher) -> bool {
        enricher
            .select_anime_items(registry)
            .await
            .unwrap()
            .iter()
            .any(|i| i.id == "show")
    }
    // Membership in an anime collection (the harness show has no source
    // yet — give it one, unhashed so the hash branch stays quiet).
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,module_id,collection_id) VALUES('ep','episode','e','e','show','m','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json) VALUES('m','c','e.mkv','ep',1,1,0,0,0,'{}')",
    )
    .execute(&db)
    .await
    .unwrap();

    // Never asked: the name question is owed.
    assert!(
        selected(&registry, &enricher).await,
        "fresh item must be due"
    );

    // A recorded miss alone does NOT settle it — only the question does.
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, confidence, updated_at)
         VALUES ('show','anime','','miss',unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(selected(&registry, &enricher).await, "a miss must not gate");
    kahawai_hub::providers::record_question(
        &db,
        "show",
        "anime",
        "title",
        &kahawai_hub::providers::title_anchor("x", None),
    )
    .await;
    assert!(
        !selected(&registry, &enricher).await,
        "asked question settles it"
    );

    // A rename changes the question: due again, exactly once.
    sqlx::query("UPDATE items SET title='Y', norm_title='y' WHERE id='show'")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        selected(&registry, &enricher).await,
        "rename re-opens the question"
    );
    kahawai_hub::providers::record_question(
        &db,
        "show",
        "anime",
        "title",
        &kahawai_hub::providers::title_anchor("y", None),
    )
    .await;
    assert!(!selected(&registry, &enricher).await);

    // A real answer is terminal for the name branch, whatever the log.
    sqlx::query("UPDATE items SET title='Z', norm_title='z' WHERE id='show'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, confidence, updated_at)
         VALUES ('show','anilist','1148','auto',unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(
        !selected(&registry, &enricher).await,
        "identity beats any rename"
    );

    // …until the identity brings a NEW question: the bridge fetch by
    // mapped id is owed despite an old tvdb title-search miss.
    sqlx::query(
        "INSERT INTO anime_ids (item_id, anidb_id, anilist_id, mapped_tvdb) VALUES ('show',1505,1148,98611)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, confidence, updated_at)
         VALUES ('show','tvdb','','miss',unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(
        selected(&registry, &enricher).await,
        "mapped id is a new question"
    );
    kahawai_hub::providers::record_question(&db, "show", "tvdb", "mapped_id", "98611").await;
    assert!(
        !selected(&registry, &enricher).await,
        "bridge question spent"
    );
}

/// Adds a second file to an EXISTING episode item, with its own hash
/// answer: the shape of two rips sharing one slot.
async fn extra_source(
    db: &SqlitePool,
    item_id: &str,
    name: &str,
    file_aid: u32,
    eid: i64,
    epno: &str,
) {
    sqlx::query(
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json,subs_extracted,ed2k)
         VALUES('m','c',?||'.mkv',?,700,1,0,0,0,'{}',0,'hash-'||?)",
    )
    .bind(name)
    .bind(item_id)
    .bind(name)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR REPLACE INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
         VALUES ('hash-' || ?, ?, ?, ?, 7, 'Grp', unixepoch())",
    )
    .bind(name)
    .bind(file_aid)
    .bind(eid)
    .bind(epno)
    .execute(db)
    .await
    .unwrap();
}

/// Two files on one slot, from another AniDB entry, naming different
/// episodes: they are different episodes and must come apart.
///
/// Megazone 23 is the case (HUB-30, amended 2026-08-06): `pt.03-a` and
/// `pt.03-b` both landed on S00E023 — an episode number lifted out of
/// the TITLE "Megazone 23" — while their hashes name eids 39483 and
/// 39484, episodes 1 and 2 of a different aid. The numbering cannot come
/// from the hash, because that entry's episode 1 is not this show's, so
/// the first keeps the slot and the second is parked on a free season-0
/// number.
#[tokio::test]
async fn a_slot_shared_by_different_episodes_comes_apart() {
    let (enricher, db, _dir) = harness().await;
    const OTHER_AID: u32 = 3545;
    // The contested slot, and a real episode 1 that must not be disturbed.
    episode(&db, "own1", Some(0), 1, AID, "S1").await;
    episode(&db, "pt3a", Some(0), 23, OTHER_AID, "1").await;
    sqlx::query("UPDATE ed2k_aid SET eid = 39483 WHERE ed2k = 'hash-pt3a'")
        .execute(&db)
        .await
        .unwrap();
    extra_source(&db, "pt3a", "pt3b", OTHER_AID, 39484, "2").await;

    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();

    assert_eq!(moves.len(), 1, "exactly the second file moves: {moves:?}");
    let (a_season, a_ep, a_id) = slot_of(&db, "pt3a.mkv").await;
    let (b_season, b_ep, b_id) = slot_of(&db, "pt3b.mkv").await;
    assert_ne!(a_id, b_id, "different episodes must not share an item");
    assert_eq!(
        (a_season, a_ep),
        (Some(0), 23),
        "the lower eid keeps the slot"
    );
    assert_eq!(b_season, Some(0), "the other stays in the season it was in");
    assert!(
        b_ep > 23,
        "on a free number, not on top of an existing one: {b_ep}"
    );
    // The show's own episode is untouched — this pass moves nothing else.
    assert_eq!(slot_of(&db, "own1.mkv").await.1, 1);
}

/// Two rips of the SAME episode share an eid, and sharing an item is
/// what they are supposed to do. The count of sources is not the test.
#[tokio::test]
async fn two_copies_of_one_episode_stay_together() {
    let (enricher, db, _dir) = harness().await;
    const OTHER_AID: u32 = 3545;
    episode(&db, "dup", Some(0), 23, OTHER_AID, "1").await;
    sqlx::query("UPDATE ed2k_aid SET eid = 39483 WHERE ed2k = 'hash-dup'")
        .execute(&db)
        .await
        .unwrap();
    extra_source(&db, "dup", "dup720", OTHER_AID, 39483, "1").await;

    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();

    assert!(moves.is_empty(), "same eid, nothing to split: {moves:?}");
    assert_eq!(
        slot_of(&db, "dup.mkv").await.2,
        slot_of(&db, "dup720.mkv").await.2
    );
}

/// The same split on an ABSOLUTE-numbered show, which is the anime norm:
/// its episodes carry a NULL season (HUB-31), so the freed number has to
/// come from that same numbering — not from season 0, where "the first
/// free number" is 1 and a real episode 1 already sits.
///
/// The live database taught this one. The first cut parked the file at
/// season 0 episode 1, beside a real absolute episode 1, because the
/// fixture only ever used season 0.
#[tokio::test]
async fn a_split_on_an_absolute_numbered_show_stays_absolute() {
    let (enricher, db, _dir) = harness().await;
    const OTHER_AID: u32 = 3545;
    episode(&db, "abs1", None, 1, AID, "1").await;
    episode(&db, "abs23", None, 23, OTHER_AID, "1").await;
    sqlx::query("UPDATE ed2k_aid SET eid = 39483 WHERE ed2k = 'hash-abs23'")
        .execute(&db)
        .await
        .unwrap();
    extra_source(&db, "abs23", "abs23b", OTHER_AID, 39484, "2").await;

    let moves = enricher
        .bind_hashed_episodes(&db, "show", AID)
        .await
        .unwrap();

    assert_eq!(moves.len(), 1, "{moves:?}");
    let (season, ep, id) = slot_of(&db, "abs23b.mkv").await;
    assert_eq!(season, None, "an absolute show has no season to park in");
    assert_eq!(ep, 24, "the next free absolute number, past the 23 in use");
    assert_ne!(id, slot_of(&db, "abs23.mkv").await.2);
    // The show's real episode 1 is untouched.
    assert_eq!(
        slot_of(&db, "abs1.mkv").await,
        (None, 1, "abs1".to_string())
    );
}
