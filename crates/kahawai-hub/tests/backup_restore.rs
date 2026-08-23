//! OPS-5: a snapshot must rebuild the hub somewhere else, and the point
//! of it is that satellites reconnect on the certificates they already
//! hold. A backup that loses the CA turns a restore into a re-enrolment
//! of every machine, which is the failure worth testing for.

use sha2::{Digest, Sha256};
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
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type)
                 VALUES('fixture','default','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('i1','movie','Solaris','solaris','fixture','default')",
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
    assert_eq!(m.format, 3, "artifact inventory needs format 3");
    assert!(
        m.has_pki,
        "a snapshot without the CA cannot reconnect satellites"
    );
    assert!(m.has_config);
    assert_eq!(m.subtitle_files, 1);
    assert!(m.db_bytes > 0);
    assert_eq!(
        m.artifacts
            .iter()
            .map(|artifact| artifact.path.as_str())
            .collect::<Vec<_>>(),
        [
            "hub.db",
            "jwt.secret",
            "kahawai.toml",
            "pki/ca.crt",
            "pki/ca.key",
            "subtitles/abc/def.srt",
        ],
        "the manifest must list every included regular file in path order"
    );
    for artifact in &m.artifacts {
        let bytes = std::fs::read(snap.join(&artifact.path)).unwrap();
        assert_eq!(artifact.bytes, bytes.len() as u64, "{}", artifact.path);
        assert_eq!(
            artifact.sha256,
            data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes)),
            "{}",
            artifact.path
        );
    }

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
    let restored = kahawai_hub::backup::restore(&snap, fresh.path(), false)
        .await
        .unwrap();
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
async fn a_format_three_restore_rejects_size_and_hash_damage_before_live_state() {
    let live = tempfile::tempdir().unwrap();
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    let snap = tempfile::tempdir().unwrap().keep().join("snapshot");
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();
    let original = std::fs::read(snap.join("hub.db")).unwrap();

    let into = tempfile::tempdir().unwrap();
    write(&into.path().join("hub.db"), "standing database");
    write(&into.path().join("hub.db-wal"), "standing wal");

    let mut damaged = original.clone();
    damaged[0] ^= 1;
    std::fs::write(snap.join("hub.db"), damaged).unwrap();
    let error = kahawai_hub::backup::restore(&snap, into.path(), true)
        .await
        .expect_err("same-size corruption passed the manifest check");
    assert!(format!("{error:#}").contains("SHA-256"), "{error:#}");

    let mut larger = original;
    larger.push(0);
    std::fs::write(snap.join("hub.db"), larger).unwrap();
    let error = kahawai_hub::backup::restore(&snap, into.path(), true)
        .await
        .expect_err("wrong-size artifact passed the manifest check");
    assert!(format!("{error:#}").contains("bytes"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db")).unwrap(),
        "standing database"
    );
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db-wal")).unwrap(),
        "standing wal"
    );
}

#[tokio::test]
async fn a_format_three_restore_rejects_unsafe_and_duplicate_artifact_paths() {
    let live = tempfile::tempdir().unwrap();
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    let snap = tempfile::tempdir().unwrap().keep().join("snapshot");
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();
    let manifest_path = snap.join("kahawai-backup.json");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();

    let mut unsafe_path = original.clone();
    unsafe_path["artifacts"][0]["path"] = "../hub.db".into();
    let mut duplicate = original;
    let repeated = duplicate["artifacts"][0].clone();
    duplicate["artifacts"]
        .as_array_mut()
        .unwrap()
        .push(repeated);

    let into = tempfile::tempdir().unwrap();
    write(&into.path().join("hub.db"), "standing database");
    for (manifest, expected) in [
        (unsafe_path, "unsafe snapshot artifact path"),
        (duplicate, "more than once"),
    ] {
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let error = kahawai_hub::backup::restore(&snap, into.path(), true)
            .await
            .expect_err("malformed artifact inventory was accepted");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(into.path().join("hub.db")).unwrap(),
            "standing database"
        );
    }
}

#[tokio::test]
async fn a_format_two_snapshot_without_artifacts_remains_restorable() {
    let live = tempfile::tempdir().unwrap();
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    let snap = tempfile::tempdir().unwrap().keep().join("snapshot");
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();
    let manifest_path = snap.join("kahawai-backup.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["format"] = 2.into();
    manifest.as_object_mut().unwrap().remove("artifacts");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let into = tempfile::tempdir().unwrap();
    let restored = kahawai_hub::backup::restore(&snap, into.path(), false)
        .await
        .expect("format-2 snapshots predate the artifact inventory");
    assert_eq!(restored.format, 2);
    assert!(restored.artifacts.is_empty());
    kahawai_hub::db::open(into.path())
        .await
        .unwrap()
        .close()
        .await;
}

/// A snapshot is every password hash and every session, sitting in a
/// directory somebody chose. The umask it was taken under is not something to
/// depend on.
#[tokio::test]
async fn a_snapshot_is_not_readable_by_anyone_else() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    db.close().await;
    let dest = tempfile::tempdir().unwrap().keep().join("snapshot");

    // Under the umask that gives SQLite its usual 0644.
    let previous = unsafe { libc::umask(0o022) };
    let taken = kahawai_hub::backup::backup(dir.path(), None, &dest).await;
    unsafe { libc::umask(previous) };
    taken.unwrap();

    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(dest.clone()),
        0o700,
        "the snapshot directory is enterable"
    );
    assert_eq!(
        mode(dest.join("hub.db")),
        0o600,
        "the snapshot database is readable by anyone with the path"
    );
}

/// A restored hub that stops answering its scraper says nothing about why, so
/// the token travels with the snapshot like the session secret does.
#[tokio::test]
async fn a_snapshot_carries_the_metrics_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    db.close().await;
    for name in kahawai_hub::backup::SECRET_FILES {
        std::fs::write(dir.path().join(name), format!("secret-{name}")).unwrap();
    }
    let dest = tempfile::tempdir().unwrap().keep().join("snapshot");
    kahawai_hub::backup::backup(dir.path(), None, &dest)
        .await
        .unwrap();

    let into = tempfile::tempdir().unwrap();
    kahawai_hub::backup::restore(&dest, into.path(), true)
        .await
        .unwrap();
    for name in kahawai_hub::backup::SECRET_FILES {
        assert_eq!(
            std::fs::read_to_string(into.path().join(name)).unwrap(),
            format!("secret-{name}"),
            "{name} did not survive the round trip"
        );
    }
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
    let err = kahawai_hub::backup::restore(&snap, live.path(), false)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("--force"), "{err:#}");
    kahawai_hub::backup::restore(&snap, live.path(), true)
        .await
        .expect("--force restores");
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
    kahawai_hub::backup::restore(&snap, restored.path(), false)
        .await
        .unwrap();
    let db2 = kahawai_hub::db::open(restored.path()).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&db2)
        .await
        .unwrap();
    assert!(n <= 200, "snapshot cannot hold more than was written: {n}");
    db2.close().await;
    db.close().await;
}

/// A restored hub must be able to open what it restored.
///
/// The credential key is the whole of that: without it every sealed value is
/// unreadable, and because the database remembers seeding a key the hub
/// refuses to start rather than minting a replacement.
/// The manifest records which secrets a snapshot carries, so one that lost a
/// file between being taken and being restored says so — instead of restoring
/// a hub that starts and cannot open a single credential.
#[tokio::test]
async fn a_snapshot_missing_what_its_manifest_lists_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    db.close().await;
    for name in kahawai_hub::backup::SECRET_FILES {
        std::fs::write(dir.path().join(name), b"pretend-secret").unwrap();
    }
    let dest = tempfile::tempdir().unwrap().keep().join("snapshot");
    let manifest = kahawai_hub::backup::backup(dir.path(), None, &dest)
        .await
        .unwrap();
    assert_eq!(
        manifest.secrets,
        kahawai_hub::backup::SECRET_FILES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "the manifest does not say what it carries"
    );

    // Somebody's rsync excluded a dotless file, or a tar dropped one.
    std::fs::remove_file(dest.join(kahawai_hub::secrets::KEY_FILE)).unwrap();
    let into = tempfile::tempdir().unwrap();
    write(&into.path().join("hub.db"), "standing database");
    write(&into.path().join("hub.db-wal"), "standing wal");
    let e = kahawai_hub::backup::restore(&dest, into.path(), true)
        .await
        .expect_err("a snapshot missing its credential key was restored");
    assert!(
        format!("{e:#}").contains(kahawai_hub::secrets::KEY_FILE),
        "{e:#}"
    );
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db")).unwrap(),
        "standing database",
        "restore rejected the snapshot only after replacing the live database"
    );
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db-wal")).unwrap(),
        "standing wal",
        "restore removed the live journal before validating the snapshot"
    );
}

#[tokio::test]
async fn a_restore_refuses_a_key_that_does_not_match_the_snapshot_database() {
    let live = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    kahawai_hub::secrets::Secrets::load_or_create(live.path(), &db)
        .await
        .unwrap();
    db.close().await;
    let snap = tempfile::tempdir().unwrap().keep().join("snapshot");
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();

    let key_path = snap.join(kahawai_hub::secrets::KEY_FILE);
    let mut wrong_key = std::fs::read(&key_path).unwrap();
    wrong_key[0] ^= 1;
    std::fs::write(&key_path, &wrong_key).unwrap();
    let manifest_path = snap.join("kahawai-backup.json");
    let mut manifest: kahawai_hub::backup::Manifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == kahawai_hub::secrets::KEY_FILE)
        .unwrap()
        .sha256 = data_encoding::HEXLOWER.encode(&Sha256::digest(&wrong_key));
    std::fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let into = tempfile::tempdir().unwrap();
    write(&into.path().join("hub.db"), "standing database");
    write(&into.path().join("hub.db-wal"), "standing wal");
    write(
        &into.path().join(kahawai_hub::secrets::KEY_FILE),
        "standing key",
    );
    let error = kahawai_hub::backup::restore(&snap, into.path(), true)
        .await
        .expect_err("a snapshot with the wrong credential key was restored");
    assert!(format!("{error:#}").contains("does not match"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db")).unwrap(),
        "standing database"
    );
    assert_eq!(
        std::fs::read_to_string(into.path().join("hub.db-wal")).unwrap(),
        "standing wal"
    );
    assert_eq!(
        std::fs::read_to_string(into.path().join(kahawai_hub::secrets::KEY_FILE)).unwrap(),
        "standing key"
    );
}

/// A marker in the database means the key is not optional. Silently omitting
/// it produces a backup that reports success and can never start after restore.
#[tokio::test]
async fn a_backup_refuses_a_seeded_database_without_its_key() {
    let live = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    kahawai_hub::secrets::Secrets::load_or_create(live.path(), &db)
        .await
        .unwrap();
    std::fs::remove_file(live.path().join(kahawai_hub::secrets::KEY_FILE)).unwrap();
    db.close().await;
    let dest = tempfile::tempdir().unwrap().keep().join("snapshot");

    let error = kahawai_hub::backup::backup(live.path(), None, &dest)
        .await
        .expect_err("an unusable backup reported success");
    assert!(
        format!("{error:#}").contains(kahawai_hub::secrets::KEY_FILE),
        "{error:#}"
    );
    assert!(
        !dest.exists(),
        "backup created a snapshot before checking its required key"
    );
}

#[tokio::test]
async fn a_backup_refuses_a_key_that_does_not_match_its_database() {
    let live = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    kahawai_hub::secrets::Secrets::load_or_create(live.path(), &db)
        .await
        .unwrap();
    let key_path = live.path().join(kahawai_hub::secrets::KEY_FILE);
    let mut wrong_key = std::fs::read(&key_path).unwrap();
    wrong_key[0] ^= 1;
    std::fs::write(&key_path, wrong_key).unwrap();
    db.close().await;
    let dest = tempfile::tempdir().unwrap().keep().join("snapshot");

    let error = kahawai_hub::backup::backup(live.path(), None, &dest)
        .await
        .expect_err("an unusable backup reported success");
    assert!(format!("{error:#}").contains("does not match"), "{error:#}");
    assert!(
        !dest.exists(),
        "backup created a snapshot before validating its credential key"
    );
}
#[tokio::test]
async fn a_snapshot_carries_the_credential_key() {
    let live = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let snap = snap.path().join("snapshot");

    let db = kahawai_hub::db::open(live.path()).await.unwrap();
    let secrets = kahawai_hub::secrets::Secrets::load_or_create(live.path(), &db)
        .await
        .unwrap();
    let sealed = secrets.seal("", "tmdb", "api_key", "operator-key").unwrap();
    db.close().await;

    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();

    let fresh = tempfile::tempdir().unwrap();
    kahawai_hub::backup::restore(&snap, fresh.path(), false)
        .await
        .unwrap();

    let key = fresh.path().join(kahawai_hub::secrets::KEY_FILE);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600,
        "a restored key wider than the one it came from is a new leak"
    );
    let db = kahawai_hub::db::open(fresh.path()).await.unwrap();
    let restored = kahawai_hub::secrets::Secrets::load_or_create(fresh.path(), &db)
        .await
        .unwrap();
    assert_eq!(
        restored.open("", "tmdb", "api_key", &sealed).unwrap(),
        "operator-key",
        "the key travelled but does not open what it sealed"
    );
}

#[tokio::test]
async fn a_restore_replaces_a_standing_key() {
    let live = tempfile::tempdir().unwrap();
    let snap = tempfile::tempdir().unwrap();
    let snap = snap.path().join("snapshot");
    kahawai_hub::db::open(live.path())
        .await
        .unwrap()
        .close()
        .await;
    write(
        &live.path().join(kahawai_hub::secrets::KEY_FILE),
        "the-snapshot-key-32-bytes-long!!",
    );
    kahawai_hub::backup::backup(live.path(), None, &snap)
        .await
        .unwrap();

    let onto = tempfile::tempdir().unwrap();
    kahawai_hub::db::open(onto.path())
        .await
        .unwrap()
        .close()
        .await;
    write(
        &onto.path().join(kahawai_hub::secrets::KEY_FILE),
        "the-standing-key-32-bytes-long!!",
    );
    kahawai_hub::backup::restore(&snap, onto.path(), true)
        .await
        .unwrap();

    let key = onto.path().join(kahawai_hub::secrets::KEY_FILE);
    assert_eq!(
        std::fs::read_to_string(&key).unwrap(),
        "the-snapshot-key-32-bytes-long!!",
        "the restored database's credentials are sealed under the snapshot's key"
    );
    // The snapshot's own mode is whatever it came back from tar or an object
    // store with; the live copy is not allowed to inherit it.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600,
        "a restored key wider than 0600 undoes what creating it restricted"
    );
}
