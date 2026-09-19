//! Open/migrate a mediadb copy and read every library through the current Store.
//! Hub users and watch state are deliberately outside this catalogue audit.
use anyhow::{Context, Result, ensure};

#[tokio::main]
async fn main() -> Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .context("usage: library_audit DATA_DIRECTORY (containing mediadb.db)")?;
    let started = std::time::Instant::now();
    let store =
        kahawai_mediadb::Store::open(&std::path::Path::new(&directory).join("mediadb.db")).await?;
    let open_seconds = started.elapsed().as_secs_f64();
    let mut libraries = Vec::new();
    for library in store.libraries().await? {
        let mut offset = 0;
        loop {
            let (items, total) = store
                .browse_page(&library.id, offset, 200, "", "title", None)
                .await?;
            ensure!(
                items.iter().all(|item| !item.copy_ids.is_empty()),
                "active item without copies in {}",
                library.id
            );
            offset += items.len() as u32;
            if i64::from(offset) >= total {
                break;
            }
            ensure!(
                !items.is_empty(),
                "empty page before library total in {}",
                library.id
            );
        }
        libraries.push(serde_json::json!({"id":library.id,"name":library.name,"items":offset}));
    }
    println!(
        "{}",
        serde_json::json!({"open_seconds":open_seconds,"libraries":libraries})
    );
    store.close().await;
    Ok(())
}
