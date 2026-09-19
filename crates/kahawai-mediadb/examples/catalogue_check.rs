//! Runnable, separate-process persistence proof and reproducible scale fixture.
#[path = "../tests/common/mod.rs"]
mod common;
use anyhow::{Context, Result, ensure};
use kahawai_mediadb::*;
use sqlx::{Connection, Row};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args
        .get(1)
        .context("usage: catalogue_check create|migrate|seed|verify|archive|verify-archive|resurrect|verify-restored|scale DATABASE")?;
    let path = Path::new(args.get(2).context("missing database path")?);
    match mode.as_str() {
        "create" | "migrate" => {
            let s = if mode == "create" {
                Store::create(path).await?
            } else {
                Store::open(path).await?
            };
            s.close().await;
            verify_migrations(path).await?;
        }
        "seed" => seed(path).await?,
        "verify" => verify(path).await?,
        "archive" => archive(path).await?,
        "verify-archive" => verify_lifecycle(path, true).await?,
        "resurrect" => resurrect(path).await?,
        "verify-restored" => verify_lifecycle(path, false).await?,
        "scale" => scale(path).await?,
        _ => anyhow::bail!("unknown mode"),
    }
    Ok(())
}
async fn seed(path: &Path) -> Result<()> {
    let s = Store::create(path).await?;
    s.put_mediahost("host", "Persistence fixture").await?;
    let c = common::collection(
        &s,
        "fixture",
        MediaType::Movies,
        &["Film.2000.CD1.mkv", "Film.2000.CD2.mkv", "Other.2001.mkv"],
    )
    .await;
    let items = s.collection_items(&c).await?;
    let film = items
        .iter()
        .find(|i| i.detected.title == "Film")
        .context("missing film")?;
    let mut p = common::record(
        "tmdb",
        "fixture",
        "Assigned film",
        Some(2002),
        MediaType::Movies,
    );
    p.description.overview = Some("Durable description".into());
    let record = s.put_provider_record(&p).await?;
    s.assign_metadata(&film.id, Some(&record)).await?;
    s.create_library("Persistence fixture", MediaType::Movies, &[c])
        .await?;
    s.close().await;
    let ids = stored_ids(path).await?;
    std::fs::write(
        path.with_extension("identities.json"),
        serde_json::to_vec(&ids)?,
    )?;
    println!("seed committed and database closed: {}", path.display());
    Ok(())
}
async fn verify(path: &Path) -> Result<()> {
    // Read the actual rows independently, then exercise the public operations.
    let mut c = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true),
    )
    .await?;
    verify_migrations(path).await?;
    let collection: String =
        sqlx::query_scalar("SELECT id FROM collections WHERE remote_id='fixture'")
            .fetch_one(&mut c)
            .await?;
    let library: String =
        sqlx::query_scalar("SELECT id FROM libraries WHERE name='Persistence fixture'")
            .fetch_one(&mut c)
            .await?;
    let assignment: String = sqlx::query_scalar("SELECT item_id FROM metadata_assignments")
        .fetch_one(&mut c)
        .await?;
    let parts=sqlx::query("SELECT ordinal FROM media_parts p JOIN media_entries e ON e.id=p.entry_id WHERE e.item_id=? ORDER BY ordinal").bind(&assignment).fetch_all(&mut c).await?;
    ensure!(
        parts
            .iter()
            .map(|p| p.get::<i64, _>("ordinal"))
            .collect::<Vec<_>>()
            == [1, 2],
        "persisted parts changed"
    );
    c.close().await?;
    let s = Store::open(path).await?;
    ensure!(
        s.collections("host").await?[0].id == collection,
        "collection cannot be rediscovered"
    );
    let libraries = s.libraries().await?;
    ensure!(
        libraries.len() == 1
            && libraries[0].id == library
            && libraries[0].collection_ids == [collection.clone()],
        "library composition did not persist"
    );
    ensure!(
        s.roots(&collection).await?[0].active,
        "root did not persist"
    );
    let item = s
        .collection_items(&collection)
        .await?
        .into_iter()
        .find(|item| item.id == assignment)
        .context("missing assigned occurrence")?;
    ensure!(
        s.provider_record(
            item.selected_record
                .as_deref()
                .context("missing provider assignment")?
        )
        .await?
        .external_id
            == "fixture",
        "provider record did not persist"
    );
    ensure!(
        s.catalogue_cursor(&collection).await?.version == 3,
        "cursor did not persist"
    );
    let page = s.browse(&library, 0, 100).await?;
    ensure!(page.len() == 2, "wrong persisted groups");
    let movie = page
        .iter()
        .find(|g| g.representative_id == assignment)
        .context("assignment disappeared")?;
    ensure!(
        movie.title == "Assigned film" && movie.year == Some(2002),
        "wrong assigned identity"
    );
    ensure!(
        movie.metadata.description.overview.as_deref() == Some("Durable description"),
        "description did not persist"
    );
    ensure!(s.files(&collection).await?.len() == 3, "source loss");
    s.close().await;
    println!(
        "verified in a new process: schema, cursor, assignment, description, ordered parts, files, browse"
    );
    Ok(())
}
async fn verify_migrations(path: &Path) -> Result<()> {
    let mut c = reader(path).await?;
    let rows =
        sqlx::query("SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut c)
            .await?;
    let expected = sqlx::migrate!("./migrations");
    ensure!(
        rows.len() == expected.iter().len(),
        "missing or unknown migration history"
    );
    for (row, migration) in rows.iter().zip(expected.iter()) {
        ensure!(
            row.get::<i64, _>("version") == migration.version
                && row.get::<bool, _>("success")
                && row.get::<Vec<u8>, _>("checksum").as_slice() == migration.checksum.as_ref(),
            "migration history differs from embedded migrations"
        );
    }
    println!(
        "verified migration history through version {}: {}",
        rows.last()
            .context("empty migration history")?
            .get::<i64, _>("version"),
        path.display()
    );
    c.close().await?;
    Ok(())
}
async fn reader(path: &Path) -> Result<sqlx::SqliteConnection> {
    Ok(sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true),
    )
    .await?)
}
async fn stored_ids(path: &Path) -> Result<Vec<String>> {
    let mut c = reader(path).await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM library_items ORDER BY id")
            .fetch_all(&mut c)
            .await?,
    )
}
async fn archive(path: &Path) -> Result<()> {
    let s = Store::open(path).await?;
    let collection = s.collections("host").await?.remove(0).id;
    s.remove_collection(&collection).await?;
    s.close().await;
    println!("last copies removed and database closed");
    Ok(())
}
async fn resurrect(path: &Path) -> Result<()> {
    let s = Store::open(path).await?;
    // Current detected identities bring back all three rows, without restoring
    // the deleted copy's old primary assignment or relying on its old ID/path.
    let c = common::collection(
        &s,
        "fixture",
        MediaType::Movies,
        &[
            "Film.2000.CD1.mkv",
            "Film.2000.CD2.mkv",
            "Other.2001.mkv",
            "Assigned.film.2002.mkv",
        ],
    )
    .await;
    let library = s.libraries().await?.remove(0).id;
    s.set_library_collections(&library, &[c]).await?;
    s.close().await;
    println!("current identities reimported and database closed");
    Ok(())
}
async fn verify_lifecycle(path: &Path, archived: bool) -> Result<()> {
    let saved: Vec<String> =
        serde_json::from_slice(&std::fs::read(path.with_extension("identities.json"))?)?;
    ensure!(
        saved.len() == 3 && stored_ids(path).await? == saved,
        "persistent library IDs changed"
    );
    let mut c = reader(path).await?;
    let assignments: i64 = sqlx::query_scalar("SELECT count(*) FROM metadata_assignments")
        .fetch_one(&mut c)
        .await?;
    ensure!(assignments == 0, "deleted metadata assignment was restored");
    let provider_count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_records")
        .fetch_one(&mut c)
        .await?;
    ensure!(provider_count == 1, "provider evidence was lost");
    let copies: i64 = sqlx::query_scalar("SELECT count(*) FROM collection_items")
        .fetch_one(&mut c)
        .await?;
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&mut c)
        .await?;
    ensure!(copies == if archived { 0 } else { 3 }, "wrong copy count");
    ensure!(files == if archived { 0 } else { 4 }, "wrong source count");
    let empty: i64 = sqlx::query_scalar("SELECT count(*) FROM library_items w WHERE NOT EXISTS(SELECT 1 FROM collection_items i WHERE i.library_item_id=w.id)").fetch_one(&mut c).await?;
    ensure!(
        empty == if archived { 3 } else { 0 },
        "wrong derived archival state"
    );
    c.close().await?;
    let s = Store::open(path).await?;
    for id in saved {
        ensure!(
            s.library_item_record(&id).await?.archived == archived,
            "public archival state differs"
        );
    }
    let library = s.libraries().await?.remove(0).id;
    ensure!(
        s.browse(&library, 0, 100).await?.len() == if archived { 0 } else { 3 },
        "wrong visible item count"
    );
    s.close().await;
    println!(
        "verified in a new process: unchanged IDs, derived archival={archived}, sources, browse, no restored assignments"
    );
    Ok(())
}
async fn scale(path: &Path) -> Result<()> {
    let s = Store::create(path).await?;
    s.close().await;
    // Synthetic relational fixture: 50k title occurrences (half provider-assigned) and 250k physical
    // files, five parts each, plus 50k interleaved archived identities. Import correctness is covered by seed/verify and
    // integration tests; direct seeding keeps this a browse benchmark.
    let mut c = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await?;
    let mut tx = c.begin().await?;
    sqlx::raw_sql("INSERT INTO mediahosts VALUES('scale','Scale');
        INSERT INTO collections(id,mediahost_id,remote_id,media_type,epoch,version) VALUES('scale','scale','scale','movies','scale',250000);
        INSERT INTO collection_roots VALUES('scale','scale','scale','/scale',1);
        INSERT INTO libraries VALUES('scale','Scale','movies'); INSERT INTO library_collections VALUES('scale','scale','movies',0);
        WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<50000)
        INSERT INTO library_items(id,media_type,title,title_key,year,singleton)
        SELECT 'w'||x,'movies',printf('Movie %06d',x),printf('movie %06d',x),2000,'' FROM n;
        INSERT INTO collection_items(id,collection_id,root_id,occurrence,title,year,library_item_id,description_json)
        SELECT 'i'||substr(id,2),'scale','scale','copy'||id,title,year,id,'{}' FROM library_items;
        INSERT INTO provider_records(id,provider,namespace,external_id,language,media_type,title,year,description_json)
        SELECT 'p'||id,'tmdb','movie',id,'en','movies',title,year,'{\"overview\":\"Provider description\"}'
        FROM collection_items WHERE CAST(substr(id,2) AS INTEGER)%2=0;
        INSERT INTO metadata_assignments SELECT substr(id,2),id FROM provider_records;
        INSERT INTO media_entries(id,collection_id,item_id,occurrence,kind,title)
        SELECT 'e'||id,'scale',id,occurrence,'movie',title FROM collection_items;
        WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<250000)
        INSERT INTO files(id,collection_id,root_id,path,version,size,mtime,media_json)
        SELECT 'f'||x,'scale','scale','file'||x,1,123,456,'{}' FROM n;
        WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<250000)
        INSERT INTO media_parts SELECT 'ei'||(((x-1)/5)+1),'scale',((x-1)%5)+1,'f'||x FROM n;
        INSERT INTO library_items(id,media_type,title,title_key,year,singleton)
        SELECT 'archived-'||id,media_type,title||' archived',title_key||' archived',year,'' FROM library_items;").execute(&mut *tx).await?;
    tx.commit().await?;
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM library_items),
            (SELECT count(*) FROM collection_items),
            (SELECT count(*) FROM files),
            (SELECT count(*) FROM metadata_assignments)",
    )
    .fetch_one(&mut c)
    .await?;
    ensure!(
        counts == (100_000, 50_000, 250_000, 25_000),
        "wrong scale fixture counts: {counts:?}"
    );
    c.close().await?;
    let s = Store::open(path).await?;
    for offset in [0, 25000, 49900] {
        let started = std::time::Instant::now();
        let page = s.browse("scale", offset, 100).await?;
        let elapsed = started.elapsed();
        ensure!(page.len() == 100, "wrong scale page count");
        for (position, item) in page.iter().enumerate() {
            ensure!(
                item.id == format!("w{}", offset as usize + position + 1)
                    && item.copy_ids.len() == 1,
                "wrong scale page membership"
            );
        }
        println!(
            "50k active + 50k archived items / 25k assignments / 250k files: offset {offset}, 100 results, {} ms",
            elapsed.as_millis()
        );
        ensure!(
            elapsed <= std::time::Duration::from_millis(200),
            "browse exceeded the 200 ms target: {elapsed:?}"
        );
    }
    s.close().await;
    Ok(())
}
