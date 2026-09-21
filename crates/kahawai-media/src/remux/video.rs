use super::*;

/// decodebin → videoconvert → H.264 encoder → h264parse (byte-stream
/// for the TS muxer). Rescues codecs no browser decodes (MPEG-4 Part 2,
/// AV1/VP9-in-TS). videoconvert costs one GPU→CPU hop with hw decoders;
/// ponytail: cudaconvert zero-copy path when both ends are NVENC/NVDEC.
/// HUB-15a: the PQ→SDR fragment shader (see tonemap.frag for the why).
pub(super) const TONEMAP_FRAG: &str = include_str!("../tonemap.frag");

/// How long the burn-in index walk may take before the session gives
/// up on it and plays without subtitles (HUB-32b). Generous for local
/// disk, far short of the playlist deadline.
pub(super) const BURN_INDEX_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// PQ encode (SMPTE ST 2084 inverse EOTF): linear (1.0 = 10000 nits)
/// → PQ code. Rust twin of the shader's pq_encode.
pub(super) fn pq_encode(y: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let p = y.max(0.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

pub(super) fn pq_eotf(e: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let p = e.max(0.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

/// PQ(203 nits) — the EETF's SDR target, fixed.
pub(super) const TGT_E: f64 = 0.580688881;

/// Scene-peak probe (HUB-15a dynamic adaptation): sample the luma
/// plane of every buffer entering the GL segment, track a smoothed
/// p99.9 peak (instant attack, slow decay — libplacebo's shape), and
/// feed the shader's EETF uniforms. A static 1000-nit assumption
/// measured ~0.7 signal on real scene highlights where libplacebo
/// reaches ~0.98 (the owner's "grey smear"): typical frames peak at
/// 200–800 nits, far below mastering ceilings.
/// Sample every 16th row and column of a luma plane into `out` as
/// 10-bit codes (~32k samples at 4K: enough for p99.9, cheap enough
/// for every frame).
///
/// Pure and bounds-checked on purpose. It runs inside a pad probe
/// called from C, where a panic cannot unwind and ABORTS the worker,
/// and it is fed whatever buffer layout a decoder chose: `stride` is
/// the FRAME's, which for a padded buffer exceeds the one the caps
/// imply, and the final row may be short where the allocator packed
/// tightly. Neither may index past the plane.
pub(crate) fn sample_luma(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    ten_bit: bool,
    out: &mut Vec<u16>,
) {
    let bpp = if ten_bit { 2 } else { 1 };
    if stride < w * bpp {
        return; // nonsense geometry: sample nothing rather than guess
    }
    let mut y = 0;
    while y < h {
        let Some(row) = data.get(y * stride..(y * stride + w * bpp).min(data.len())) else {
            break;
        };
        let mut x = 0;
        while x * bpp + bpp <= row.len() && x < w {
            out.push(if ten_bit {
                let lo = row[x * 2] as u16;
                let hi = row[x * 2 + 1] as u16;
                ((hi << 8) | lo) >> 6 // P010: 10 bits in the high bits
            } else {
                (row[x] as u16) << 2
            });
            x += 16;
        }
        y += 16;
    }
}

pub(super) fn attach_peak_probe(upload: &gst::Element, shader: &gst::Element) {
    let pad = upload.static_pad("sink").unwrap();
    let shader = shader.clone();
    // (smoothed peak, last-set peak, reusable sample buffer)
    let state = std::sync::Mutex::new((1000.0f64, 1000.0f64, Vec::<u16>::new()));
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(caps) = pad.current_caps() else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(vinfo) = gst_video::VideoInfo::from_caps(&caps) else {
            return gst::PadProbeReturn::Ok;
        };
        use gst_video::VideoFormat;
        use gst_video::prelude::VideoFrameExt;
        let ten_bit = match vinfo.format() {
            VideoFormat::P01010le | VideoFormat::I42010le => true,
            VideoFormat::Nv12 | VideoFormat::I420 => false,
            _ => return gst::PadProbeReturn::Ok,
        };
        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &vinfo) else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(data) = frame.plane_data(0) else {
            return gst::PadProbeReturn::Ok;
        };
        // Stride and size come from the FRAME, never from the caps.
        // A decoder pads its buffers to suit itself — likely whenever
        // the coded size exceeds the display size, e.g. 1920x1038,
        // whose coded height is 1040 — so the caps-derived stride can
        // be smaller than the real one, and `y * stride` then indexes
        // past the plane. This probe runs in a pad probe called from C,
        // where a panic cannot unwind and ABORTS the worker: reported
        // as a SIGABRT at session start on an HDR title, and it
        // vanished when the client forced HDR (no tone-map, no probe).
        let stride = frame.plane_stride()[0].max(0) as usize;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let mut st = state.lock().unwrap();
        let (_, _, ref mut samples) = *st;
        samples.clear();
        sample_luma(data, stride, w, h, ten_bit, samples);
        if samples.is_empty() {
            return gst::PadProbeReturn::Ok;
        }
        samples.sort_unstable();
        let p999 = samples[samples.len() - 1 - samples.len() / 1000];
        let nits = (pq_eotf(p999 as f64 / 1023.0) * 10000.0).clamp(203.0, 4000.0);
        // Instant attack (a clipped flash is worse than a dim one),
        // ~2 s decay at 24 fps.
        st.0 = if nits > st.0 {
            nits
        } else {
            st.0 * 0.98 + nits * 0.02
        };
        if (st.0 - st.1).abs() / st.1 > 0.01 {
            st.1 = st.0;
            let max_e = pq_encode(st.0 / 10000.0);
            let max_tgt = TGT_E / max_e;
            let uniforms = gst::Structure::builder("uniforms")
                .field("uMaxE", max_e as f32)
                .field("uMaxTgt", max_tgt as f32)
                .field("uKS", (1.5 * max_tgt - 0.5) as f32)
                .build();
            shader.set_property("uniforms", &uniforms);
        }
        gst::PadProbeReturn::Ok
    });
}

/// What the tone-map segment may hand the encoder behind it. NV12
/// first: the hardware encoders take it directly, so they keep
/// negotiating it. I420 is what openh264enc — sink template I420 and
/// nothing else — needs to be linkable at all.
pub(crate) const TONEMAP_OUT_FORMATS: [&str; 2] = ["NV12", "I420"];

/// The first buffer of whatever is about to be pushed.
///
/// BUFFER **and** BUFFER_LIST: a parser is free to push lists, and a
/// BUFFER-only probe never sees those — half of why the first attempt at
/// the seeked-start fix below was a no-op. What matters about a list is
/// its FIRST buffer: that is what the muxer sees at the head of the
/// fragment.
pub(super) fn head_buffer<'a>(
    data: &'a Option<gst::PadProbeData<'a>>,
) -> Option<&'a gst::BufferRef> {
    match data {
        Some(gst::PadProbeData::Buffer(b)) => Some(b),
        Some(gst::PadProbeData::BufferList(l)) => l.get(0),
        _ => None,
    }
}

pub(super) fn is_keyframe(b: &gst::BufferRef) -> bool {
    !b.flags().contains(gst::BufferFlags::DELTA_UNIT)
}

/// Whether the muxer will KEEP this buffer, or discard it as lying
/// outside the segment it was told to play.
///
/// A buffer whose running time is negative is wholly before the segment
/// start and does not survive the muxer. That is not an error — it is
/// what an edit list means — but a keyframe that gets discarded cannot
/// open a fragment, and a gate that accepted one would let the frames
/// after it through with nothing to decode them against.
pub(super) fn survives_the_segment(pad: &gst::Pad, b: &gst::BufferRef) -> bool {
    let Some(pts) = b.pts() else { return true };
    let Some(seg) = pad
        .sticky_event::<gst::event::Segment>(0)
        .and_then(|e| e.segment().downcast_ref::<gst::ClockTime>().cloned())
    else {
        return true; // no segment yet: nothing to be outside of
    };
    !matches!(
        seg.to_running_time_full(pts),
        Some(gst::format::Signed::Negative(_))
    )
}

/// Hold video out of the muxer until a keyframe it will actually keep.
///
/// Installed on the muxer's video pad for EVERY session. The first
/// fragment a player gets has no earlier parameter sets to fall back on,
/// so it must open on an IDR; a fragment of slices referencing pictures
/// the decoder was never given is invalid HLS, and every player is
/// entitled to reject it. hls.js 1.7 does, fatally — playback never
/// leaves 0:00 — and 1.6 retried the segment a few times before starting
/// late with the un-referenced frames jittering alongside the real ones.
///
/// BOTH conditions, because either alone lets that fragment through:
///
///   * A source may open on frames that precede its first keyframe.
///   * A source may open on a keyframe the MUXER then discards.
///
/// The second is the one that took the diagnosis twice.
/// `Alita Battle Angel (2019)_SBS.mp4` leads with SPS, PPS and an IDR —
/// the bitstream is exactly right — but its edit list puts that IDR
/// 83 ms before the segment start, so it is clipped as out-of-range
/// while the 27 delta frames behind it, the first of which ends exactly
/// on the boundary, are kept. `segment00000.ts` came out as 27 slices,
/// 0 SPS, 0 PPS, 0 IDR. Nothing about the file is malformed and nothing
/// upstream is misconfigured; the parameter sets are simply on the one
/// access unit the segment excludes. Reading the keyframe FLAG alone
/// says "opened at a keyframe, dropped 0" and changes nothing.
///
/// ffprobe agrees with the muxer and reports that IDR as `key_frame=0`,
/// because it applies the edit list too. That is what first suggested a
/// source with no leading keyframe, which it is not.
///
/// The cost is the trimmed frames plus everything up to the next IDR,
/// and it is paid in silence too: the audio for that span IS muxed, but
/// a media element's buffered range is the INTERSECTION of its tracks,
/// so with no video before the first IDR playback cannot begin there and
/// the sound goes unheard. 1.126 s on that file — 1.126 s that before
/// this could not be decoded at all.
///
/// Keeping it instead means shifting BOTH pads' segments so nothing is
/// clipped; sync would hold, since they move together. WEIGHED AND
/// DECLINED, 2026-08-16: across 2,176 files in the movies, anime,
/// animore and 3d collections, exactly TWO lose anything —
/// `The Heroes of Telemark (1965).mp4` at 7.382 s (179 buffers dropped)
/// and this one at 1.126 s (29). Both `.mp4` with a trimmed opening IDR.
/// 8.5 s of library, against rewriting segment timestamps for every
/// session in the element whose documented failure mode is a
/// "Timestamps going backwards" panic, is not a trade worth taking.
/// Revisit if the count grows: the scan is one head-read per file, and
/// ffprobe's first keyframe agreed with this gate to the millisecond on
/// both.
///
/// The muxer's own pad is the one place that covers this once. It is
/// requested a single time per session, and on a multi-part source it is
/// `concat`'s downstream end, so every part passes through it — while the
/// branches upstream are per-part and per-mode, and gating them would be
/// several installs enforcing one invariant.
pub(super) fn open_on_keyframe(pad: &gst::Pad) {
    let dropped = std::sync::atomic::AtomicUsize::new(0);
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |pad, info| {
            let Some(b) = head_buffer(&info.data) else {
                return gst::PadProbeReturn::Ok;
            };
            if is_keyframe(b) && survives_the_segment(pad, b) {
                let n = dropped.load(std::sync::atomic::Ordering::SeqCst);
                if n > 0 {
                    tracing::info!(dropped = n, "video opened at the first usable keyframe");
                }
                return gst::PadProbeReturn::Remove;
            }
            dropped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Drop
        },
    );
}

/// Hold the ENCODER's output back until its first keyframe, asking it for
/// one as soon as data actually flows.
///
/// Installed only on a seeked start: an encoder resuming mid-stream does
/// not reliably lead with an IDR, and without this the wait for its next
/// natural one is the wait for the picture to appear. Measured on
/// VideoToolbox: start_ms=0 gives an IDR and parameter sets immediately,
/// start_ms=300000 gives 48 slices and none of the three. NVENC leads
/// with an IDR and never showed it, which is why every local test passed.
///
/// The request is sent from INSIDE the probe, on the first buffer we drop,
/// and that timing is the point: sent while building the pipeline it
/// reaches an encoder that is not playing yet and does nothing (measured —
/// the first attempt at this fix changed exactly nothing). By the time a
/// buffer arrives here the chain is running, so the event lands.
/// `hlssink3`'s own fragment-boundary requests prove the encoder honours
/// them; this just needs one earlier.
///
/// WHY THIS DROPS TOO, when [`open_on_keyframe`] on the muxer's pad would
/// keep the same frames out of the same segment: this pad is also where
/// [`SeekGate::open_reporting`] reads the playlist origin, from the first
/// buffer to carry a PTS. Let the pre-IDR frames past here and `start.pos`
/// names one of them — a position the muxer then discards, so the seekbar
/// and every subtitle hang off a frame the player never receives. On the
/// VideoToolbox measurement above that is 48 frames of error, about two
/// seconds. Dropping at the muxer alone is correct for the SEGMENT and
/// wrong for the ORIGIN.
pub(super) fn open_encoder_on_keyframe(pad: &gst::Pad) {
    let asked = std::sync::atomic::AtomicBool::new(false);
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |pad, info| {
            let Some(b) = head_buffer(&info.data) else {
                return gst::PadProbeReturn::Ok;
            };
            if is_keyframe(b) {
                return gst::PadProbeReturn::Remove;
            }
            if !asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let sent = pad.send_event(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
                tracing::info!(sent, "seeked start: asked the encoder for a keyframe");
            }
            gst::PadProbeReturn::Drop
        },
    );
}

/// The GL tone-map segment: upload → RGBA → PQ→SDR shader → back to
/// system memory, then capssetter rewrites the colorimetry tag to
/// bt709 so the encoder's VUI tells the player the truth (the shader
/// changed the pixels; nothing else knows to change the label).
/// The one output format to pin for `encoder`: the first of
/// [`TONEMAP_OUT_FORMATS`] its sink pad actually accepts.
///
/// A LIST is not a preference order. Offering `{NV12, I420}` to
/// `vah264enc` — which takes NV12 and not I420 — resolves to I420 and
/// the pipeline dies with not-negotiated. Measured on the J5005:
///
/// ```text
///   {NV12,I420}   not-negotiated
///   NV12          OK
///   I420          FAILED
/// ```
///
/// So the pin has to name the format the DOWNSTREAM ENCODER takes,
/// which means knowing which encoder that is. Falls back to the whole
/// list when the element cannot be probed — no worse than before, and
/// the only case where a list is honest.
pub(crate) fn tonemap_out_caps(encoder: &str) -> gst::Caps {
    let accepted: Vec<&str> = gst::ElementFactory::find(encoder)
        .map(|f| {
            let sinks: Vec<gst::Caps> = f
                .static_pad_templates()
                .into_iter()
                .filter(|t| t.direction() == gst::PadDirection::Sink)
                .map(|t| t.caps())
                .collect();
            TONEMAP_OUT_FORMATS
                .iter()
                .copied()
                .filter(|fmt| {
                    // Feature-agnostic: hardware templates publish under
                    // memory:VAMemory/GLMemory/CUDAMemory, and it is the
                    // FORMAT agreement being tested, not the memory space.
                    let mut want = gst::Caps::builder("video/x-raw")
                        .field("format", *fmt)
                        .build();
                    want.get_mut()
                        .unwrap()
                        .set_features(0, Some(gst::CapsFeatures::new_any()));
                    sinks.iter().any(|c| !c.intersect(&want).is_empty())
                })
                .collect()
        })
        .unwrap_or_default();
    let formats = if accepted.is_empty() {
        TONEMAP_OUT_FORMATS.to_vec()
    } else {
        accepted
    };
    gst::Caps::builder("video/x-raw")
        .field("format", gst::List::new(formats))
        .build()
}

pub(crate) fn tonemap_segment(encoder: &str) -> Vec<gst::Element> {
    let upload = gst::ElementFactory::make("glupload").build().unwrap();
    let to_rgba = gst::ElementFactory::make("glcolorconvert").build().unwrap();
    let rgba = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .features(["memory:GLMemory"])
                .field("format", "RGBA")
                .build(),
        )
        .build()
        .unwrap();
    let shader = gst::ElementFactory::make("glshader")
        .property("fragment", TONEMAP_FRAG)
        .build()
        .unwrap();
    let from_rgba = gst::ElementFactory::make("glcolorconvert").build().unwrap();
    let download = gst::ElementFactory::make("gldownload").build().unwrap();
    // Pinned HERE, GPU-side: without it glcolorconvert stays RGBA and a
    // VA encoder with no converter between (the non-CUDA path has none
    // after this segment) refuses system-memory RGBA — observed as
    // not-negotiated on the J5005.
    //
    // A LIST, not NV12 alone. "Every encoder we place takes NV12" was
    // written for the hardware ones and is false for openh264enc, whose
    // sink template is I420 and nothing else: pinning NV12 left the
    // segment's trailing capssetter unlinkable to it, and because the
    // chain is linked from a pad-added callback the `unwrap` on that
    // link became a non-unwinding panic — SIGABRT, no session error, on
    // every HDR title a software-encoder box was asked to tone-map
    // (field report 2026-08-01; the user's workaround was forcing HDR on
    // in the browser, which skips this segment entirely). NV12 stays
    // first so hardware still negotiates it; I420 is what makes the
    // software path exist at all.
    let nv12 = gst::ElementFactory::make("capsfilter")
        .property("caps", tonemap_out_caps(encoder))
        .build()
        .unwrap();
    let relabel = gst::ElementFactory::make("capssetter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("colorimetry", "bt709")
                .build(),
        )
        .build()
        .unwrap();
    attach_peak_probe(&upload, &shader);
    vec![
        upload, to_rgba, rgba, shader, from_rgba, download, nv12, relabel,
    ]
}

/// How long the picture waits for its subtitles before giving up and
/// encoding without them. Generous: the pads are usually milliseconds
/// apart, and the only thing on the other side of this timeout is a
/// session that never starts.
pub(super) const ASS_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// HUB-32a: the two halves of an ASS burn meet here.
///
/// `assrender` needs the picture on `video_sink` and the styled events
/// on `text_sink`, and the two are decided in different callbacks with
/// no ordering guarantee: the encode chain (which owns the renderer) is
/// built from `pad-added`, and so is the demuxer's subtitle pad. Same
/// shape as `WaitingPads`, which already rendezvouses muxer pads across
/// the same callbacks — whichever half lands second does the linking.
#[derive(Default)]
pub(crate) struct AssBurnLink {
    /// `assrender`'s `text_sink` and the pipeline it lives in, once the
    /// encode chain exists.
    text: Option<(gst::Pipeline, gst::Pad)>,
    source: Option<AssSource>,
    /// A block probe holding the PICTURE until the subtitles are
    /// attached. Without it the encode simply outruns them: measured
    /// live on a dispatched session, the video chain was built 88 ms
    /// before the demuxer exposed its subtitle pad, and `assrender`
    /// logged "rendering disabled, doing buffer passthrough" for every
    /// frame in between — a clean picture with no subtitles in it,
    /// which nothing downstream can detect. `wait-text` does not cover
    /// this: it waits only once a text stream is LINKED.
    gate: Option<(gst::Pad, gst::PadProbeId)>,
}

/// Where the events come from. The two cases are genuinely different
/// mechanisms, not one with a flag (2026-08-02 experiments, recorded in
/// `docs/kahawai-implementation.md`).
pub(crate) enum AssSource {
    /// An EMBEDDED track: the demuxer's own `application/x-ass` pad,
    /// which is the only path that also carries the file's attached
    /// fonts.
    Pad(gst::Pad),
    /// A user's SIDECAR script — header (the container's `codec_data`
    /// equivalent) and one matroska-shaped payload per event. No fonts;
    /// it renders with the system's.
    File(String, Vec<(u64, u64, String)>),
}

impl AssBurnLink {
    pub(super) fn couple(&mut self) {
        let (Some((pipe, sink)), Some(source)) = (&self.text, &self.source) else {
            return; // the other half has not arrived yet
        };
        if sink.is_linked() {
            return;
        }
        let result = match source {
            AssSource::Pad(src) => src.link(sink).map(|_| ()),
            AssSource::File(header, events) => play_ass_file(pipe, sink, header, events),
        };
        match result {
            Ok(()) => tracing::info!("ASS burn: subtitles linked to assrender"),
            // Loud: the encode carries on and produces a picture with no
            // subtitles in it, which is the failure this tier exists to
            // prevent and which nothing downstream can detect.
            Err(e) => tracing::error!(error = %e, "ASS burn: subtitle → assrender link failed"),
        }
        self.open_gate();
    }

    /// Let the picture through. Called once the subtitles are attached
    /// — or by the watchdog, which would rather ship a subtitle-less
    /// encode (loudly) than a session that never starts.
    pub(super) fn open_gate(&mut self) {
        if let Some((pad, id)) = self.gate.take() {
            pad.remove_probe(id);
        }
    }

    /// Hold the video branch closed until [`couple`] succeeds. Installed
    /// on the encode chain's own sink pad, before decodebin is linked to
    /// it, so no frame can reach `assrender` un-subtitled.
    pub(super) fn shut_gate(link: &Arc<Mutex<Self>>, pad: &gst::Pad) {
        let Some(id) = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
            gst::PadProbeReturn::Ok
        }) else {
            return;
        };
        link.lock().unwrap().gate = Some((pad.clone(), id));
        // Never deadlock on a subtitle pad that does not come: a source
        // whose ASS track vanished between planning and demuxing would
        // otherwise wedge the session at zero segments forever.
        let link = link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(ASS_GATE_WAIT);
            let mut l = link.lock().unwrap();
            if l.gate.is_some() {
                tracing::error!(
                    "ASS burn: no subtitle stream after {:?} — encoding WITHOUT subtitles",
                    ASS_GATE_WAIT
                );
                l.open_gate();
            }
        });
    }

    pub(crate) fn offer_text_sink(link: &Arc<Mutex<Self>>, pipe: &gst::Pipeline, pad: gst::Pad) {
        let mut l = link.lock().unwrap();
        l.text = Some((pipe.clone(), pad));
        l.couple();
    }

    pub(crate) fn offer_source(link: &Arc<Mutex<Self>>, source: AssSource) {
        let mut l = link.lock().unwrap();
        l.source = Some(source);
        l.couple();
    }
}

/// Play a sidecar script into `assrender` from an `appsrc`, because no
/// GStreamer element turns a subtitle FILE into the
/// `application/x-ass` stream a demuxer produces (`ssaparse` flattens
/// to `text/x-raw`, which is the other tier entirely).
///
/// Built and linked in one go, deliberately: an `appsrc` that reaches
/// PAUSED with its src pad unlinked pushes its caps into nothing and
/// pauses its own streaming task, and every later `push_buffer` then
/// queues into a task that will never run again. Measured — the events
/// arrived, `push_buffer` returned Ok for all of them, and assrender
/// logged "rendering disabled, doing buffer passthrough" for every
/// frame because its text caps had never been set.
pub(super) fn play_ass_file(
    pipe: &gst::Pipeline,
    sink: &gst::Pad,
    header: &str,
    events: &[(u64, u64, String)],
) -> Result<(), gst::PadLinkError> {
    let src = gstreamer_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("application/x-ass")
                .field(
                    "codec_data",
                    gst::Buffer::from_slice(header.to_string().into_bytes()),
                )
                .build(),
        )
        .format(gst::Format::Time)
        .build();
    pipe.add(&src).unwrap();
    src.static_pad("src").unwrap().link(sink)?;
    src.sync_state_with_parent().unwrap();
    let events = events.to_vec();
    // A thread, not a `need-data` handler: the whole script is already
    // in memory (tens of kilobytes), so there is nothing to pull it
    // lazily for, and appsrc's queue applies the backpressure.
    std::thread::spawn(move || {
        for (start, end, line) in events {
            let mut buf = gst::Buffer::from_slice(line.into_bytes());
            {
                let b = buf.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_mseconds(start));
                b.set_duration(gst::ClockTime::from_mseconds(end.saturating_sub(start)));
            }
            if src.push_buffer(buf).is_err() {
                return; // shutting down; assrender keeps what it got
            }
        }
        let _ = src.end_of_stream();
    });
    Ok(())
}

/// Read a sidecar `.ass` into the form [`AssBurnLink`] plays from.
pub(super) fn load_ass_file(path: &Path) -> Option<AssSource> {
    let text = match std::fs::read(path) {
        Ok(b) => crate::subtitles::decode_text(&b),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ASS burn: sidecar unreadable");
            return None;
        }
    };
    let (header, events) = crate::subtitles::ass_file_events(&text);
    if events.is_empty() {
        tracing::warn!(path = %path.display(), "ASS burn: sidecar has no events");
        return None;
    }
    tracing::info!(path = %path.display(), events = events.len(), "ASS burn: sidecar loaded");
    Some(AssSource::File(header, events))
}

/// Link an encode branch and bring it up, reporting failure instead of
/// panicking.
///
/// Every caller runs inside a `pad-added` callback, where a Rust panic
/// cannot unwind out of the C frame and aborts the process instead: no
/// session error, no verdict, no TC-6 retry (the sink is downstream of
/// the panic, so its retry never sees one) — just SIGABRT. Both steps
/// here are genuinely fallible: linking is caps-dependent, and a state
/// change can be refused. `openh264enc` accepts I420 and nothing else,
/// so a tone-mapped chain pinned to NV12 could not link to it, and the
/// `unwrap` that used to be here turned a format mismatch into a dead
/// player for every HDR title on a software-encoder box (field report
/// 2026-08-01).
///
/// Returns false when the branch could not be brought up; the caller
/// abandons it and the session fails on its missing playlist, with the
/// reason in the log.
#[must_use]
pub(super) fn link_and_start(chain: &[&gst::Element], also: Option<&gst::Element>) -> bool {
    if let Err(e) = gst::Element::link_many(chain.iter().copied()) {
        tracing::error!(error = %e, "encode chain failed to link; session will fail");
        return false;
    }
    for el in also.into_iter().chain(chain.iter().copied()) {
        if let Err(e) = el.sync_state_with_parent() {
            tracing::error!(
                element = %el.name(),
                error = %e,
                "encode element refused its state; session will fail"
            );
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)] // one plan, spelled out
pub(super) fn build_video_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    gate: &Option<Arc<SeekGate>>,
    target: VideoTarget,
    video_kbps: Option<u32>,
    max_height: Option<u32>,
    source_dimensions: Option<(i32, i32)>,
    tone_map: bool,
    deinterlace: bool,
    burn: Option<std::sync::Arc<crate::burnin::Timeline>>,
    // HUB-32a: Some when the plan burns an ASS track — the element is
    // built here and its text pad published for the subtitle branch.
    ass_burn: Option<Arc<Mutex<AssBurnLink>>>,
) {
    // A height ceiling changes the picture after this point. Its exact width
    // is fixed by videoscale from the source's display aspect, so demuxed
    // dimensions are authoritative only when no scaling will occur.
    let dimensions = source_dimensions.filter(|(_, height)| {
        max_height.is_none_or(|max| u32::try_from(*height).is_ok_and(|height| height <= max))
    });
    let (encoder, parser) = match target {
        VideoTarget::H264 => (
            verified_video_encoder(H264_ENCODERS, dimensions),
            "h264parse",
        ),
        VideoTarget::Hevc => (
            verified_video_encoder(HEVC_ENCODERS, dimensions),
            "h265parse",
        ),
        VideoTarget::Av1 => (verified_video_encoder(AV1_ENCODERS, dimensions), "av1parse"),
    };
    let Some(enc_name) = encoder else {
        tracing::error!(
            target = target.as_str(),
            "video encode routed with no verified encoder"
        );
        return;
    };
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    // Hardware decoders output device memory (nvav1dec → CUDAMemory,
    // 10-bit P010) that videoconvert cannot take. NVENC target: stay on
    // the GPU end to end (cudaupload passes CUDA through, uploads system
    // memory; cudaconvert handles format — zero copy for NVDEC→NVENC).
    // Other targets: cudadownload first (passthrough for system memory),
    // then videoconvert. All availability-guarded.
    let converter_names = encode_converter_names(enc_name);
    let converters: Vec<gst::Element> = converter_names
        .iter()
        .map(|n| gst::ElementFactory::make(n).build().unwrap())
        .collect();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    // Sane defaults, guarded per element (props differ across encoders;
    // verified by gst-inspect per candidate):
    //   bitrate         kbit/s  nv*enc, qsv*, va*, x264enc, x265enc, vtenc_*
    //   bitrate         bit/s   rav1enc (the one bps outlier)
    //   target-bitrate  kbit/s  svtav1enc, av1enc (neither has `bitrate`)
    // The plan may clamp (bandwidth cap).
    let kbps = video_kbps.unwrap_or(6000);
    if enc_name == "rav1enc" {
        set_prop_str_if_present(&enc, "bitrate", &(kbps * 1000).to_string());
    } else {
        set_prop_str_if_present(&enc, "bitrate", &kbps.to_string());
        set_prop_str_if_present(&enc, "target-bitrate", &kbps.to_string());
    }
    // A BACKSTOP, not the cadence. The sink asks for a keyframe at every
    // fragment boundary (see make_hls_sink) and that is what actually
    // cuts segments; this only bounds the GOP for an encoder that
    // ignores the request, where the alternative is an encoder-default
    // GOP of ~10 s and a 7 s session start on a 4K HDR encode (measured;
    // the encode itself ran 1.8× realtime and was not the problem).
    //
    // Deliberately FAR longer than the fragment interval. A pin near it
    // competes with the sink's request — the two cadences count
    // different things, frames against seconds, so they cannot line up
    // and the sink cuts at both, which is how segments came out 1.96 s
    // and 1.04 s alternating. Ten seconds never fires first in practice.
    // One name per encoder family, each guarded:
    // TC-6: which encoder this session actually got. The requirement is
    // to degrade to software PREDICTABLY, and a degradation nobody can
    // see afterwards is not predictable — the preference list resolves
    // per worker process, so the answer differs between two sessions on
    // one box and only the session's own log can say which it was.
    // `HLS sink selected` has been logged the same way for the same
    // reason; the encoder was the louder omission of the two.
    tracing::info!(
        encoder = enc_name,
        target = target.as_str(),
        hardware = !SW_VIDEO_ENCODERS.contains(&enc_name),
        "video encoder selected"
    );
    // Before the GOP pins, so a log reading the element's properties back
    // shows the ceiling next to the bitrate it competes with.
    if let Some(&n) = ENCODER_THREADS.get().filter(|n| **n > 0) {
        set_encoder_threads_on(&enc, enc_name, n);
    }
    set_prop_str_if_present(&enc, "gop-size", "240"); // nvenc, qsv
    set_prop_str_if_present(&enc, "key-int-max", "240"); // x264, x265, va
    set_prop_str_if_present(&enc, "max-keyframe-interval", "240"); // vtenc
    set_prop_str_if_present(&enc, "intra-period-length", "240"); // svtav1enc
    set_prop_str_if_present(&enc, "max-key-frame-interval", "240"); // rav1enc
    set_prop_str_if_present(&enc, "keyframe-max-dist", "240"); // av1enc
    let parse = gst::ElementFactory::make(parser).build().unwrap();
    // Parameter sets on every keyframe (independently decodable
    // segments). h26xparse only; av1parse has no such knob.
    set_prop_if_present(&parse, "config-interval", -1i32);

    // HUB-15 resolution ceiling: a RANGE capsfilter after videoscale, so
    // sources already within the ceiling pass through untouched and only
    // larger ones downscale (aspect preserved by videoscale's fixation).
    let scaler: Vec<gst::Element> = match max_height {
        Some(h) if gst::ElementFactory::find("videoscale").is_some() => {
            let scale = gst::ElementFactory::make("videoscale").build().unwrap();
            let caps = gst::Caps::builder("video/x-raw")
                .field("height", gst::IntRange::new(16i32, h as i32))
                .build();
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", caps)
                .build()
                .unwrap();
            vec![scale, filter]
        }
        _ => vec![],
    };

    // Fields go before everything that reads the picture: the tone map,
    // the scaler and any burn-in all blend rows, and blending rows that
    // belong to different instants is how interlaced content turns into
    // combing that no later stage can undo.
    let deinterlacer: Vec<gst::Element> = match deinterlace {
        // mode=interlaced, not the default auto. Auto believes the caps,
        // and a 1080i BluRay remux arrives with progressive-looking caps
        // and field flags on the buffers: auto passes it through and the
        // encoder then fails on the flags. The element is only in the
        // chain because discovery already said this source has fields,
        // so there is nothing left for auto to decide.
        true => match gst::ElementFactory::make("deinterlace")
            .property_from_str("mode", "interlaced")
            .build()
        {
            Ok(el) => vec![el],
            Err(_) => {
                tracing::warn!("interlaced source and no deinterlace element — encoding fields");
                vec![]
            }
        },
        false => vec![],
    };

    // HUB-15a: the GL segment sits in the same system-memory zone as
    // the scaler, before it — tone-map at source resolution, then
    // scale (scaling PQ-coded pixels before linearizing would blur
    // across the transfer curve; and the shader is per-pixel GPU work,
    // its cost does not care).
    // Ask about THIS target, not "can the box tone-map at all": the
    // segment's output pin comes from this encoder, so that is the only
    // question with an answer (AR-13a).
    let tonemap: Vec<gst::Element> = if tone_map && encoder.is_some_and(tonemap_into) {
        tonemap_segment(encoder.unwrap_or_default())
    } else {
        if tone_map {
            tracing::warn!(
                encoder = encoder.unwrap_or("-"),
                "tone-map requested but the GL segment cannot feed this encoder — encoding as-is"
            );
        }
        vec![]
    };

    // The scaler works on system memory: it must sit right after
    // videoconvert, BEFORE any CUDA upload — a capsfilter on raw caps
    // cannot link against CUDAMemory.
    let scale_at = converter_names
        .iter()
        .position(|n| *n == "videoconvert")
        .map(|i| i + 1)
        .unwrap_or(converters.len());
    // HUB-32b burn-in goes LAST in system memory: after the tone map
    // (subtitle white is already SDR — mapping it through the PQ curve
    // would crush it) and after the scaler (blit at output size, and
    // the rectangles scale to the frame the encoder actually sees).
    // HUB-32a ASS burn: libass renders the styled events onto the same
    // frames, in the same slot and for the same reasons. The text pad is
    // published for whichever pad-added callback needs it — see
    // `AssBurnLink`.
    let ass_el: Vec<gst::Element> = ass_burn
        .as_ref()
        .and_then(|_| {
            // wait-text: hold the picture until the subtitle stream is
            // actually there. Off by default, and off means the encode
            // wins the race every time — a 50-frame clip ran to EOS in
            // 45 ms while assrender logged "rendering disabled, doing
            // buffer passthrough" for every frame, because the text
            // caps had not landed yet.
            gst::ElementFactory::make("assrender")
                .property("wait-text", true)
                .build()
                .ok()
        })
        .into_iter()
        .collect();
    if ass_burn.is_some() && ass_el.is_empty() {
        tracing::error!("ASS burn requested but assrender is missing — encoding without subtitles");
    }
    let burn_wanted = burn.is_some();
    let burn_el: Vec<gst::Element> = burn
        .filter(|t| !t.is_empty())
        .and_then(crate::burnin::blend_element)
        .into_iter()
        .collect();
    if burn_wanted && burn_el.is_empty() {
        tracing::warn!("burn-in requested but no overlay/timeline — encoding without subtitles");
    }

    let mut chain: Vec<&gst::Element> = Vec::new();
    chain.extend(converters[..scale_at].iter());
    chain.extend(deinterlacer.iter());
    chain.extend(tonemap.iter());
    chain.extend(scaler.iter());
    chain.extend(burn_el.iter());
    // link_many picks a compatible pad by caps, so the raw video lands
    // on `video_sink` and never on `text_sink` (application/x-ass).
    chain.extend(ass_el.iter());
    chain.extend(converters[scale_at..].iter());
    chain.push(&enc);
    chain.push(&parse);
    pipe.add(&decode).unwrap();
    pipe.add_many(chain.iter().copied()).unwrap();
    // Shut the gate BEFORE anything can flow OR couple: an ASS burn
    // that lets frames past un-subtitled produces exactly the silent,
    // undetectable failure this tier exists to prevent, and the
    // coupling further down is what opens it again.
    if let Some(link) = &ass_burn
        && !ass_el.is_empty()
    {
        AssBurnLink::shut_gate(link, &chain[0].static_pad("sink").unwrap());
    }
    if !link_and_start(&chain, Some(&decode)) {
        return;
    }
    // AFTER link_and_start, and not a line earlier. Two ways this bites:
    // the pads must share a pipeline or the link fails outright ("no
    // common grandparent"), and `assrender`'s text pad is not ACTIVE
    // until the element has its state — an appsrc linked to an inactive
    // pad pushes once, fails, and pauses its own streaming task, which
    // looks from the outside like a text stream that simply never
    // arrived (measured: every frame logged "rendering disabled, doing
    // buffer passthrough").
    if let (Some(link), Some(el)) = (&ass_burn, ass_el.first())
        && let Some(text) = el.static_pad("text_sink")
    {
        AssBurnLink::offer_text_sink(link, pipe, text);
    }
    let out = parse.static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        open_encoder_on_keyframe(&out);
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "video encode chain → muxer link failed");
    }
    let convert_sink = chain[0].static_pad("sink").unwrap();
    decode.connect_pad_added(move |_, pad| {
        if convert_sink.is_linked() {
            return; // first decoded stream wins
        }
        if let Err(e) = pad.link(&convert_sink) {
            tracing::warn!(error = %e, "decodebin → video encode chain link failed");
        }
    });
    if let Err(e) = from.link(&decode.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "→ decodebin link failed");
    }
}
