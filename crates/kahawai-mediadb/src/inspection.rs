//! Read-only catalogue summaries. SQL and physical ownership remain inside Store.
use crate::*;
use sqlx::Row;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CollectionSummary {
    pub collection: Collection,
    pub roots: Vec<Root>,
    pub file_count: i64,
    pub snapshot: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogueStats {
    pub items: i64,
    pub files: i64,
    pub file_bytes: i64,
    pub unassigned_copies: i64,
}

impl Store {
    pub async fn collection_summaries(&self) -> Result<Vec<CollectionSummary>> {
        let mut tx = self.db.read_pool().begin().await?;
        let rows = sqlx::query("SELECT c.*, (SELECT COUNT(*) FROM files f WHERE f.collection_id=c.id) AS file_count FROM collections c ORDER BY mediahost_id,remote_id")
            .fetch_all(&mut *tx).await?;
        let mut out = Vec::new();
        for r in rows {
            let id: String = r.get("id");
            let roots =
                sqlx::query("SELECT * FROM collection_roots WHERE collection_id=? ORDER BY token")
                    .bind(&id)
                    .fetch_all(&mut *tx)
                    .await?
                    .into_iter()
                    .map(|r| Root {
                        id: r.get("id"),
                        token: r.get("token"),
                        path: r.get("path"),
                        active: r.get("active"),
                    })
                    .collect();
            out.push(CollectionSummary {
                collection: Collection {
                    id,
                    mediahost_id: r.get("mediahost_id"),
                    remote_id: r.get("remote_id"),
                    media_type: MediaType::parse(r.get("media_type"))?,
                    epoch: r.get("epoch"),
                    version: r.get::<i64, _>("version") as u64,
                },
                roots,
                file_count: r.get("file_count"),
                snapshot: r.get("snapshot_active"),
            });
        }
        tx.commit().await?;
        Ok(out)
    }

    pub async fn stats(&self) -> Result<CatalogueStats> {
        let r = sqlx::query("SELECT
            (SELECT COUNT(*) FROM library_items w WHERE EXISTS(SELECT 1 FROM collection_items i WHERE i.library_item_id=w.id)) AS items,
            (SELECT COUNT(*) FROM files) AS files,
            (SELECT COALESCE(SUM(size),0) FROM files) AS file_bytes,
            (SELECT COUNT(*) FROM collection_items i WHERE NOT EXISTS(SELECT 1 FROM metadata_assignments a WHERE a.item_id=i.id)) AS unassigned")
            .fetch_one(self.db.read_pool()).await?;
        Ok(CatalogueStats {
            items: r.get("items"),
            files: r.get("files"),
            file_bytes: r.get("file_bytes"),
            unassigned_copies: r.get("unassigned"),
        })
    }
}
