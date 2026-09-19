//! Enrichment is stored work, not a catalogue projection. One assignment carries
//! both the selected identity and its manual pin. Candidates never select weak
//! matches; rejections survive retries. Every mutation invalidates captured work
//! through the copy's input revision. Completion and identity rebinding commit
//! together. Provider clients and clocks belong to the caller, not this crate.
//!
//! Each provider claims its own due jobs. Paused providers may resolve cached
//! answers; work needing the network is deferred without recording an attempt.
//! A parked provider owns no job claim;
//! other providers (and local metadata) remain independently runnable. Failed
//! requests are not cached misses. Completed questions and provider records are
//! retained: rebuilding them consumes provider budget, while reading metadata
//! must not wait for a provider. No eviction or repair-on-read is involved.
use crate::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnrichmentJob {
    pub force: bool,
    pub item_id: String,
    pub provider: String,
    pub revision: i64,
    pub token: String,
    pub attempts: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RecordLink {
    pub provider: String,
    pub namespace: String,
    pub external_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnrichmentCandidate {
    #[serde(default)]
    pub complete: bool,
    pub record: ProviderRecord,
    /// 0 = suggestion; 10 = confident name; 20 = content; 30 = local identity.
    pub strength: i32,
    #[serde(default)]
    pub links: Vec<RecordLink>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnrichmentAnswer {
    #[serde(default)]
    pub artist: Option<ArtistIdentity>,
    #[serde(default)]
    pub links: Vec<(String, RecordLink)>,
    #[serde(default)]
    pub refresh_at: Option<i64>,
    pub candidates: Vec<EnrichmentCandidate>,
    /// A local sidecar may describe a copy without claiming its identity.
    pub local: Option<ProviderRecord>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnrichmentSource {
    pub file_id: String,
    pub root_token: String,
    pub root_path: String,
    pub path: String,
    pub size: Option<i64>,
    #[schema(value_type=Object)]
    pub media: Option<kahawai_core::media::MediaInfo>,
    #[serde(skip)]
    pub hashes: Vec<kahawai_proto::v1::FileHash>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnrichmentInput {
    pub item_id: String,
    pub library_item_id: String,
    pub collection_id: String,
    pub mediahost_id: String,
    pub remote_id: String,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i32>,
    pub artist: Option<String>,
    pub revision: i64,
    pub selected: Option<(String, ProviderRecord)>,
    pub manual: bool,
    pub links: Vec<RecordLink>,
    pub sources: Vec<EnrichmentSource>,
}
/// A saved identity offered for manual matching, including items in other libraries.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct IdentityChoice {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReviewItem {
    pub id: String,
    pub root_id: String,
    pub occurrence: String,
    pub collection_id: String,
    pub title: String,
    pub year: Option<i32>,
    pub revision: i64,
    pub selected_title: Option<String>,
    pub manual: bool,
    pub review_needed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReviewCandidate {
    pub id: String,
    pub record: ProviderRecord,
    pub strength: i32,
    pub rejected: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProviderWorkStatus {
    pub provider: String,
    pub state: String,
    pub count: i64,
    pub due_at: i64,
    pub error: Option<String>,
}
#[derive(Debug)]
pub struct StaleEnrichment;
impl std::fmt::Display for StaleEnrichment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("metadata changed; reload and try again")
    }
}
impl std::error::Error for StaleEnrichment {}

fn record(row: &sqlx::sqlite::SqliteRow) -> Result<ProviderRecord> {
    Ok(ProviderRecord {
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

impl Store {
    pub async fn provider_order(&self, kind: MediaType) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT provider FROM provider_order WHERE media_type=? ORDER BY position",
        )
        .bind(kind.as_str())
        .fetch_all(self.db.read_pool())
        .await?)
    }
    pub async fn enrichment_input(&self, item: &str) -> Result<EnrichmentInput> {
        use sqlx::Connection;
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let row = sqlx::query("SELECT i.*,c.mediahost_id,c.remote_id,c.media_type,COALESCE(a.manual,0) AS pinned FROM collection_items i JOIN collections c ON c.id=i.collection_id LEFT JOIN metadata_assignments a ON a.item_id=i.id WHERE i.id=?")
            .bind(item).fetch_one(&mut *tx).await?;
        let selected = sqlx::query("SELECT p.* FROM metadata_assignments a JOIN provider_records p ON p.id=a.record_id WHERE a.item_id=?")
            .bind(item).fetch_optional(&mut *tx).await?.map(|r| Ok::<_,anyhow::Error>((r.get("id"),record(&r)?))).transpose()?;
        let links = sqlx::query("SELECT l.provider,l.namespace,l.external_id FROM provider_links l JOIN metadata_assignments a ON a.record_id=l.record_id WHERE a.item_id=?1 UNION SELECT p.provider,p.namespace,p.external_id FROM metadata_supplements s JOIN provider_records p ON p.id=s.record_id WHERE s.item_id=?1")
            .bind(item).fetch_all(&mut *tx).await?.into_iter().map(|r| RecordLink {provider:r.get("provider"),namespace:r.get("namespace"),external_id:r.get("external_id")}).collect();
        let rows = sqlx::query("SELECT f.*,r.token,r.path AS root_path FROM files f JOIN collection_roots r ON r.id=f.root_id JOIN media_parts p ON p.file_id=f.id JOIN media_entries e ON e.id=p.entry_id WHERE e.item_id=? ORDER BY f.path")
            .bind(item).fetch_all(&mut *tx).await?;
        let mut sources = Vec::new();
        for f in rows {
            let file_id: String = f.get("id");
            let payload: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT payload FROM source_facts WHERE file_id=? AND kind='file_hashes'",
            )
            .bind(&file_id)
            .fetch_optional(&mut *tx)
            .await?;
            let hashes = match payload {
                Some(p) => match SourceFact::decode("file_hashes", &p)? {
                    SourceFact::Hashes(h) => h.hashes,
                    _ => vec![],
                },
                None => vec![],
            };
            sources.push(EnrichmentSource {
                file_id,
                root_token: f.get("token"),
                root_path: f.get("root_path"),
                path: f.get("path"),
                size: f.get("size"),
                media: f
                    .get::<Option<&str>, _>("media_json")
                    .map(serde_json::from_str)
                    .transpose()?,
                hashes,
            });
        }
        let input = EnrichmentInput {
            item_id: item.into(),
            library_item_id: row.get("library_item_id"),
            collection_id: row.get("collection_id"),
            mediahost_id: row.get("mediahost_id"),
            remote_id: row.get("remote_id"),
            media_type: MediaType::parse(row.get("media_type"))?,
            title: row.get("title"),
            year: row.get("year"),
            artist: row.get("artist"),
            revision: row.get("enrichment_revision"),
            selected,
            manual: row.get("pinned"),
            links,
            sources,
        };
        tx.commit().await?;
        Ok(input)
    }
    pub async fn claim_enrichment(
        &self,
        provider: &str,
        now: i64,
        lease_seconds: i64,
    ) -> Result<Option<EnrichmentJob>> {
        ensure!(lease_seconds > 0, "lease must be positive");
        let mut tx = self.db.begin().await?;
        let row = sqlx::query("SELECT j.* FROM enrichment_jobs j JOIN collection_items i ON i.id=j.item_id JOIN collections col ON col.id=i.collection_id WHERE col.snapshot_active=0 AND j.provider=? AND j.revision=i.enrichment_revision AND (((j.state IN ('pending','retry') OR (j.state='done' AND j.due_at>0)) AND j.due_at<=?) OR (j.state='running' AND j.lease_until<=?)) AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id) ORDER BY j.due_at,j.item_id LIMIT 1")
            .bind(provider).bind(now).bind(now).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(None) };
        let job = EnrichmentJob {
            force: row.get("force"),
            item_id: row.get("item_id"),
            provider: provider.into(),
            revision: row.get("revision"),
            token: id(),
            attempts: row.get::<i64, _>("attempts") + 1,
        };
        sqlx::query("UPDATE enrichment_jobs SET state='running',token=?,lease_until=?,attempts=? WHERE item_id=? AND provider=?")
            .bind(&job.token).bind(now+lease_seconds).bind(job.attempts).bind(&job.item_id).bind(provider).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(job))
    }
    /// A cache miss during a provider pause made no request. Release the claim
    /// against the existing pause without inflating attempts or changing its
    /// deadline/error. Cached answers for other items remain runnable.
    pub async fn defer_enrichment(&self, job: &EnrichmentJob) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let pause: Option<(i64, bool, Option<String>)> = sqlx::query_as(
            "SELECT due_at,blocked,error FROM enrichment_providers WHERE provider=?",
        )
        .bind(&job.provider)
        .fetch_optional(&mut *tx)
        .await?;
        let (due_at, blocked, error) = pause.unwrap_or((0, false, None));
        let changed = sqlx::query("UPDATE enrichment_jobs SET state=?,due_at=?,error=?,attempts=max(0,attempts-1),token=NULL,lease_until=0 WHERE item_id=? AND provider=? AND token=?")
            .bind(if blocked { "blocked" } else { "retry" }).bind(due_at).bind(error)
            .bind(&job.item_id).bind(&job.provider).bind(&job.token)
            .execute(&mut *tx).await?.rows_affected();
        if changed > 0 {
            choose_automatic(&mut tx, &job.item_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Failure releases the claim. Provider-wide pauses are used for credentials
    /// and gate backoff; bad individual records delay only their own job.
    pub async fn fail_enrichment(
        &self,
        job: &EnrichmentJob,
        due_at: i64,
        error: &str,
        provider_wide: bool,
        blocked: bool,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let changed=sqlx::query("UPDATE enrichment_jobs SET state=?,due_at=?,error=?,token=NULL,lease_until=0 WHERE item_id=? AND provider=? AND token=?")
            .bind(if blocked {"blocked"} else {"retry"}).bind(due_at).bind(error).bind(&job.item_id).bind(&job.provider).bind(&job.token).execute(&mut *tx).await?.rows_affected();
        if provider_wide && changed > 0 {
            sqlx::query("INSERT INTO enrichment_providers VALUES(?,?,?,?) ON CONFLICT(provider) DO UPDATE SET due_at=excluded.due_at,blocked=excluded.blocked,error=excluded.error")
                .bind(&job.provider).bind(due_at).bind(blocked).bind(error).execute(&mut *tx).await?;
        }
        if changed > 0 {
            choose_automatic(&mut tx, &job.item_id).await?;
            if provider_wide {
                let waiting:Vec<String>=sqlx::query_scalar("SELECT DISTINCT ec.item_id FROM enrichment_candidates ec JOIN collection_items i ON i.id=ec.item_id JOIN enrichment_jobs j ON j.item_id=i.id WHERE j.provider=? AND ec.revision=i.enrichment_revision AND ec.strength>=10 AND NOT EXISTS(SELECT 1 FROM metadata_assignments WHERE item_id=i.id)").bind(&job.provider).fetch_all(&mut *tx).await?;
                for item in waiting {
                    choose_automatic(&mut tx, &item).await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn wake_enrichment(&self, provider: Option<&str>) -> Result<()> {
        let mut tx = self.db.begin().await?;
        // Explicit wake does not erase rate-limit backoff. A credential change
        // releases configuration blocks, not a provider's timed cooldown.
        sqlx::query("UPDATE enrichment_providers SET blocked=0,error=NULL WHERE (? IS NULL OR provider=?) AND blocked=1")
            .bind(provider).bind(provider).execute(&mut *tx).await?;
        sqlx::query("UPDATE enrichment_jobs SET state='pending',error=NULL WHERE state='blocked' AND (? IS NULL OR provider=?)")
            .bind(provider).bind(provider).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn finish_enrichment(
        &self,
        job: &EnrichmentJob,
        answer: &EnrichmentAnswer,
    ) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM enrichment_jobs j JOIN collection_items i ON i.id=j.item_id WHERE j.item_id=? AND j.provider=? AND j.token=? AND i.enrichment_revision=? AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id))")
            .bind(&job.item_id).bind(&job.provider).bind(&job.token).bind(job.revision).fetch_one(&mut *tx).await?;
        if !valid {
            return Ok(false);
        }
        let before: Option<String> =
            sqlx::query_scalar("SELECT record_id FROM metadata_assignments WHERE item_id=?")
                .bind(&job.item_id)
                .fetch_optional(&mut *tx)
                .await?;
        let kind:String=sqlx::query_scalar("SELECT c.media_type FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?").bind(&job.item_id).fetch_one(&mut *tx).await?;
        for candidate in &answer.candidates {
            ensure!(
                candidate.record.provider == job.provider,
                "provider answered as another provider"
            );
            let record_id = put_candidate(&mut tx, candidate).await?;
            crate::metadata::compatible(&mut tx, &record_id, &kind).await?;
            sqlx::query("INSERT INTO enrichment_candidates VALUES(?,?,?,?) ON CONFLICT(item_id,record_id) DO UPDATE SET revision=excluded.revision,strength=excluded.strength")
                .bind(&job.item_id).bind(&record_id).bind(job.revision).bind(candidate.strength).execute(&mut *tx).await?;
            for link in &candidate.links {
                sqlx::query("INSERT OR IGNORE INTO provider_links VALUES(?,?,?,?)")
                    .bind(&record_id)
                    .bind(&link.provider)
                    .bind(&link.namespace)
                    .bind(&link.external_id)
                    .execute(&mut *tx)
                    .await?;
            }
            let rejected: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM metadata_rejections WHERE item_id=? AND record_id=?)",
            )
            .bind(&job.item_id)
            .bind(&record_id)
            .fetch_one(&mut *tx)
            .await?;
            if rejected || candidate.strength == 0 {
                continue;
            }
            let old: Option<(String, bool, i32, String)> = sqlx::query_as(
                "SELECT a.record_id,a.manual,a.strength,p.provider FROM metadata_assignments a JOIN provider_records p ON p.id=a.record_id WHERE a.item_id=?",
            )
            .bind(&job.item_id)
            .fetch_optional(&mut *tx)
            .await?;
            if old.as_ref().map_or(
                candidate.strength >= 20,
                |(primary, pin, strength, provider)| {
                    // A corrected NFO replaces its previous automatic identity,
                    // even when both answers have the same authoritative strength.
                    // Replaying the same answer must not invalidate every job again.
                    !pin && (candidate.strength > *strength
                        || (job.provider == "local"
                            && provider == "local"
                            && candidate.strength >= 30
                            && primary != &record_id))
                },
            ) {
                replace_assignment(
                    &mut tx,
                    &job.item_id,
                    Some(&record_id),
                    false,
                    candidate.strength,
                )
                .await?;
            } else if let Some((primary, _, _, _)) = old {
                let linked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_links l JOIN provider_records p ON p.provider=l.provider AND p.namespace=l.namespace AND p.external_id=l.external_id WHERE (l.record_id=? AND p.id=?) OR (l.record_id=? AND p.id=?))")
                    .bind(&primary).bind(&record_id).bind(&record_id).bind(&primary).fetch_one(&mut *tx).await?;
                if linked && primary != record_id {
                    sqlx::query("DELETE FROM metadata_supplements WHERE item_id=? AND record_id IN (SELECT id FROM provider_records WHERE provider=?)")
                        .bind(&job.item_id).bind(&job.provider).execute(&mut *tx).await?;
                    sqlx::query("INSERT OR IGNORE INTO metadata_supplements VALUES(?,?,?)")
                        .bind(&job.item_id)
                        .bind(primary)
                        .bind(&record_id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        for (anchor, link) in &answer.links {
            let selected: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM metadata_assignments WHERE item_id=? AND record_id=?)",
            )
            .bind(&job.item_id)
            .bind(anchor)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(selected, "links require the captured selected record");
            let added = sqlx::query("INSERT OR IGNORE INTO provider_links VALUES(?,?,?,?)")
                .bind(anchor)
                .bind(&link.provider)
                .bind(&link.namespace)
                .bind(&link.external_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if added > 0 {
                sqlx::query("UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=? AND provider=? AND state='done'").bind(&job.item_id).bind(&link.provider).execute(&mut *tx).await?;
            }
        }
        if job.provider == "local" {
            sqlx::query("DELETE FROM local_metadata WHERE item_id=?")
                .bind(&job.item_id)
                .execute(&mut *tx)
                .await?;
            if let Some(local) = &answer.local {
                let record_id = crate::metadata::put_record(&mut tx, local).await?;
                sqlx::query("INSERT INTO local_metadata VALUES(?,?)")
                    .bind(&job.item_id)
                    .bind(record_id)
                    .execute(&mut *tx)
                    .await?;
            }
            if answer.candidates.iter().all(|c| c.strength < 30) {
                let selected_local:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM metadata_assignments a JOIN provider_records p ON p.id=a.record_id WHERE a.item_id=? AND a.manual=0 AND p.provider='local')").bind(&job.item_id).fetch_one(&mut *tx).await?;
                if selected_local {
                    replace_assignment(&mut tx, &job.item_id, None, false, 0).await?;
                }
            }
        }
        if let Some(artist) = &answer.artist {
            ensure!(
                job.provider == "musicbrainz",
                "artist identity must come from MusicBrainz"
            );
            sqlx::query(
                "INSERT INTO artists VALUES(?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            )
            .bind(&artist.id)
            .bind(&artist.name)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO collection_item_artists SELECT id,?,enrichment_revision FROM collection_items WHERE id=? ON CONFLICT(item_id) DO UPDATE SET artist_id=excluded.artist_id,revision=excluded.revision").bind(&artist.id).bind(&job.item_id).execute(&mut *tx).await?;
            sqlx::query("UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=? AND provider IN ('fanart','theaudiodb') AND state='done'").bind(&job.item_id).execute(&mut *tx).await?;
        }
        // Assignment invalidation may have replaced this job in the transaction.
        // Its answer is current; siblings must rerun against the new parent.
        let after: Option<String> =
            sqlx::query_scalar("SELECT record_id FROM metadata_assignments WHERE item_id=?")
                .bind(&job.item_id)
                .fetch_optional(&mut *tx)
                .await?;
        sqlx::query("UPDATE enrichment_jobs SET state=?,due_at=?,force=0,token=NULL,lease_until=0,error=NULL WHERE item_id=? AND provider=?")
            .bind(if before!=after {"pending"} else {"done"}).bind(if before!=after {0} else {answer.refresh_at.unwrap_or(0)}).bind(&job.item_id).bind(&job.provider).execute(&mut *tx).await?;
        if job.provider.ends_with("-artwork") || job.provider == "coverartarchive" {
            sqlx::query("UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=? AND provider='artist-collage' AND state='done'").bind(&job.item_id).execute(&mut *tx).await?;
        }
        if job.provider == "anidb-hash" {
            sqlx::query("UPDATE enrichment_jobs SET state='pending',due_at=0,token=NULL,lease_until=0 WHERE item_id=? AND provider='anidb'").bind(&job.item_id).execute(&mut *tx).await?;
        }
        choose_automatic(&mut tx, &job.item_id).await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn enrichment_status(&self) -> Result<Vec<ProviderWorkStatus>> {
        Ok(sqlx::query("WITH work AS (SELECT j.provider,CASE WHEN j.state='done' THEN 'done' WHEN p.blocked=1 THEN 'blocked' WHEN p.due_at>unixepoch() AND j.state<>'running' THEN 'retry' ELSE j.state END AS state,max(j.due_at,COALESCE(p.due_at,0)) AS due_at,COALESCE(p.error,j.error) AS error FROM enrichment_jobs j JOIN collection_items i ON i.id=j.item_id LEFT JOIN enrichment_providers p ON p.provider=j.provider WHERE EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id)) SELECT provider,state,count(*) AS n,min(due_at) AS due_at,max(error) AS error FROM work GROUP BY provider,state ORDER BY provider,state")
            .fetch_all(self.db.read_pool()).await?.into_iter().map(|r| ProviderWorkStatus {provider:r.get("provider"),state:r.get("state"),count:r.get("n"),due_at:r.get("due_at"),error:r.get("error")}).collect())
    }
    pub async fn enrichment_items(
        &self,
        library: Option<&str>,
        collection: Option<&str>,
        review_only: bool,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<ReviewItem>> {
        ensure!(limit > 0 && limit <= 1000, "invalid page size");
        Ok(sqlx::query("SELECT i.id,i.collection_id,i.root_id,i.occurrence,i.title,i.year,i.enrichment_revision,p.title AS selected_title,COALESCE(a.manual,0) AS manual,a.item_id IS NULL AND EXISTS(SELECT 1 FROM enrichment_candidates ec WHERE ec.item_id=i.id AND ec.strength=0 AND ec.revision=i.enrichment_revision AND NOT EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=i.id AND r.record_id=ec.record_id)) AS review_needed FROM collection_items i LEFT JOIN metadata_assignments a ON a.item_id=i.id LEFT JOIN provider_records p ON p.id=a.record_id WHERE EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id AND (?1 IS NULL OR lc.library_id=?1)) AND (?2 IS NULL OR i.collection_id=?2) AND (?3=0 OR (a.item_id IS NULL AND EXISTS(SELECT 1 FROM enrichment_candidates ec WHERE ec.item_id=i.id AND ec.strength=0 AND ec.revision=i.enrichment_revision AND NOT EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=i.id AND r.record_id=ec.record_id)))) AND (?6='' OR instr(lower(i.title),lower(?6))>0 OR instr(lower(p.title),lower(?6))>0 OR instr(lower(i.occurrence),lower(?6))>0) ORDER BY i.title,i.id LIMIT ?4 OFFSET ?5")
            .bind(library).bind(collection).bind(review_only).bind(limit).bind(offset).bind(query.trim()).fetch_all(self.db.read_pool()).await?.into_iter().map(|r| ReviewItem {id:r.get("id"),root_id:r.get("root_id"),occurrence:r.get("occurrence"),collection_id:r.get("collection_id"),title:r.get("title"),year:r.get("year"),revision:r.get("enrichment_revision"),selected_title:r.get("selected_title"),manual:r.get("manual"),review_needed:r.get("review_needed")}).collect())
    }
    pub async fn review_candidates(&self, item: &str) -> Result<Vec<ReviewCandidate>> {
        sqlx::query("SELECT p.*,c.strength,EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=c.item_id AND r.record_id=c.record_id) AS rejected FROM enrichment_candidates c JOIN provider_records p ON p.id=c.record_id JOIN collection_items i ON i.id=c.item_id WHERE c.item_id=? AND c.revision=i.enrichment_revision ORDER BY c.strength DESC,p.title")
            .bind(item).fetch_all(self.db.read_pool()).await?.into_iter().map(|r| Ok(ReviewCandidate {id:r.get("id"),record:record(&r)?,strength:r.get("strength"),rejected:r.get("rejected")})).collect()
    }
    pub async fn correct_metadata(
        &self,
        item: &str,
        revision: i64,
        action: &str,
        record_id: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let current: i64 =
            sqlx::query_scalar("SELECT enrichment_revision FROM collection_items WHERE id=?")
                .bind(item)
                .fetch_one(&mut *tx)
                .await?;
        if current != revision {
            return Err(StaleEnrichment.into());
        }
        match action {
            "pick" | "confirm" => {
                let r = record_id.context("record required")?;
                let offered:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM enrichment_candidates WHERE item_id=? AND record_id=? AND revision=?) OR EXISTS(SELECT 1 FROM metadata_assignments WHERE item_id=? AND record_id=?)").bind(item).bind(r).bind(revision).bind(item).bind(r).fetch_one(&mut *tx).await?;
                ensure!(offered, "record was not offered for this item");
                let kind:String=sqlx::query_scalar("SELECT c.media_type FROM collections c JOIN collection_items i ON i.collection_id=c.id WHERE i.id=?").bind(item).fetch_one(&mut *tx).await?;
                crate::metadata::compatible(&mut tx, r, &kind).await?;
                replace_assignment(&mut tx, item, Some(r), true, 100).await?;
            }
            "reject" => {
                let r = record_id.context("record required")?;
                let offered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM enrichment_candidates WHERE item_id=? AND record_id=?) OR EXISTS(SELECT 1 FROM metadata_assignments WHERE item_id=? AND record_id=?)")
                    .bind(item).bind(r).bind(item).bind(r).fetch_one(&mut *tx).await?;
                ensure!(offered, "record was not offered for this item");
                sqlx::query("INSERT OR IGNORE INTO metadata_rejections VALUES(?,?)")
                    .bind(item)
                    .bind(r)
                    .execute(&mut *tx)
                    .await?;
                let selected: Option<String> = sqlx::query_scalar(
                    "SELECT record_id FROM metadata_assignments WHERE item_id=?",
                )
                .bind(item)
                .fetch_optional(&mut *tx)
                .await?;
                if selected.as_deref() == Some(r) {
                    replace_assignment(&mut tx, item, None, false, 0).await?;
                } else {
                    sqlx::query("UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=?").bind(item).execute(&mut *tx).await?;
                }
            }
            "clear" => {
                replace_assignment(&mut tx, item, None, false, 0).await?;
            }
            "retry" => {
                sqlx::query("UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=?").bind(item).execute(&mut *tx).await?;
                sqlx::query("UPDATE enrichment_jobs SET force=1 WHERE item_id=?")
                    .bind(item)
                    .execute(&mut *tx)
                    .await?;
            }
            "restore" => {
                sqlx::query("DELETE FROM metadata_rejections WHERE item_id=? AND record_id=?")
                    .bind(item)
                    .bind(record_id.context("record required")?)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=?").bind(item).execute(&mut *tx).await?;
            }
            _ => anyhow::bail!("unknown correction"),
        }
        tx.commit().await?;
        Ok(())
    }
    /// Alternative identities for this copy; exclude its current membership before pagination.
    pub async fn matching_identities(
        &self,
        item: &str,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<IdentityChoice>> {
        ensure!((1..=200).contains(&limit), "invalid page size");
        Ok(sqlx::query("SELECT w.id,w.title,w.year FROM library_items w WHERE w.id<>(SELECT library_item_id FROM collection_items WHERE id=?1) AND w.media_type=(SELECT c.media_type FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?1) AND EXISTS(SELECT 1 FROM collection_items i JOIN library_collections lc ON lc.collection_id=i.collection_id WHERE i.library_item_id=w.id) AND instr(w.title_key,?2)>0 ORDER BY w.title_key,w.year,w.id LIMIT ?3 OFFSET ?4")
            .bind(item).bind(title_key(query)).bind(limit).bind(offset).fetch_all(self.db.read_pool()).await?
            .into_iter().map(|r| IdentityChoice { id:r.get("id"),title:r.get("title"),year:r.get("year") }).collect())
    }

    /// Choosing a saved work changes only this copy's metadata.
    /// Normal rebinding decides membership; this operation cannot
    /// manufacture duplicate title/year identities or coalesce albums.
    pub async fn assign_identity(&self, item: &str, revision: i64, saved: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let row = sqlx::query("SELECT i.enrichment_revision,c.media_type FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?")
            .bind(item).fetch_one(&mut *tx).await?;
        if row.get::<i64, _>("enrichment_revision") != revision {
            return Err(StaleEnrichment.into());
        }
        let kind = MediaType::parse(row.get("media_type"))?;
        let row = sqlx::query("SELECT title,year FROM library_items WHERE id=? AND media_type=?")
            .bind(saved)
            .bind(kind.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let title = row.get::<String, _>("title");
        let year = row.get::<Option<i32>, _>("year");
        // Local answers can contain root-relative artwork and belong to their
        // physical copy. Prefer this copy's matching retained answer; only
        // provider answers without local artwork can be shared from another.
        let own = sqlx::query("SELECT p.* FROM local_metadata l JOIN provider_records p ON p.id=l.record_id WHERE l.item_id=?")
                .bind(item).fetch_all(&mut *tx).await?;
        let mut chosen = None;
        for row in own {
            let candidate = record(&row)?;
            if title_key(&candidate.title) == title_key(&title) && candidate.year == year {
                chosen = Some(row.get::<String, _>("id"));
                break;
            }
        }
        if chosen.is_none() {
            let offered = sqlx::query("SELECT p.* FROM collection_items i JOIN metadata_assignments a ON a.item_id=i.id JOIN provider_records p ON p.id=a.record_id WHERE i.library_item_id=? ORDER BY a.manual DESC,a.strength DESC,i.id")
                    .bind(saved).fetch_all(&mut *tx).await?;
            for row in offered {
                let candidate = record(&row)?;
                if candidate.provider != "local"
                    && !candidate
                        .description
                        .artwork
                        .as_ref()
                        .is_some_and(|urls| urls.iter().any(|url| url.starts_with("local://")))
                {
                    chosen = Some(row.get::<String, _>("id"));
                    break;
                }
            }
        }
        if let Some(record) = chosen {
            replace_assignment(&mut tx, item, Some(&record), true, 100).await?;
            tx.commit().await?;
            return Ok(());
        }
        let record = crate::metadata::put_record(
            &mut tx,
            &ProviderRecord {
                provider: "manual".into(),
                namespace: kind.as_str().into(),
                external_id: id(),
                language: String::new(),
                media_type: kind,
                title,
                year,
                description: Description::default(),
            },
        )
        .await?;
        replace_assignment(&mut tx, item, Some(&record), true, 100).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn cache_answer(&self, provider: &str, question: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT answer FROM enrichment_cache WHERE provider=? AND question=?",
        )
        .bind(provider)
        .bind(question)
        .fetch_optional(self.db.read_pool())
        .await?)
    }
    pub async fn put_cache_answer(
        &self,
        provider: &str,
        question: &str,
        answer: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO enrichment_cache VALUES(?,?,?,?) ON CONFLICT(provider,question) DO UPDATE SET answer=excluded.answer,updated_at=excluded.updated_at").bind(provider).bind(question).bind(answer).bind(now).execute(&self.db).await?;
        Ok(())
    }
}
async fn replace_assignment(
    c: &mut sqlx::SqliteConnection,
    item: &str,
    record: Option<&str>,
    manual: bool,
    strength: i32,
) -> Result<()> {
    sqlx::query("DELETE FROM metadata_assignments WHERE item_id=?")
        .bind(item)
        .execute(&mut *c)
        .await?;
    if let Some(record) = record {
        if manual {
            sqlx::query("DELETE FROM metadata_rejections WHERE item_id=? AND record_id=?")
                .bind(item)
                .bind(record)
                .execute(&mut *c)
                .await?;
        }
        sqlx::query(
            "INSERT INTO metadata_assignments(item_id,record_id,manual,strength) VALUES(?,?,?,?)",
        )
        .bind(item)
        .bind(record)
        .bind(manual)
        .bind(strength)
        .execute(&mut *c)
        .await?;
    }
    crate::library::rebind(c, item).await?;
    Ok(())
}
impl Store {
    pub async fn offer_candidates(
        &self,
        item: &str,
        revision: i64,
        candidates: &[EnrichmentCandidate],
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (current,kind):(i64,String)=sqlx::query_as("SELECT i.enrichment_revision,c.media_type FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?").bind(item).fetch_one(&mut *tx).await?;
        if current != revision {
            return Err(StaleEnrichment.into());
        }
        for candidate in candidates {
            let id = put_candidate(&mut tx, candidate).await?;
            crate::metadata::compatible(&mut tx, &id, &kind).await?;
            sqlx::query("INSERT INTO enrichment_candidates VALUES(?,?,?,0) ON CONFLICT(item_id,record_id) DO UPDATE SET revision=excluded.revision")
                .bind(item).bind(id).bind(revision).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
impl Store {
    pub async fn enrichment_counts(&self) -> Result<(i64, i64, i64)> {
        Ok(sqlx::query_as("SELECT count(a.item_id),COALESCE(sum(a.item_id IS NULL AND EXISTS(SELECT 1 FROM enrichment_candidates c WHERE c.item_id=i.id AND c.strength=0 AND c.revision=i.enrichment_revision AND NOT EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=i.id AND r.record_id=c.record_id))),0),COALESCE(sum(a.item_id IS NULL AND NOT EXISTS(SELECT 1 FROM enrichment_candidates c WHERE c.item_id=i.id AND c.revision=i.enrichment_revision) AND NOT EXISTS(SELECT 1 FROM enrichment_jobs j WHERE j.item_id=i.id AND j.state<>'done')),0) FROM collection_items i LEFT JOIN metadata_assignments a ON a.item_id=i.id WHERE EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id)")
            .fetch_one(self.db.read_pool()).await?)
    }
    pub async fn add_supplement(&self, item: &str, revision: i64, record: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (current,kind,primary):(i64,String,String)=sqlx::query_as("SELECT i.enrichment_revision,c.media_type,a.record_id FROM collection_items i JOIN collections c ON c.id=i.collection_id JOIN metadata_assignments a ON a.item_id=i.id WHERE i.id=?").bind(item).fetch_one(&mut *tx).await?;
        if current != revision {
            return Err(StaleEnrichment.into());
        }
        crate::metadata::compatible(&mut tx, record, &kind).await?;
        let offered:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM enrichment_candidates WHERE item_id=? AND record_id=? AND revision=?)").bind(item).bind(record).bind(revision).fetch_one(&mut *tx).await?;
        ensure!(offered, "record not offered for this item");
        let different:bool=sqlx::query_scalar("SELECT a.provider<>b.provider FROM provider_records a,provider_records b WHERE a.id=? AND b.id=?").bind(&primary).bind(record).fetch_one(&mut *tx).await?;
        ensure!(different, "supplement must use another provider");
        sqlx::query("DELETE FROM metadata_supplements WHERE item_id=? AND record_id IN (SELECT id FROM provider_records WHERE provider=(SELECT provider FROM provider_records WHERE id=?))").bind(item).bind(record).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO metadata_supplements VALUES(?,?,?)")
            .bind(item)
            .bind(primary)
            .bind(record)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=?",
        )
        .bind(item)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
impl Store {
    pub async fn recent_cache_answer(
        &self,
        provider: &str,
        question: &str,
        since: i64,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT answer FROM enrichment_cache WHERE provider=? AND question=? AND updated_at>?",
        )
        .bind(provider)
        .bind(question)
        .bind(since)
        .fetch_optional(self.db.read_pool())
        .await?)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ContentIdentification {
    pub aid: u32,
    pub eid: Option<u32>,
    pub epno: Option<String>,
    pub gid: Option<u32>,
    pub group_name: Option<String>,
}
impl Store {
    pub async fn seed_cache_answer(
        &self,
        provider: &str,
        question: &str,
        answer: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO enrichment_cache VALUES(?,?,?,?)")
            .bind(provider)
            .bind(question)
            .bind(answer)
            .bind(now)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ArtistIdentity {
    pub id: String,
    pub name: String,
}
impl Store {
    pub async fn artist_identity(&self, item: &str) -> Result<Option<ArtistIdentity>> {
        Ok(sqlx::query("SELECT a.* FROM artists a JOIN collection_item_artists ca ON ca.artist_id=a.id JOIN collection_items i ON i.id=ca.item_id WHERE i.id=? AND ca.revision=i.enrichment_revision").bind(item).fetch_optional(self.db.read_pool()).await?.map(|r|ArtistIdentity {id:r.get("id"),name:r.get("name")}))
    }
    pub async fn put_artist_artwork(
        &self,
        artist: &str,
        provider: &str,
        url: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO artist_artwork VALUES(?,?,?,?) ON CONFLICT(artist_id,provider) DO UPDATE SET image_url=excluded.image_url,updated_at=excluded.updated_at").bind(artist).bind(provider).bind(url).bind(now).execute(&self.db).await?;
        Ok(())
    }
    /// Artist navigation groups exact imported album-artist names within a library.
    /// Any current identity in that group can supply its portrait; an unenriched
    /// first album must not hide images learned from the other albums.
    pub async fn artist_artwork(&self, item: &str, library: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT DISTINCT aa.image_url,CASE aa.provider WHEN 'fanart' THEN 0 ELSE 1 END AS priority FROM collection_items target JOIN library_collections visible ON visible.collection_id=target.collection_id AND visible.library_id=?2 JOIN collection_items i ON i.artist=target.artist JOIN library_collections lc ON lc.collection_id=i.collection_id AND lc.library_id=?2 JOIN collection_item_artists ca ON ca.item_id=i.id AND ca.revision=i.enrichment_revision JOIN artist_artwork aa ON aa.artist_id=ca.artist_id WHERE target.id=?1 AND aa.image_url IS NOT NULL ORDER BY priority,aa.image_url").bind(item).bind(library).fetch_all(self.db.read_pool()).await?)
    }
}
async fn put_candidate(
    c: &mut sqlx::SqliteConnection,
    candidate: &EnrichmentCandidate,
) -> Result<String> {
    let record = &candidate.record;
    if !candidate.complete {
        let existing:Option<String>=sqlx::query_scalar("SELECT id FROM provider_records WHERE provider=? AND namespace=? AND external_id=? AND language=?").bind(&record.provider).bind(&record.namespace).bind(&record.external_id).bind(&record.language).fetch_optional(&mut *c).await?;
        if let Some(id) = existing {
            return Ok(id);
        }
    }
    crate::metadata::put_record(c, record).await
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ArtistDonor {
    pub id: String,
    pub library_item_id: String,
    pub revision: i64,
    pub poster: String,
}
impl Store {
    pub async fn copy_libraries(&self, item: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT lc.library_id FROM library_collections lc JOIN collection_items i ON i.collection_id=lc.collection_id WHERE i.id=? ORDER BY lc.library_id").bind(item).fetch_all(self.db.read_pool()).await?)
    }
    pub async fn artist_donors(&self, item: &str, library: &str) -> Result<Vec<ArtistDonor>> {
        let mut tx = self.db.read_pool().begin().await?;
        let rows=sqlx::query("SELECT i.id,i.library_item_id,i.enrichment_revision FROM collection_items i JOIN collections c ON c.id=i.collection_id LEFT JOIN metadata_assignments a ON a.item_id=i.id LEFT JOIN provider_records p ON p.id=a.record_id WHERE c.media_type='music' AND i.artist IS NOT NULL AND ((SELECT artist_id FROM collection_item_artists WHERE item_id=i.id AND revision=i.enrichment_revision) IS (SELECT a.artist_id FROM collection_item_artists a JOIN collection_items target ON target.id=a.item_id AND target.enrichment_revision=a.revision WHERE a.item_id=?1)) AND (EXISTS(SELECT 1 FROM collection_item_artists a WHERE a.item_id=i.id AND a.revision=i.enrichment_revision) OR lower(i.artist)=(SELECT lower(artist) FROM collection_items WHERE id=?1)) AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id AND lc.library_id=?2) AND EXISTS(SELECT 1 FROM collection_items target JOIN library_collections lc ON lc.collection_id=target.collection_id WHERE target.id=?1 AND lc.library_id=?2) ORDER BY COALESCE(p.year,i.year) DESC,i.id")
            .bind(item).bind(library).fetch_all(&mut *tx).await?;
        let mut donors = Vec::new();
        for r in rows {
            let id: String = r.get("id");
            let description = crate::metadata::resolve(&mut tx, &id).await?;
            if let Some(poster) = description
                .description
                .artwork
                .and_then(|v| v.into_iter().next())
            {
                donors.push(ArtistDonor {
                    id,
                    library_item_id: r.get("library_item_id"),
                    revision: r.get("enrichment_revision"),
                    poster,
                });
            }
        }
        tx.commit().await?;
        Ok(donors)
    }
}
/// Wait for higher-ranked *runnable* identity questions, never for a failed
/// provider. This does not hold a worker: answers land independently, and the
/// last success/failure transaction selects the best available confident answer.
async fn choose_automatic(c: &mut sqlx::SqliteConnection, item: &str) -> Result<()> {
    let selected: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM metadata_assignments WHERE item_id=?)")
            .bind(item)
            .fetch_one(&mut *c)
            .await?;
    if selected {
        return Ok(());
    }
    let candidate:Option<(String,i32,i64)>=sqlx::query_as("SELECT ec.record_id,ec.strength,COALESCE(o.position,-1) FROM enrichment_candidates ec JOIN collection_items i ON i.id=ec.item_id JOIN collections col ON col.id=i.collection_id JOIN provider_records p ON p.id=ec.record_id LEFT JOIN provider_order o ON o.media_type=col.media_type AND o.provider=p.provider WHERE ec.item_id=? AND ec.revision=i.enrichment_revision AND ec.strength>=10 AND NOT EXISTS(SELECT 1 FROM metadata_rejections r WHERE r.item_id=i.id AND r.record_id=ec.record_id) AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id) ORDER BY ec.strength DESC,COALESCE(o.position,-1),p.id LIMIT 1").bind(item).fetch_optional(&mut *c).await?;
    let Some((record, strength, rank)) = candidate else {
        return Ok(());
    };
    let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM enrichment_jobs j JOIN collection_items i ON i.id=j.item_id JOIN collections col ON col.id=i.collection_id JOIN provider_order o ON o.media_type=col.media_type AND o.provider=j.provider LEFT JOIN enrichment_providers p ON p.provider=j.provider WHERE j.item_id=? AND j.revision=i.enrichment_revision AND o.position<? AND j.state IN ('pending','running') AND COALESCE(p.blocked,0)=0 AND COALESCE(p.due_at,0)<=unixepoch())").bind(item).bind(rank).fetch_one(&mut *c).await?;
    if !waiting || strength >= 20 {
        replace_assignment(c, item, Some(&record), false, strength).await?;
    }
    Ok(())
}
impl Store {
    pub async fn provider_pause(&self, provider: &str) -> Result<Option<(i64, bool)>> {
        Ok(
            sqlx::query_as("SELECT due_at,blocked FROM enrichment_providers WHERE provider=?")
                .bind(provider)
                .fetch_optional(self.db.read_pool())
                .await?,
        )
    }
}
