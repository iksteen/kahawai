//! Generated artifacts stay in the subtitle cache. Mediadb supplies physical
//! revisions and probes; no shadow catalogue or generated-track table is needed.
//! Keys include the source revision and parent stream (or acquired subtitle ID),
//! never a library item. OCR is idle work; rasterisation is demanded by the ASS
//! ladder. Both remain retained: rebuilding is expensive and playback needs them
//! without repeating extraction/rendering. Atomic publication also records empty
//! OCR answers. Sessions capture immutable raster paths and OCR text.
use super::*;
use crate::sessions::FileId;
use crate::{sessions::PartSource, tracks::Track};
use std::path::Path;

pub(crate) fn key(
    part: &PartSource,
    info: &kahawai_core::media::MediaInfo,
    parent: &str,
) -> String {
    use sha2::Digest;
    let mut info = info.clone();
    // The scanner's companion-file revision must not invalidate expensive
    // embedded/downloaded artifacts. Only external subtitle parents read it.
    if !parent.starts_with("sidecar:") {
        info.sidecar_revision = None;
    }
    info.external_subtitles
        .retain(|s| !s.path_rel.starts_with("mediadb-download:"));
    let identity = format!(
        "{:?}:{}:{}:{}:{}:{parent}:{}",
        part.file_id,
        part.size,
        part.mtime_unix,
        part.head_xxh3,
        part.tail_xxh3,
        serde_json::to_string(&info).expect("probe serializes")
    );
    data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(identity.as_bytes()))
}
// v1 artifacts may have been built from path-only extraction caches. They
// cannot be promoted safely, even when their outer source revision matches.
fn path(dir: &Path, parent: &Track, kind: &str) -> Result<PathBuf> {
    Ok(dir.join(format!(
        "derived-v2-{}-{kind}",
        parent
            .artifact_key
            .as_deref()
            .context("missing physical artifact identity")?
    )))
}
fn derived(parent: &Track, raster: bool) -> Result<Track> {
    let mut track = parent.clone();
    // Source-local streams occupy the low positive IDs; downloads are negative.
    // A disjoint, deterministic range avoids a database row just to assign IDs.
    // Reserve two bits for origin/sign and stay inside JavaScript's exact range.
    track.id = parent
        .id
        .checked_abs()
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| {
            n.checked_add((1i64 << 40) + i64::from(parent.id < 0) * 2 + i64::from(raster))
        })
        .filter(|n| *n < (1i64 << 53))
        .context("derived subtitle ID overflow")?;
    track.origin = if raster { "raster" } else { "ocr" }.into();
    track.format = if raster { "raster" } else { "srt" }.into();
    track.derived_from = Some(parent.id);
    track.machine = !raster;
    track.created_by = None;
    track.acquired = None;
    track.label = None;
    Ok(track)
}
pub(crate) fn cached(dir: &Path, parents: &[Track]) -> Result<Vec<Track>> {
    let mut out = vec![];
    for parent in parents {
        if crate::tracks::is_image_format(&parent.format) {
            match std::fs::read(path(dir, parent, "ocr.json")?) {
                Ok(bytes) => {
                    let text: Extracted = serde_json::from_slice(&bytes)?;
                    if !text.cues.is_empty() {
                        let mut track = derived(parent, false)?;
                        track.acquired = Some(Arc::new(text));
                        out.push(track);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        } else if matches!(parent.format.as_str(), "ass" | "ssa") {
            let file = path(dir, parent, "raster.jsonl")?;
            if file.try_exists()? {
                let mut track = derived(parent, true)?;
                track.raster = Some(file);
                out.push(track);
            }
        }
    }
    Ok(out)
}
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("artifact directory")?)?;
    let temp = path.with_extension("partial");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, path)?;
    Ok(())
}
impl Subtitles {
    pub(crate) async fn catalogue_raster(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        parent: &Track,
    ) -> Result<()> {
        let path = path(&self.dir, parent, "raster.jsonl")?;
        let lock = self.download_lock(path.to_string_lossy().into_owned());
        let guard = lock.lock_owned().await;
        if path.try_exists()? {
            return Ok(());
        }
        let script = self
            .ass_for_burn(registry, sessions, parent)
            .await
            .context("no ASS script to render")?;
        let (width, height, fps) = self.raster_geometry(registry, parent).await?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            // The worker owns publication and the lock. A player's wait timeout
            // drops only its waiter; rendering still lands for the next session.
            let _guard = guard;
            let mut render = kahawai_media::assraster::Raster::new(width, height)?;
            let sets = render.render(
                &script,
                fps,
                kahawai_media::assraster::script_end_ms(&script),
            )?;
            anyhow::ensure!(!sets.is_empty(), "the script rendered no display sets");
            publish(&path, &kahawai_media::assraster::to_ndjson(&sets)?)
        })
        .await??;
        Ok(())
    }
    #[cfg(feature = "ocr")]
    pub(super) async fn catalogue_ocr(
        &self,
        registry: &Registry,
        parent: &Track,
    ) -> Result<OcrGeneration> {
        let path = path(&self.dir, parent, "ocr.json")?;
        let lock = self.download_lock(path.to_string_lossy().into_owned());
        let guard = lock.lock_owned().await;
        if path.try_exists()? {
            return Ok(OcrGeneration::Generated);
        }
        let (host, collection, root, file, index, language) =
            self.extract_ref(registry, parent).await?;
        let model = crate::ocr::model_for(language.as_deref())
            .context("no OCR model installed for this language")?;
        let sets = match self
            .image_sets_state(
                registry,
                &host,
                &collection,
                &root,
                &file,
                index,
                parent.source_revision()?,
                SETS_WAIT_IDLE,
            )
            .await
        {
            ImageSetsState::Ready(path) => path,
            ImageSetsState::RetryOnReconnect => {
                return Ok(OcrGeneration::RetryOnReconnect { module_id: host });
            }
            ImageSetsState::Unavailable => bail!("display sets unavailable"),
        };
        tokio::task::spawn_blocking(move || -> Result<OcrGeneration> {
            let _guard = guard;
            let cues = crate::ocr::ocr_sets_file(&sets, &model)?;
            let empty = cues.is_empty();
            publish(&path, &serde_json::to_vec(&Extracted { cues, ass: None })?)?;
            Ok(if empty {
                OcrGeneration::NoText
            } else {
                OcrGeneration::Generated
            })
        })
        .await?
    }
    /// Files whose TEXT subtitles are not extracted yet, one entry per
    /// file: the mediahost extracts every track of a file in one pass
    /// (`tracks=38 elapsed=21.3s` is a real line from a live host), so
    /// naming a file twice would just buy the same 20 seconds twice.
    ///
    /// Sidecars are skipped — those are one small read on demand, with no
    /// container to walk. Only embedded tracks are worth warming.
    pub(super) async fn catalogue_text_prewarm(
        &self,
        registry: &Registry,
    ) -> Result<Vec<(String, String, kahawai_proto::v1::SourcePath)>> {
        let mut out = vec![];
        let mut seen = std::collections::HashSet::new();
        for summary in registry.catalogue().collection_summaries().await? {
            let c = summary.collection;
            if !registry.is_connected(&c.mediahost_id) {
                continue;
            }
            for f in registry.catalogue().files(&c.id).await? {
                let (Some(info), Some(size)) = (&f.media, f.size) else {
                    continue;
                };
                let part = PartSource {
                    file_id: FileId::Catalogue(f.id),
                    module_id: c.mediahost_id.clone(),
                    collection_id: c.remote_id.clone(),
                    root_token: f.root_token,
                    path_rel: f.path,
                    size,
                    mtime_unix: f.mtime.unwrap_or(0),
                    head_xxh3: f.head_hash.unwrap_or(0) as i64,
                    tail_xxh3: f.tail_hash.unwrap_or(0) as i64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                };
                for track in crate::sessions::catalogue::tracks(
                    f.item_id.as_deref().unwrap_or(""),
                    &part,
                    info,
                ) {
                    if crate::tracks::is_image_format(&track.format) || track.origin != "embedded" {
                        continue;
                    }
                    let (Some(source), Ok(revision)) = (&track.physical, track.source_revision())
                    else {
                        continue;
                    };
                    let cached = self
                        .dir
                        .join(format!(
                            "{}.json",
                            super::cache_key(
                                &source.module_id,
                                &source.collection_id,
                                &source.root_token,
                                &source.path_rel,
                                &track.internal_key(),
                                revision,
                            )
                        ))
                        .try_exists()?;
                    if cached {
                        continue;
                    }
                    let file = (
                        source.module_id.clone(),
                        source.collection_id.clone(),
                        source.root_token.clone(),
                        source.path_rel.clone(),
                    );
                    if seen.insert(file) {
                        out.push((
                            source.module_id.clone(),
                            source.collection_id.clone(),
                            kahawai_proto::v1::SourcePath {
                                root_token: source.root_token.clone(),
                                path_rel: source.path_rel.clone(),
                            },
                        ));
                    }
                    break;
                }
            }
        }
        Ok(out)
    }

    #[cfg(feature = "ocr")]
    pub(super) async fn catalogue_ocr_candidates(&self, registry: &Registry) -> Result<Vec<Track>> {
        let mut tracks = vec![];
        for summary in registry.catalogue().collection_summaries().await? {
            let c = summary.collection;
            if !registry.is_connected(&c.mediahost_id) {
                continue;
            }
            for f in registry.catalogue().files(&c.id).await? {
                let (Some(info), Some(size)) = (&f.media, f.size) else {
                    continue;
                };
                let part = PartSource {
                    file_id: FileId::Catalogue(f.id),
                    module_id: c.mediahost_id.clone(),
                    collection_id: c.remote_id.clone(),
                    root_token: f.root_token,
                    path_rel: f.path,
                    size,
                    mtime_unix: f.mtime.unwrap_or(0),
                    head_xxh3: f.head_hash.unwrap_or(0) as i64,
                    tail_xxh3: f.tail_hash.unwrap_or(0) as i64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                };
                for track in crate::sessions::catalogue::tracks(
                    f.item_id.as_deref().unwrap_or(""),
                    &part,
                    info,
                ) {
                    if crate::tracks::is_image_format(&track.format)
                        && !path(&self.dir, &track, "ocr.json")?.try_exists()?
                    {
                        tracks.push(track);
                    }
                }
            }
        }
        Ok(tracks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SCRIPT: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 180\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,DejaVu Sans,24,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:00.50,Default,,0,0,0,,Source subtitle\n";
    fn parent(format: &str) -> Track {
        let part = PartSource {
            file_id: FileId::Catalogue("file-a".into()),
            module_id: "host".into(),
            collection_id: "movies".into(),
            root_token: "root".into(),
            path_rel: "film.mkv".into(),
            size: 10,
            mtime_unix: 1,
            head_xxh3: 12,
            tail_xxh3: 34,
            base_ms: 0,
            duration_ms: 1000,
        };
        let info = serde_json::from_value(serde_json::json!({"container":"mkv","duration_ms":1000,"video":[{"codec":"h264","width":320,"height":180,"interlaced":false,"fps":[24,1]}],"subtitles":[{"format":format,"language":"eng"}]})).unwrap();
        crate::sessions::catalogue::tracks("movie-a", &part, &info).remove(0)
    }
    #[tokio::test]
    async fn extraction_caches_follow_source_and_sidecar_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(
            crate::db::open_in_memory().await.unwrap(),
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let sessions = Sessions::new(dir.path().join("sessions"));
        let subs = Subtitles::new(dir.path().join("subtitles"));
        for sidecar in [false, true] {
            let mut track = parent("ass");
            if sidecar {
                track.origin = "sidecar".into();
                track.physical.as_mut().unwrap().info.external_subtitles =
                    vec![kahawai_core::media::SidecarSubtitle {
                        path_rel: "film.ass".into(),
                        format: "ass".into(),
                        language: None,
                        track: None,
                    }];
            }
            let key = track.internal_key();
            let old_revision = track.source_revision().unwrap().to_owned();
            let body = Extracted {
                cues: vec![],
                ass: Some(SCRIPT.into()),
            };
            subs.store_extracted(
                "host",
                "movies",
                "root",
                "film.mkv",
                &key,
                &old_revision,
                &body,
            )
            .unwrap();
            assert_eq!(
                subs.load(&registry, &sessions, &track)
                    .await
                    .unwrap()
                    .ass
                    .as_deref(),
                Some(SCRIPT)
            );
            let source = track.physical.as_mut().unwrap();
            if sidecar {
                source.sidecar_revision.push_str("-edited");
            } else {
                source.revision.push_str("-replaced");
            }
            // Offline source makes a cache miss observable without extracting media.
            assert!(subs.load(&registry, &sessions, &track).await.is_err());
            // A late result for the previous source must not fill the new cache.
            subs.store_extracted(
                "host",
                "movies",
                "root",
                "film.mkv",
                &key,
                &old_revision,
                &body,
            )
            .unwrap();
            assert!(subs.load(&registry, &sessions, &track).await.is_err());
            subs.store_extracted(
                "host",
                "movies",
                "root",
                "film.mkv",
                &key,
                track.source_revision().unwrap(),
                &body,
            )
            .unwrap();
            assert!(subs.load(&registry, &sessions, &track).await.is_ok());
        }
        let message = kahawai_proto::v1::ImageSubtitles {
            collection_id: "movies".into(),
            source: Some(kahawai_proto::v1::SourcePath::new("root", "film.mkv")),
            source_revision: "old".into(),
            ..Default::default()
        };
        subs.store_image_sets("host", &message).await.unwrap();
        assert!(matches!(
            subs.image_sets_state(
                &registry,
                "host",
                "movies",
                "root",
                "film.mkv",
                0,
                "old",
                std::time::Duration::ZERO
            )
            .await,
            ImageSetsState::Ready(_)
        ));
        assert!(matches!(
            subs.image_sets_state(
                &registry,
                "host",
                "movies",
                "root",
                "film.mkv",
                0,
                "new",
                std::time::Duration::ZERO
            )
            .await,
            ImageSetsState::RetryOnReconnect
        ));
    }

    #[test]
    fn sidecar_revision_only_changes_external_artifact_keys() {
        let part = PartSource {
            file_id: FileId::Catalogue("file-a".into()),
            module_id: "host".into(),
            collection_id: "movies".into(),
            root_token: "root".into(),
            path_rel: "film.mkv".into(),
            size: 10,
            mtime_unix: 1,
            head_xxh3: 12,
            tail_xxh3: 34,
            base_ms: 0,
            duration_ms: 1000,
        };
        let before = kahawai_core::media::MediaInfo::default();
        let mut after = before.clone();
        after.sidecar_revision = Some("edited external subtitle".into());
        for parent in ["embedded:0", "download:42"] {
            assert_eq!(key(&part, &before, parent), key(&part, &after, parent));
        }
        assert_ne!(
            key(&part, &before, "sidecar:0"),
            key(&part, &after, "sidecar:0")
        );
    }

    #[test]
    fn cached_ocr_keeps_parent_provenance_and_empty_answers() {
        let dir = tempfile::tempdir().unwrap();
        let mut parent = parent("pgs");
        let ex = Extracted {
            cues: vec![kahawai_media::subtitles::Cue {
                start_ms: 100,
                end_ms: 500,
                text: "OCR answer".into(),
            }],
            ass: None,
        };
        publish(
            &path(dir.path(), &parent, "ocr.json").unwrap(),
            &serde_json::to_vec(&ex).unwrap(),
        )
        .unwrap();
        let track = cached(dir.path(), &[parent.clone()]).unwrap().remove(0);
        assert!(track.machine);
        assert_eq!(track.derived_from, Some(parent.id));
        assert_eq!(track.acquired.unwrap().cues[0].text, "OCR answer");
        parent.item_id = "corrected-metadata".into();
        assert_eq!(
            cached(dir.path(), &[parent.clone()]).unwrap()[0].id,
            track.id
        );
        parent.artifact_key = Some("another-source-revision".into());
        assert!(cached(dir.path(), &[parent.clone()]).unwrap().is_empty());
        let empty = path(dir.path(), &parent, "ocr.json").unwrap();
        publish(&empty, br#"{"cues":[],"ass":null}"#).unwrap();
        assert!(cached(dir.path(), &[parent]).unwrap().is_empty());
        assert!(empty.exists(), "empty OCR is an answer, not missing work");
    }
    #[tokio::test]
    async fn rasterisation_publishes_real_display_sets_once_for_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let catalogue = crate::db::open_catalogue(dir.path()).await.unwrap();
        let registry = Registry::new(db, Default::default(), catalogue);
        let sessions = Sessions::new(dir.path().join("sessions"));
        let subs = Subtitles::new(dir.path().join("subtitles"));
        let mut image = parent("vobsub");
        image.origin = "sidecar".into();
        image.physical.as_mut().unwrap().info.external_subtitles =
            vec![kahawai_core::media::SidecarSubtitle {
                path_rel: "film.idx".into(),
                format: "vobsub".into(),
                language: Some("eng".into()),
                track: Some(7),
            }];
        let reference = subs.extract_ref(&registry, &image).await.unwrap();
        assert_eq!(
            (reference.2.as_str(), reference.3.as_str(), reference.4),
            ("root", "film.idx", 7)
        );
        let mut parent = parent("ass");
        parent.acquired = Some(Arc::new(Extracted {
            cues: vec![],
            ass: Some(SCRIPT.into()),
        }));
        let (a, b) = tokio::join!(
            subs.catalogue_raster(&registry, &sessions, &parent),
            subs.catalogue_raster(&registry, &sessions, &parent)
        );
        a.unwrap();
        b.unwrap();
        let tracks = cached(subs.cache_dir(), &[parent.clone()]).unwrap();
        assert_eq!(tracks.len(), 1);
        let raster = &tracks[0];
        assert_eq!(raster.origin, "raster");
        assert!(!raster.machine);
        assert_eq!(raster.derived_from, Some(parent.id));
        let path = raster.raster.as_ref().unwrap();
        let bytes = std::fs::read_to_string(path).unwrap();
        assert!(
            bytes.lines().any(
                |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["o"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty())
            ),
            "{bytes}"
        );
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        parent.acquired = Some(Arc::new(Extracted {
            cues: vec![],
            ass: Some("invalid; cached asset must be used".into()),
        }));
        subs.catalogue_raster(&registry, &sessions, &parent)
            .await
            .unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            modified
        );
        parent.artifact_key = Some("replaced-file".into());
        assert!(cached(subs.cache_dir(), &[parent]).unwrap().is_empty());
    }
    /// A file is named once however many text tracks it carries: the
    /// mediahost extracts the whole container in one pass, so a second
    /// entry would buy the same twenty seconds twice. Image tracks belong
    /// to the OCR sweep and sidecars are read on demand, so neither counts.
    #[tokio::test]
    async fn text_prewarm_names_each_cold_file_once_and_ignores_image_tracks() {
        use kahawai_proto::v1 as p;
        use prost::Message;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let store = crate::db::open_catalogue(dir.path()).await.unwrap();
        store.put_mediahost("host", "Fixture").await.unwrap();
        store
            .offer_collection(
                "host",
                &p::CatalogCollection {
                    id: "series".into(),
                    media_type: "series".into(),
                    epoch: "epoch".into(),
                    current_version: 2,
                    roots: vec![p::CollectionRoot::new("root", "/fixture")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Three embedded text tracks on one file, and a second file whose
        // only subtitle is an image track.
        let many = p::FileRecord {
            source: Some(p::SourcePath::new("root", "Many.mkv")),
            size: 10,
            mtime_unix: 1,
            streams_json: serde_json::json!({"container":"mkv","subtitles":[
                {"format":"subrip","language":"eng"},
                {"format":"ass","language":"nld"},
                {"format":"subrip","language":"fra"}
            ]})
            .to_string(),
            ..Default::default()
        };
        let image_only = p::FileRecord {
            source: Some(p::SourcePath::new("root", "Image.mkv")),
            size: 10,
            mtime_unix: 1,
            streams_json: serde_json::json!({"container":"mkv","subtitles":[
                {"format":"pgs","language":"eng"}
            ]})
            .to_string(),
            ..Default::default()
        };
        store
            .apply_catalogue(
                "host",
                &p::CatalogDelta {
                    collection_id: "series".into(),
                    epoch: "epoch".into(),
                    snapshot: true,
                    done: true,
                    through_version: 2,
                    records: vec![
                        p::CatalogRecord {
                            version: 1,
                            kind: "file".into(),
                            key: b"root\0Many.mkv".to_vec(),
                            payload: p::FileUpsert {
                                collection_id: "series".into(),
                                files: vec![many],
                            }
                            .encode_to_vec(),
                            deleted: false,
                        },
                        p::CatalogRecord {
                            version: 2,
                            kind: "file".into(),
                            key: b"root\0Image.mkv".to_vec(),
                            payload: p::FileUpsert {
                                collection_id: "series".into(),
                                files: vec![image_only],
                            }
                            .encode_to_vec(),
                            deleted: false,
                        },
                    ],
                },
            )
            .await
            .unwrap();
        let registry = Registry::new(db, Default::default(), store);
        registry.connected("host", "mediahost", "Fixture", "fp", "test");
        let subs = Subtitles::new(dir.path().join("subtitles"));

        let cold = subs.catalogue_text_prewarm(&registry).await.unwrap();
        assert_eq!(
            cold.iter()
                .map(|(_, _, s)| s.path_rel.as_str())
                .collect::<Vec<_>>(),
            vec!["Many.mkv"],
            "one entry for the three-track file, none for the image-only one"
        );

        // Once every text track of that file is cached, it drops out.
        let (module, collection, source) = cold[0].clone();
        for key in ["e0", "e1", "e2"] {
            subs.store_extracted(
                &module,
                &collection,
                &source.root_token,
                &source.path_rel,
                key,
                revision_of(&subs, &registry, &source).await.as_str(),
                &kahawai_media::subtitles::Extracted {
                    cues: vec![],
                    ass: None,
                },
            )
            .unwrap();
        }
        assert!(
            subs.catalogue_text_prewarm(&registry)
                .await
                .unwrap()
                .is_empty(),
            "a fully cached file is not swept again"
        );
    }

    /// The revision the prewarm keyed its cache path on, read back the same
    /// way the candidate scan derives it.
    #[cfg(test)]
    async fn revision_of(
        subs: &Subtitles,
        registry: &Registry,
        source: &kahawai_proto::v1::SourcePath,
    ) -> String {
        let _ = subs;
        for summary in registry
            .catalogue()
            .collection_summaries()
            .await
            .unwrap()
            .into_iter()
        {
            for f in registry
                .catalogue()
                .files(&summary.collection.id)
                .await
                .unwrap()
            {
                if f.path != source.path_rel {
                    continue;
                }
                let (Some(info), Some(size)) = (&f.media, f.size) else {
                    continue;
                };
                let part = PartSource {
                    file_id: FileId::Catalogue(f.id),
                    module_id: summary.collection.mediahost_id.clone(),
                    collection_id: summary.collection.remote_id.clone(),
                    root_token: f.root_token,
                    path_rel: f.path,
                    size,
                    mtime_unix: f.mtime.unwrap_or(0),
                    head_xxh3: f.head_hash.unwrap_or(0) as i64,
                    tail_xxh3: f.tail_hash.unwrap_or(0) as i64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                };
                if let Some(t) = crate::sessions::catalogue::tracks(
                    f.item_id.as_deref().unwrap_or(""),
                    &part,
                    info,
                )
                .first()
                {
                    return t.source_revision().unwrap().to_string();
                }
            }
        }
        panic!("no revision for {}", source.path_rel);
    }

    #[cfg(feature = "ocr")]
    #[tokio::test]
    async fn empty_ocr_answer_removes_mediadb_source_from_idle_work_after_reopen() {
        use kahawai_proto::v1 as p;
        use prost::Message;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let store = crate::db::open_catalogue(dir.path()).await.unwrap();
        store.put_mediahost("host", "Fixture").await.unwrap();
        store
            .offer_collection(
                "host",
                &p::CatalogCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    epoch: "epoch".into(),
                    current_version: 1,
                    roots: vec![p::CollectionRoot::new("root", "/fixture")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let file = p::FileRecord {
            source:Some(p::SourcePath::new("root","Film.2000.mkv")),size:10,mtime_unix:1,
            streams_json:serde_json::json!({"container":"mkv","subtitles":[{"format":"pgs","language":"eng"}]}).to_string(), ..Default::default()
        };
        store
            .apply_catalogue(
                "host",
                &p::CatalogDelta {
                    collection_id: "movies".into(),
                    epoch: "epoch".into(),
                    snapshot: true,
                    done: true,
                    through_version: 1,
                    records: vec![p::CatalogRecord {
                        version: 1,
                        kind: "file".into(),
                        key: b"root\0Film.2000.mkv".to_vec(),
                        payload: p::FileUpsert {
                            collection_id: "movies".into(),
                            files: vec![file],
                        }
                        .encode_to_vec(),
                        deleted: false,
                    }],
                },
            )
            .await
            .unwrap();
        let registry = Registry::new(db, Default::default(), store);
        registry.connected("host", "mediahost", "Fixture", "fp", "test");
        let subs = Subtitles::new(dir.path().join("subtitles"));
        let candidates = subs.catalogue_ocr_candidates(&registry).await.unwrap();
        assert_eq!(candidates.len(), 1);
        subs.store_image_sets(
            "host",
            &p::ImageSubtitles {
                collection_id: "movies".into(),
                source: Some(p::SourcePath::new("root", "Film.2000.mkv")),
                source_revision: candidates[0].source_revision().unwrap().into(),
                codec: "S_HDMV/PGS".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            subs.catalogue_ocr(&registry, &candidates[0]).await.unwrap(),
            OcrGeneration::NoText
        ));
        let reopened = Subtitles::new(subs.cache_dir().into());
        assert!(
            reopened
                .catalogue_ocr_candidates(&registry)
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[cfg(feature = "ocr")]
    #[tokio::test]
    async fn bitmap_source_is_ocr_ed_into_a_cached_text_track() {
        use kahawai_proto::v1 as p;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = Registry::new(
            db,
            Default::default(),
            crate::db::open_catalogue(dir.path()).await.unwrap(),
        );
        let subs = Subtitles::new(dir.path().join("subtitles"));
        let parent = parent("pgs");
        // A real glyph image, encoded into a small PGS fixture: this exercises
        // the display-set decoder and Tesseract, not an injected OCR answer.
        let mut raster = kahawai_media::assraster::Raster::new(320, 180).unwrap();
        let sets = raster.render(SCRIPT, (24, 1), 500).unwrap();
        let object = &sets
            .iter()
            .find(|(_, s)| !s.objects.is_empty())
            .unwrap()
            .1
            .objects[0];
        let mut rle = vec![];
        for row in object.rgba.chunks_exact(object.w as usize * 4) {
            for pixel in row.chunks_exact(4) {
                if pixel[3] > 128 && pixel[0] > 128 {
                    rle.push(1);
                } else {
                    rle.extend([0, 1]);
                }
            }
            rle.extend([0, 0]);
        }
        let mut ods = vec![0, 7, 0, 0xc0];
        ods.extend_from_slice(&(rle.len() as u32 + 4).to_be_bytes()[1..]);
        ods.extend_from_slice(&(object.w as u16).to_be_bytes());
        ods.extend_from_slice(&(object.h as u16).to_be_bytes());
        ods.extend(rle);
        let mut pcs = vec![];
        pcs.extend_from_slice(&320u16.to_be_bytes());
        pcs.extend_from_slice(&180u16.to_be_bytes());
        pcs.extend([0x10, 0, 1, 0x80, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0]);
        let segment = |tag, body: &[u8]| {
            let mut out = vec![tag];
            out.extend_from_slice(&(body.len() as u16).to_be_bytes());
            out.extend_from_slice(body);
            out
        };
        let block = [
            segment(0x16, &pcs),
            segment(0x14, &[0, 0, 1, 235, 128, 128, 255]),
            segment(0x15, &ods),
            segment(0x80, &[]),
        ]
        .concat();
        subs.store_image_sets(
            "host",
            &p::ImageSubtitles {
                collection_id: "movies".into(),
                source: Some(p::SourcePath::new("root", "film.mkv")),
                source_revision: parent.source_revision().unwrap().into(),
                codec: "S_HDMV/PGS".into(),
                blocks: vec![p::ImageSubBlock {
                    start_ms: 100,
                    duration_ms: 1000,
                    payload: block,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            subs.catalogue_ocr(&registry, &parent).await.unwrap(),
            OcrGeneration::Generated
        ));
        let track = cached(subs.cache_dir(), &[parent]).unwrap().remove(0);
        let text = &track.acquired.unwrap().cues[0];
        assert_eq!((text.start_ms, text.end_ms), (100, 1100));
        assert!(text.text.contains("Source"), "{}", text.text);
    }
}
