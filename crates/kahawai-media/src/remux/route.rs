use super::*;

/// Streams parsebin must be stopped from parsing, so the chain we attach
/// gets the demuxer's own buffers instead of parsebin's output.
///
/// AV1 is stopped for every container. parsebin fixes a pad's caps when it
/// exposes it and never renegotiates, so it settles AV1 on
/// `alignment=frame`; isofmp4mux demands `alignment=tu`, and the
/// frame→tu conversion a second av1parse performs drops every buffer
/// timestamp — 16,796 dropped by `guard_pts`, muxer starved, session
/// frozen. Fed raw OBUs, our av1parse produces `tu` directly.
///
/// Already-packetized HEVC is also stopped, but only for fMP4. A Dolby
/// Vision profile 8.1 SPS carries a multilayer extension GStreamer's
/// h265parse cannot preserve; parsebin's parser rewrites the codec_data and
/// isofmp4mux rejects it. The Matroska demuxer already supplied hvc1/au, so
/// that parser had no conversion to perform. The untouched caps work through
/// h265timestamper and isofmp4mux.
///
/// This is NOT the general rule "one parser per stream", which was tried
/// and measurably costs more than it buys. Applied to h264 it
/// makes mpegtsmux emit one timestamp-less PES per keyframe — a bare
/// 9-byte PPS, which ffmpeg reports as an access unit with no picture
/// and the sweep flags as a missing DTS (6 of 60 files). The bytes
/// h264parse produces are byte-identical either way; what the second
/// parser contributes is re-segmenting them, and that is what keeps the
/// muxer's PES boundaries honest. Measured with plain gst-launch, no
/// kahawai involved: `qtdemux ! h264parse ! mpegtsmux` gives 106
/// timestamp-less packets, `qtdemux ! h264parse ! h264parse !
/// mpegtsmux` gives none.
pub(super) fn fmp4_ready_hevc(caps: &gst::CapsRef) -> bool {
    caps.structure(0).is_some_and(|s| {
        s.name() == "video/x-h265"
            && matches!(s.get::<&str>("stream-format").ok(), Some("hvc1" | "hev1"))
            && s.get::<&str>("alignment").ok() == Some("au")
    })
}

pub(super) fn parsebin_must_not_parse(caps: &gst::CapsRef, format: SegmentFormat) -> bool {
    caps.structure(0).is_some_and(|s| s.name() == "video/x-av1")
        || (format == SegmentFormat::Fmp4 && fmp4_ready_hevc(caps))
}

/// TS muxing needs specific stream-formats (h26x as Annex-B byte-stream,
/// AAC as ADTS) while containers store avc/hvc1/raw. A per-stream parser
/// between demux and muxer converts during caps negotiation — pure
/// repackaging, still no re-encoding.
///
/// Do not parse HEVC twice when Matroska has already supplied the exact
/// hvc1/au shape fMP4 needs. GStreamer's H.265 parser cannot preserve the
/// multilayer SPS extension carried by Dolby Vision profile 8.1: a second
/// pass rewrites `codec_data`, after which `isofmp4mux` rejects caps that it
/// accepted before the rewrite. `h265timestamper` still follows and supplies
/// the DTS fMP4 requires.
pub(super) fn parser_for(caps: &gst::CapsRef, format: SegmentFormat) -> Option<&'static str> {
    let s = caps.structure(0)?;
    if format == SegmentFormat::Fmp4 && fmp4_ready_hevc(caps) {
        return None;
    }
    let element = match s.name().as_str() {
        "video/x-h264" => "h264parse",
        "video/x-h265" => "h265parse",
        // mpegvideoparse only takes MPEG-1/2; Part 2 (DivX/XviD-era AVIs)
        // needs mpeg4videoparse or the muxer pad starves and the pipeline
        // hangs forever.
        "video/mpeg" => match s.get::<i32>("mpegversion").ok() {
            Some(4) => "mpeg4videoparse",
            _ => "mpegvideoparse",
        },
        "video/x-av1" => "av1parse",
        "video/x-vp9" => "vp9parse",
        "audio/mpeg" => match s.get::<i32>("mpegversion").ok() {
            Some(1) => "mpegaudioparse",
            _ => "aacparse",
        },
        "audio/x-ac3" | "audio/x-eac3" => "ac3parse",
        "audio/x-dts" => "dcaparse",
        "audio/x-opus" => "opusparse",
        _ => return None,
    };
    // Availability-guarded (plugin-fallback strategy): missing parser →
    // try a direct link rather than failing outright.
    gst::ElementFactory::find(element)
        .is_some()
        .then_some(element)
}

/// H.26x streams with B-frames need PTS/DTS recomputed from picture order
/// count, or mpegtsmux emits one frame out of decode order at each segment
/// boundary. mpv tolerates the DTS glitch; hls.js's MSE transmuxer rejects
/// the segment (`bufferAppendError`) and the browser shows garbage. The
/// timestamper fixes it at zero re-encode cost. Availability-guarded.
pub(super) fn timestamper_for(caps: &gst::CapsRef) -> Option<&'static str> {
    let element = match caps.structure(0)?.name().as_str() {
        "video/x-h264" => "h264timestamper",
        "video/x-h265" => "h265timestamper",
        _ => return None,
    };
    gst::ElementFactory::find(element)
        .is_some()
        .then_some(element)
}

/// hlssink2 pads requested up front (splitmuxsink wants them before start);
/// each is taken by the first matching parsed stream.
/// Where each stream's branch terminates, once its real caps are known:
/// the muxer's pad for that stream, claimed once.
///
/// A MULTI-PART source has one of these PER PART, holding that part's
/// pre-claimed `concat` sink pad instead of the muxer's. concat plays its
/// sink pads in the order they were REQUESTED, so they are requested up
/// front in timeline order — claiming them lazily from `pad-added` races
/// across the parts' parsebins and can run CD2 first.
pub(super) type WaitingPads = Arc<Mutex<std::collections::HashMap<&'static str, gst::Pad>>>;

/// Offset-start gate: splitmuxsink is not flush-safe once it has seen
/// data (g_assert !ctx->is_reference aborts on a mid-GOP flush), so for
/// start_ms > 0 every pad feeding the HLS sink is blocked until the
/// initial seek has flushed through a still-virgin muxer.
pub(super) struct SeekGate {
    blocked: Mutex<Vec<(gst::Pad, gst::PadProbeId)>>,
    triggered: std::sync::atomic::AtomicUsize,
    expected: usize,
}

impl SeekGate {
    pub(super) fn new(expected: usize) -> Arc<Self> {
        Arc::new(Self {
            blocked: Mutex::new(Vec::new()),
            triggered: std::sync::atomic::AtomicUsize::new(0),
            expected,
        })
    }

    /// Block `pad` (a muxer feed) until [`open`]; counts the first
    /// arrival so the seek can wait for all branches to be negotiated.
    pub(super) fn install(self: &Arc<Self>, pad: &gst::Pad) {
        let gate = self.clone();
        let counted = std::sync::atomic::AtomicBool::new(false);
        let id = pad
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    if !counted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        gate.triggered
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        self.blocked.lock().unwrap().push((pad.clone(), id));
    }

    pub(super) fn all_triggered(&self) -> bool {
        self.triggered.load(std::sync::atomic::Ordering::SeqCst) >= self.expected
    }

    /// Open the gates. The seek's KEY_UNIT flag snapped to a keyframe at
    /// or before the requested start, so the true playlist origin is only
    /// knowable now: the first post-flush buffer on each feed reports its
    /// stream time into `start.pos` (players align subtitles/seekbar to
    /// it — TS PTS can't carry this, mpegtsmux rebases to a fixed epoch).
    pub(super) fn open_reporting(&self, start_pos: std::path::PathBuf) {
        let min = Arc::new(std::sync::Mutex::new(u64::MAX));
        for (pad, id) in self.blocked.lock().unwrap().drain(..) {
            let min = min.clone();
            let path = start_pos.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data
                    && let Some(pts) = b.pts()
                    && let Some(seg) = pad
                        .sticky_event::<gst::event::Segment>(0)
                        .and_then(|e| e.segment().downcast_ref::<gst::ClockTime>().cloned())
                    && let Some(st) = seg.to_stream_time(pts)
                {
                    report_start(&min, &path, st.mseconds());
                    return gst::PadProbeReturn::Remove;
                }
                gst::PadProbeReturn::Ok // unstamped buffer: keep waiting
            });
            pad.remove_probe(id);
        }
    }
}

/// Record `ms` as the playlist origin if it is the earliest a gated pad
/// has reported.
///
/// The decision and the WRITE are one critical section on purpose. Each
/// pad reports from its own streaming thread, and an atomic compare that
/// leaves the write outside it is a check-then-act: audio decides 2020 is
/// the lowest so far, video then decides 2000 and writes it, and audio —
/// preempted between its decision and its write — lands 2020 on top. The
/// file then claims playback began LATER than it did, and every client
/// aligning subtitles and the seekbar to `start.pos` is off by the
/// difference. Two threads, one file, one lock.
pub(super) fn report_start(min: &std::sync::Mutex<u64>, path: &std::path::Path, ms: u64) {
    let mut lowest = min.lock().unwrap_or_else(|e| e.into_inner());
    if ms < *lowest {
        *lowest = ms;
        let _ = std::fs::write(path, ms.to_string());
    }
}

/// Plumb a fresh parsebin pad. Muxable-looking streams are routed right
/// here, synchronously — elements built before data flows behave
/// differently from elements inserted mid-stream (h264parse merges
/// parameter-set AUs when added mid-flow and drains a timestampless PPS
/// runt at EOS; corpus-sweep regression), so the pre-roll path must stay
/// the pre-roll path. But advertised caps can also lie: mislabeled
/// tracks (E-AC-3 tag, AC-3 bitstream) are re-typed by parsebin's
/// internal parser only once data flows, and fakesinking them on the
/// advertised caps starved the muxer pad forever (corpus-sweep finding).
/// So only apparently-unmuxable streams defer routing to the real caps
/// event; the queue absorbs data while the decision waits, and the caps
/// event precedes the first buffer in the same streaming thread, so
/// deciding in the probe is race-free.
/// The plan's mode for a stream of these caps.
pub(super) fn mode_for(caps_name: &str, plan: &RemuxPlan) -> StreamMode {
    if caps_name.starts_with("video/") {
        plan.video
    } else if caps_name.starts_with("audio/") {
        plan.audio
    } else {
        StreamMode::Off
    }
}

/// Would route_stream do something useful with a stream of these caps?
pub(super) fn routable(caps: &gst::Caps, plan: &RemuxPlan) -> bool {
    let Some(caps_name) = caps.structure(0).map(|s| s.name()) else {
        return false;
    };
    match mode_for(caps_name, plan) {
        StreamMode::Copy => sink_compatible(caps, plan.segment_format).is_some(),
        StreamMode::Encode => can_decode(caps_name),
        StreamMode::Off => false,
    }
}

#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
pub(super) fn plumb_parsed_pad(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    pad: &gst::Pad,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
    audio_seen: &Arc<std::sync::atomic::AtomicUsize>,
    video_seen: &Arc<std::sync::atomic::AtomicUsize>,
    subs_seen: &Arc<std::sync::atomic::AtomicUsize>,
    video_canvas: &Arc<Mutex<Option<(u32, u32)>>>,
    subs_dir: &std::path::Path,
    burn: &Option<std::sync::Arc<crate::burnin::Timeline>>,
    // HUB-32a: Some when this session burns ASS — the chosen track's pad
    // goes to assrender instead of a tap file.
    ass_link: &Option<Arc<Mutex<AssBurnLink>>>,
    // False for the second and later parts of a multi-part source: the
    // tracks are the same ones continuing, so extracting them again would
    // overwrite the first part's files with a stream that starts at its
    // own zero.
    extract_subs: bool,
) {
    // queue: decouples the muxer from parsebin's threads (the aggregator
    // deadlocks without it). Default queue limits (1 MiB / 1 s) are far
    // too small: the HLS sink holds one branch back while waiting for a
    // keyframe-aligned cut on the other, and files with uneven track ends
    // or high bitrates deadlock (corpus-sweep finding). Bound by bytes
    // only — generous enough for real interleave skew, still OOM-safe.
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-bytes", 64u32 * 1024 * 1024)
        .property("max-size-buffers", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .unwrap();
    pipe.add(&queue).unwrap();
    queue.sync_state_with_parent().unwrap();
    pad.link(&queue.static_pad("sink").unwrap()).unwrap();
    let qsrc = queue.static_pad("src").unwrap();
    // `GstStream::caps()` at pad-added time often names only the codec;
    // width/height land in the later CAPS event. Observe that event for every
    // video stream, including streams we can route immediately, so an MP4
    // VobSub tap can use the real canvas instead of racing pad discovery.
    let event_video_canvas = video_canvas.clone();
    qsrc.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let gst::EventView::Caps(caps) = event.view()
        {
            remember_video_canvas(caps.caps(), &event_video_canvas);
        }
        gst::PadProbeReturn::Ok
    });

    let advertised = pad
        .stream()
        .and_then(|s| s.caps())
        .or_else(|| pad.current_caps())
        .unwrap_or_else(gst::Caps::new_empty);
    let name = advertised
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    remember_video_canvas(&advertised, video_canvas);
    // `GstStream::caps()` is intentionally preferred above for routing, but
    // parsebin's already-negotiated current caps are where qtdemux puts the
    // video dimensions. A generic stream caps value otherwise masks them.
    if let Some(current) = pad.current_caps() {
        remember_video_canvas(&current, video_canvas);
    }
    // Track selection: only the plan's audio track proceeds (demux order
    // matches discovery order — the assumption subtitle extraction
    // already relies on). Streams whose advertised caps hide their
    // audio-ness take the deferred path uncounted; acceptable, they're
    // also unroutable-looking to the picker UI.
    // HUB-32 live tap: ASS events already flow through this pipeline
    // from the session origin — write them to a session file the hub
    // streams to ASS-rendering clients. No second read of the source.
    // Indexing counts every subtitle pad in demux order, matching the
    // discovery-order e{n} keys.
    if name.starts_with("application/x-subtitle")
        || name.starts_with("application/x-ssa")
        || name.starts_with("application/x-ass")
        || name.starts_with("text/")
        || name.starts_with("subpicture/")
    {
        if !extract_subs {
            let fake = gst::ElementFactory::make("fakesink").build().unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = qsrc.link(&fake.static_pad("sink").unwrap());
            return;
        }
        let idx = subs_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // HUB-32a: the burned track feeds assrender, not a tap. No tee —
        // a burned track is in the picture, so nothing would fetch the
        // sidecar, and switching away from a burn restarts the session.
        if plan.burn_ass == Some(idx)
            && let Some(link) = ass_link
        {
            tracing::info!(track = idx, caps = %name, "ASS burn: routing track to assrender");
            AssBurnLink::offer_source(link, AssSource::Pad(qsrc));
            return;
        }
        if name.starts_with("subpicture/") {
            tap_image_track(pipe, &qsrc, &advertised, subs_dir, idx, &name, video_canvas);
        } else {
            tap_text_track(pipe, &qsrc, &advertised, subs_dir, idx, &name);
        }
        return;
    }
    let unselected = if name.starts_with("audio/") {
        let idx = audio_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (idx != plan.audio_track).then_some((idx, plan.audio_track, "audio"))
    } else if name.starts_with("video/") || name.starts_with("image/") {
        let idx = video_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (idx != plan.video_track).then_some((idx, plan.video_track, "video"))
    } else {
        None
    };
    {
        if let Some((idx, selected, kind)) = unselected {
            tracing::info!(caps = %name, idx, selected, kind, "remux: dropping unselected track");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            if let Err(e) = qsrc.link(&fake.static_pad("sink").unwrap()) {
                tracing::warn!(caps = %name, error = %e, "remux: fakesink link failed");
            }
            return;
        }
    }
    if routable(&advertised, &plan) {
        route_stream(
            pipe,
            waiting,
            &qsrc,
            &advertised,
            plan,
            gate,
            burn,
            ass_link,
            subs_dir,
        );
        return;
    }

    let pipe = pipe.clone();
    let waiting = waiting.clone();
    let gate = gate.clone();
    let burn = burn.clone();
    let ass_link = ass_link.clone();
    let facts_dir = subs_dir.to_path_buf();
    qsrc.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |qpad, info| {
        if let Some(gst::PadProbeData::Event(ev)) = &info.data
            && let gst::EventView::Caps(c) = ev.view()
            && qpad.peer().is_none()
        {
            route_stream(
                &pipe,
                &waiting,
                qpad,
                &c.caps_owned(),
                plan,
                &gate,
                &burn,
                &ass_link,
                &facts_dir,
            );
        }
        gst::PadProbeReturn::Ok
    });
}

pub(super) fn remember_video_canvas(caps: &gst::CapsRef, canvas: &Arc<Mutex<Option<(u32, u32)>>>) {
    let Some(structure) = caps.structure(0) else {
        return;
    };
    if !structure.name().starts_with("video/") {
        return;
    }
    let Ok(width) = structure.get::<i32>("width") else {
        return;
    };
    let Ok(height) = structure.get::<i32>("height") else {
        return;
    };
    if width > 0 && height > 0 {
        *canvas.lock().unwrap_or_else(|e| e.into_inner()) = Some((width as u32, height as u32));
    }
}

/// Route a stream to the muxer (via parser/timestamper) or a fakesink,
/// now that its negotiated caps are known.
/// hlssink3 (≤0.15.3, imp.rs:304) unwraps the PTS of each fragment's
/// first buffer; a PTS-less frame (old AVI streams, parser EOS drains)
/// aborts the whole process — a Rust panic in an FFI callback cannot
/// unwind. Guard every pad that feeds the sink: borrow the DTS, or drop
/// the buffer.
pub(super) fn guard_pts(pad: &gst::Pad) {
    pad.add_probe(gst::PadProbeType::BUFFER, |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
            // ponytail: pts=dts misorders B-frames on the copy path
            // (sweep flags those [bad dts] → they plan as Encode now);
            // dropping instead starves fragments and trips more panics.
            if buffer.pts().is_none() {
                match buffer.dts() {
                    Some(dts) => buffer.make_mut().set_pts(dts),
                    None => return gst::PadProbeReturn::Drop,
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
pub(super) fn route_stream(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    from: &gst::Pad,
    caps: &gst::Caps,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
    burn: &Option<std::sync::Arc<crate::burnin::Timeline>>,
    ass_link: &Option<Arc<Mutex<AssBurnLink>>>,
    facts_dir: &std::path::Path,
) {
    let caps_name = caps
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    let source_dimensions = caps.structure(0).and_then(|structure| {
        let width = structure.get::<i32>("width").ok()?;
        let height = structure.get::<i32>("height").ok()?;
        (width > 0 && height > 0).then_some((width, height))
    });
    let mode = mode_for(&caps_name, &plan);
    // Encode: claim the kind's muxer pad for the decode→re-encode branch.
    if mode == StreamMode::Encode && can_decode(&caps_name) {
        let kind = if caps_name.starts_with("video/") {
            "video"
        } else {
            "audio"
        };
        if let Some(sinkpad) = waiting.lock().unwrap().remove(kind) {
            tracing::info!(caps = %caps_name, kind, "transcoding stream");
            if kind == "video" {
                build_video_encode_chain(
                    pipe,
                    from,
                    sinkpad,
                    gate,
                    plan.video_codec,
                    plan.video_kbps,
                    plan.max_height,
                    source_dimensions,
                    plan.tone_map,
                    plan.deinterlace,
                    burn.clone(),
                    ass_link.clone(),
                );
            } else {
                build_audio_encode_chain(
                    pipe,
                    from,
                    sinkpad,
                    &caps_name,
                    gate,
                    plan.audio_codec,
                    plan.max_channels,
                    AudioLoudnessGains {
                        exact: plan.loudness_gains,
                        stereo_db: plan.stereo_gain_db,
                        native_db: plan.native_gain_db,
                        source_channels: plan.loudness_source_channels,
                    },
                    facts_dir,
                );
            }
            return;
        }
    }
    let target = (mode == StreamMode::Copy)
        .then(|| {
            sink_compatible(caps, plan.segment_format)
                .and_then(|kind| waiting.lock().unwrap().remove(kind))
        })
        .flatten();
    match target {
        Some(sinkpad) => {
            // Stamp BEFORE the parser as well as after it.
            //
            // AVI carries no per-frame presentation times, so avidemux
            // hands out h264 with a DTS and no PTS on all but the
            // keyframes — 114,154 of 124,532 buffers on the file that
            // found this. h264parse then holds every one of them: an
            // hour of video went in and nothing came out, splitmuxsink
            // sat on `Sleeping for running time 99:99:99.999999999`
            // (that is CLOCK_TIME_NONE) waiting for a video pad that
            // never produced anything, and the session yielded audio
            // and no segments at all. Fixing the timestamps at the
            // chain's exit was too late to help the parser inside it.
            //
            // A no-op for anything that already carries a PTS, which is
            // every container that stores one.
            //
            // VIDEO only. Matroska LACES audio — eight-ish Opus frames
            // per block, and the demuxer stamps only the first of each
            // lace — so a drop here threw away seven of every eight
            // audio packets and the muxer stretched the survivor over
            // the hole: 20 ms of sound in every 160 ms, the choppiness
            // itself. The parser right behind this pad is the thing
            // that fills those timestamps in; the tail guard below
            // still protects the muxer.
            if caps_name.starts_with("video/") {
                guard_pts(from);
            }
            let mut tail = from.clone();
            // parser → timestamper, each present only when it applies;
            // every hop is pure repackaging, no decode.
            for name in [parser_for(caps, plan.segment_format), timestamper_for(caps)]
                .into_iter()
                .flatten()
            {
                let el = gst::ElementFactory::make(name).build().unwrap();
                // HLS requires independently decodable segments: h26x
                // parameter sets must ride every keyframe, or only the
                // first segment can start a decoder (players stall on
                // transitions and cold seeks).
                if name.ends_with("parse") {
                    set_prop_if_present(&el, "config-interval", -1i32);
                }
                pipe.add(&el).unwrap();
                el.sync_state_with_parent().unwrap();
                tail.link(&el.static_pad("sink").unwrap()).unwrap();
                tail = el.static_pad("src").unwrap();
            }
            guard_pts(&tail);
            // GATE UPSTREAM OF THE PARSER CHAIN, not at the muxer feed.
            //
            // The gate exists so the muxer is still virgin when the
            // offset seek flushes (splitmuxsink aborts on a mid-GOP
            // flush). Blocking at `from` keeps that property — nothing
            // gets past — and adds the one this path also needs: the
            // parser and `h264timestamper` never see pre-seek data
            // either, so the flush finds them with no state to corrupt.
            //
            // With the gate at the muxer feed instead, the timestamper
            // HAS built state by the time the seek flushes it, and
            // afterwards emits duplicate DTS (measured
            // `1.920, 1.960, 1.960, 2.000, 2.000` on a 25 fps source).
            // mpegtsmux rebases everything onto its own epoch and never
            // noticed; `isofmp4mux` PANICKED — "Timestamps going
            // backwards" — killing the worker, so the session produced
            // no segments and the player buffered forever. Reported
            // 2026-08-03 against an h264+FLAC episode, where every seek
            // and every resume rolled the dice.
            //
            // Blocking earlier is strictly stronger for the original
            // reason too: less reaches the sink, not more.
            if let Some(g) = gate {
                g.install(from);
            }
            if let Err(e) = tail.link(&sinkpad) {
                tracing::warn!(caps = %caps_name, error = %e, "remux: pad link failed");
            }
        }
        None => {
            tracing::info!(
                caps = %caps_name,
                container = plan.segment_format.as_str(),
                "remux: dropping stream (container cannot carry it, or duplicate)"
            );
            // sync=false: don't pace the dropped stream at realtime speed;
            // async=false: don't hold pipeline preroll hostage to a sparse
            // track (subtitles) that may not produce a buffer for minutes
            // (sweep finding: multi-track files deadlocked in PAUSED).
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            if let Err(e) = from.link(&fake.static_pad("sink").unwrap()) {
                tracing::warn!(caps = %caps_name, error = %e, "remux: fakesink link failed");
            }
        }
    }
}

pub(super) fn seek_parsed_stream(parsebin: &gst::Element, seek: gst::Event) -> bool {
    // GstBin broadcasts upstream events to its source pads. A Matroska
    // push-mode seek returns after requesting Cues, before its streaming
    // thread seeks back to the target cluster. A duplicate through another
    // pad can collide with that flush (or be deferred as another seek).
    // Send once, through video for keyframe alignment, or audio otherwise.
    // https://github.com/GStreamer/gstreamer/blob/1.28.7/subprojects/gstreamer/gst/gstbin.c
    let pads = parsebin.src_pads();
    let pad = ["video/", "audio/"].into_iter().find_map(|kind| {
        pads.iter().find(|pad| {
            pad.current_caps().is_some_and(|caps| {
                caps.structure(0)
                    .is_some_and(|s| s.name().starts_with(kind))
            })
        })
    });
    pad.is_some_and(|pad| pad.send_event(seek))
}
