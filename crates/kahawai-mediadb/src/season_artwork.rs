//! Covers are provider evidence, not a second catalogue of seasons. Resolving
//! them must not manufacture groups or change native child identities. Retain
//! all providers' answers for prefetch and resolve the first applicable cover.
use crate::*;
use sqlx::{Row, SqliteConnection};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct ResolvedSeasonArtwork {
    pub metadata: ResolvedDescription,
    /// The visible, contributing copy authorizing this artwork's local paths.
    pub representative_id: String,
}

pub(crate) struct SeasonEvidence {
    pub record_id: String,
    pub provider: String,
    pub rank: i64,
    pub native: bool,
    pub covers: Vec<ProviderSeasonArtwork>,
}

pub(crate) async fn evidence(c: &mut SqliteConnection, item: &str) -> Result<Vec<SeasonEvidence>> {
    crate::metadata::evidence(c, item)
        .await?
        .into_iter()
        .map(|r| {
            Ok(SeasonEvidence {
                record_id: r.get("id"),
                provider: r.get("provider"),
                rank: r.get("position"),
                native: matches!(r.get::<&str, _>("provider"), "anidb" | "anilist")
                    && r.get::<&str, _>("namespace") == "anime",
                covers: r
                    .get::<Option<&str>, _>("season_artwork_json")
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub(crate) fn resolve(
    copies: &[(&str, bool, &[SeasonEvidence])],
    season: Option<u32>,
) -> Option<ResolvedSeasonArtwork> {
    // A shared native record and one group per copy prove the entry mapping.
    let mut shared: Option<BTreeSet<&str>> = None;
    for (_, single_group, records) in copies {
        let ids: BTreeSet<_> = records
            .iter()
            .filter(|r| r.native && *single_group)
            .map(|r| r.record_id.as_str())
            .collect();
        shared = Some(match shared {
            None => ids,
            Some(s) => s.intersection(&ids).copied().collect(),
        });
    }
    let shared = shared.unwrap_or_default();
    copies
        .iter()
        .enumerate()
        .flat_map(|(index, (copy, _, records))| {
            records.iter().flat_map(move |record| {
                record
                    .covers
                    .iter()
                    .map(move |cover| (index, *copy, record, cover))
            })
        })
        .filter(|(_, _, record, cover)| {
            !cover.artwork.is_empty()
                && match cover.scope {
                    SeasonArtworkScope::Season { number } => season == Some(number),
                    SeasonArtworkScope::NativeEntry => {
                        season != Some(0) && shared.contains(record.record_id.as_str())
                    }
                }
        })
        .min_by_key(|(index, _, record, _)| (record.rank, *index))
        .map(|(_, copy, record, cover)| ResolvedSeasonArtwork {
            representative_id: copy.into(),
            metadata: ResolvedDescription {
                description: Description {
                    artwork: Some(cover.artwork.clone()),
                    ..Default::default()
                },
                provenance: [("artwork".into(), record.record_id.clone())].into(),
                providers: [(record.record_id.clone(), record.provider.clone())].into(),
            },
        })
}

impl Store {
    /// Collect all eligible provider answers, not only the resolved winner.
    pub async fn provider_artwork(&self, item: &str, provider: &str) -> Result<Vec<String>> {
        let mut c = self.db.read_pool().acquire().await?;
        let rows = crate::metadata::evidence(&mut c, item).await?;
        let mut urls = BTreeSet::new();
        for row in rows
            .into_iter()
            .filter(|r| r.get::<&str, _>("provider") == provider)
        {
            let description: Description = serde_json::from_str(row.get("description_json"))?;
            urls.extend(description.artwork.unwrap_or_default());
            let covers: Vec<ProviderSeasonArtwork> = row
                .get::<Option<&str>, _>("season_artwork_json")
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default();
            urls.extend(covers.into_iter().flat_map(|c| c.artwork));
        }
        Ok(urls.into_iter().collect())
    }
}
