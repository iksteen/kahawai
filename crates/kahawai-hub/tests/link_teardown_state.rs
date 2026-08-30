//! What a disconnect is allowed to forget.
//!
//! `set_disabled` persists the admin's drain to `satellites` on purpose, with
//! the stated reason that "a drained box must not rejoin because the hub
//! bounced". Clearing the in-memory set inside `unregister_link` undid that for
//! something far more common than a restart — any disconnect at all — after
//! which the row said drained and the live hub said enabled.

use std::sync::Arc;

use kahawai_hub::registry::Registry;

async fn enrolled() -> (Arc<Registry>, kahawai_sqlite::Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at)
         VALUES ('01MH', 'mediahost', 'nas', 'fp', 1)",
    )
    .execute(&db)
    .await
    .unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    (reg, db, dir)
}

/// What the admin panel would show for `01TC`.
async fn shown_as_disabled(reg: &Registry) -> bool {
    reg.satellites_overview()
        .await
        .unwrap()
        .into_iter()
        .find(|satellite| satellite.module_id == "01MH")
        .expect("the satellite is enrolled")
        .disabled
}

#[tokio::test]
async fn a_disconnect_does_not_undrain_a_satellite() {
    let (reg, db, _dir) = enrolled().await;
    reg.set_disabled("01MH", true).await.unwrap();
    assert!(shown_as_disabled(&reg).await, "the drain is in effect");
    let row: i64 = sqlx::query_scalar("SELECT disabled FROM satellites WHERE module_id = '01MH'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(row, 1, "and persisted");

    // The link drops — a restart of the box, a network blip, anything.
    reg.unregister_link("01MH");

    assert!(
        shown_as_disabled(&reg).await,
        "a disconnect must not undo the operator's drain: the row still says \
         disabled, so reporting it as enabled makes the hub disagree with \
         itself until it is restarted"
    );
    let row_after: i64 =
        sqlx::query_scalar("SELECT disabled FROM satellites WHERE module_id = '01MH'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row_after, 1, "and nothing wrote the row either");
}

/// ...but deleting the satellite does forget it.
///
/// This used to fall out of `unregister_link` clearing the set as a side
/// effect. Removing that side effect left `delete_satellite` clearing `links`
/// and `connected` and not the drain, so a re-enrolled module id came back
/// drained in memory while its fresh row said enabled — placement would skip a
/// box the operator had just re-added.
#[tokio::test]
async fn deleting_a_satellite_forgets_its_drain() {
    let (reg, db, _dir) = enrolled().await;
    reg.set_disabled("01MH", true).await.unwrap();
    assert!(shown_as_disabled(&reg).await);
    let (link_tx, _link_rx) = tokio::sync::mpsc::channel(1);
    let (generation, _) = reg.register_link(
        "01MH",
        link_tx,
        kahawai_proto::PROTOCOL_MINOR,
        kahawai_core::segments::DETECTOR_GENERATION,
    );

    let deleted = reg.delete_satellite("01MH").await.unwrap();
    assert_eq!(
        deleted.mediahost_link_generation,
        Some(generation),
        "the composition layer needs the exact generation to cancel its waiter"
    );
    // Same module id enrolled again, a fresh row with the toggle off.
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at)
         VALUES ('01MH', 'mediahost', 'nas', 'fp2', 2)",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(
        !shown_as_disabled(&reg).await,
        "a re-enrolled satellite starts undrained; a leftover entry would have \
         placement quietly skipping a box the operator just added back"
    );
}

/// The same rule for transcoders, which is where it was missing.
///
/// A transcoder that dies without a FIN sits out a 35-second heartbeat window.
/// If the box reconnects inside it, the old task's teardown used to clear by
/// module id and delete the LIVE connection's sender, its load accounting and
/// its capabilities — and capabilities are sent once per connection, so
/// placement (which requires both a link and caps) never chose that box again
/// until the transcoder process itself restarted.
#[tokio::test]
async fn a_replaced_transcoder_link_is_left_alone() {
    let (reg, _db, _dir) = enrolled().await;
    let (old_tx, _old_rx) = tokio::sync::mpsc::channel(4);
    let (new_tx, _new_rx) = tokio::sync::mpsc::channel(4);

    reg.register_tc_link("01TC", 4, old_tx.clone());
    // The box comes back and registers afresh while the old task is still
    // sitting in its heartbeat window.
    reg.register_tc_link("01TC", 5, new_tx.clone());
    assert!(reg.transcoder_supports_layout_gains("01TC"));

    assert!(
        !reg.unregister_tc_link_if_current("01TC", &old_tx),
        "the dead task must not claim a link it no longer owns"
    );
    assert!(
        reg.transcoder_supports_layout_gains("01TC"),
        "old teardown must not remove the live link's protocol capabilities"
    );
    // The live one is still registered — which is the whole point, and is
    // observable precisely because ITS teardown still succeeds.
    assert!(
        reg.unregister_tc_link_if_current("01TC", &new_tx),
        "the reconnected link must still be the registered one"
    );
    // And now nothing owns it.
    assert!(!reg.unregister_tc_link_if_current("01TC", &new_tx));
}
