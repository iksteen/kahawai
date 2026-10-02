//! Disk is extraction state. Queue rows describe only work still outstanding.
//! Reconcile once at startup and on filesystem events/event loss, never on a
//! periodic catalogue sweep. Addressable paths avoid a reverse artifact index.
use super::*;
use anyhow::ensure;
use kahawai_mediadb::SourceFile;
use notify::Watcher;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Publication must never expose a partially written result as completion.
pub(super) fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("artifact has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[derive(Default)]
struct CacheEvents {
    full: bool,
    reinstall: bool,
    files: HashSet<String>,
}

/// The only probe correction reconstructible without guessing historical data.
fn legacy_probes(media: &kahawai_core::media::MediaInfo) -> Vec<kahawai_core::media::MediaInfo> {
    let mut variants = vec![media.clone()];
    let mut aggregate = media.clone();
    for video in &mut aggregate.video {
        video.bit_depth = match video.bit_depth {
            Some(8) => Some(24),
            Some(10) => Some(30),
            Some(12) => Some(36),
            depth => depth,
        };
    }
    if aggregate != *media {
        variants.push(aggregate);
    }
    variants
}

impl CacheEvents {
    fn record(&mut self, event: notify::Result<notify::Event>, cache: &Path, extracted: &Path) {
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                self.full = true;
                self.reinstall = true;
                return;
            }
        };
        // notify's primary contract: need_rescan means events may have been
        // missed (https://docs.rs/notify/8/notify/struct.Event.html#method.need_rescan).
        if event.need_rescan() {
            self.full = true;
        }
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        for path in event.paths {
            if path == cache
                && matches!(
                    event.kind,
                    notify::EventKind::Remove(_)
                        | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                )
            {
                self.reinstall = true;
            }
            if path == cache || path == extracted {
                self.full = true;
            }
            if let Ok(rel) = path.strip_prefix(extracted)
                && let Some(file) = rel.components().next()
            {
                // Bounded source IDs, not individual track events. Overflow
                // buys one reconciliation instead of growing callback memory.
                if self.files.len() >= 256 {
                    self.full = true;
                } else {
                    self.files
                        .insert(file.as_os_str().to_string_lossy().into_owned());
                }
            }
        }
    }
}

impl Subtitles {
    pub(crate) fn accepts_extraction(file: &SourceFile, path: &str, revision: &str) -> bool {
        catalogue::source_revision(&work::part(file), &file.media, path != file.path) == revision
    }

    pub(crate) async fn text_landed(&self, registry: &Registry, file: &SourceFile) -> Result<()> {
        if self.text_item(&work::tracks(file))?.is_some() {
            registry
                .catalogue()
                .fail_subtitle_job(
                    file,
                    "text",
                    crate::queue::now() + work::HOST_ERROR_RETRY_SECS,
                    "extraction reply omitted required text tracks",
                )
                .await?;
            tracing::warn!(file = %file.file_id, "subtitle extraction incomplete; retry scheduled");
        } else {
            registry
                .catalogue()
                .reconcile_subtitle_job(file, "text", false)
                .await?;
        }
        self.wake();
        Ok(())
    }

    pub(super) async fn cache_miss(&self, registry: &Registry, revision: &str) -> Result<()> {
        if let Some((file, _)) = revision
            .strip_prefix("v4-")
            .and_then(|s| s.rsplit_once('-'))
            && let Some(file) = registry.catalogue().subtitle_source_by_id(file).await?
        {
            self.reconcile_file(registry, &file).await?;
        }
        Ok(())
    }
    /// A complete file, including a valid empty text track, has no queue row.
    pub(crate) async fn reconcile_file(
        &self,
        registry: &Registry,
        file: &SourceFile,
    ) -> Result<()> {
        let parents = work::tracks(file);
        let text_missing = self.text_item(&parents)?.is_some();
        let sets_missing = !self.sets_items(registry, &parents).await?.is_empty();
        for (kind, missing) in [("text", text_missing), ("sets", sets_missing)] {
            registry
                .catalogue()
                .reconcile_subtitle_job(file, kind, missing)
                .await?;
        }
        #[cfg(feature = "ocr")]
        if work::has_image_tracks(&file.media) && self.ocr_pending(registry, file).await {
            self.ocr_enqueue(file.clone());
        }
        self.wake();
        Ok(())
    }

    pub(crate) async fn reconcile_cache(&self, registry: &Registry) -> Result<()> {
        let mut after = String::new();
        let mut files = 0usize;
        loop {
            let page = registry
                .catalogue()
                .subtitle_sources_page(&after, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            for file in &page {
                self.reconcile_file(registry, file).await?;
            }
            files += page.len();
            after = page.last().unwrap().file_id.clone();
            tokio::task::yield_now().await;
        }
        tracing::info!(files, "subtitle cache presence reconciled");
        Ok(())
    }

    /// Old names encode the full probe in an opaque hash. Only reconstructible
    /// names prove byte identity. Known component-depth migrations are the sole
    /// historical variants: never guess unknown probes or promote path-only v2.
    async fn transition_cache(&self, registry: &Registry) -> Result<()> {
        self.transition_extractions(registry).await?;
        self.transition_ocr(registry).await
    }

    async fn transition_extractions(&self, registry: &Registry) -> Result<()> {
        let marker = self.dir.join(".extracted-v4-transition");
        if marker.try_exists()? {
            return Ok(());
        }
        let mut after = String::new();
        let mut moved = 0usize;
        loop {
            let page = registry
                .catalogue()
                .subtitle_sources_page(&after, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            for file in &page {
                let part = work::part(file);
                let variants = legacy_probes(&file.media);
                let parents = work::tracks(file);
                for track in &parents {
                    let source = track.physical.as_ref().context("track has no source")?;
                    let image = crate::tracks::is_image_format(&track.format);
                    let (rel, key) = if image {
                        let (_, _, _, rel, index, _) = self.extract_ref(registry, track).await?;
                        (rel, format!("i{index}"))
                    } else {
                        (source.path_rel.clone(), track.internal_key())
                    };
                    let extension = if image { "sets" } else { "json" };
                    let target = self.dir.join(format!(
                        "{}.{}",
                        cache_key(
                            &source.module_id,
                            &source.collection_id,
                            &source.root_token,
                            &rel,
                            &key,
                            track.source_revision()?
                        ),
                        extension
                    ));
                    if target.try_exists()? {
                        continue;
                    }
                    for info in &variants {
                        let old_revision = catalogue::key(
                            &part,
                            info,
                            if track.origin == "sidecar" {
                                "sidecar:"
                            } else {
                                "embedded"
                            },
                        );
                        let old = self.dir.join(format!(
                            "{}.{}",
                            cache_key(
                                &source.module_id,
                                &source.collection_id,
                                &source.root_token,
                                &rel,
                                &key,
                                &old_revision
                            ),
                            extension
                        ));
                        if !old.try_exists()? {
                            continue;
                        }
                        let bytes = std::fs::read(&old)?;
                        let valid = if image {
                            kahawai_media::burnin::validate_sets(&bytes).is_ok()
                        } else {
                            serde_json::from_slice::<Extracted>(&bytes).is_ok()
                        };
                        if !valid {
                            tracing::warn!(path = %old.display(), "invalid legacy subtitle artifact left untouched");
                            continue;
                        }
                        std::fs::create_dir_all(target.parent().unwrap())?;
                        std::fs::rename(&old, &target)?;
                        moved += 1;
                        break;
                    }
                }
            }
            after = page.last().unwrap().file_id.clone();
            tokio::task::yield_now().await;
        }
        publish(&marker, b"4\n")?;
        tracing::info!(moved, "subtitle extraction cache transition complete");
        Ok(())
    }

    /// Recognition is expensive too. Run independently of the raw-cache marker:
    /// installations which already transitioned raw sets still need their OCR
    /// answers moved before reconciliation can schedule recognition again.
    async fn transition_ocr(&self, registry: &Registry) -> Result<()> {
        let marker = self.dir.join(".derived-v3-ocr-transition");
        if marker.try_exists()? {
            return Ok(());
        }
        let mut after = String::new();
        let mut answers = 0usize;
        let mut failures = 0usize;
        loop {
            let page = registry
                .catalogue()
                .subtitle_sources_page(&after, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            for file in &page {
                let part = work::part(file);
                let variants = legacy_probes(&file.media);
                for track in work::tracks(file) {
                    if !crate::tracks::is_image_format(&track.format) {
                        continue;
                    }
                    let parent = format!("{}:{}", track.origin, track.stream_index.unwrap_or(0));
                    let target = catalogue::path(&self.dir, &track, "ocr.json")?;
                    if !target.try_exists()? {
                        for info in &variants {
                            let key = catalogue::key(&part, info, &parent);
                            let old = self.dir.join(format!("derived-v2-{key}-ocr.json"));
                            if !old.try_exists()? {
                                continue;
                            }
                            if serde_json::from_slice::<Extracted>(&std::fs::read(&old)?).is_err() {
                                tracing::warn!(path = %old.display(), "invalid legacy OCR answer left untouched");
                                continue;
                            }
                            std::fs::rename(&old, &target)?;
                            answers += 1;
                            break;
                        }
                    }
                    // An answer wins over a failure. Preserve terminal failures
                    // only when no verified answer exists in any known variant.
                    let failed = catalogue::path(&self.dir, &track, "ocr.failed")?;
                    if target.try_exists()? || failed.try_exists()? {
                        continue;
                    }
                    for info in &variants {
                        let key = catalogue::key(&part, info, &parent);
                        let old = self.dir.join(format!("derived-v2-{key}-ocr.failed"));
                        if old.try_exists()? {
                            std::fs::rename(old, &failed)?;
                            failures += 1;
                            break;
                        }
                    }
                }
            }
            after = page.last().unwrap().file_id.clone();
            tokio::task::yield_now().await;
        }
        publish(&marker, b"3\n")?;
        tracing::info!(answers, failures, "subtitle OCR cache transition complete");
        Ok(())
    }

    pub(super) fn start_recovery(self: &Arc<Self>, registry: Arc<Registry>) {
        let subs = self.clone();
        tokio::spawn(async move {
            let mut started = false;
            loop {
                if let Err(error) = subs.watch_cache(&registry, &mut started).await {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "subtitle cache watcher failed; retrying installation"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    async fn watch_cache(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        started: &mut bool,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let extracted = self.dir.join("extracted-v4");
        let cache = self.dir.clone();
        // Bound callback bookkeeping. A burst larger than this becomes one
        // explicit event-loss reconciliation instead of growing with deletes.
        let events = Arc::new(Mutex::new(CacheEvents::default()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let pending = events.clone();
        let signal = wake.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let mut pending = pending.lock().unwrap();
                pending.record(event, &cache, &extracted);
                if pending.full || pending.reinstall || !pending.files.is_empty() {
                    signal.notify_one();
                }
            })?;
        watcher.watch(&self.dir, notify::RecursiveMode::Recursive)?;
        watcher.watch(
            self.dir.parent().context("cache has no parent")?,
            notify::RecursiveMode::NonRecursive,
        )?;
        // Watch before the pass so deletion during startup cannot be lost.
        self.transition_cache(registry).await?;
        self.reconcile_cache(registry).await?;
        if !*started {
            let subs = self.clone();
            let registry = registry.clone();
            crate::queue::spawn("subtitles", self.wake.clone(), work::FALLBACK, move || {
                let subs = subs.clone();
                let registry = registry.clone();
                async move { subs.step(&registry).await }
            });
            *started = true;
        }
        loop {
            wake.notified().await;
            let CacheEvents {
                full,
                files,
                reinstall,
            } = std::mem::take(&mut *events.lock().unwrap());
            ensure!(
                !reinstall && self.dir.is_dir(),
                "subtitle cache watcher requires reinstallation"
            );
            if full {
                self.reconcile_cache(registry).await?;
            } else {
                for file in files {
                    if let Some(file) = registry.catalogue().subtitle_source_by_id(&file).await? {
                        self.reconcile_file(registry, &file).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::work::tests::{fixture, states};
    use super::*;
    use std::time::Duration;

    /// Exercise the real transition without starting a service or OCR worker.
    /// The companion supplies an isolated snapshot with hard-linked immutable
    /// caches; renaming fixture paths cannot modify installed artifacts.
    #[cfg(feature = "ocr")]
    #[tokio::test]
    #[ignore = "scripts/kahawai-subtitle-cache-check.py --upgrade-from DATA_DIR"]
    async fn installed_cache_upgrade_fixture() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("KAHAWAI_SUBTITLE_UPGRADE_FIXTURE")
                .expect("fixture directory required"),
        );
        assert!(dir.join(".subtitle-upgrade-fixture").is_file());
        let db = crate::db::open(&dir).await.unwrap();
        let store = crate::db::open_catalogue(&dir).await.unwrap();
        let registry = Registry::new(db, Default::default(), store);
        let subs = Subtitles::new(dir.join("subtitles"));
        subs.transition_cache(&registry).await.unwrap();
        let mut after = String::new();
        let mut pending = 0usize;
        let mut answered = 0usize;
        loop {
            let files = registry
                .catalogue()
                .subtitle_sources_page(&after, 256)
                .await
                .unwrap();
            if files.is_empty() {
                break;
            }
            after = files.last().unwrap().file_id.clone();
            for file in files {
                if work::has_image_tracks(&file.media) {
                    pending += usize::from(subs.ocr_pending(&registry, &file).await);
                    for track in work::tracks(&file) {
                        if crate::tracks::is_image_format(&track.format)
                            && catalogue::path(&subs.dir, &track, "ocr.json")
                                .unwrap()
                                .exists()
                        {
                            answered += 1;
                        }
                    }
                }
            }
        }
        assert!(answered > 0, "fixture must contain reusable OCR answers");
        println!(
            "installed cache upgrade: retained OCR answers={answered}, pending files={pending}"
        );
        assert!(subs.dir.join(".derived-v3-ocr-transition").exists());
    }

    async fn source(registry: &Registry, path: &str) -> SourceFile {
        registry
            .catalogue()
            .source_file(
                "host",
                "series",
                &kahawai_proto::v1::SourcePath::new("root", path),
            )
            .await
            .unwrap()
            .unwrap()
    }

    fn text_path(subs: &Subtitles, file: &SourceFile, index: usize) -> PathBuf {
        let revision = catalogue::source_revision(&work::part(file), &file.media, false);
        subs.dir.join(format!(
            "{}.json",
            cache_key(
                &file.host,
                &file.remote_id,
                &file.root_token,
                &file.path,
                &format!("e{index}"),
                &revision
            )
        ))
    }

    fn put_text(subs: &Subtitles, file: &SourceFile, index: usize) {
        publish(
            &text_path(subs, file, index),
            &serde_json::to_vec(&Extracted {
                cues: vec![],
                ass: None,
            })
            .unwrap(),
        )
        .unwrap();
    }

    async fn wait_state(registry: &Registry, expected: Vec<(String, i64)>) {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if states(registry, "text").await == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cache event did not change outstanding work");
    }

    #[tokio::test]
    async fn event_loss_and_callback_overflow_reconcile_missing_artifacts() {
        let (_dir, registry, subs) = fixture(vec![(
            "Film.mkv",
            serde_json::json!({"subtitles":[{"format":"text"}]}),
        )])
        .await;
        let file = source(&registry, "Film.mkv").await;
        put_text(&subs, &file, 0);
        subs.reconcile_cache(&registry).await.unwrap();
        std::fs::remove_file(text_path(&subs, &file, 0)).unwrap();
        assert!(states(&registry, "text").await.is_empty());
        let mut hints = CacheEvents::default();
        let extracted = subs.dir.join("extracted-v4");
        hints.record(
            Ok(notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)),
            &subs.dir,
            &extracted,
        );
        assert!(hints.full);
        subs.reconcile_cache(&registry).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("pending".into(), 1)]);
        let mut hints = CacheEvents::default();
        for n in 0..300 {
            hints.record(
                Ok(
                    notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                        .add_path(extracted.join(format!("{n}/revision/e0.json"))),
                ),
                &subs.dir,
                &extracted,
            );
        }
        assert!(hints.full);
        assert!(hints.files.len() <= 256);
    }

    #[tokio::test]
    async fn missing_image_sets_are_recreated_even_with_a_cached_ocr_answer() {
        let (_dir, registry, subs) = fixture(vec![(
            "Film.mkv",
            serde_json::json!({"subtitles":[{"format":"pgs","language":"en"}]}),
        )])
        .await;
        let file = source(&registry, "Film.mkv").await;
        let track = work::tracks(&file).remove(0);
        let revision = track.source_revision().unwrap();
        let path = subs.dir.join(format!(
            "{}.sets",
            cache_key(
                &file.host,
                &file.remote_id,
                &file.root_token,
                &file.path,
                "i0",
                revision
            )
        ));
        publish(
            &path,
            &kahawai_media::burnin::encode_sets_zstd("S_HDMV/PGS", None, &[]),
        )
        .unwrap();
        publish(
            &catalogue::path(&subs.dir, &track, "ocr.json").unwrap(),
            br#"{"cues":[]}"#,
        )
        .unwrap();
        subs.reconcile_cache(&registry).await.unwrap();
        assert!(states(&registry, "sets").await.is_empty());
        std::fs::remove_file(path).unwrap();
        subs.reconcile_cache(&registry).await.unwrap();
        assert_eq!(states(&registry, "sets").await, vec![("pending".into(), 1)]);
    }

    #[tokio::test]
    async fn transition_renames_verified_depth_variants_and_resumes() {
        let (_dir, registry, subs) = fixture(vec![("Film.mkv", serde_json::json!({"video":[{"codec":"h264","width":320,"height":180,"interlaced":false,"bit_depth":8}],"subtitles":[{"format":"text"},{"format":"text"}]}))]).await;
        let file = source(&registry, "Film.mkv").await;
        let mut legacy = file.media.clone();
        legacy.video[0].bit_depth = Some(24);
        let revision = catalogue::key(&work::part(&file), &legacy, "embedded");
        let old = subs.dir.join(format!(
            "{}.json",
            cache_key(
                &file.host,
                &file.remote_id,
                &file.root_token,
                &file.path,
                "e0",
                &revision
            )
        ));
        publish(&old, br#"{"cues":[],"ass":null}"#).unwrap();
        // Simulate a transition interrupted after another track moved.
        put_text(&subs, &file, 1);
        subs.transition_cache(&registry).await.unwrap();
        assert!(!old.exists());
        assert!(text_path(&subs, &file, 0).exists());
        assert!(text_path(&subs, &file, 1).exists());
        subs.reconcile_cache(&registry).await.unwrap();
        assert!(states(&registry, "text").await.is_empty());
        // Transition is one-time, not a read-time compatibility fallback.
        std::fs::remove_file(text_path(&subs, &file, 0)).unwrap();
        subs.transition_cache(&registry).await.unwrap();
        assert!(!text_path(&subs, &file, 0).exists());
    }

    #[tokio::test]
    async fn ocr_transition_preserves_answers_empty_results_failures_and_resumes() {
        for depth in [8, 10, 12] {
            let (_dir, registry, subs) = fixture(vec![("Film.mkv", serde_json::json!({
                "video":[{"codec":"h264","width":320,"height":180,"interlaced":false,"bit_depth":depth}],
                "subtitles":[{"format":"pgs","language":"en"},{"format":"pgs","language":"en"}],
                "external_subtitles":[{"format":"vobsub","language":"nl","path_rel":"Film.idx","track":0}]
            }))]).await;
            let file = source(&registry, "Film.mkv").await;
            let parents = work::tracks(&file);
            let variants = legacy_probes(&file.media);
            let old =
                |track: &crate::tracks::Track, info: &kahawai_core::media::MediaInfo, kind| {
                    let key = catalogue::key(
                        &work::part(&file),
                        info,
                        &format!("{}:{}", track.origin, track.stream_index.unwrap()),
                    );
                    subs.dir.join(format!("derived-v2-{key}-{kind}"))
                };
            let answer =
                br#"{"cues":[{"start_ms":0,"end_ms":1000,"text":"retained recognition"}]}"#;
            let legacy_answer = old(&parents[0], &variants[1], "ocr.json");
            publish(&legacy_answer, answer).unwrap();
            publish(
                &old(&parents[1], &variants[0], "ocr.json"),
                br#"{"cues":[]}"#,
            )
            .unwrap();
            let legacy_failed = old(&parents[2], &variants[1], "ocr.failed");
            publish(&legacy_failed, b"recognition failed").unwrap();
            publish(&subs.dir.join(".extracted-v4-transition"), b"4\n").unwrap();
            subs.transition_cache(&registry).await.unwrap();
            assert!(!legacy_answer.exists());
            assert!(!legacy_failed.exists());
            assert_eq!(
                std::fs::read(catalogue::path(&subs.dir, &parents[0], "ocr.json").unwrap())
                    .unwrap(),
                answer
            );
            assert_eq!(
                std::fs::read(catalogue::path(&subs.dir, &parents[1], "ocr.json").unwrap())
                    .unwrap(),
                br#"{"cues":[]}"#
            );
            assert!(
                catalogue::path(&subs.dir, &parents[2], "ocr.failed")
                    .unwrap()
                    .exists()
            );
            assert_eq!(catalogue::cached(&subs.dir, &parents).unwrap().len(), 1);
            // Simulate a partly completed OCR transition. Published v3 answers
            // win over duplicates; a missing marker only resumes other moves.
            std::fs::remove_file(subs.dir.join(".derived-v3-ocr-transition")).unwrap();
            publish(&legacy_answer, br#"{"cues":[]}"#).unwrap();
            subs.transition_cache(&registry).await.unwrap();
            assert!(legacy_answer.exists());
            assert_eq!(
                std::fs::read(catalogue::path(&subs.dir, &parents[0], "ocr.json").unwrap())
                    .unwrap(),
                answer
            );
        }
    }

    #[tokio::test]
    async fn ocr_transition_rejects_unknown_invalid_and_changed_language_answers() {
        let (_dir, registry, subs) = fixture(vec![("Film.mkv", serde_json::json!({
            "video":[{"codec":"h264","width":320,"height":180,"interlaced":false,"bit_depth":10}],"subtitles":[{"format":"pgs","language":"en"},{"format":"pgs","language":"en"}]
        }))]).await;
        let file = source(&registry, "Film.mkv").await;
        let mut legacy = file.media.clone();
        legacy.subtitles[0].language = Some("nl".into());
        let wrong_language = subs.dir.join(format!(
            "derived-v2-{}-ocr.json",
            catalogue::key(&work::part(&file), &legacy, "embedded:0")
        ));
        publish(&wrong_language, br#"{"cues":[]}"#).unwrap();
        let invalid = subs.dir.join(format!(
            "derived-v2-{}-ocr.json",
            catalogue::key(&work::part(&file), &file.media, "embedded:1")
        ));
        publish(&invalid, b"unfinished JSON").unwrap();
        let unknown = subs.dir.join("derived-v2-unknown-ocr.json");
        publish(&unknown, br#"{"cues":[]}"#).unwrap();
        subs.transition_cache(&registry).await.unwrap();
        assert!(unknown.exists() && wrong_language.exists() && invalid.exists());
        assert!(
            catalogue::cached(&subs.dir, &work::tracks(&file))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_legacy_keys_and_stale_replies_cannot_settle_current_work() {
        let (_dir, registry, subs) = fixture(vec![(
            "Film.mkv",
            serde_json::json!({"subtitles":[{"format":"text"},{"format":"text"}]}),
        )])
        .await;
        let file = source(&registry, "Film.mkv").await;
        let old = subs.dir.join("v3-unknown-e0.json");
        publish(&old, br#"{"cues":[]}"#).unwrap();
        subs.transition_cache(&registry).await.unwrap();
        assert!(old.exists());
        let revision = catalogue::source_revision(&work::part(&file), &file.media, false);
        assert!(Subtitles::accepts_extraction(&file, &file.path, &revision));
        let mut replaced = file.clone();
        replaced.mtime += 1;
        assert!(!Subtitles::accepts_extraction(
            &replaced, &file.path, &revision
        ));
        assert!(!Subtitles::accepts_extraction(
            &file,
            &file.path,
            "old-revision"
        ));
        put_text(&subs, &file, 0);
        registry
            .catalogue()
            .claim_subtitle_jobs("text", "host", &[], 0, 60, 16)
            .await
            .unwrap();
        subs.text_landed(&registry, &file).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("retry".into(), 1)]);
        subs.reconcile_file(&registry, &file).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("retry".into(), 1)]);
        put_text(&subs, &file, 1);
        subs.text_landed(&registry, &file).await.unwrap();
        assert!(states(&registry, "text").await.is_empty());
    }

    #[tokio::test]
    async fn watcher_recovers_track_bulk_and_directory_deletion_without_playback() {
        let (_dir, registry, subs) = fixture(vec![
            (
                "A.mkv",
                serde_json::json!({"subtitles":[{"format":"text"},{"format":"text"}]}),
            ),
            (
                "B.mkv",
                serde_json::json!({"subtitles":[{"format":"text"}]}),
            ),
        ])
        .await;
        let a = source(&registry, "A.mkv").await;
        let b = source(&registry, "B.mkv").await;
        put_text(&subs, &a, 0);
        put_text(&subs, &a, 1);
        put_text(&subs, &b, 0);
        let subs = Arc::new(subs);
        let registry = Arc::new(registry);
        subs.start_recovery(registry.clone());
        wait_state(&registry, vec![]).await;
        std::fs::remove_file(text_path(&subs, &a, 1)).unwrap();
        wait_state(&registry, vec![("pending".into(), 1)]).await;
        put_text(&subs, &a, 1);
        wait_state(&registry, vec![]).await;
        std::fs::remove_dir_all(subs.dir.join("extracted-v4")).unwrap();
        wait_state(&registry, vec![("pending".into(), 2)]).await;
        put_text(&subs, &a, 0);
        put_text(&subs, &a, 1);
        put_text(&subs, &b, 0);
        wait_state(&registry, vec![]).await;
        std::fs::remove_dir_all(&subs.dir).unwrap();
        std::fs::create_dir_all(&subs.dir).unwrap();
        wait_state(&registry, vec![("pending".into(), 2)]).await;
        put_text(&subs, &a, 0);
        put_text(&subs, &a, 1);
        put_text(&subs, &b, 0);
        wait_state(&registry, vec![]).await;
    }

    #[tokio::test]
    async fn startup_recovers_missing_completed_artifacts_and_preserves_leases() {
        let (_dir, registry, subs) = fixture(vec![(
            "Film.mkv",
            serde_json::json!({"subtitles":[{"format":"text"}]}),
        )])
        .await;
        let file = source(&registry, "Film.mkv").await;
        registry
            .catalogue()
            .finish_subtitle_job(&file.file_id, "text")
            .await
            .unwrap();
        subs.reconcile_cache(&registry).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("pending".into(), 1)]);
        registry
            .catalogue()
            .claim_subtitle_jobs("text", "host", &[], 0, 60, 16)
            .await
            .unwrap();
        subs.reconcile_cache(&registry).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("running".into(), 1)]);
        // A deletion racing publication is repaired by the same predicate.
        put_text(&subs, &file, 0);
        std::fs::remove_file(text_path(&subs, &file, 0)).unwrap();
        subs.text_landed(&registry, &file).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("retry".into(), 1)]);
    }
}
