//! Catalogue input to the existing media engine. No hub catalogue projection.
//! Identity and physical addresses are captured together before negotiation;
//! rematches cannot redirect a running session's progress or byte reads.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileId {
    Legacy(i64),
    Catalogue(String),
}
impl FileId {
    pub fn legacy(&self) -> Result<i64> {
        match self {
            Self::Legacy(id) => Ok(*id),
            Self::Catalogue(_) => bail!("not a legacy file"),
        }
    }
    pub(super) async fn audio_loudness(
        &self,
        registry: &Registry,
        stream: usize,
        size: u64,
        mtime: i64,
    ) -> Result<Option<kahawai_media::loudness::AudioLoudnessMeasurement>> {
        let Self::Catalogue(id) = self else {
            return registry.audio_loudness(self.legacy()?, stream).await;
        };
        use kahawai_media::loudness::*;
        for fact in registry.catalogue().source_facts(id).await? {
            if let kahawai_mediadb::SourceFact::Loudness(fact) = fact
                && fact.size == size
                && fact.mtime_unix == mtime
                && fact.analyzer == ANALYZER
                && fact.error.is_empty()
                && let Some(track) = fact
                    .tracks
                    .into_iter()
                    .find(|t| t.stream_index as usize == stream)
            {
                return Ok(Some(AudioLoudnessMeasurement {
                    source: AudioLayout::new(track.source_channels, track.source_channel_mask),
                    layouts: track
                        .layouts
                        .into_iter()
                        .map(|l| AudioLayoutLoudness {
                            layout: AudioLayout::new(l.channels, l.channel_mask),
                            loudness: AudioLoudness {
                                integrated_lufs: l.integrated_lufs,
                                true_peak_dbtp: l.true_peak_dbtp,
                            },
                        })
                        .collect(),
                }));
            }
        }
        Ok(None)
    }
}

pub struct CataloguePlayback {
    pub subtitle_cache: std::path::PathBuf,
    pub library_id: String,
    pub item: kahawai_mediadb::PlaybackItem,
    pub media_entry_id: Option<String>,
}
#[derive(Clone)]
pub struct CatalogueSession {
    pub library_id: String,
    pub parent_id: String,
    pub track: bool,
    pub media_entry_id: String,
    pub copy_id: String,
    pub source_id: i64,
    pub item_ids: Vec<String>,
    pub segments: Vec<crate::segments::Segment>,
    pub subtitles: Vec<crate::tracks::Track>,
}
pub(super) fn fingerprint(parts: &[PartSource]) -> String {
    if parts
        .iter()
        .any(|p| matches!(p.file_id, FileId::Catalogue(_)) && p.head_xxh3 == 0 && p.tail_xxh3 == 0)
    {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        hash.update(b"kahawai-unhashed-source-v1");
        for p in parts {
            hash.update(format!(
                "{:?}:{}:{}:{}:{}",
                p.file_id, p.size, p.mtime_unix, p.head_xxh3, p.tail_xxh3
            ));
        }
        return data_encoding::BASE64URL_NOPAD.encode(&hash.finalize());
    }
    crate::registry::source_fingerprint(
        &parts
            .iter()
            .map(|p| (p.size as i64, p.head_xxh3, p.tail_xxh3))
            .collect::<Vec<_>>(),
    )
}
impl CataloguePlayback {
    pub(crate) fn candidates(
        &self,
    ) -> Result<Vec<(Vec<PartSource>, kahawai_core::media::MediaInfo)>> {
        let mut out = Vec::new();
        for rendition in &self.item.renditions {
            if self
                .media_entry_id
                .as_ref()
                .is_some_and(|id| *id != rendition.entry.id)
            {
                continue;
            }
            if let kahawai_mediadb::EntryKind::Episode { episodes } = &rendition.entry.data.kind
                && episodes
                    .iter()
                    .map(|s| {
                        u64::from(s.episode_end.unwrap_or(s.episode)) - u64::from(s.episode) + 1
                    })
                    .sum::<u64>()
                    > crate::watch::MAX_BATCH_ITEMS as u64
            {
                continue;
            }
            let expected = rendition
                .entry
                .data
                .parts
                .iter()
                .map(|p| p.ordinal)
                .max()
                .unwrap_or(0) as usize;
            if expected == 0
                || expected != rendition.files.len()
                || !rendition
                    .entry
                    .data
                    .parts
                    .iter()
                    .map(|p| p.ordinal)
                    .eq(1..=expected as u32)
                || rendition.files.iter().any(|f| {
                    f.size.is_none()
                        || f.media.is_none()
                        || (expected > 1 && f.media.as_ref().and_then(|i| i.duration_ms).is_none())
                })
            {
                continue;
            }
            let mut parts = Vec::new();
            let mut base_ms = 0u64;
            for file in &rendition.files {
                let duration_ms = file.media.as_ref().unwrap().duration_ms.unwrap_or(0);
                parts.push(PartSource {
                    file_id: FileId::Catalogue(file.id.clone()),
                    head_xxh3: file.head_hash.unwrap_or(0) as i64,
                    tail_xxh3: file.tail_hash.unwrap_or(0) as i64,
                    module_id: rendition.mediahost_id.clone(),
                    collection_id: rendition.collection_id.clone(),
                    root_token: file.root_token.clone(),
                    path_rel: file.path.clone(),
                    size: file.size.unwrap(),
                    mtime_unix: file.mtime.unwrap_or(0),
                    base_ms,
                    duration_ms,
                });
                base_ms = base_ms
                    .checked_add(duration_ms)
                    .context("source duration overflow")?;
            }
            let mut info = rendition.files[0].media.clone().unwrap();
            for download in &rendition.downloaded_subtitles {
                info.external_subtitles
                    .push(kahawai_core::media::SidecarSubtitle {
                        path_rel: format!("mediadb-download:{}", download.id),
                        format: download.format.clone(),
                        language: download.language.clone(),
                        ..Default::default()
                    });
            }
            out.push((parts, info));
        }
        Ok(out)
    }
    pub(crate) async fn negotiate(
        &self,
        neg: &mut Negotiation<'_>,
        mode: Option<&str>,
        recovery: Option<&str>,
    ) -> Result<(
        Vec<PartSource>,
        kahawai_core::media::MediaInfo,
        kahawai_media::negotiate::SourcePlan,
        String,
    )> {
        if self.item.track {
            neg.loudness = LoudnessPreference::Off;
        }
        let mut candidates = self.candidates()?;
        let mut raster = std::collections::HashSet::new();
        for (parts, _) in &candidates {
            let capture = self.capture(parts, &self.item.parent_id)?;
            neg.ocr_set.extend(ocr_sources(&capture.subtitles));
            if capture.subtitles.iter().any(|t| t.origin == "raster") {
                raster.insert(parts[0].file_id.clone());
            }
        }
        neg.raster_sources = Some(raster);
        if let Some(expected) = recovery {
            candidates.retain(|(parts, _)| fingerprint(parts) == expected);
        }
        let choice = neg.choose_sources(candidates, mode).await?;
        neg.ass.overlay_ready = neg
            .raster_sources
            .as_ref()
            .is_some_and(|r| r.contains(&choice.0[0].file_id));
        Ok(choice)
    }
    pub(crate) fn capture(&self, parts: &[PartSource], item: &str) -> Result<CatalogueSession> {
        let (index, rendition) = self
            .item
            .renditions
            .iter()
            .enumerate()
            .find(|(_, r)| {
                r.files.first().is_some_and(|f| {
                    parts
                        .first()
                        .is_some_and(|p| p.file_id == FileId::Catalogue(f.id.clone()))
                })
            })
            .context("chosen rendition missing from snapshot")?;
        let item_ids =
            if let kahawai_mediadb::EntryKind::Episode { episodes } = &rendition.entry.data.kind {
                let mut ids = std::collections::BTreeSet::new();
                for span in episodes {
                    for episode in span.episode..=span.episode_end.unwrap_or(span.episode) {
                        ids.insert(
                            kahawai_mediadb::ChildId {
                                parent: self.item.parent_id.clone(),
                                position: kahawai_mediadb::ChildPosition::Episode {
                                    season: span.season,
                                    episode,
                                },
                            }
                            .encode(),
                        );
                    }
                }
                ids.into_iter().collect()
            } else {
                vec![item.to_owned()]
            };
        Ok(CatalogueSession {
            segments: rendition_segments(rendition, parts),
            subtitles: {
                let downloads = downloaded_tracks(rendition, item, &parts[0])?;
                let parents = tracks(item, &parts[0], rendition.files[0].media.as_ref().unwrap())
                    .into_iter()
                    .chain(downloads.iter().cloned())
                    .collect::<Vec<_>>();
                downloads
                    .into_iter()
                    .chain(crate::subtitles::catalogue::cached(
                        &self.subtitle_cache,
                        &parents,
                    )?)
                    .collect()
            },
            library_id: self.library_id.clone(),
            parent_id: self.item.parent_id.clone(),
            track: self.item.track,
            media_entry_id: rendition.entry.id.clone(),
            copy_id: rendition.entry.item_id.clone(),
            source_id: index as i64 + 1,
            item_ids,
        })
    }
}

pub(crate) fn physical(
    part: &PartSource,
    info: &kahawai_core::media::MediaInfo,
) -> crate::subtitles::FileSource {
    crate::subtitles::FileSource {
        module_id: part.module_id.clone(),
        collection_id: part.collection_id.clone(),
        root_token: part.root_token.clone(),
        path_rel: part.path_rel.clone(),
        size: part.size,
        revision: crate::subtitles::catalogue::key(part, info, "embedded"),
        sidecar_revision: crate::subtitles::catalogue::key(part, info, "sidecar:"),
        info: info.clone(),
    }
}
/// Stream IDs are local to the selected rendition. Resources bind them to an
/// owned session, so a rematch or another rendition cannot substitute a track.
pub(crate) fn tracks(
    item: &str,
    part: &PartSource,
    info: &kahawai_core::media::MediaInfo,
) -> Vec<crate::tracks::Track> {
    info.subtitles
        .iter()
        .enumerate()
        .map(|(index, s)| {
            (
                index,
                "embedded",
                s.format.clone(),
                s.language.clone(),
                None,
            )
        })
        .chain(
            info.external_subtitles
                .iter()
                .enumerate()
                .map(|(index, s)| {
                    (
                        index,
                        "sidecar",
                        s.format.clone(),
                        s.language.clone(),
                        Some(s.path_rel.clone()),
                    )
                }),
        )
        .enumerate()
        .map(
            |(at, (index, origin, format, language, path))| crate::tracks::Track {
                acquired: None,
                artifact_key: Some(crate::subtitles::catalogue::key(
                    part,
                    info,
                    &format!("{origin}:{index}"),
                )),
                raster: None,
                id: at as i64 + 1,
                item_id: item.into(),
                origin: origin.into(),
                source_id: None,
                physical: Some(physical(part, info)),
                module_id: Some(part.module_id.clone()),
                collection_id: Some(part.collection_id.clone()),
                root_token: Some(part.root_token.clone()),
                source_path: Some(part.path_rel.clone()),
                path_rel: path,
                stream_index: Some(index as i64),
                format,
                language,
                label: None,
                machine: false,
                derived_from: None,
                payload_id: None,
                created_by: None,
            },
        )
        .filter(|t| {
            !t.path_rel
                .as_deref()
                .is_some_and(|p| p.starts_with("mediadb-download:"))
        })
        .collect()
}
pub(crate) fn listing(
    item: &str,
    parts: &[PartSource],
    info: &kahawai_core::media::MediaInfo,
    profile: &kahawai_core::media::CapabilityProfile,
    ass: &kahawai_media::negotiate::AssPolicy,
    downloads: &[crate::tracks::Track],
) -> Vec<crate::subtitles::TrackListing> {
    let Some(part) = parts.first() else {
        return vec![];
    };
    tracks(item, part, info)
        .into_iter()
        .chain(downloads.iter().cloned())
        .map(|track| {
            let burn_capable =
                track.acquired.is_none() || matches!(track.format.as_str(), "ass" | "ssa");
            let (delivery, note) = crate::tracks::delivery(&track, profile, burn_capable, ass);
            crate::subtitles::TrackListing {
                track,
                delivery,
                note,
                deletable: false,
            }
        })
        .collect()
}
impl Session {
    pub(crate) fn catalogue_track(&self, id: i64) -> Option<crate::tracks::Track> {
        tracks(&self.item_id, self.parts.first()?, &self.info)
            .into_iter()
            .chain(self.catalogue.as_ref()?.subtitles.iter().cloned())
            .find(|t| t.id == id)
    }
    pub(crate) fn catalogue_listing(&self) -> Vec<crate::subtitles::TrackListing> {
        listing(
            &self.item_id,
            &self.parts,
            &self.info,
            self.effective_profile(),
            self.ass_policy(),
            &self
                .catalogue
                .as_ref()
                .expect("catalogue session")
                .subtitles,
        )
    }
    pub(crate) fn physical_source(&self) -> Option<crate::subtitles::FileSource> {
        Some(physical(self.parts.first()?, &self.info))
    }
}

/// Timestamps belong to the captured file version, never its logical item.
/// Each multipart file keeps its own timeline; only its playback offset is added.
fn rendition_segments(
    rendition: &kahawai_mediadb::PlaybackRendition,
    parts: &[PartSource],
) -> Vec<crate::segments::Segment> {
    use kahawai_core::segments::{DETECTOR_GENERATION, inferred_within_bounds, named};
    let mut out = Vec::new();
    for part in parts {
        let Some(file) = rendition
            .files
            .iter()
            .find(|f| part.file_id == FileId::Catalogue(f.id.clone()))
        else {
            continue;
        };
        let mut found = std::collections::BTreeMap::new();
        if let Some(fact) = rendition.segment_facts.get(&file.id)
            && fact.detector == DETECTOR_GENERATION
            && fact.collection_id == part.collection_id
            && fact.error.is_empty()
            && let [result] = fact.episodes.as_slice()
            && !result.unreadable
            && !result.retryable
            && result.error.is_empty()
            && result.observed_size == part.size
            && file.mtime == Some(result.observed_mtime_unix)
            && result
                .source
                .as_ref()
                .is_some_and(|s| s.root_token == part.root_token && s.path_rel == part.path_rel)
        {
            for segment in &result.segments {
                if inferred_within_bounds(
                    &segment.kind,
                    &segment.analyzer,
                    segment.start_ms,
                    segment.end_ms,
                    part.duration_ms,
                ) {
                    found.insert(
                        segment.kind.clone(),
                        (segment.start_ms, segment.end_ms, segment.analyzer.clone()),
                    );
                }
            }
        }
        // Explicit chapter names outrank inferred boundaries for the same kind.
        if let Some(chapters) = file.media.as_ref().and_then(|i| i.chapters.as_deref()) {
            for segment in named(chapters, part.duration_ms) {
                found.insert(
                    segment.kind.into(),
                    (segment.start_ms, segment.end_ms, "chapter".into()),
                );
            }
        }
        for (kind, (start, end, source)) in found {
            if let Some((start_ms, end_ms)) = part.base_ms.checked_add(start).and_then(|s| {
                Some((
                    i64::try_from(s).ok()?,
                    i64::try_from(part.base_ms.checked_add(end)?).ok()?,
                ))
            }) {
                out.push(crate::segments::Segment {
                    kind,
                    start_ms,
                    end_ms,
                    source,
                });
            }
        }
    }
    out.sort_by_key(|s| (s.start_ms, s.end_ms));
    out
}

fn downloaded_tracks(
    r: &kahawai_mediadb::PlaybackRendition,
    item: &str,
    part: &PartSource,
) -> Result<Vec<crate::tracks::Track>> {
    let info = r.files[0].media.as_ref().context("missing source probe")?;
    r.downloaded_subtitles
        .iter()
        .enumerate()
        .map(|(index, d)| {
            Ok(crate::tracks::Track {
                // Negative IDs are persistent downloads; positive IDs remain source-local streams.
                id: -d.id,
                acquired: Some(Arc::new(serde_json::from_str(&d.payload)?)),
                artifact_key: Some(crate::subtitles::catalogue::key(
                    part,
                    info,
                    &format!("download:{}", d.id),
                )),
                raster: None,
                physical: Some(physical(part, info)),
                item_id: item.into(),
                origin: "downloaded".into(),
                source_id: None,
                module_id: Some(part.module_id.clone()),
                collection_id: Some(part.collection_id.clone()),
                root_token: Some(part.root_token.clone()),
                source_path: Some(part.path_rel.clone()),
                path_rel: None,
                stream_index: Some((info.external_subtitles.len() + index) as i64),
                format: d.format.clone(),
                language: d.language.clone(),
                label: d.label.clone(),
                machine: false,
                derived_from: None,
                payload_id: None,
                created_by: Some(d.created_by.clone()),
            })
        })
        .collect()
}

pub(super) fn ocr_sources(
    tracks: &[crate::tracks::Track],
) -> std::collections::HashSet<(String, String, String, String, i64)> {
    tracks
        .iter()
        .filter(|t| t.origin == "ocr")
        .filter_map(|t| {
            Some((
                t.module_id.clone()?,
                t.collection_id.clone()?,
                t.root_token.clone()?,
                t.source_path.clone()?,
                t.stream_index?,
            ))
        })
        .collect()
}
