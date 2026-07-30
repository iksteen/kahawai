//! SEC-7: renewal keeps a satellite admitted through the whole overlap —
//! new fingerprint admitted before issuance, old retired only once the
//! renewed cert is actually used (or the grace lapses).

use kahawai_hub::registry::Registry;
use kahawai_transport::mtls::AllowedCerts;
use sqlx::Row;

async fn setup(dir: &std::path::Path) -> (Registry, AllowedCerts) {
    let db = kahawai_hub::db::open(dir).await.unwrap();
    let allowed = AllowedCerts::default();
    let reg = Registry::new(db, allowed.clone());
    reg.record_satellite("01MH", "mediahost", "nas", "fp-old")
        .await
        .unwrap();
    (reg, allowed)
}

#[tokio::test]
async fn renewal_admits_new_before_old_is_retired() {
    let dir = tempfile::tempdir().unwrap();
    let (reg, allowed) = setup(dir.path()).await;
    assert!(allowed.contains("fp-old"));

    // Issue: both fingerprints admitted — no lockout window.
    reg.record_renewal("01MH", "fp-new").await.unwrap();
    assert!(allowed.contains("fp-old"));
    assert!(allowed.contains("fp-new"));

    // Reconnect on the OLD cert within grace: nothing changes.
    reg.settle_renewal("01MH", "fp-old").await.unwrap();
    assert!(allowed.contains("fp-old"));
    assert!(allowed.contains("fp-new"));

    // Reconnect with the NEW cert: old fingerprint retired.
    reg.settle_renewal("01MH", "fp-new").await.unwrap();
    assert!(!allowed.contains("fp-old"));
    assert!(allowed.contains("fp-new"));
    let row = sqlx::query("SELECT cert_fingerprint, pending_fingerprint FROM satellites")
        .fetch_one(reg.db())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("cert_fingerprint"), "fp-new");
    assert!(
        row.get::<Option<String>, _>("pending_fingerprint")
            .is_none()
    );
}

#[tokio::test]
async fn retried_renewal_supersedes_the_pending_one() {
    let dir = tempfile::tempdir().unwrap();
    let (reg, allowed) = setup(dir.path()).await;

    reg.record_renewal("01MH", "fp-lost").await.unwrap();
    reg.record_renewal("01MH", "fp-new").await.unwrap();
    assert!(allowed.contains("fp-old"));
    assert!(
        !allowed.contains("fp-lost"),
        "superseded pending must be evicted"
    );
    assert!(allowed.contains("fp-new"));
}

#[tokio::test]
async fn lapsed_grace_retires_the_unused_renewal() {
    let dir = tempfile::tempdir().unwrap();
    let (reg, allowed) = setup(dir.path()).await;

    reg.record_renewal("01MH", "fp-new").await.unwrap();
    sqlx::query("UPDATE satellites SET pending_issued_at = unixepoch() - 90000")
        .execute(reg.db())
        .await
        .unwrap();

    // Satellite comes back on the old cert after the grace: the renewal
    // evidently never landed — retire it; the old cert stays admitted.
    reg.settle_renewal("01MH", "fp-old").await.unwrap();
    assert!(allowed.contains("fp-old"));
    assert!(!allowed.contains("fp-new"));
    let pending: Option<Option<String>> =
        sqlx::query_scalar("SELECT pending_fingerprint FROM satellites")
            .fetch_optional(reg.db())
            .await
            .unwrap();
    assert_eq!(pending, Some(None));
}

#[tokio::test]
async fn startup_load_admits_pending_within_grace_and_sweeps_lapsed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (reg, _) = setup(dir.path()).await;
        reg.record_renewal("01MH", "fp-fresh").await.unwrap();
        reg.record_satellite("01TC", "transcoder", "gpu", "fp-tc")
            .await
            .unwrap();
        reg.record_renewal("01TC", "fp-stale").await.unwrap();
        sqlx::query("UPDATE satellites SET pending_issued_at = unixepoch() - 90000 WHERE module_id = '01TC'")
            .execute(reg.db())
            .await
            .unwrap();
    }

    // Hub restart.
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let allowed = AllowedCerts::default();
    let reg = Registry::new(db, allowed.clone());
    reg.load_allowlist().await.unwrap();
    assert!(allowed.contains("fp-old"));
    assert!(
        allowed.contains("fp-fresh"),
        "pending within grace stays admitted"
    );
    assert!(allowed.contains("fp-tc"));
    assert!(
        !allowed.contains("fp-stale"),
        "lapsed pending swept at startup"
    );
}
