//! OPS-5: a snapshot must rebuild the hub somewhere else, and the point
//! of it is that satellites reconnect on the certificates they already
//! hold. A backup that loses the CA turns a restore into a re-enrolment
//! of every machine, which is the failure worth testing for.

use std::path::Path;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[tokio::test]
async fn a_snapshot_restores_the_database_pki_and_subtitles() {
    let live = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let snap = snap.path().join("snapshot"); // backup() insists it not exist

    // A hub with state worth keeping: a user, a satellite's fingerprint,
    // the CA that admits it, a downloaded subtitle, and a token secret.
    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title) VALUES ('i1','movie','Solaris','solaris')",
    )
    .execute(&db)
    .await
    .unwrap();
    db.close().await;
    write(&live.path().join("pki/ca.crt"), "CERTIFICATE");
    write(&live.path().join("pki/ca.key"), "PRIVATE KEY");
    write(
        &live.path().join("subtitles/abc/def.srt"),
        "1\n00:00:01,000 --> 00:00:02,000\nhi\n",
    );
    write(&live.path().join("jwt.secret"), "s3cret");
    // Caches, which must NOT travel: re-derivable, and far larger.
    write(&live.path().join("artwork/deadbeef"), "JPEG");
    write(&live.path().join("anime/anime-titles.dat.gz"), "gzip");

    let cfg = live.path().join("kahawai.toml");
    write(&cfg, "[hub]\nbind = \"127.0.0.1:8420\"\n");

    let m = kahawai_hub::backup::backup(live.path(), Some(&cfg), &snap)
        .await
        .unwrap();
    assert!(
        m.has_pki,
        "a snapshot without the CA cannot reconnect satellites"
    );
    assert!(m.has_config);
    assert_eq!(m.subtitle_files, 1);
    assert!(m.db_bytes > 0);

    // The exclusions are the point of the requirement, not an oversight.
    assert!(
        !snap.join("artwork").exists(),
        "the image cache is re-derivable"
    );
    assert!(
        !snap.join("anime").exists(),
        "provider caches are re-derivable"
    );

    // Restore onto a FRESH install, as the requirement words it.
    let fresh = tempfile::tempdir().unwrap();
    let restored = kahawai_hub::backup::restore(&snap, fresh.path(), false).unwrap();
    assert_eq!(restored.kahawai_version, m.kahawai_version);
    assert_eq!(
        std::fs::read_to_string(fresh.path().join("pki/ca.key")).unwrap(),
        "PRIVATE KEY",
        "the CA is what lets existing satellites back in"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.path().join("jwt.secret")).unwrap(),
        "s3cret"
    );
    assert!(fresh.path().join("subtitles/abc/def.srt").exists());

    // And the database is a working one, not just bytes.
    let db = kahawai_hub::db::open(fresh.path()).await.unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM items WHERE id = 'i1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "Solaris");
    db.close().await;
}

#[tokio::test]
async fn a_restore_refuses_to_overwrite_without_being_told() {
    let live = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let snap = snap.path().join("snapshot");
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();

    // The data dir already holds a database — quietly replacing it while
    // a hub might be running is how you lose the state you meant to keep.
    let err = kahawai_hub::backup::restore(&snap, live.path(), false).unwrap_err();
    assert!(format!("{err:#}").contains("--force"), "{err:#}");
    kahawai_hub::backup::restore(&snap, live.path(), true).expect("--force restores");
}

#[tokio::test]
async fn backup_refuses_an_existing_destination() {
    let live = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    // Merging into a directory would blend two snapshots into one that
    // never existed.
    let err = kahawai_hub::backup::backup(live.path(), None, dest.path())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
}

#[tokio::test]
async fn a_snapshot_is_taken_while_the_hub_keeps_writing() {
    // "Online" is the requirement's word: VACUUM INTO must not need the
    // database to itself. Writes continue on the same pool throughout.
    let live = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let snap = snap.path().join("snapshot");
    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    let writer = {
        let db = db.clone();
        tokio::spawn(async move {
            for n in 0..200 {
                let _ = sqlx::query(
                    "INSERT INTO items (id, kind, title, norm_title) VALUES (?, 'movie', ?, ?)",
                )
                .bind(format!("w{n}"))
                .bind(format!("Film {n}"))
                .bind(format!("film {n}"))
                .execute(&db)
                .await;
            }
        })
    };
    let m = kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();
    writer.await.unwrap();
    assert!(m.db_bytes > 0);

    // The snapshot is consistent: it opens, and whatever it caught is a
    // real point in time rather than a torn page.
    let restored = tempfile::tempdir().unwrap();
    kahawai_hub::backup::restore(&snap, restored.path(), false).unwrap();
    let db2 = kahawai_hub::db::open(restored.path()).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&db2)
        .await
        .unwrap();
    assert!(n <= 200, "snapshot cannot hold more than was written: {n}");
    db2.close().await;
    db.close().await;
}
