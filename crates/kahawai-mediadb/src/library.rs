use crate::*;
use anyhow::{Result, ensure};
use sqlx::{Connection, Row};

impl Store {
    pub async fn create_library(
        &self,
        name: &str,
        kind: MediaType,
        collections: &[String],
    ) -> Result<String> {
        let library = id();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO libraries VALUES(?,?,?)")
            .bind(&library)
            .bind(name)
            .bind(kind.as_str())
            .execute(&mut *tx)
            .await?;
        set_collections(&mut tx, &library, kind.as_str(), collections).await?;
        tx.commit().await?;
        Ok(library)
    }
    pub async fn set_library_collections(
        &self,
        library: &str,
        collections: &[String],
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let kind: String = sqlx::query_scalar("SELECT media_type FROM libraries WHERE id=?")
            .bind(library)
            .fetch_one(&mut *tx)
            .await?;
        set_collections(&mut tx, library, &kind, collections).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn remove_library(&self, library: &str) -> Result<()> {
        sqlx::query("DELETE FROM libraries WHERE id=?")
            .bind(library)
            .execute(&self.db)
            .await?;
        Ok(())
    }
    /// Page identities before descriptions, keeping membership and metadata in
    /// one read snapshot. Library visibility depends on its accessible copies;
    /// global archival depends on whether any copy still references the item.
    pub async fn browse(&self, library: &str, offset: u32, limit: u32) -> Result<Vec<LibraryItem>> {
        ensure!(limit > 0, "page size must be positive");
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let kind: String = sqlx::query_scalar("SELECT media_type FROM libraries WHERE id=?")
            .bind(library)
            .fetch_one(&mut *tx)
            .await?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT w.id FROM library_items w WHERE w.media_type=? AND EXISTS(
                SELECT 1 FROM collection_items i JOIN library_collections lc ON lc.collection_id=i.collection_id
                WHERE i.library_item_id=w.id AND lc.library_id=?)
             ORDER BY w.title_key,w.year,w.id LIMIT ? OFFSET ?")
            .bind(kind).bind(library).bind(i64::from(limit)).bind(i64::from(offset))
            .fetch_all(&mut *tx).await?;
        let mut out = Vec::with_capacity(ids.len());
        for item in ids {
            out.push(read_item(&mut tx, library, &item).await?);
        }
        tx.commit().await?;
        Ok(out)
    }
    pub async fn library_item(&self, library: &str, item: &str) -> Result<LibraryItem> {
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let result = read_item(&mut tx, library, item).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn library_item_record(&self, item: &str) -> Result<LibraryItemRecord> {
        let row = sqlx::query("SELECT w.*,NOT EXISTS(SELECT 1 FROM collection_items i WHERE i.library_item_id=w.id) AS archived FROM library_items w WHERE w.id=?")
            .bind(item).fetch_one(self.db.read_pool()).await?;
        Ok(LibraryItemRecord {
            id: row.get("id"),
            media_type: MediaType::parse(row.get("media_type"))?,
            title: row.get("title"),
            year: row.get("year"),
            archived: row.get("archived"),
        })
    }
}

/// The only identity allocation operation. Call inside the copy mutation's
/// transaction; selected metadata supplies the identity as a whole, otherwise
/// use the incoming detected values. Matching includes archived rows and never
/// changes an old key.
pub(crate) async fn identity(
    c: &mut sqlx::SqliteConnection,
    copy: &str,
    kind: &str,
    detected_title: &str,
    detected_year: Option<i32>,
) -> Result<String> {
    let selected: Option<(String, Option<i32>)> = sqlx::query_as(
        "SELECT p.title,p.year FROM metadata_assignments a JOIN provider_records p ON p.id=a.record_id WHERE a.item_id=?")
        .bind(copy).fetch_optional(&mut *c).await?;
    let (title, year) = selected
        .as_ref()
        .map(|(title, year)| (title.as_str(), *year))
        .unwrap_or((detected_title, detected_year));
    let key = title_key(title);
    let singleton = if kind == "music" || key.is_empty() || year.is_none() {
        copy
    } else {
        ""
    };
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM library_items WHERE media_type=? AND title_key=? AND year IS ? AND singleton=?")
        .bind(kind).bind(&key).bind(year).bind(singleton).fetch_optional(&mut *c).await?;
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let item = id();
    sqlx::query("INSERT INTO library_items(id,media_type,title,title_key,year,singleton) VALUES(?,?,?,?,?,?)")
        .bind(&item).bind(kind).bind(title).bind(key).bind(year).bind(singleton).execute(c).await?;
    Ok(item)
}

/// Effective identity is selected as a whole; a missing provider year never
/// falls back to the detected year. Used after primary assignments/records change.
pub(crate) async fn rebind(c: &mut sqlx::SqliteConnection, copy: &str) -> Result<()> {
    let row = sqlx::query(
        "SELECT col.media_type,i.title,i.year FROM collection_items i
        JOIN collections col ON col.id=i.collection_id WHERE i.id=?",
    )
    .bind(copy)
    .fetch_one(&mut *c)
    .await?;
    let item = identity(
        c,
        copy,
        row.get("media_type"),
        row.get("title"),
        row.get("year"),
    )
    .await?;
    sqlx::query("UPDATE collection_items SET library_item_id=? WHERE id=? AND library_item_id<>?")
        .bind(&item)
        .bind(copy)
        .bind(&item)
        .execute(c)
        .await?;
    Ok(())
}

async fn set_collections(
    c: &mut sqlx::SqliteConnection,
    library: &str,
    kind: &str,
    collections: &[String],
) -> Result<()> {
    sqlx::query("DELETE FROM library_collections WHERE library_id=?")
        .bind(library)
        .execute(&mut *c)
        .await?;
    for (pos, collection) in collections.iter().enumerate() {
        sqlx::query("INSERT INTO library_collections VALUES(?,?,?,?)")
            .bind(library)
            .bind(collection)
            .bind(kind)
            .bind(pos as i64)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}
async fn read_item(
    c: &mut sqlx::SqliteConnection,
    library: &str,
    item: &str,
) -> Result<LibraryItem> {
    let rows = sqlx::query(
        "SELECT i.id,
        CASE WHEN a.record_id IS NULL THEN i.title ELSE p.title END AS title,
        CASE WHEN a.record_id IS NULL THEN i.year ELSE p.year END AS year
        FROM collection_items i JOIN library_collections lc ON lc.collection_id=i.collection_id
        LEFT JOIN metadata_assignments a ON a.item_id=i.id
        LEFT JOIN provider_records p ON p.id=a.record_id
        WHERE i.library_item_id=? AND lc.library_id=?
        ORDER BY a.record_id IS NULL,lc.position,i.id",
    )
    .bind(item)
    .bind(library)
    .fetch_all(&mut *c)
    .await?;
    let first = rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("library item has no accessible copy"))?;
    let representative_id: String = first.get("id");
    Ok(LibraryItem {
        id: item.into(),
        title: first.get("title"),
        year: first.get("year"),
        metadata: crate::metadata::resolve(c, &representative_id).await?,
        representative_id,
        copy_ids: rows.iter().map(|r| r.get("id")).collect(),
    })
}

impl Store {
    pub async fn libraries(&self) -> Result<Vec<Library>> {
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let rows = sqlx::query("SELECT * FROM libraries ORDER BY name,id")
            .fetch_all(&mut *tx)
            .await?;
        let mut libraries = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.get("id");
            let collection_ids=sqlx::query_scalar("SELECT collection_id FROM library_collections WHERE library_id=? ORDER BY position")
                .bind(&id).fetch_all(&mut *tx).await?;
            libraries.push(Library {
                id,
                name: row.get("name"),
                media_type: MediaType::parse(row.get("media_type"))?,
                collection_ids,
            });
        }
        tx.commit().await?;
        Ok(libraries)
    }
}
