//! `media_entry_episodes` records the numbered episode spans covered by each
//! physical media entry; logical episode identities are derived separately.
//! Music disc and track positions are positive numbers or unknown. Automatic
//! detection treats zero as unknown in both tags and filename/directory guesses;
//! the original file metadata remains intact even when its position is unknown.
use crate::*;
use anyhow::{Result, ensure};
use kahawai_core::{media::MediaInfo, names};
use sqlx::{Row, SqliteConnection};
use std::collections::HashSet;

impl Store {
    /// Explicit grouping for layouts the automatic resolver cannot identify.
    /// Reuses an occurrence ID and adds/replaces named entries transactionally.
    pub async fn put_occurrence(&self, occurrence: &NewOccurrence) -> Result<String> {
        let mut tx = self.db.begin().await?;
        let result = put(&mut tx, occurrence, true).await?;
        prune(&mut tx).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn collection_items(&self, collection: &str) -> Result<Vec<CollectionItem>> {
        let rows=sqlx::query("SELECT i.*,a.record_id FROM collection_items i LEFT JOIN metadata_assignments a ON a.item_id=i.id WHERE collection_id=? ORDER BY i.id")
            .bind(collection).fetch_all(self.db.read_pool()).await?;
        rows.into_iter()
            .map(|r| {
                Ok(CollectionItem {
                    id: r.get("id"),
                    library_item_id: r.get("library_item_id"),
                    collection_id: r.get("collection_id"),
                    root_id: r.get("root_id"),
                    occurrence: r.get("occurrence"),
                    detected: DetectedMetadata {
                        title: r.get("title"),
                        year: r.get("year"),
                        artist: r.get("artist"),
                        description: serde_json::from_str(r.get("description_json"))?,
                    },
                    selected_record: r.get("record_id"),
                })
            })
            .collect()
    }
    pub async fn media_entries(&self, item: &str) -> Result<Vec<MediaEntry>> {
        let mut c = self.db.read_pool().acquire().await?;
        use sqlx::Connection;
        let mut tx = c.begin().await?;
        let entries = read_entries(&mut tx, item).await?;
        tx.commit().await?;
        Ok(entries)
    }
}
pub(crate) async fn read_entries(c: &mut SqliteConnection, item: &str) -> Result<Vec<MediaEntry>> {
    let rows = sqlx::query("SELECT * FROM media_entries WHERE item_id=? ORDER BY id")
        .bind(item)
        .fetch_all(&mut *c)
        .await?;
    let mut entries = vec![];
    for r in rows {
        let id: String = r.get("id");
        let parts = sqlx::query(
            "SELECT file_id,ordinal FROM media_parts WHERE entry_id=? ORDER BY ordinal",
        )
        .bind(&id)
        .fetch_all(&mut *c)
        .await?
        .into_iter()
        .map(|p| Part {
            file_id: p.get("file_id"),
            ordinal: p.get::<i64, _>("ordinal") as u32,
        })
        .collect();
        let kind = match r.get::<&str, _>("kind") {
            "movie" => EntryKind::Movie,
            "track" => EntryKind::Track {
                disc: r.get::<Option<i64>, _>("disc").map(|n| n as u32),
                track: r.get::<Option<i64>, _>("track").map(|n| n as u32),
            },
            _ => {
                let rows=sqlx::query("SELECT season,episode,episode_end FROM media_entry_episodes WHERE entry_id=? ORDER BY ordinal")
                        .bind(&id).fetch_all(&mut *c).await?;
                EntryKind::Episode {
                    episodes: rows
                        .into_iter()
                        .map(|e| EpisodeSpan {
                            season: e.get::<Option<i64>, _>("season").map(|n| n as u32),
                            episode: e.get::<i64, _>("episode") as u32,
                            episode_end: e.get::<Option<i64>, _>("episode_end").map(|n| n as u32),
                        })
                        .collect(),
                }
            }
        };
        entries.push(MediaEntry {
            id,
            item_id: item.into(),
            data: NewEntry {
                occurrence: r.get("occurrence"),
                title: r.get("title"),
                artist: r.get("artist"),
                kind,
                parts,
            },
        });
    }
    Ok(entries)
}
pub(crate) async fn prune(c: &mut SqliteConnection) -> Result<()> {
    sqlx::query("DELETE FROM media_entries WHERE NOT EXISTS(SELECT 1 FROM media_parts WHERE entry_id=media_entries.id)").execute(&mut *c).await?;
    sqlx::query("DELETE FROM collection_items WHERE NOT EXISTS(SELECT 1 FROM media_entries WHERE item_id=collection_items.id)").execute(&mut *c).await?;
    Ok(())
}
pub(crate) async fn put(
    c: &mut SqliteConnection,
    value: &NewOccurrence,
    manual: bool,
) -> Result<String> {
    ensure!(
        !value.occurrence.is_empty() && !value.entries.is_empty(),
        "empty occurrence"
    );
    let media_type: String = sqlx::query_scalar("SELECT media_type FROM collections WHERE id=?")
        .bind(&value.collection_id)
        .fetch_one(&mut *c)
        .await?;
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM collection_items WHERE root_id=? AND occurrence=?")
            .bind(&value.root_id)
            .bind(&value.occurrence)
            .fetch_optional(&mut *c)
            .await?;
    let item = existing.unwrap_or_else(id);
    let library_item = crate::library::identity(
        c,
        &item,
        &media_type,
        &value.detected.title,
        value.detected.year,
    )
    .await?;
    sqlx::query("INSERT INTO collection_items(id,collection_id,root_id,occurrence,title,year,library_item_id,artist,description_json,manual) VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,year=excluded.year,library_item_id=excluded.library_item_id,artist=excluded.artist,description_json=excluded.description_json,manual=MAX(collection_items.manual,excluded.manual)")
        .bind(&item).bind(&value.collection_id).bind(&value.root_id).bind(&value.occurrence).bind(&value.detected.title).bind(value.detected.year).bind(library_item)
        .bind(&value.detected.artist).bind(serde_json::to_string(&value.detected.description)?).bind(manual).execute(&mut *c).await?;
    let mut entry_keys = HashSet::new();
    let mut files = HashSet::new();
    for entry in &value.entries {
        ensure!(
            !entry.occurrence.is_empty()
                && entry_keys.insert(&entry.occurrence)
                && !entry.parts.is_empty(),
            "empty or duplicate media entry"
        );
        let (kind, disc, track) = match &entry.kind {
            EntryKind::Movie => {
                ensure!(
                    matches!(media_type.as_str(), "movies" | "anime"),
                    "movie in incompatible collection"
                );
                ("movie", None, None)
            }
            EntryKind::Episode { episodes } => {
                ensure!(
                    matches!(media_type.as_str(), "series" | "anime") && !episodes.is_empty(),
                    "episode in incompatible collection or missing coverage"
                );
                ("episode", None, None)
            }
            EntryKind::Track { disc, track } => {
                ensure!(media_type == "music", "track in incompatible collection");
                ("track", *disc, *track)
            }
        };
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM media_entries WHERE item_id=? AND occurrence=?")
                .bind(&item)
                .bind(&entry.occurrence)
                .fetch_optional(&mut *c)
                .await?;
        let entry_id = existing.unwrap_or_else(id);
        sqlx::query("INSERT INTO media_entries VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,title=excluded.title,artist=excluded.artist,disc=excluded.disc,track=excluded.track")
            .bind(&entry_id).bind(&value.collection_id).bind(&item).bind(&entry.occurrence).bind(kind).bind(&entry.title).bind(&entry.artist).bind(disc.map(i64::from)).bind(track.map(i64::from)).execute(&mut *c).await?;
        sqlx::query("DELETE FROM media_entry_episodes WHERE entry_id=?")
            .bind(&entry_id)
            .execute(&mut *c)
            .await?;
        if let EntryKind::Episode { episodes } = &entry.kind {
            for (position, episode) in episodes.iter().enumerate() {
                let end = episode.episode_end.unwrap_or(episode.episode);
                ensure!(end >= episode.episode, "reversed episode coverage");
                ensure!(
                    !episodes[..position]
                        .iter()
                        .any(|old| old.season == episode.season
                            && old.episode <= end
                            && old.episode_end.unwrap_or(old.episode) >= episode.episode),
                    "overlapping episode coverage"
                );
                sqlx::query("INSERT INTO media_entry_episodes VALUES(?,?,?,?,?,?)")
                    .bind(&entry_id)
                    .bind(position as i64 + 1)
                    .bind(if episode.season.is_some() {
                        "season"
                    } else {
                        "absolute"
                    })
                    .bind(episode.season.map(i64::from))
                    .bind(i64::from(episode.episode))
                    .bind(episode.episode_end.map(i64::from))
                    .execute(&mut *c)
                    .await?;
            }
        }
        if manual {
            sqlx::query("DELETE FROM media_parts WHERE entry_id=?")
                .bind(&entry_id)
                .execute(&mut *c)
                .await?;
        }
        for part in &entry.parts {
            ensure!(
                files.insert(&part.file_id),
                "file appears twice in occurrence"
            );
            let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM files WHERE id=? AND root_id=? AND collection_id=? AND media_json IS NOT NULL)")
                .bind(&part.file_id).bind(&value.root_id).bind(&value.collection_id).fetch_one(&mut *c).await?;
            ensure!(
                valid,
                "media part belongs to a different occurrence root or has no probe"
            );
            sqlx::query("UPDATE files SET mapping_error=NULL WHERE id=?")
                .bind(&part.file_id)
                .execute(&mut *c)
                .await?;
            sqlx::query("DELETE FROM media_parts WHERE file_id=?")
                .bind(&part.file_id)
                .execute(&mut *c)
                .await?;
            sqlx::query("INSERT INTO media_parts VALUES(?,?,?,?)")
                .bind(&entry_id)
                .bind(&value.collection_id)
                .bind(i64::from(part.ordinal))
                .bind(&part.file_id)
                .execute(&mut *c)
                .await?;
        }
    }
    Ok(item)
}

pub(crate) async fn resolve_file(
    c: &mut SqliteConnection,
    collection: &str,
    root: &str,
    file: &str,
    path: &str,
    media: &MediaInfo,
) -> Result<()> {
    let manual:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_parts p JOIN media_entries e ON e.id=p.entry_id JOIN collection_items i ON i.id=e.item_id WHERE p.file_id=? AND i.manual=1)")
        .bind(file).fetch_one(&mut *c).await?;
    if manual {
        return Ok(());
    }
    let kind: String = sqlx::query_scalar("SELECT media_type FROM collections WHERE id=?")
        .bind(collection)
        .fetch_one(&mut *c)
        .await?;
    if let Some(mut value) = detect(MediaType::parse(&kind)?, path, media) {
        value.collection_id = collection.into();
        value.root_id = root.into();
        value.entries[0].parts[0].file_id = file.into();
        put(c, &value, false).await?;
    } else {
        sqlx::query("UPDATE files SET mapping_error=? WHERE id=?")
            .bind("Filename and tags do not identify a title; attach this source explicitly")
            .bind(file)
            .execute(&mut *c)
            .await?;
        sqlx::query("DELETE FROM media_parts WHERE file_id=?")
            .bind(file)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}
fn detect(kind: MediaType, path: &str, media: &MediaInfo) -> Option<NewOccurrence> {
    let mut description = Description::default();
    if let Some(art) = &media.artwork {
        description.artwork = Some(vec![art.clone()]);
    }
    let mut detected = DetectedMetadata {
        title: String::new(),
        year: None,
        artist: None,
        description,
    };
    let mut entry = NewEntry {
        occurrence: path.into(),
        title: String::new(),
        artist: None,
        kind: EntryKind::Movie,
        parts: vec![Part {
            file_id: String::new(),
            ordinal: 1,
        }],
    };
    let occurrence = if kind == MediaType::Music {
        let (parent, disc_folder) = owner_directory(path, true);
        let parsed_path = if disc_folder.is_some() {
            format!("{parent}/{}", path.rsplit('/').next()?)
        } else {
            path.into()
        };
        let guess = names::parse_music(&parsed_path);
        let tag = |name: &str| {
            media
                .tags
                .get(name)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        detected.title = tag("album")
            .map(str::to_owned)
            .or_else(|| guess.as_ref().map(|g| g.album.clone()))?;
        let artist = tag("album_artist")
            .map(str::to_owned)
            .or_else(|| guess.as_ref().map(|g| g.artist.clone()))
            .or_else(|| {
                parent
                    .rsplit_once('/')
                    .and_then(|(artist_path, _)| artist_path.rsplit('/').next())
                    .filter(|artist| !artist.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| tag("artist").map(str::to_owned));
        detected.artist = artist;
        detected.year = tag("date")
            .or_else(|| tag("year"))
            .and_then(|s| s.get(..4))
            .and_then(|s| s.parse().ok())
            .or_else(|| guess.as_ref().and_then(|g| g.album_year.map(|y| y as i32)));
        entry.artist = tag("artist")
            .map(str::to_owned)
            .or_else(|| guess.as_ref().map(|g| g.artist.clone()));
        entry.title = tag("title")
            .map(str::to_owned)
            .or_else(|| guess.as_ref().map(|g| g.title.clone()))
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).into());
        let number = |name: &str| {
            tag(name)
                .and_then(|v| v.split('/').next())
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|n| *n > 0)
        };
        entry.kind = EntryKind::Track {
            disc: number("disc_number").or(disc_folder.filter(|n| *n > 0)),
            track: number("track_number")
                .or_else(|| guess.as_ref().map(|g| g.track).filter(|n| *n > 0)),
        };
        // A physical album directory is authoritative. Flat tagged tracks have
        // no directory boundary, so their exact album/artist tags partition it.
        if parent.is_empty() {
            format!(
                "flat-album:{}",
                serde_json::to_string(&(&detected.artist, &detected.title)).ok()?
            )
        } else {
            format!("album:{parent}")
        }
    } else {
        let episode = if kind == MediaType::Anime {
            names::parse_anime(path)
        } else if kind == MediaType::Series {
            names::parse_episode(path)
        } else {
            None
        };
        if let Some(guess) = episode {
            detected.title = guess.show_title;
            detected.year = guess.show_year.map(i32::from);
            entry.title = guess
                .episode_title
                .unwrap_or_else(|| format!("Episode {}", guess.episode));
            let end = guess.episode_end.unwrap_or(guess.episode);
            if end < guess.episode {
                return None;
            }
            entry.kind = EntryKind::Episode {
                episodes: vec![EpisodeSpan {
                    season: guess.season,
                    episode: guess.episode,
                    episode_end: guess.episode_end,
                }],
            };
            let (dir, _) = owner_directory(path, false);
            if dir.is_empty() {
                format!("flat-series:{}", detected.title)
            } else {
                format!("series:{dir}")
            }
        } else {
            if kind == MediaType::Series {
                return None;
            }
            let (family, part) = multipart(path);
            let filename = path.rsplit('/').next()?;
            let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
            let bare_part = numbered_component(stem, false).is_some();
            let guess = if part.is_some() && bare_part {
                let directory = path
                    .split('/')
                    .rev()
                    .skip(1)
                    .find(|p| numbered_component(p, false).is_none())?;
                names::parse_movie_file(&format!("{directory}.mkv"))?
            } else {
                names::parse_movie_file(path)?
            };
            detected.title = guess.title;
            detected.year = guess.year.map(i32::from);
            entry.title = detected.title.clone();
            if let Some(part) = part {
                entry.parts[0].ordinal = part;
                entry.occurrence = family.clone();
                format!("multipart:{family}")
            } else {
                format!("movie:{path}")
            }
        }
    };
    Some(NewOccurrence {
        collection_id: String::new(),
        root_id: String::new(),
        occurrence,
        detected,
        entries: vec![entry],
    })
}
/// Only a whole CD/disc/disk/part directory is structural; never strip ordinary
/// title words such as "Deathly Hallows Part 2".
fn numbered_component(value: &str, season: bool) -> Option<u32> {
    let folded = value.to_lowercase().replace([' ', '_', '-'], "");
    let prefixes: &[&str] = if season {
        &["season"]
    } else {
        &["cd", "disc", "disk", "part"]
    };
    prefixes
        .iter()
        .find_map(|p| folded.strip_prefix(p).and_then(|s| s.parse().ok()))
}
fn owner_directory(path: &str, music: bool) -> (String, Option<u32>) {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    let marker = parts
        .iter()
        .enumerate()
        .find_map(|(i, p)| numbered_component(p, !music).map(|n| (i, n)));
    if let Some((at, n)) = marker {
        (parts[..at].join("/"), Some(n))
    } else {
        (parts.join("/"), None)
    }
}
/// Retain exact release punctuation and case. Remove only a recognized part
/// marker; the collection root supplies the other half of this physical key.
fn multipart(path: &str) -> (String, Option<u32>) {
    let mut components: Vec<String> = path.split('/').map(str::to_owned).collect();
    let mut part = None;
    let directory_count = components.len().saturating_sub(1);
    for component in components.iter_mut().take(directory_count) {
        if let Some(n) = numbered_component(component, false).filter(|n| *n > 0) {
            *component = "{part}".into();
            part = Some(n);
        }
    }
    if let Some(file) = components.last_mut() {
        let end = file.rfind('.').unwrap_or(file.len());
        let stem = &file[..end];
        let mut start = 0;
        let mut tokens = Vec::new();
        for (i, ch) in stem.char_indices() {
            if !ch.is_alphanumeric() {
                if start < i {
                    tokens.push((start, i));
                }
                start = i + ch.len_utf8();
            }
        }
        if start < stem.len() {
            tokens.push((start, stem.len()));
        }
        for (index, (a, b)) in tokens.iter().copied().enumerate() {
            let token = &stem[a..b];
            let mut marker = numbered_component(token, false).map(|n| (n, b));
            if (["cd", "disc", "disk"].contains(&token.to_lowercase().as_str())
                || (token.eq_ignore_ascii_case("part") && tokens.len() == 2 && index == 0))
                && let Some((na, nb)) = tokens.get(index + 1)
            {
                marker = stem[*na..*nb].parse().ok().map(|n| (n, *nb));
            }
            // A bare partN filename is structural; a title suffix PartN is not.
            if token.to_lowercase().starts_with("part")
                && tokens.len() != 1
                && !(tokens.len() == 2 && index == 0)
            {
                continue;
            }
            if let Some((n, end)) = marker.filter(|(n, _)| *n > 0) {
                file.replace_range(a..end, "{part}");
                part = Some(n);
                break;
            }
        }
    }
    (components.join("/"), part)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multipart_preserves_release_boundaries() {
        assert_eq!(
            multipart("Film.A.CD1.mkv"),
            ("Film.A.{part}.mkv".into(), Some(1))
        );
        assert_eq!(
            multipart("Film.A.CD2.mkv"),
            ("Film.A.{part}.mkv".into(), Some(2))
        );
        assert_ne!(multipart("Film.A.CD1.mkv").0, multipart("Film.B.CD2.mkv").0);
        assert_ne!(multipart("Film-A.CD1.mkv").0, multipart("Film.A.CD2.mkv").0);
        assert_eq!(
            multipart("Film/CD2/film.avi"),
            ("Film/{part}/film.avi".into(), Some(2))
        );
        assert_eq!(multipart("Deathly Hallows Part 2.mkv").1, None);
    }
}
