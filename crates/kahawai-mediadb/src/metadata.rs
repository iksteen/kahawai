//! Provider descriptions and child catalogues are separate answers. Description
//! resolution merges absent fields; child resolution selects one whole catalogue
//! using the same local/primary/supplement order. Provider positions are evidence
//! for attaching descriptions to physical entries, never a source of library IDs.
//! `children_json` preserves missing versus explicitly empty answers. Migration 5
//! moves existing records and cached enrichment answers without refetching providers
//! or scheduling artwork work for a structural change.
use crate::*;
use anyhow::{Result, ensure};
use sqlx::{Row, SqliteConnection};
use std::collections::BTreeMap;

impl Store {
    pub async fn put_provider_record(&self, record: &ProviderRecord) -> Result<String> {
        ensure!(
            !record.provider.is_empty()
                && !record.namespace.is_empty()
                && !record.external_id.is_empty(),
            "empty provider identity"
        );
        let mut tx = self.db.begin().await?;
        let record_id = put_record(&mut tx, record).await?;
        tx.commit().await?;
        Ok(record_id)
    }
    /// Replace one copy's primary identity. Supplying the same record is a no-op.
    pub async fn assign_metadata(&self, item: &str, record: Option<&str>) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let kind: String = sqlx::query_scalar("SELECT c.media_type FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?")
            .bind(item).fetch_one(&mut *tx).await?;
        if let Some(record) = record {
            compatible(&mut tx, record, &kind).await?;
        }
        let old: Option<String> =
            sqlx::query_scalar("SELECT record_id FROM metadata_assignments WHERE item_id=?")
                .bind(item)
                .fetch_optional(&mut *tx)
                .await?;
        if old.as_deref() != record {
            sqlx::query("DELETE FROM metadata_assignments WHERE item_id=?")
                .bind(item)
                .execute(&mut *tx)
                .await?;
            if let Some(record) = record {
                sqlx::query("INSERT INTO metadata_assignments(item_id,record_id) VALUES(?,?)")
                    .bind(item)
                    .bind(record)
                    .execute(&mut *tx)
                    .await?;
            }
            crate::library::rebind(&mut tx, item).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Explicit evidence association, never an inferred title-based provider match.
    /// At most one supplemental answer from each provider is eligible per copy.
    pub async fn set_supplements(&self, item: &str, records: &[String]) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (primary, kind): (String,String)=sqlx::query_as("SELECT a.record_id,c.media_type FROM metadata_assignments a JOIN collection_items i ON i.id=a.item_id JOIN collections c ON c.id=i.collection_id WHERE a.item_id=?")
            .bind(item).fetch_one(&mut *tx).await?;
        let primary_provider: String =
            sqlx::query_scalar("SELECT provider FROM provider_records WHERE id=?")
                .bind(&primary)
                .fetch_one(&mut *tx)
                .await?;
        let mut providers = std::collections::HashSet::from([primary_provider]);
        sqlx::query("DELETE FROM metadata_supplements WHERE item_id=?")
            .bind(item)
            .execute(&mut *tx)
            .await?;
        for record in records {
            compatible(&mut tx, record, &kind).await?;
            let provider: String =
                sqlx::query_scalar("SELECT provider FROM provider_records WHERE id=?")
                    .bind(record)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(
                providers.insert(provider),
                "duplicate supplemental provider"
            );
            sqlx::query("INSERT INTO metadata_supplements VALUES(?,?,?)")
                .bind(item)
                .bind(&primary)
                .bind(record)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn set_provider_order(
        &self,
        media_type: MediaType,
        providers: &[String],
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM provider_order WHERE media_type=?")
            .bind(media_type.as_str())
            .execute(&mut *tx)
            .await?;
        for (position, provider) in providers.iter().enumerate() {
            ensure!(!provider.is_empty(), "empty provider name");
            sqlx::query("INSERT INTO provider_order VALUES(?,?,?)")
                .bind(media_type.as_str())
                .bind(provider)
                .bind(position as i64)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn resolve_metadata(&self, item: &str) -> Result<ResolvedDescription> {
        let mut c = self.db.read_pool().acquire().await?;
        resolve(&mut c, item).await
    }
}
pub(crate) async fn compatible(c: &mut SqliteConnection, record: &str, kind: &str) -> Result<()> {
    let actual: String = sqlx::query_scalar("SELECT media_type FROM provider_records WHERE id=?")
        .bind(record)
        .fetch_one(c)
        .await?;
    ensure!(
        actual == kind || (kind == "anime" && matches!(actual.as_str(), "movies" | "series")),
        "provider and collection media types differ"
    );
    Ok(())
}
// Local, assigned and supplemental evidence use one eligibility/order rule for
// descriptions and child catalogues. Child lists are selected whole, never merged.
async fn evidence(c: &mut SqliteConnection, item: &str) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
    Ok(sqlx::query("SELECT p.id,p.provider,p.description_json,p.children_json,-1 AS position FROM local_metadata l JOIN provider_records p ON p.id=l.record_id WHERE l.item_id=?1
        AND NOT EXISTS(SELECT 1 FROM metadata_assignments a WHERE a.item_id=l.item_id AND a.manual=1 AND a.record_id<>l.record_id)
        AND NOT EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=l.item_id AND r.record_id=l.record_id)
        UNION ALL SELECT p.id,p.provider,p.description_json,p.children_json,0 AS position FROM metadata_assignments a JOIN provider_records p ON p.id=a.record_id WHERE a.item_id=?1
        UNION ALL SELECT p.id,p.provider,p.description_json,p.children_json,o.position+1 FROM metadata_supplements s JOIN provider_records p ON p.id=s.record_id
        JOIN collection_items i ON i.id=s.item_id JOIN collections col ON col.id=i.collection_id
        JOIN provider_order o ON o.media_type=col.media_type AND o.provider=p.provider WHERE s.item_id=?1 ORDER BY position")
        .bind(item).fetch_all(&mut *c).await?)
}

pub(crate) struct ChildCatalogue {
    pub record_id: String,
    pub provider: String,
    pub children: Vec<ProviderChild>,
}
pub(crate) async fn resolve_children(
    c: &mut SqliteConnection,
    item: &str,
) -> Result<Option<ChildCatalogue>> {
    for row in evidence(c, item).await? {
        if let Some(json) = row.get::<Option<&str>, _>("children_json") {
            return Ok(Some(ChildCatalogue {
                record_id: row.get("id"),
                provider: row.get("provider"),
                children: serde_json::from_str(json)?,
            }));
        }
    }
    Ok(None)
}
pub(crate) async fn resolve(c: &mut SqliteConnection, item: &str) -> Result<ResolvedDescription> {
    let rows = evidence(c, item).await?;
    let mut result = ResolvedDescription {
        description: Description::default(),
        provenance: BTreeMap::new(),
        providers: BTreeMap::new(),
    };
    if rows.is_empty() {
        let json: String =
            sqlx::query_scalar("SELECT description_json FROM collection_items WHERE id=?")
                .bind(item)
                .fetch_one(&mut *c)
                .await?;
        fill(&mut result, serde_json::from_str(&json)?, "detected");
    } else {
        for row in rows {
            fill(
                &mut result,
                serde_json::from_str(row.get::<&str, _>("description_json"))?,
                row.get("id"),
            );
            let record: &str = row.get("id");
            // The primary supplies the displayed title/year even when every
            // descriptive field comes from local metadata or supplements.
            if row.get::<i64, _>("position") == 0
                || result.provenance.values().any(|id| id == record)
            {
                result.providers.insert(record.into(), row.get("provider"));
            }
        }
    }
    Ok(result)
}
fn fill(target: &mut ResolvedDescription, source: Description, record: &str) {
    macro_rules! fields { ($($name:ident),*) => { $(if target.description.$name.is_none() && source.$name.is_some() {
        target.description.$name=source.$name; target.provenance.insert(stringify!($name).into(),record.into());
    })* }; }
    fields!(
        overview,
        original_title,
        original_language,
        release_date,
        rating,
        artwork,
        genres,
        cast
    );
}

impl Store {
    pub async fn provider_record(&self, id: &str) -> Result<ProviderRecord> {
        let row = sqlx::query("SELECT * FROM provider_records WHERE id=?")
            .bind(id)
            .fetch_one(self.db.read_pool())
            .await?;
        record(&row)
    }
}

pub(crate) fn record(row: &sqlx::sqlite::SqliteRow) -> Result<ProviderRecord> {
    Ok(ProviderRecord {
        children: row
            .get::<Option<&str>, _>("children_json")
            .map(serde_json::from_str)
            .transpose()?,
        provider: row.get("provider"),
        namespace: row.get("namespace"),
        external_id: row.get("external_id"),
        language: row.get("language"),
        media_type: MediaType::parse(row.get("media_type"))?,
        title: row.get("title"),
        year: row.get("year"),
        description: serde_json::from_str(row.get("description_json"))?,
    })
}

pub(crate) async fn put_record(
    c: &mut SqliteConnection,
    record: &ProviderRecord,
) -> Result<String> {
    let existing: Option<(String,String,String,Option<i32>)> = sqlx::query_as("SELECT id,media_type,title,year FROM provider_records WHERE provider=? AND namespace=? AND external_id=? AND language=?")
            .bind(&record.provider).bind(&record.namespace).bind(&record.external_id).bind(&record.language).fetch_optional(&mut *c).await?;
    if let Some((_, kind, _, _)) = &existing {
        ensure!(
            kind == record.media_type.as_str(),
            "provider record changed media type"
        );
    }
    let identity_changed = existing
        .as_ref()
        .is_some_and(|r| title_key(&r.2) != title_key(&record.title) || r.3 != record.year);
    let record_id = existing.map(|r| r.0).unwrap_or_else(id);
    sqlx::query("INSERT INTO provider_records(id,provider,namespace,external_id,language,media_type,title,year,description_json,children_json) VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,year=excluded.year,description_json=excluded.description_json,children_json=excluded.children_json")
            .bind(&record_id).bind(&record.provider).bind(&record.namespace).bind(&record.external_id).bind(&record.language)
            .bind(record.media_type.as_str()).bind(&record.title).bind(record.year).bind(serde_json::to_string(&record.description)?)
            .bind(record.children.as_ref().map(serde_json::to_string).transpose()?)
            .execute(&mut *c).await?;
    if identity_changed {
        let copies: Vec<String> =
            sqlx::query_scalar("SELECT item_id FROM metadata_assignments WHERE record_id=?")
                .bind(&record_id)
                .fetch_all(&mut *c)
                .await?;
        for copy in copies {
            crate::library::rebind(c, &copy).await?;
        }
    }
    Ok(record_id)
}
