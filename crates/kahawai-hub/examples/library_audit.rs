//! Open/migrate a hub database and check library invariants without contacting
//! satellites or metadata providers. Use a database copy for upgrade rehearsals.
use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .context("usage: library_audit DATA_DIRECTORY")?;
    let started = std::time::Instant::now();
    let db = kahawai_hub::db::open(std::path::Path::new(&directory)).await?;
    let elapsed = started.elapsed().as_secs_f64();
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM library_pending")
        .fetch_one(&db)
        .await?;
    let unassigned:i64=sqlx::query_scalar("SELECT COUNT(*) FROM collection_items i WHERE NOT EXISTS(SELECT 1 FROM collection_item_library_items a WHERE a.collection_item_id=i.id)").fetch_one(&db).await?;
    let incompatible:i64=sqlx::query_scalar("SELECT COUNT(*) FROM collection_item_library_items a JOIN collection_items i ON i.id=a.collection_item_id JOIN library_items c ON c.id=a.library_item_id WHERE c.merged_into IS NOT NULL OR c.kind<>CASE i.kind WHEN 'show' THEN 'series' WHEN 'track' THEN 'song' ELSE i.kind END").fetch_one(&db).await?;
    let kinds: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind,COUNT(*) FROM library_items WHERE merged_into IS NULL GROUP BY kind",
    )
    .fetch_all(&db)
    .await?;
    anyhow::ensure!(
        pending == 0 && unassigned == 0 && incompatible == 0,
        "library invariant failure: pending={pending}, unassigned={unassigned}, incompatible={incompatible}"
    );
    println!(
        "{}",
        serde_json::json!({"open_seconds":elapsed,"kinds":kinds,"pending":pending,"unassigned":unassigned,"incompatible":incompatible})
    );
    db.close().await;
    Ok(())
}
