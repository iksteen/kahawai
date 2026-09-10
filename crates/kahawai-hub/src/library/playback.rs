//! Capture the complete physical version and its optional episode boundaries.
use super::*;

pub struct PlaybackSnapshot {
    pub playable_source_id: i64,
    pub collection_item_id: String,
    pub revision: i64,
    pub library_item_ids: Vec<String>,
    pub fingerprint: String,
    pub boundaries: Vec<SourceBoundary>,
}

pub async fn playback_snapshot(
    db: &Database,
    requested: &str,
    file: i64,
) -> Result<PlaybackSnapshot> {
    let mut tx = db.begin_with_label("capture playback identity").await?;
    let (source,copy):(i64,String)=sqlx::query_as("SELECT ps.id,ps.item_id FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id WHERE p.file_id=?").bind(file).fetch_one(&mut *tx).await?;
    let assignment = assignment(&mut tx, &copy).await?;
    anyhow::ensure!(
        assignment.library_item_ids.iter().any(|id| id == requested),
        "assignment changed while selecting playback; retry"
    );
    let parts:Vec<(i64,i64,i64)>=sqlx::query_as("SELECT f.size,f.head_xxh3,f.tail_xxh3 FROM playable_source_parts p JOIN files f ON f.id=p.file_id WHERE p.playable_source_id=? ORDER BY p.ordinal,p.file_id").bind(source).fetch_all(&mut *tx).await?;
    let fingerprint = crate::registry::source_fingerprint(&parts);
    let rows:Vec<(i64,i64,i64)>=sqlx::query_as("SELECT ordinal,start_ms,end_ms FROM source_boundaries WHERE playable_source_id=? AND source_fingerprint=? ORDER BY ordinal").bind(source).bind(&fingerprint).fetch_all(&mut *tx).await?;
    // Partial, stale or contradictory coverage means combined playback.
    let valid = rows.len() == assignment.library_item_ids.len()
        && rows.iter().enumerate().all(|(n, (ordinal, start, end))| {
            *ordinal == n as i64 + 1
                && *start >= 0
                && end > start
                && (n == 0 || *start >= rows[n - 1].2)
        });
    let boundaries = if valid {
        rows.into_iter()
            .zip(&assignment.library_item_ids)
            .map(|((_, start, end), id)| SourceBoundary {
                library_item_id: id.clone(),
                start_ms: start as u64,
                end_ms: end as u64,
            })
            .collect()
    } else {
        Vec::new()
    };
    tx.commit().await?;
    Ok(PlaybackSnapshot {
        playable_source_id: source,
        collection_item_id: copy,
        revision: assignment.revision,
        library_item_ids: assignment.library_item_ids,
        fingerprint,
        boundaries,
    })
}

/// Exact physical coverage. Without a complete valid set, playback stays combined.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SourceBoundary {
    pub library_item_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Resume provenance includes the coordinate mapping as well as the bytes.
/// Changing coverage on unchanged media cannot reinterpret an old offset.
pub fn resume_fingerprint(snapshot: &PlaybackSnapshot) -> String {
    if snapshot.boundaries.is_empty() {
        snapshot.fingerprint.clone()
    } else {
        format!(
            "{}:bounds:{}",
            snapshot.fingerprint,
            serde_json::to_string(
                &snapshot
                    .boundaries
                    .iter()
                    .map(|b| (&b.library_item_id, b.start_ms, b.end_ms))
                    .collect::<Vec<_>>()
            )
            .expect("coverage serializes")
        )
    }
}

/// Member placement is part of a resume coordinate. Permanent first-identification
/// aliases are equivalent; correcting an established assignment creates no alias.
pub async fn same_resume_version(
    db: &Database,
    expected: Option<&str>,
    actual: &str,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    if expected == actual {
        return Ok(true);
    }
    let Some((old_source, old_map)) = expected.split_once(":bounds:") else {
        return Ok(false);
    };
    let Some((new_source, new_map)) = actual.split_once(":bounds:") else {
        return Ok(false);
    };
    if old_source != new_source {
        return Ok(false);
    }
    type Mapping = Vec<(String, u64, u64)>;
    let (Ok(old), Ok(new)) = (
        serde_json::from_str::<Mapping>(old_map),
        serde_json::from_str::<Mapping>(new_map),
    ) else {
        return Ok(false);
    };
    if old.len() != new.len() {
        return Ok(false);
    }
    let mut c = db.read_pool().acquire().await?;
    for ((old_id, old_start, old_end), (new_id, new_start, new_end)) in old.into_iter().zip(new) {
        if (old_start, old_end) != (new_start, new_end) {
            return Ok(false);
        }
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_items WHERE id=?)")
                .bind(&old_id)
                .fetch_one(&mut *c)
                .await?;
        if !known || canonical_id(&mut c, &old_id).await? != canonical_id(&mut c, &new_id).await? {
            return Ok(false);
        }
    }
    Ok(true)
}
