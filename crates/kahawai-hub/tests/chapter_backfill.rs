//! Chapters ride the attachment declaration, and a file stays on
//! the worklist until somebody has actually answered for it.
//!
//! The pair matters. Two facts come off one walk of the container header,
//! so they share a worklist — which only works if the list keeps offering
//! a file whose OTHER fact is still missing.

use kahawai_hub::registry::Declared;
use sqlx::Row;

/// A declaration with no attachments — chapters are what these tests are
/// about, and the two ride together.
fn declared(chapters_json: Option<&str>) -> Declared<'_> {
    Declared {
        attachments_json: "[]",
        chapters_json,
    }
}

async fn library() -> (tempfile::TempDir, kahawai_hub::registry::Registry) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO collections(module_id,collection_id,media_type)
           VALUES('m','c','series');
         INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
           VALUES('m','c','r','/series');
         INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           SELECT 'm','c',id,'e.mkv',10,1,0,0,0,'{\"container\":\"matroska\",\"future_fact\":7}'
             FROM collection_roots;",
    )
    .execute(&db)
    .await
    .unwrap();
    let registry = kahawai_hub::registry::Registry::new(db, Default::default());
    (dir, registry)
}

async fn pending(registry: &kahawai_hub::registry::Registry) -> usize {
    registry.attachments_worklist("m", "c").await.unwrap().len()
}

async fn chapters(registry: &kahawai_hub::registry::Registry) -> Option<String> {
    sqlx::query("SELECT json_extract(streams_json,'$.chapters') AS c FROM files")
        .fetch_one(registry.db())
        .await
        .unwrap()
        .get("c")
}
async fn normalized(registry: &kahawai_hub::registry::Registry) -> (i64, i64) {
    sqlx::query_as("SELECT chapter_segment_kinds,chapter_segments_detector FROM files")
        .fetch_one(registry.db())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_file_leaves_the_worklist_once_both_are_declared() {
    let (_dir, registry) = library().await;
    assert_eq!(pending(&registry).await, 1, "never looked at");

    let stored = registry
        .record_file_attachments(
            "m",
            "c",
            "r",
            "e.mkv",
            10,
            declared(Some(r#"[{"start_ms":0,"end_ms":60000,"title":"Intro"}]"#)),
        )
        .await
        .unwrap();
    assert!(stored);
    assert_eq!(pending(&registry).await, 0);
    assert!(chapters(&registry).await.unwrap().contains("Intro"));
    assert_eq!(
        normalized(&registry).await,
        (
            kahawai_core::segments::NAMED_INTRO as i64,
            kahawai_core::segments::DETECTOR_GENERATION,
        )
    );
    let future_fact: i64 =
        sqlx::query_scalar("SELECT json_extract(streams_json,'$.future_fact') FROM files")
            .fetch_one(registry.db())
            .await
            .unwrap();
    assert_eq!(
        future_fact, 7,
        "json_set must preserve unknown source facts"
    );
}

#[tokio::test]
async fn a_host_that_says_nothing_about_chapters_does_not_settle_it() {
    // An older mediahost declares attachments and omits the field. Marking
    // the file done there would leave it without chapters for ever, since
    // nothing else revisits a file whose bytes have not changed.
    let (_dir, registry) = library().await;
    registry
        .record_file_attachments("m", "c", "r", "e.mkv", 10, declared(None))
        .await
        .unwrap();

    assert_eq!(chapters(&registry).await, None);
    assert_eq!(pending(&registry).await, 1, "still owed");
}

#[tokio::test]
async fn a_file_with_no_chapters_is_answered_for() {
    // "Looked, there are none" has to be storable, or every chapterless
    // file in the library comes back on every reconnect.
    let (_dir, registry) = library().await;
    registry
        .record_file_attachments("m", "c", "r", "e.mkv", 10, declared(Some("[]")))
        .await
        .unwrap();
    assert_eq!(chapters(&registry).await.as_deref(), Some("[]"));
    assert_eq!(pending(&registry).await, 0);
    assert_eq!(
        normalized(&registry).await,
        (0, kahawai_core::segments::DETECTOR_GENERATION)
    );
}

#[tokio::test]
async fn a_declaration_for_a_file_that_moved_on_is_dropped() {
    let (_dir, registry) = library().await;
    let stored = registry
        .record_file_attachments("m", "c", "r", "e.mkv", 999, declared(Some("[]")))
        .await
        .unwrap();
    assert!(!stored, "size guard: these are not the bytes we declared");
    assert_eq!(pending(&registry).await, 1);
}

#[tokio::test]
async fn malformed_chapters_are_refused_rather_than_stored() {
    let (_dir, registry) = library().await;
    let err = registry
        .record_file_attachments(
            "m",
            "c",
            "r",
            "e.mkv",
            10,
            declared(Some(r#"[{"start_ms":"x"}]"#)),
        )
        .await;
    assert!(err.is_err());
    assert_eq!(chapters(&registry).await, None);
}
