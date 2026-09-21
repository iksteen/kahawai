use super::*;

pub struct RemuxJob {
    pipeline: gst::Pipeline,
    error: Arc<Mutex<Option<String>>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    /// Unblocks pacing probes on teardown.
    stopping: Arc<std::sync::atomic::AtomicBool>,
}

/// Start a remux/transcode writing `master.m3u8` + `segment*.ts` into
/// `out_dir`, pulling bytes from `source` on demand (seeks included).
/// The plan comes from discovery via [`plan_streams`] — the muxer pads
/// must be requested before the pipeline starts, and an unfed pad would
/// stall it.
pub fn start(out_dir: &Path, plan: RemuxPlan, source: Box<dyn RemuxSource>) -> Result<RemuxJob> {
    start_full(out_dir, plan, source, 0, None)
}

/// Like [`start`], seeking to `start_ms` (nearest keyframe at or before
/// it) before rolling — the §6 seek story: a seek beyond produced
/// segments is a pipeline restart at the target offset.
pub fn start_at(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
) -> Result<RemuxJob> {
    start_full(out_dir, plan, source, start_ms, None)
}

/// Full-control variant: offset plus an HLS sink override (TC-6 retry)
/// and an optional pacing window.
pub fn start_full(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
    sink: Option<&str>,
) -> Result<RemuxJob> {
    start_paced(out_dir, plan, source, start_ms, sink, None)
}

pub fn start_paced(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
    sink: Option<&str>,
    pace: Option<PaceConfig>,
) -> Result<RemuxJob> {
    start_parts(
        out_dir,
        plan,
        vec![source],
        start_ms,
        sink,
        pace,
        None,
        None,
    )
}

/// One pipeline spanning a multi-part source, in timeline order.
///
/// A CD1/CD2 boundary used to be a pipeline restart: stop the worker,
/// delete the segments, start again in the next file, and let the client
/// stitch the two playlists together when the video element fired
/// `ended`. The most predictable event in the file paid the price of a
/// random seek. Here the parts are branches of ONE pipeline joined by
/// `concat`, so the boundary produces no event at all — one playlist,
/// continuous running time, no discontinuity tag.
///
/// `start_ms` applies to the FIRST part only; the rest play whole, which
/// is what makes them concatenable. Seeking is NOT done this way: concat
/// accepts a seek after preroll and then plays from zero, and refuses one
/// during playback outright (both measured — see `concat_spike`). A seek
/// therefore stays what it is today, a restart in the target part, and
/// this function is handed the parts from that point on.
#[allow(clippy::too_many_arguments)] // one pipeline, spelled out
pub fn start_parts(
    out_dir: &Path,
    plan: RemuxPlan,
    sources: Vec<Box<dyn RemuxSource>>,
    start_ms: u64,
    sink: Option<&str>,
    pace: Option<PaceConfig>,
    // HUB-32b: display sets read for us (mediahost-side). None = walk
    // the source index ourselves, affordable only for local sources.
    burn_sets: Option<&Path>,
    // HUB-32a: a user's own sidecar .ass to burn into the picture. An
    // EMBEDDED ASS burn comes from `plan.burn_ass` instead and takes the
    // demuxer's pad, because that is the only path carrying the file's
    // attached fonts; a sidecar has none and renders with system fonts.
    burn_ass_file: Option<&Path>,
) -> Result<RemuxJob> {
    crate::init()?;
    // Dolby Digital Plus reaches us as ONE container block holding an
    // AC-3 core plus its E-AC-3 extension substream. ac3parse splits
    // that block into two "frames" and interpolates a timestamp
    // between them, so a 32 ms unit leaves the parser as two buffers
    // 16 ms apart — and the audio timeline comes out at HALF the audio
    // content, i.e. sound racing the picture 2:1 (measured on a DD+ 7.1
    // title: 60 s of samples stamped across 30 s; 2815 AAC frames of
    // content muxed against 30 s of video). avdec_eac3 — which
    // build_audio_encode_chain forces for the whole AC-3 family anyway
    // — decodes the intact block correctly, and yields the full 7.1
    // that the split path silently reduces to the 5.1 core. So while
    // the audio is being decoded the parser buys nothing and costs
    // sync; demote it and let parsebin expose the block whole.
    //
    // Decode only: a COPY still needs the parser's framed caps to mux.
    // The rank is process-global, which is safe precisely because every
    // real pipeline runs in its own `remux-worker` child — one run per
    // process, both from the transcoder and from the hub.
    if plan.audio == StreamMode::Encode
        && let Some(f) = gst::ElementFactory::find("ac3parse")
    {
        f.set_rank(gst::Rank::NONE);
    }
    anyhow::ensure!(!sources.is_empty(), "no source parts to remux");
    let multipart = sources.len() > 1;
    let mut sources = sources;

    // HUB-32b burn-in: read the display-set timeline from the FIRST
    // part's own container index before its bytes are handed to the
    // pipeline (the source is random-access and stateless, so the walk
    // costs a few scattered kilobytes and leaves nothing behind). Doing
    // it up front — rather than following the demuxer's subtitle pad —
    // is what makes a session that STARTS mid-set show that set.
    let burn_timeline = match (burn_sets, plan.burn_subtitle) {
        // Handed to us: no walk at all, and correct wherever the
        // source lives.
        (Some(path), _) => match crate::burnin::timeline_from_file(path) {
            Ok(Some(t)) if !t.is_empty() => {
                tracing::info!(sets = %path.display(), entries = t.len(),
                    "burn-in: display sets loaded");
                Some(std::sync::Arc::new(t))
            }
            Ok(_) => {
                tracing::warn!(sets = %path.display(), "burn-in: display sets empty");
                None
            }
            Err(e) => {
                tracing::warn!(sets = %path.display(), error = format!("{e:#}"),
                    "burn-in: display sets unreadable");
                None
            }
        },
        (None, Some(idx)) => {
            let t0 = std::time::Instant::now();
            // Bounded: a session must start even when the timeline
            // cannot be had. Local sources finish in milliseconds; a
            // lease-backed one may not finish at all, and then the
            // encode runs without the burn and the verdict says so.
            match crate::burnin::timeline(&mut *sources[0], idx, BURN_INDEX_BUDGET) {
                Ok(Some(t)) if !t.is_empty() => {
                    tracing::info!(
                        track = idx,
                        sets = t.len(),
                        ms = t0.elapsed().as_millis(),
                        "burn-in: display-set timeline read"
                    );
                    Some(std::sync::Arc::new(t))
                }
                Ok(_) => {
                    tracing::warn!(track = idx, "burn-in: no display sets — burning nothing");
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        track = idx,
                        error = format!("{e:#}"),
                        "burn-in: timeline failed"
                    );
                    None
                }
            }
        }
        (None, None) => None,
    };

    // Both halves of an ASS burn meet here — see `AssBurnLink`. Shared
    // across parts on purpose, unlike the per-part track counters: the
    // video chain is built once, from whichever part reaches it first.
    let ass_link = (plan.burn_ass.is_some() || burn_ass_file.is_some())
        .then(|| Arc::new(Mutex::new(AssBurnLink::default())));

    let pipeline = gst::Pipeline::new();
    if let (Some(path), Some(link)) = (burn_ass_file, &ass_link)
        && let Some(source) = load_ass_file(path)
    {
        AssBurnLink::offer_source(link, source);
    }
    // The segment sink pair: TS = the hlssink family (TC-6 prefer
    // override intact); fMP4 = isofmp4mux + the fmp4sink writer. The
    // sink override is a TS-crash workaround and means nothing here.
    enum SegSink {
        Ts(gst::Element),
        Fmp4(gst::Element),
    }
    let hlssink = match plan.segment_format {
        SegmentFormat::Ts => {
            let (el, _name) = make_hls_sink(out_dir, sink)?;
            pipeline.add(&el)?;
            SegSink::Ts(el)
        }
        SegmentFormat::Fmp4 => SegSink::Fmp4(crate::fmp4sink::attach(&pipeline, out_dir)?),
    };

    // The first part owns the start offset and the seek gate; later parts
    // are held by concat until it EOSes, then play from their own zero.
    let mut parsebins = Vec::with_capacity(sources.len());
    for source in sources {
        let appsrc = seekable_appsrc(source);
        let parsebin = gst::ElementFactory::make("parsebin").build()?;
        // Keep parsebin's own parser away from AV1, and only AV1.
        //
        // `autoplug-continue` is asked before each element is plugged;
        // returning false exposes the stream as the demuxer produced it.
        // Container caps are never in `parsebin_must_not_parse`, so
        // demuxing always continues.
        let segment_format = plan.segment_format;
        parsebin.connect("autoplug-continue", false, move |args| {
            let caps = args[2].get::<gst::Caps>().ok()?;
            Some((!parsebin_must_not_parse(&caps, segment_format)).to_value())
        });
        pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;
        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;
        parsebins.push(parsebin);
    }
    let parsebin = parsebins[0].clone();

    // Request the muxer pads *now* — splitmuxsink inside hlssink2 must see
    // them before starting or it never leaves Ready.
    anyhow::ensure!(plan.playable(), "nothing to remux");
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pace = pace.map(Arc::new);
    let mut pace_meter = pace.as_ref().map(|_| {
        Arc::new(Mutex::new(PaceMeter {
            t0: None,
            first_ms: 0,
            done: false,
        }))
    });
    let per_part: Vec<WaitingPads> = (0..parsebins.len())
        .map(|_| Arc::new(Mutex::new(std::collections::HashMap::new())))
        .collect();
    for kind in ["video", "audio"] {
        let wanted = if kind == "video" {
            plan.has_video()
        } else {
            plan.has_audio()
        };
        if !wanted {
            continue;
        }
        // TS sinks name their pads; the fMP4 mux counts them — the
        // loop order (video first) puts video on track 0 either way.
        let pad = match &hlssink {
            SegSink::Ts(el) => el.request_pad_simple(kind),
            SegSink::Fmp4(mux) => mux.request_pad_simple("sink_%u"),
        }
        .with_context(|| format!("requesting {kind} pad"))?;
        // BEFORE the pace probe, which must not meter a buffer that never
        // reaches the muxer: probe iteration stops at the first DROP.
        if kind == "video" {
            open_on_keyframe(&pad);
        }
        if let Some(cfg) = &pace {
            // HUB-36: meter the first pad only — video when the plan has
            // it (loop order), which is the stream whose production rate
            // placement cares about.
            let m = pace_meter.take();
            install_pace_probe(&pad, cfg.clone(), stopping.clone(), m);
        }
        if !multipart {
            per_part[0].lock().unwrap().insert(kind, pad);
            continue;
        }
        let concat = gst::ElementFactory::make("concat").build()?;
        pipeline.add(&concat)?;
        let src = concat.static_pad("src").context("concat has no src pad")?;
        // Same reason as every other pad feeding this sink: hlssink3
        // unwraps each fragment's first PTS and a panic in an FFI
        // callback takes the process with it.
        guard_pts(&src);
        src.link(&pad).context("linking concat to the muxer")?;
        // Request order IS play order — one pad per part, in sequence.
        for slot in per_part.iter() {
            let sink = concat
                .request_pad_simple("sink_%u")
                .context("concat sink pad")?;
            slot.lock().unwrap().insert(kind, sink);
        }
    }

    // Every parsed stream gets a queue immediately (no buffer ever hits an
    // unlinked pad); routing to the pre-requested muxer pads happens per
    // stream once its real caps flow (see plumb_parsed_pad).
    let gate = (start_ms > 0)
        .then(|| SeekGate::new(plan.has_video() as usize + plan.has_audio() as usize));
    let subs_dir = out_dir.to_path_buf();
    for (n, pb) in parsebins.iter().enumerate() {
        let pipe = pipeline.clone();
        let waiting2 = per_part[n].clone();
        // Only the first part is seeked, so only its branches are gated.
        // Gating the others would break the gate twice over: it expects
        // one branch per stream and would see one per stream PER PART,
        // and `start.pos` is the minimum stream time across gated pads —
        // a later part's branch starts at its own zero and would report
        // the whole session as starting at 0, shifting the client's
        // timeline by the entire resume offset.
        let gate2 = if n == 0 { gate.clone() } else { None };
        // Per part, NOT shared: these count tracks in demux order and the
        // count is what `plan.audio_track` / `plan.video_track` select
        // against. Track indices are a property of a file, so sharing
        // them across parts would offset part two's tracks past the
        // selection and play it silent.
        let audio_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let video_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subs_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let video_canvas = Arc::new(Mutex::new(None));
        let subs_dir = subs_dir.clone();
        let burn_tl = burn_timeline.clone();
        let ass_link = ass_link.clone();
        pb.connect_pad_added(move |_, pad| {
            plumb_parsed_pad(
                &pipe,
                &waiting2,
                pad,
                plan,
                &gate2,
                &audio_seen,
                &video_seen,
                &subs_seen,
                &video_canvas,
                &subs_dir,
                &burn_tl,
                &ass_link,
                n == 0,
            );
        });
    }

    let error = Arc::new(Mutex::new(None::<String>));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bus = pipeline.bus().context("pipeline has no bus")?;
    let err2 = error.clone();
    let fin2 = finished.clone();
    // Watch the bus on a plain thread: EOS finalizes the playlist (ENDLIST).
    let pipeline2 = pipeline.clone();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    let _ = pipeline2.set_state(gst::State::Null);
                    fin2.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                gst::MessageView::Error(e) => {
                    let text = format!("{} ({:?})", e.error(), e.debug());
                    tracing::error!(error = %text, "remux pipeline failed");
                    *err2.lock().unwrap() = Some(text);
                    let _ = pipeline2.set_state(gst::State::Null);
                    fin2.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                _ => {}
            }
        }
    });

    if let Some(gate) = &gate {
        // Offset start. splitmuxsink cannot survive a flush once it has
        // seen data (C assert aborts on mid-GOP flushes), so every muxer
        // feed is gated: roll toward PAUSED until all branches have data
        // blocked at the gates (source, demuxer and parsers are then
        // negotiated), seek through the still-virgin muxer, then open.
        pipeline.set_state(gst::State::Paused)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !gate.all_triggered() {
            if let Some(e) = error.lock().unwrap().clone() {
                anyhow::bail!("pipeline failed before offset seek: {e}");
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "streams never reached the seek gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Through one parsed stream: the gated HLS sink refuses a
        // pipeline-level seek, and broadcasting through parsebin sends
        // duplicate requests to the demuxer.
        let seek = gst::event::Seek::new(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::SeekType::Set,
            gst::ClockTime::from_mseconds(start_ms),
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        anyhow::ensure!(
            seek_parsed_stream(&parsebin, seek),
            "demuxer refused the start-offset seek"
        );
        gate.open_reporting(out_dir.join("start.pos"));
    }
    if start_ms == 0 {
        // Consistent origin reporting: zero-offset runs have a known
        // origin, write it so players can always sum base + start.pos.
        let _ = std::fs::write(out_dir.join("start.pos"), "0");
    }
    pipeline.set_state(gst::State::Playing)?;
    Ok(RemuxJob {
        pipeline,
        error,
        finished,
        stopping,
    })
}

impl RemuxJob {
    pub fn failed(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// True once EOS or error fully processed (playlist finalized).
    pub fn finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hard stop (session teardown).
    pub fn stop(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Pacing (§4.6): hold the pipeline while the viewer catches up.
    pub fn pause(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
    }

    pub fn resume(&self) {
        let _ = self.pipeline.set_state(gst::State::Playing);
    }

    /// Media-time position of the output (absolute — reflects offset
    /// starts), for the pacing window.
    pub fn position_ms(&self) -> Option<u64> {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| p.mseconds())
    }
}

impl Drop for RemuxJob {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod multipart {
    //! A multi-part source plays as one stream (HUB-17 / §4.6).
    use super::*;

    fn part(dir: &std::path::Path, name: &str, pattern: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=125 pattern={pattern} ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux name=m audiotestsrc num-buffers=215 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
            path.display()
        ));
        path
    }

    /// Every gated pad reports from its own thread, so the file must end
    /// at the EARLIEST report however the threads interleave. The atomic
    /// this replaced compared correctly and wrote outside the comparison,
    /// so a thread preempted between the two could land a later value on
    /// top — the origin a client aligns subtitles to, wrong by the gap.
    #[test]
    fn the_playlist_origin_is_the_earliest_report_whoever_wins_the_race() {
        let dir = tempfile::tempdir().unwrap();
        for round in 0..200 {
            let path = dir.path().join(format!("start-{round}.pos"));
            let min = std::sync::Arc::new(std::sync::Mutex::new(u64::MAX));
            // Descending, so every thread but the last has to be overtaken:
            // the interleaving that loses a write is the common case here.
            let reports: Vec<u64> = (0..8).map(|n| 4_000 - n * 20).collect();
            let hands: Vec<_> = reports
                .iter()
                .map(|&ms| {
                    let (min, path) = (min.clone(), path.clone());
                    std::thread::spawn(move || super::report_start(&min, &path, ms))
                })
                .collect();
            for hand in hands {
                hand.join().unwrap();
            }
            let wrote: u64 = std::fs::read_to_string(&path).unwrap().parse().unwrap();
            assert_eq!(
                wrote,
                *reports.iter().min().unwrap(),
                "round {round}: the file kept a later origin than one that was reported"
            );
        }
    }

    /// Resuming inside part one still reports where playback actually
    /// began. `start.pos` is the minimum stream time across the GATED
    /// pads and the client adds it to the part base, so gating a later
    /// part — whose branch starts at its own zero — reported the session
    /// as starting at 0 and shifted the whole timeline by the resume
    /// offset. Only the part being seeked is gated.
    #[test]
    fn resuming_inside_the_first_part_reports_its_own_start() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["fdkaacenc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            part(dir.path(), "a1.mkv", "smpte"),
            part(dir.path(), "a2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let sources: Vec<Box<dyn RemuxSource>> = parts
            .iter()
            .map(|p| Box::new(FileSource::open(p).unwrap()) as Box<dyn RemuxSource>)
            .collect();
        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            audio_track: 0,
            video_track: 0,
            ..Default::default()
        };
        // 2 s into a 5 s first part.
        let job = start_parts(&out, plan, sources, 2_000, None, None, None, None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !job.finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            job.failed().is_none(),
            "pipeline failed: {:?}",
            job.failed()
        );
        let pos: u64 = std::fs::read_to_string(out.join("start.pos"))
            .expect("no start.pos written")
            .trim()
            .parse()
            .expect("start.pos is not a number");
        // Keyframe-snapped at or before the request, never zero — zero is
        // what the second part's branch reports for itself.
        assert!(
            pos > 500,
            "start.pos {pos} — a later part's zero won the minimum"
        );
        assert!(
            pos <= 2_000,
            "start.pos {pos} is past the requested resume point"
        );
    }

    /// Two 5 s parts, one playlist, no seam: the muxer never learns the
    /// source changed file, so there is nothing for a client to stitch.
    #[test]
    fn two_parts_render_as_one_continuous_playlist() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["fdkaacenc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            part(dir.path(), "cd1.mkv", "smpte"),
            part(dir.path(), "cd2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();

        let sources: Vec<Box<dyn RemuxSource>> = parts
            .iter()
            .map(|p| Box::new(FileSource::open(p).unwrap()) as Box<dyn RemuxSource>)
            .collect();
        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            audio_track: 0,
            video_track: 0,
            ..Default::default()
        };
        let job = start_parts(&out, plan, sources, 0, None, None, None, None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !job.finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            job.failed().is_none(),
            "pipeline failed: {:?}",
            job.failed()
        );
        assert!(job.finished(), "pipeline never finished");

        let playlist =
            std::fs::read_to_string(out.join("master.m3u8")).expect("no playlist written");
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        assert!(
            total > 9.0,
            "playlist covers {total}s — the second part never played"
        );
        assert!(
            !playlist.contains("EXT-X-DISCONTINUITY"),
            "the timeline broke at the seam"
        );
        assert!(
            playlist.contains("EXT-X-ENDLIST"),
            "playlist never finalised"
        );
    }
}

#[cfg(test)]
mod concat_spike {
    //! SPIKE (not a requirement): can one pipeline span a multi-part
    //! source, so a CD1->CD2 boundary produces no event at all? Today the
    //! boundary is implemented as a seek — tear the pipeline down, delete
    //! the segments, restart in the next file — and the client stitches
    //! it back together on `ended`.
    //!
    //! Uses the real seekable appsrc, not filesrc: production feeds bytes
    //! from a lease, and whether a seek reaches back through concat to the
    //! right appsrc is the whole question.
    use super::*;

    fn fixture(dir: &std::path::Path, name: &str, pattern: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=125 pattern={pattern} ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ));
        path
    }

    /// Two parts, one concat, one sink. `link` receives concat's src pad.
    fn concat_pipeline(
        parts: &[std::path::PathBuf],
        tail: &[&str],
    ) -> (gst::Pipeline, gst::Element) {
        crate::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let concat = gst::ElementFactory::make("concat").build().unwrap();
        pipeline.add(&concat).unwrap();
        let mut prev = concat.clone();
        for name in tail {
            let el = gst::ElementFactory::make(name).build().unwrap();
            pipeline.add(&el).unwrap();
            prev.link(&el).unwrap();
            prev = el;
        }
        for part in parts {
            let src = seekable_appsrc(Box::new(FileSource::open(part).unwrap()));
            let parsebin = gst::ElementFactory::make("parsebin").build().unwrap();
            pipeline
                .add_many([src.upcast_ref::<gst::Element>(), &parsebin])
                .unwrap();
            src.link(&parsebin).unwrap();
            // This part's concat pad is requested NOW, in parts order, not
            // from the callback below. `concat` plays its sink pads in the
            // order they were requested and ends the stream when the last
            // pad it knows about goes EOS — so requesting from `pad-added`
            // raced typefind against playback: under load part one drained
            // and EOS'd while part two's parsebin was still typefinding,
            // concat saw a single pad, forwarded EOS, and the playlist
            // ended at five seconds with part two nowhere. Requested up
            // front, the ORDER is the parts' order and concat cannot
            // finish before every part has run.
            let seat = concat.request_pad_simple("sink_%u").unwrap();
            let pipe = pipeline.downgrade();
            parsebin.connect_pad_added(move |_, pad| {
                let Some(pipe) = pipe.upgrade() else { return };
                // The queue is not optional. Without it the A/V variant
                // deadlocks outright: a demuxer pushes every stream from
                // one thread, so a branch blocked on a concat that is
                // still draining the other part stalls its siblings too.
                let queue = gst::ElementFactory::make("queue")
                    .property("max-size-buffers", 0u32)
                    .property("max-size-bytes", 0u32)
                    .property("max-size-time", 0u64)
                    .build()
                    .unwrap();
                pipe.add(&queue).unwrap();
                queue.sync_state_with_parent().unwrap();
                pad.link(&queue.static_pad("sink").unwrap()).unwrap();
                // One seat per part: these fixtures are video-only, so a
                // second pad here means the fixture grew a stream the seat
                // plan does not cover. Say so rather than fail obscurely.
                queue
                    .static_pad("src")
                    .unwrap()
                    .link(&seat)
                    .expect("a part exposed a second stream; concat_pipeline seats one per part");
            });
        }
        (pipeline, prev)
    }

    fn run_to_eos(pipeline: &gst::Pipeline, secs: u64) -> Option<gst::Message> {
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(secs),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        msg
    }

    /// Whatever the tone-map segment pins must be linkable to EVERY
    /// encoder we might place behind it. It pinned NV12 alone, which no
    /// hardware encoder minded and openh264enc (sink template: I420,
    /// nothing else) could not accept — and since the chain is linked
    /// from a pad-added callback, the failed link panicked where a
    /// panic cannot unwind: SIGABRT, no session error, on every HDR
    /// title a software-encoder box was asked to tone-map.
    #[test]
    fn tonemap_output_suits_every_encoder_we_place() {
        crate::init().unwrap();
        let mut checked = 0;
        for name in [
            "openh264enc",
            "x264enc",
            "x265enc",
            "nvh264enc",
            "nvh265enc",
            "vah264enc",
            "vah265enc",
            "vtenc_h264_hw",
            "vtenc_h265_hw",
            "svtav1enc",
        ] {
            let Some(f) = gst::ElementFactory::find(name) else {
                continue;
            };
            let sink: Vec<gst::Caps> = f
                .static_pad_templates()
                .into_iter()
                .filter(|t| t.direction() == gst::PadDirection::Sink)
                .map(|t| t.caps())
                .collect();
            if sink.is_empty() {
                continue;
            }
            // What the segment WILL pin for this encoder — not merely
            // whether the two sets overlap. A list is not a preference
            // order: offering {NV12, I420} to an encoder that takes only
            // NV12 resolved to I420 and died with not-negotiated on the
            // J5005, which is how HDR transcoding broke there for a day
            // while every "does it overlap" check stayed green.
            let mut pinned = tonemap_out_caps(name);
            pinned
                .get_mut()
                .unwrap()
                .set_features(0, Some(gst::CapsFeatures::new_any()));
            assert!(
                sink.iter().any(|c| !c.intersect(&pinned).is_empty()),
                "{name} accepts none of what the tone-map segment would pin \
                 ({pinned:?}). Its sink caps: {sink:?}"
            );
            // And every format in the pin must suit it, since the one
            // negotiation picks is not ours to choose.
            let Some(list) = pinned
                .structure(0)
                .and_then(|st| st.get::<gst::List>("format").ok())
            else {
                panic!("{name}: pinned caps carry no format list");
            };
            for fmt in list.iter() {
                let fmt = fmt.get::<String>().unwrap();
                let mut one = gst::Caps::builder("video/x-raw")
                    .field("format", &fmt)
                    .build();
                one.get_mut()
                    .unwrap()
                    .set_features(0, Some(gst::CapsFeatures::new_any()));
                assert!(
                    sink.iter().any(|c| !c.intersect(&one).is_empty()),
                    "{name} would be offered {fmt}, which it does not accept"
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "no encoders on this box to check against");
    }

    /// AR-13a: the tone-map probe has to END IN THE ENCODER it claims
    /// to feed. The old one ran `videotestsrc ! segment ! fakesink`
    /// with no encoder named, so the output pin fell back to the whole
    /// format list and a sink that accepts anything swallowed the
    /// result. It reported healthy on a box where every HDR session
    /// died at negotiation.
    ///
    /// This asserts the probe agrees with what the box can really do,
    /// for each encoder the box actually has: a target the segment can
    /// feed must verify, and `tonemap_available` must be exactly
    /// "some target verified" rather than a claim of its own.
    #[test]
    fn tonemap_is_verified_against_a_real_encoder() {
        crate::init().unwrap();
        let targets: Vec<&str> = [h264_encoder(), hevc_encoder(), av1_encoder()]
            .into_iter()
            .flatten()
            .collect();
        if targets.is_empty() {
            crate::testutil::not_applicable("no verified video encoder to test tone mapping");
            return; // no video encoder on this box; nothing to claim
        }
        let any = targets.iter().any(|e| tonemap_into(e));
        assert_eq!(
            tonemap_available(),
            any,
            "tonemap_available must mean 'some real target verified', nothing else"
        );
        // An element that is not an encoder at all cannot be a target,
        // and must not be reported as one — the fakesink-shaped hole.
        assert!(
            !tonemap_into("fakesink"),
            "a sink that accepts anything must not count as a verified target"
        );
        // Whatever the segment would pin for a verified target has to be
        // a format that target accepts; that pairing is the thing the
        // old probe could not see.
        for enc in &targets {
            if !tonemap_into(enc) {
                continue;
            }
            let pinned = tonemap_out_caps(enc);
            let list = pinned
                .structure(0)
                .and_then(|st| st.get::<gst::List>("format").ok())
                .expect("pinned caps carry a format list");
            assert!(!list.is_empty(), "{enc}: nothing pinned");
        }
    }

    /// The FIRST segment must be independently decodable, which is the
    /// property that actually reaches a viewer: a player that cannot
    /// decode segment 0 wedges at the start of the session no matter
    /// how healthy everything downstream is.
    ///
    /// HONEST LIMIT: this asserts the invariant but does NOT reproduce
    /// the failure that motivated it. That one needed a SEEKED start on
    /// a real source through vtenc — segment00000 carried slices with no
    /// SPS/PPS while the worker produced two minutes of good segments
    /// behind it. Here x264enc's first output is an IDR whatever the
    /// sink asks for, so this passes with the regression reintroduced
    /// (verified). It guards the invariant for the plain path; catching
    /// the seeked one needs a fixture on the real start_at path.
    #[test]
    fn the_first_segment_is_independently_decodable() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["hlssink3", "x264enc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 240i32)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(24000, 1001))
                    .field("width", 320i32)
                    .field("height", 180i32)
                    .build(),
            )
            .build()
            .unwrap();
        // No keyframe pin at all: the sink's request is what must put an
        // IDR at the head of fragment one.
        let enc = gst::ElementFactory::make("x264enc").build().unwrap();
        let parse = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()
            .unwrap();
        pipeline
            .add_many([&src, &caps, &enc, &parse, &sink])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();
        let pad = sink.request_pad_simple("video").unwrap();
        parse.static_pad("src").unwrap().link(&pad).unwrap();
        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
            "pipeline did not reach EOS"
        );

        let first = out.join("segment00000.ts");
        let bytes = std::fs::read(&first).expect("no first segment");
        // Walk the TS payload for H.264 start codes. Parameter sets and
        // an IDR must BOTH be present, or the segment cannot stand on
        // its own — which is all a player gets at session start.
        let mut kinds = std::collections::HashSet::new();
        for w in bytes.windows(4) {
            if w[0] == 0 && w[1] == 0 && w[2] == 1 {
                kinds.insert(w[3] & 0x1f);
            }
        }
        assert!(kinds.contains(&7), "segment00000 has no SPS: {kinds:?}");
        assert!(kinds.contains(&8), "segment00000 has no PPS: {kinds:?}");
        assert!(kinds.contains(&5), "segment00000 has no IDR: {kinds:?}");
    }

    /// The case the test above admits it cannot reach: a source that OPENS
    /// on frames preceding its first keyframe.
    ///
    /// Real and not rare — `Alita Battle Angel (2019)_SBS.mp4` spends 27
    /// B/P frames before its first IDR at 1.126 s. The sink cuts a new
    /// segment at that IDR, so those frames became the whole of
    /// `segment00000.ts`: 27 slices, no SPS, no PPS, no IDR. hls.js 1.7
    /// raises a fatal `bufferAppendError` on it and playback never leaves
    /// 0:00.
    ///
    /// x264enc will not produce that shape on its own, so the probe below
    /// makes it: drop the leading IDR and the stream opens on the rest of
    /// GOP 0, delta frames referencing a picture that is no longer there.
    /// Exactly the reference file's shape, and with the fix removed this
    /// test fails on the first assertion (verified — the segment's NAL
    /// types are {1, 9}: slices and access-unit delimiters, nothing a
    /// decoder can start from).
    #[test]
    fn a_source_that_opens_before_its_first_keyframe_still_decodes() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["hlssink3", "x264enc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 240i32)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(24000, 1001))
                    .field("width", 320i32)
                    .field("height", 180i32)
                    .build(),
            )
            .build()
            .unwrap();
        // Two GOPs, so there IS a later keyframe to open on.
        let enc = gst::ElementFactory::make("x264enc")
            .property("key-int-max", 48u32)
            .build()
            .unwrap();
        let parse = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()
            .unwrap();
        pipeline
            .add_many([&src, &caps, &enc, &parse, &sink])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();

        let tail = parse.static_pad("src").unwrap();
        let seen = std::sync::atomic::AtomicUsize::new(0);
        tail.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, info| {
                let first = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                if first && head_buffer(&info.data).is_some_and(is_keyframe) {
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            },
        );

        let pad = sink.request_pad_simple("video").unwrap();
        // What production installs on this pad, and the whole subject.
        open_on_keyframe(&pad);
        tail.link(&pad).unwrap();

        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
            "pipeline did not reach EOS"
        );

        let bytes = std::fs::read(out.join("segment00000.ts")).expect("no first segment");
        let mut kinds = std::collections::HashSet::new();
        for w in bytes.windows(4) {
            if w[0] == 0 && w[1] == 0 && w[2] == 1 {
                kinds.insert(w[3] & 0x1f);
            }
        }
        assert!(kinds.contains(&7), "segment00000 has no SPS: {kinds:?}");
        assert!(kinds.contains(&8), "segment00000 has no PPS: {kinds:?}");
        assert!(kinds.contains(&5), "segment00000 has no IDR: {kinds:?}");
    }

    /// One cut source, not two. The sink's keyframe request defines the
    /// fragment; the encode chain's GOP pin is a far-away backstop that
    /// must never fire first. Pinned NEAR the fragment interval the two
    /// cadences compete — they count different things, frames against
    /// seconds — and the sink cuts at both: 1.96 s and 1.04 s segments
    /// alternating, every short one costing a keyframe. Built through
    /// `make_hls_sink` so the production configuration is under test,
    /// and with production's backstop rather than a competing pin.
    #[test]
    fn segments_run_one_gop_each() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["hlssink3", "x264enc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 480i32) // 20 s at 23.976
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(24000, 1001))
                    .field("width", 320i32)
                    .field("height", 180i32)
                    .build(),
            )
            .build()
            .unwrap();
        // The backstop production uses: far longer than the fragment
        // interval, so the sink's request is what actually cuts.
        let enc = gst::ElementFactory::make("x264enc")
            .property("key-int-max", 240u32)
            .build()
            .unwrap();
        let parse = gst::ElementFactory::make("h264parse").build().unwrap();
        pipeline
            .add_many([&src, &caps, &enc, &parse, &sink])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();
        let pad = sink.request_pad_simple("video").unwrap();
        parse
            .static_pad("src")
            .unwrap()
            .link(&pad)
            .expect("linking to the sink");

        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
            "pipeline did not reach EOS"
        );

        let pl = std::fs::read_to_string(out.join("master.m3u8")).unwrap();
        let durs: Vec<f64> = pl
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse().ok())
            .collect();
        assert!(durs.len() >= 4, "too few segments to judge:\n{pl}");
        // The last segment is whatever content remained at EOS; every
        // other one is a whole GOP. A short segment among them is the
        // sink cutting on its own schedule again.
        let (body, _) = durs.split_at(durs.len() - 1);
        let shortest = body.iter().cloned().fold(f64::INFINITY, f64::min);
        let longest = body.iter().cloned().fold(0.0, f64::max);
        assert!(
            longest - shortest < 0.2,
            "segments are not one GOP each (min {shortest:.3}, max {longest:.3}): {body:?}"
        );
    }

    /// A viewer that never reports must not freeze the session.
    ///
    /// `viewer.pos` absent and a zero-width window is the shape of the
    /// stall measured at 42 s (copy) and 47 s (transcode): the pacer
    /// shuts after the first buffer, nothing reopens it, the playlist
    /// stops changing, and a LIVE client calls that an error. The age
    /// release is what reopens it — with the release off, the same
    /// pipeline does not finish at all, which is the control that says
    /// the release is what did it and not the fixture being too small
    /// to pace.
    #[test]
    fn a_stale_playlist_releases_a_shut_pacing_window() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["hlssink3", "x264enc"]) {
            return;
        }

        // Returns (reached EOS, segments listed).
        let run = |stale_ms: Option<u64>, secs: u64| -> (bool, usize) {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("hls");
            std::fs::create_dir_all(&out).unwrap();
            let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();

            let pipeline = gst::Pipeline::new();
            let src = gst::ElementFactory::make("videotestsrc")
                .property("num-buffers", 120i32) // 5 s at 24
                .build()
                .unwrap();
            let caps = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("framerate", gst::Fraction::new(24, 1))
                        .field("width", 320i32)
                        .field("height", 180i32)
                        .build(),
                )
                .build()
                .unwrap();
            // Second-long GOPs: several segments inside a short run.
            let enc = gst::ElementFactory::make("x264enc")
                .property("key-int-max", 24u32)
                .build()
                .unwrap();
            let parse = gst::ElementFactory::make("h264parse").build().unwrap();
            pipeline
                .add_many([&src, &caps, &enc, &parse, &sink])
                .unwrap();
            gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();

            let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
            install_pace_probe(
                &parse.static_pad("src").unwrap(),
                Arc::new(PaceConfig {
                    // Zero window: every buffer past the floor is held,
                    // so only the age release can let anything through.
                    window_ms: 0,
                    floor_ms: 0,
                    // Never written — the non-reporting client.
                    viewer_file: out.join("viewer.pos"),
                    out_dir: out.clone(),
                    stale_ms,
                }),
                stopping.clone(),
                None,
            );
            let pad = sink.request_pad_simple("video").unwrap();
            parse.static_pad("src").unwrap().link(&pad).unwrap();

            // Not run_to_eos: the probe holds a streaming thread, and
            // NULL waits for that thread, so the stop flag has to be
            // set BEFORE the transition or teardown deadlocks — the
            // control case blocks forever by construction.
            pipeline.set_state(gst::State::Playing).unwrap();
            let msg = pipeline.bus().unwrap().timed_pop_filtered(
                gst::ClockTime::from_seconds(secs),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
            stopping.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = pipeline.set_state(gst::State::Null);

            let segments = std::fs::read_to_string(out.join("master.m3u8"))
                .map(|p| p.matches("#EXTINF:").count())
                .unwrap_or(0);
            (
                matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
                segments,
            )
        };

        let (eos, segments) = run(Some(100), 60);
        assert!(eos, "a stale playlist did not release the pacer");
        assert!(
            segments >= 3,
            "released, but produced {segments} segments — the playlist would still look frozen"
        );

        let (control_eos, _) = run(Some(0), 5);
        assert!(
            !control_eos,
            "the control finished with the release off, so the window was never shut \
             and this test proves nothing"
        );
    }

    /// The allowance decides how much of the pacing window a
    /// non-reporting viewer gets back, so both of its inputs matter.
    #[test]
    fn the_stale_allowance_paces_by_segment_and_defers_to_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let pl = dir.path().join("master.m3u8");
        let write = |body: &str| std::fs::write(&pl, body).unwrap();

        // An explicit setting wins outright, including "never".
        write("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\na.ts\n");
        assert_eq!(playlist_stale_allowance_ms(&pl, Some(1234)), 1234);
        assert_eq!(playlist_stale_allowance_ms(&pl, Some(0)), 0);

        // Honest declaration: the segment sets the pace (6 s of media
        // per 6 s of wall ≈ real time), well inside the client's 21 s.
        assert_eq!(playlist_stale_allowance_ms(&pl, None), 6_000);

        // The `ignore` profile's constant 2 against 5.18 s GOPs — the
        // case measured at 2.6x. The client's 3.5 x 2 x 0.7 = 4.9 s now
        // binds instead of the declared 2 s.
        write("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:5.18,\na.ts\n#EXTINF:4.0,\nb.ts\n");
        assert_eq!(playlist_stale_allowance_ms(&pl, None), 4_900);

        // Nothing to go on yet: the fallback, not zero — zero would
        // mean "never release" and re-freeze the session.
        write("#EXTM3U\n");
        assert_eq!(
            playlist_stale_allowance_ms(&pl, None),
            PACE_STALE_FALLBACK_MS
        );
        assert_eq!(
            playlist_stale_allowance_ms(&dir.path().join("nope.m3u8"), None),
            PACE_STALE_FALLBACK_MS
        );
    }

    /// HALF ONE: does concat, fed by the production appsrc, produce a
    /// single continuous HLS playlist across a part boundary?
    #[test]
    fn concat_over_appsrc_yields_one_continuous_playlist() {
        crate::init().unwrap();
        if !crate::testutil::require_elements(&["hlssink3"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            fixture(dir.path(), "p1.mkv", "smpte"),
            fixture(dir.path(), "p2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();

        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse"]);
        let sink = gst::ElementFactory::make("hlssink3")
            .property("target-duration", 2u32)
            .property("playlist-length", 0u32)
            .property("max-files", 0u32)
            .property("location", out.join("seg%05d.ts").to_str().unwrap())
            .property("playlist-location", out.join("play.m3u8").to_str().unwrap())
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        // hlssink3 muxes internally: elementary streams on request pads.
        let pad = sink.request_pad_simple("video").unwrap();
        // imp.rs:304 unwraps each fragment's first PTS and a panic in an
        // FFI callback kills the process, so guard here as production does.
        guard_pts(&tail.static_pad("src").unwrap());
        tail.static_pad("src").unwrap().link(&pad).unwrap();

        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(
                msg.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            ),
            "pipeline did not reach EOS: {msg:?}"
        );

        let playlist = std::fs::read_to_string(out.join("play.m3u8")).unwrap();
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        // Two 5 s parts arriving as one stream, and NO discontinuity tag:
        // the muxer never learns there was a boundary.
        assert!(
            total > 9.0,
            "playlist covers only {total}s — the second part is missing"
        );
        assert!(
            !playlist.contains("EXT-X-DISCONTINUITY"),
            "timeline broke at the seam"
        );
        assert!(
            playlist.contains("EXT-X-ENDLIST"),
            "playlist never finalised"
        );
    }

    /// HALF TWO: can the concatenated timeline be SEEKED, or must a seek
    /// keep restarting the pipeline in the target part as it does today?
    /// `concat`'s documentation says nothing about seeking, so this is
    /// the deciding measurement for whether one pipeline can serve a
    /// whole multi-part film.
    #[test]
    fn seeking_across_the_concat_boundary() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            fixture(dir.path(), "s1.mkv", "smpte"),
            fixture(dir.path(), "s2.mkv", "ball"),
        ];
        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse", "fakesink"]);
        tail.set_property("sync", false);

        // First buffer PTS after the seek: where playback actually resumed.
        let seen: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        tail.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    let mut s = seen2.lock().unwrap();
                    if s.is_none() {
                        *s = b.pts().map(|p| p.mseconds());
                    }
                }
                gst::PadProbeReturn::Ok
            });

        // (a) seek from PAUSED, after preroll.
        pipeline.set_state(gst::State::Paused).unwrap();
        let _ = pipeline.state(gst::ClockTime::from_seconds(30));
        // 7 s lands inside part two (two 5 s parts).
        let paused_ok = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(7_000),
        );
        let msg = run_to_eos(&pipeline, 60);
        let from_paused = *seen.lock().unwrap();
        eprintln!(
            "SPIKE paused-seek accepted={paused_ok:?} first_pts={from_paused:?}ms eos={}",
            matches!(
                msg.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            )
        );
        // Characterisation, deliberately pinned to today's behaviour: the
        // seek is ACCEPTED and then ignored — playback resumes at zero,
        // not at 7 s. Anything built on concat seeks would look correct in
        // a paused test and silently restart the film in a real player.
        assert!(paused_ok.is_ok(), "a paused seek used to be accepted");
        assert_eq!(
            from_paused,
            Some(0),
            "concat now honours seeks — revisit the design"
        );

        // (b) the realistic case: scrub while playing.
        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse", "fakesink"]);
        tail.set_property("sync", false);
        let live: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let live2 = live.clone();
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let armed2 = armed.clone();
        tail.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data
                    && armed2.load(std::sync::atomic::Ordering::SeqCst)
                {
                    let mut s = live2.lock().unwrap();
                    if s.is_none() {
                        *s = b.pts().map(|p| p.mseconds());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let live_ok = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(7_000),
        );
        let msg2 = run_to_eos(&pipeline, 60);
        eprintln!(
            "SPIKE live-seek accepted={live_ok:?} first_pts={:?}ms eos={}",
            *live.lock().unwrap(),
            matches!(
                msg2.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            )
        );
        // Seeking while PLAYING is refused outright. If this ever starts
        // succeeding, one pipeline could serve seeks too and the
        // restart-per-part path could go.
        assert!(
            live_ok.is_err(),
            "concat now accepts a live seek — revisit the design"
        );
        // Recorded, not asserted: this test exists to MEASURE, and the
        // answer decides the design. Whatever it prints is the finding.
    }
}
