//! Child identities name native positions under an immutable library identity.
//! They are derived, never allocated or persisted. Sources determine visibility;
//! provider descriptions cannot manufacture children. Numbering corrections move
//! sources between keys; metadata edits and list order never change a key.
//! Absolute and seasonal episodes, and unknown/explicit disc numbers, remain
//! distinct. Unnumbered tracks use a physical-entry discriminator.
//! Queries merge compact coverage intervals before counting/paging, so a combined
//! file cannot allocate an unbounded list. Reads never repair or write the database.
use crate::*;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChildPosition {
    Episode { season: Option<u32>, episode: u32 },
    Track { disc: Option<u32>, track: u32 },
    UnnumberedTrack { disc: Option<u32>, entry_id: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildId {
    pub parent: String,
    pub position: ChildPosition,
}
impl ChildId {
    pub fn encode(&self) -> String {
        let opt = |n: Option<u32>| n.map_or("u".into(), |n| n.to_string());
        let suffix = match &self.position {
            ChildPosition::Episode { season, episode } => format!("e:{}:{episode}", opt(*season)),
            ChildPosition::Track { disc, track } => format!("t:{}:{track}", opt(*disc)),
            ChildPosition::UnnumberedTrack { disc, entry_id } => {
                format!("u:{}:{entry_id}", opt(*disc))
            }
        };
        format!("child1:{}:{suffix}", self.parent)
    }
    pub fn parse(value: &str) -> Result<Self> {
        let p: Vec<_> = value.split(':').collect();
        ensure!(p.len() >= 4 && p[0] == "child1", "invalid child ID");
        ensure!(ulid::Ulid::from_string(p[1]).is_ok(), "invalid parent ID");
        let opt =
            |s: &str| -> Result<Option<u32>> { Ok(if s == "u" { None } else { Some(s.parse()?) }) };
        let position = match p.as_slice() {
            [_, _, "e", season, episode] => ChildPosition::Episode {
                season: opt(season)?,
                episode: episode.parse()?,
            },
            [_, _, "t", disc, track] => {
                let disc = opt(disc)?;
                let track = track.parse()?;
                ensure!(disc != Some(0) && track > 0, "invalid track position");
                ChildPosition::Track { disc, track }
            }
            [_, _, "u", disc, entry] => {
                let disc = opt(disc)?;
                ensure!(disc != Some(0), "invalid disc");
                ensure!(ulid::Ulid::from_string(entry).is_ok(), "invalid entry ID");
                ChildPosition::UnnumberedTrack {
                    disc,
                    entry_id: (*entry).into(),
                }
            }
            _ => anyhow::bail!("invalid child ID"),
        };
        let result = Self {
            parent: p[1].into(),
            position,
        };
        ensure!(result.encode() == value, "noncanonical child ID");
        Ok(result)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LibraryChild {
    pub id: String,
    pub parent_id: String,
    pub position: ChildPosition,
    pub title: String,
    pub artist: Option<String>,
    pub metadata: ResolvedDescription,
    pub representative_id: String,
    pub source_count: usize,
}
#[derive(Debug, Clone)]
pub struct LibraryChildDetail {
    pub child: LibraryChild,
    pub parent: LibraryItem,
    pub renditions: Vec<MediaEntry>,
}
#[derive(Debug, Clone, Default)]
pub struct ChildFilter {
    /// None is all numbering groups; Some(None) is absolute numbering.
    pub season: Option<Option<u32>>,
    pub disc: Option<Option<u32>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ChildGroup {
    pub kind: String,
    pub number: Option<u32>,
    pub total: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LibraryChildren {
    pub children: Vec<LibraryChild>,
    pub total: u64,
    pub groups: Vec<ChildGroup>,
    pub offset: u64,
    pub limit: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    Episode(Option<u32>),
    Track(Option<u32>),
    Unnumbered(Option<u32>),
}
struct Copy {
    id: String,
    entries: Vec<MediaEntry>,
    children: Option<crate::metadata::ChildCatalogue>,
}
struct Snapshot {
    parent: LibraryItem,
    copies: Vec<Copy>,
    ranges: BTreeMap<Group, Vec<(u32, u32)>>,
    unnumbered: BTreeMap<Option<u32>, Vec<String>>,
}
impl Snapshot {
    async fn read(c: &mut sqlx::SqliteConnection, library: &str, parent: &str) -> Result<Self> {
        let parent = crate::library::read_item(c, library, parent).await?;
        let mut copies = Vec::new();
        let mut ranges: BTreeMap<Group, Vec<(u32, u32)>> = BTreeMap::new();
        let mut unnumbered: BTreeMap<Option<u32>, Vec<String>> = BTreeMap::new();
        for id in &parent.copy_ids {
            let entries = crate::occurrence::read_entries(c, id).await?;
            for entry in &entries {
                match &entry.data.kind {
                    EntryKind::Movie => {}
                    EntryKind::Episode { episodes } => {
                        for span in episodes {
                            ranges
                                .entry(Group::Episode(span.season))
                                .or_default()
                                .push((span.episode, span.episode_end.unwrap_or(span.episode)));
                        }
                    }
                    EntryKind::Track {
                        disc,
                        track: Some(track),
                    } => {
                        ranges
                            .entry(Group::Track(*disc))
                            .or_default()
                            .push((*track, *track));
                    }
                    EntryKind::Track { disc, track: None } => {
                        unnumbered.entry(*disc).or_default().push(entry.id.clone())
                    }
                }
            }
            copies.push(Copy {
                id: id.clone(),
                entries,
                children: crate::metadata::resolve_children(c, id).await?,
            });
        }
        for intervals in ranges.values_mut() {
            intervals.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::new();
            for &(start, end) in intervals.iter() {
                if let Some(last) = merged.last_mut()
                    && u64::from(start) <= u64::from(last.1) + 1
                {
                    last.1 = last.1.max(end);
                } else {
                    merged.push((start, end));
                }
            }
            *intervals = merged;
        }
        for (disc, entries) in &mut unnumbered {
            entries.sort();
            ranges.insert(Group::Unnumbered(*disc), vec![]);
        }
        Ok(Self {
            parent,
            copies,
            ranges,
            unnumbered,
        })
    }
    fn contains(entry: &MediaEntry, position: &ChildPosition) -> bool {
        match (&entry.data.kind, position) {
            (EntryKind::Episode { episodes }, ChildPosition::Episode { season, episode }) => {
                episodes.iter().any(|s| {
                    s.season == *season
                        && s.episode <= *episode
                        && *episode <= s.episode_end.unwrap_or(s.episode)
                })
            }
            (
                EntryKind::Track {
                    disc,
                    track: Some(track),
                },
                ChildPosition::Track { disc: d, track: t },
            ) => disc == d && track == t,
            (
                EntryKind::Track { disc, track: None },
                ChildPosition::UnnumberedTrack { disc: d, entry_id },
            ) => disc == d && entry.id == *entry_id,
            _ => false,
        }
    }
    fn resolve(&self, position: ChildPosition) -> Result<(LibraryChild, Vec<MediaEntry>)> {
        let mut renditions = Vec::new();
        let mut representative = None;
        for copy in &self.copies {
            for entry in &copy.entries {
                if Self::contains(entry, &position) {
                    representative.get_or_insert((copy, entry));
                    renditions.push(entry.clone());
                }
            }
        }
        let (copy, entry) = representative.ok_or(crate::NotFound)?;
        let descriptions = copy
            .children
            .as_ref()
            .map(|catalogue| catalogue.children.as_slice())
            .unwrap_or_default();
        let matches: Vec<_> = descriptions
            .iter()
            .filter(|d| match &position {
                ChildPosition::Episode {
                    season: Some(s),
                    episode,
                } => d.position.season == Some(*s) && d.position.episode == Some(*episode),
                ChildPosition::Episode {
                    season: None,
                    episode,
                } => {
                    d.position.absolute == Some(*episode)
                        || (d.position.season.is_none() && d.position.episode == Some(*episode))
                }
                ChildPosition::Track { disc, track } => {
                    d.position.track == Some(*track) && (disc.is_none() || d.position.disc == *disc)
                }
                ChildPosition::UnnumberedTrack { .. } => false,
            })
            .collect();
        let matched = (matches.len() == 1).then(|| matches[0]);
        let fallback_title = if let ChildPosition::Episode { episode, .. } = &position
            && let EntryKind::Episode { episodes } = &entry.data.kind
            && (episodes.len() > 1
                || episodes
                    .iter()
                    .any(|s| s.episode_end.is_some_and(|end| end != s.episode)))
        {
            format!("Episode {episode}")
        } else {
            entry.data.title.clone()
        };
        let title = matched
            .map(|d| d.title.as_str())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&fallback_title)
            .to_owned();
        let description = matched.map(|d| d.description.clone()).unwrap_or_default();
        let mut provenance = BTreeMap::from([("title".into(), "detected".into())]);
        if let Some(source) =
            matched.and_then(|_| copy.children.as_ref().map(|catalogue| &catalogue.record_id))
        {
            for (field, supplied) in [
                ("title", matched.is_some_and(|d| !d.title.trim().is_empty())),
                ("overview", description.overview.is_some()),
                ("rating", description.rating.is_some()),
                ("release_date", description.release_date.is_some()),
                ("artwork", description.artwork.is_some()),
                ("original_title", description.original_title.is_some()),
                ("original_language", description.original_language.is_some()),
                ("genres", description.genres.is_some()),
                ("cast", description.cast.is_some()),
            ] {
                if supplied {
                    provenance.insert(field.into(), source.clone());
                }
            }
        }
        // Child cards can show the parent's poster/title too. Preserve those
        // credits and add the provider that supplies this child's own fields.
        let mut providers = self.parent.metadata.providers.clone();
        if let Some(catalogue) = copy.children.as_ref().filter(|_| matched.is_some()) {
            providers.insert(catalogue.record_id.clone(), catalogue.provider.clone());
        }
        Ok((
            LibraryChild {
                id: ChildId {
                    parent: self.parent.id.clone(),
                    position: position.clone(),
                }
                .encode(),
                parent_id: self.parent.id.clone(),
                position,
                title,
                artist: entry
                    .data
                    .artist
                    .clone()
                    .or_else(|| self.parent.artist.clone()),
                metadata: ResolvedDescription {
                    description,
                    provenance,
                    providers,
                },
                representative_id: copy.id.clone(),
                source_count: renditions.len(),
            },
            renditions,
        ))
    }
}
/// A rendition and its exact physical addresses, read with membership and child
/// coverage in one transaction. The hub owns connection/codec decisions.
#[derive(Debug, Clone)]
pub struct PlaybackRendition {
    pub entry: MediaEntry,
    pub mediahost_id: String,
    pub collection_id: String,
    pub files: Vec<FileInfo>,
    pub downloaded_subtitles: Vec<DownloadedSubtitle>,
    /// Source-owned observations captured in the same read transaction as files.
    pub segment_facts: BTreeMap<String, kahawai_proto::v1::SegmentDetectionResult>,
}
#[derive(Debug, Clone)]
pub struct PlaybackItem {
    pub parent_id: String,
    pub track: bool,
    pub renditions: Vec<PlaybackRendition>,
}
impl Store {
    pub async fn playback_item(&self, library: &str, item: &str) -> Result<PlaybackItem> {
        use sqlx::Row;
        let mut tx = self.db.read_pool().begin().await?;
        let (parent_id, track, entries) = if item.starts_with("child1:") {
            let key = ChildId::parse(item)?;
            let snapshot = Snapshot::read(&mut tx, library, &key.parent).await?;
            let (child, entries) = snapshot.resolve(key.position)?;
            (
                key.parent,
                !matches!(child.position, ChildPosition::Episode { .. }),
                entries,
            )
        } else {
            let parent = crate::library::read_item(&mut tx, library, item).await?;
            let mut entries = Vec::new();
            if parent.kind == LibraryItemKind::Movie {
                for copy in parent.copy_ids {
                    entries.extend(crate::occurrence::read_entries(&mut tx, &copy).await?);
                }
            }
            (item.to_owned(), false, entries)
        };
        let mut renditions = Vec::new();
        for entry in entries {
            let (mediahost_id, collection_id): (String, String) = sqlx::query_as(
                "SELECT c.mediahost_id,c.remote_id FROM collection_items i JOIN collections c ON c.id=i.collection_id WHERE i.id=?")
                .bind(&entry.item_id).fetch_one(&mut *tx).await?;
            let rows = sqlx::query("SELECT f.*,r.token FROM media_parts p JOIN files f ON f.id=p.file_id JOIN collection_roots r ON r.id=f.root_id WHERE p.entry_id=? ORDER BY p.ordinal")
                .bind(&entry.id).fetch_all(&mut *tx).await?;
            let mut files = rows
                .into_iter()
                .map(|r| {
                    Ok(FileInfo {
                        id: r.get("id"),
                        root_id: r.get("root_id"),
                        root_token: r.get("token"),
                        path: r.get("path"),
                        size: r.get::<Option<i64>, _>("size").map(|v| v as u64),
                        mtime: r.get("mtime"),
                        head_hash: crate::catalogue::hash(r.get("head_hash"))?,
                        tail_hash: crate::catalogue::hash(r.get("tail_hash"))?,
                        oshash: crate::catalogue::hash(r.get("oshash"))?,
                        media: r
                            .get::<Option<&str>, _>("media_json")
                            .map(serde_json::from_str)
                            .transpose()?,
                        item_id: Some(entry.item_id.clone()),
                        mapping_error: r.get("mapping_error"),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let mut segment_facts = BTreeMap::new();
            for file in &mut files {
                let Some(info) = &mut file.media else {
                    continue;
                };
                let facts = sqlx::query("SELECT kind,payload FROM source_facts WHERE file_id=?")
                    .bind(&file.id)
                    .fetch_all(&mut *tx)
                    .await?;
                for row in facts {
                    match SourceFact::decode(row.get("kind"), row.get("payload"))? {
                        SourceFact::Segments(fact) => {
                            segment_facts.insert(file.id.clone(), fact);
                        }
                        SourceFact::Attachments(fact) => {
                            info.attachments = Some(serde_json::from_str(&fact.attachments_json)?);
                            if let Some(chapters) = fact.chapters_json {
                                info.chapters = Some(serde_json::from_str(&chapters)?);
                            }
                        }
                        SourceFact::Keyframe(fact) => {
                            for video in &mut info.video {
                                video.max_keyframe_interval_ms = fact.max_keyframe_interval_ms;
                            }
                        }
                        SourceFact::Geometry(fact) if fact.error.is_empty() => {
                            let geometry: Vec<kahawai_core::media::VideoGeometry> =
                                serde_json::from_str(&fact.geometry_json)?;
                            if geometry.len() == info.video.len() {
                                for (video, geometry) in info.video.iter_mut().zip(geometry) {
                                    video.pixel_aspect_ratio = Some(geometry.pixel_aspect_ratio);
                                    video.orientation = Some(geometry.orientation);
                                    video.display_width = Some(geometry.display_width);
                                    video.display_height = Some(geometry.display_height);
                                }
                                info.video_geometry_probed = true;
                                info.video_geometry_error = None;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let downloaded_subtitles =
                crate::subtitles::read_downloaded(&mut tx, &entry.id, &files).await?;
            renditions.push(PlaybackRendition {
                downloaded_subtitles,
                segment_facts,
                entry,
                mediahost_id,
                collection_id,
                files,
            });
        }
        tx.commit().await?;
        Ok(PlaybackItem {
            parent_id,
            track,
            renditions,
        })
    }
}
impl Store {
    pub async fn library_child(&self, library: &str, child: &str) -> Result<LibraryChildDetail> {
        let key = ChildId::parse(child)?;
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let snapshot = Snapshot::read(&mut tx, library, &key.parent).await?;
        let (child, renditions) = snapshot.resolve(key.position)?;
        let result = LibraryChildDetail {
            child,
            parent: snapshot.parent,
            renditions,
        };
        tx.commit().await?;
        Ok(result)
    }
    /// Find the next physical episode without expanding coverage intervals.
    /// The caller supplies its sequence anchor and already-finished positions;
    /// user history itself remains outside mediadb.
    pub async fn next_episode(
        &self,
        library: &str,
        parent: &str,
        after: (Option<u32>, u32),
        finished: &std::collections::BTreeSet<(Option<u32>, u32)>,
    ) -> Result<Option<LibraryChildDetail>> {
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let snapshot = Snapshot::read(&mut tx, library, parent).await?;
        let mut position = None;
        'groups: for (group, intervals) in &snapshot.ranges {
            let Group::Episode(season) = group else {
                continue;
            };
            if *season < after.0 {
                continue;
            }
            for &(start, end) in intervals {
                let mut n = u64::from(start);
                if *season == after.0 {
                    n = n.max(u64::from(after.1) + 1);
                }
                while n <= u64::from(end) {
                    if !finished.contains(&(*season, n as u32)) {
                        position = Some(ChildPosition::Episode {
                            season: *season,
                            episode: n as u32,
                        });
                        break 'groups;
                    }
                    n += 1;
                }
            }
        }
        let result = if let Some(position) = position {
            let (child, renditions) = snapshot.resolve(position)?;
            Some(LibraryChildDetail {
                child,
                parent: snapshot.parent,
                renditions,
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }
    /// Validate child membership together, against one physical-source snapshot.
    pub async fn existing_children(
        &self,
        library: &str,
        parent: &str,
        ids: &[String],
    ) -> Result<Vec<ChildId>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let snapshot = Snapshot::read(&mut tx, library, parent).await?;
        let found = ids
            .iter()
            .filter_map(|id| ChildId::parse(id).ok())
            .filter(|id| {
                id.parent == parent
                    && snapshot.copies.iter().any(|copy| {
                        copy.entries
                            .iter()
                            .any(|entry| Snapshot::contains(entry, &id.position))
                    })
            })
            .collect();
        tx.commit().await?;
        Ok(found)
    }
    pub async fn library_children(
        &self,
        library: &str,
        parent: &str,
        offset: u64,
        limit: u32,
        filter: &ChildFilter,
    ) -> Result<LibraryChildren> {
        ensure!((1..=200).contains(&limit), "invalid child page size");
        ensure!(
            !(filter.season.is_some() && filter.disc.is_some()) && filter.disc != Some(Some(0)),
            "invalid child group"
        );
        let mut c = self.db.read_pool().acquire().await?;
        let mut tx = c.begin().await?;
        let snapshot = Snapshot::read(&mut tx, library, parent).await?;
        let mut groups = Vec::new();
        let mut children = Vec::new();
        let mut skip = offset;
        let mut total = 0;
        for (group, intervals) in &snapshot.ranges {
            let (kind, number, allowed) = match group {
                Group::Episode(season) => (
                    "episode",
                    *season,
                    filter.disc.is_none() && filter.season.is_none_or(|s| s == *season),
                ),
                Group::Track(disc) => (
                    "track",
                    *disc,
                    filter.season.is_none() && filter.disc.is_none_or(|d| d == *disc),
                ),
                Group::Unnumbered(disc) => (
                    "track",
                    *disc,
                    filter.season.is_none() && filter.disc.is_none_or(|d| d == *disc),
                ),
            };
            let count = if let Group::Unnumbered(disc) = group {
                snapshot.unnumbered[disc].len() as u64
            } else {
                intervals
                    .iter()
                    .map(|&(a, b)| u64::from(b) - u64::from(a) + 1)
                    .sum()
            };
            if let Some(g) = groups
                .iter_mut()
                .find(|g: &&mut ChildGroup| g.kind == kind && g.number == number)
            {
                g.total += count;
            } else {
                groups.push(ChildGroup {
                    kind: kind.into(),
                    number,
                    total: count,
                });
            }
            if !allowed {
                continue;
            }
            total += count;
            if skip >= count {
                skip -= count;
                continue;
            }
            if let Group::Unnumbered(disc) = group {
                for id in snapshot.unnumbered[disc]
                    .iter()
                    .skip(skip as usize)
                    .take(limit as usize - children.len())
                {
                    children.push(
                        snapshot
                            .resolve(ChildPosition::UnnumberedTrack {
                                disc: *disc,
                                entry_id: id.clone(),
                            })?
                            .0,
                    );
                }
                skip = 0;
            } else {
                for &(start, end) in intervals {
                    let count = u64::from(end) - u64::from(start) + 1;
                    if skip >= count {
                        skip -= count;
                        continue;
                    }
                    let from = u64::from(start) + skip;
                    skip = 0;
                    let take =
                        (u64::from(end) - from + 1).min(u64::from(limit) - children.len() as u64);
                    for n in from..from + take {
                        let position = match group {
                            Group::Episode(season) => ChildPosition::Episode {
                                season: *season,
                                episode: n as u32,
                            },
                            Group::Track(disc) => ChildPosition::Track {
                                disc: *disc,
                                track: n as u32,
                            },
                            Group::Unnumbered(_) => unreachable!(),
                        };
                        children.push(snapshot.resolve(position)?.0);
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(LibraryChildren {
            children,
            total,
            groups,
            offset,
            limit,
        })
    }
}
