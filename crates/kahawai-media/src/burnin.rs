//! HUB-32b last resort: burn image subtitles (PGS, VobSub) into the
//! video. The tier below bitmap streaming and OCR, for clients that
//! cannot composite an overlay themselves.
//!
//! The whole timeline is read up front through the container's own
//! index (`subindex::extract_image_track` — kilobytes, no demux) and
//! indexed by frame time, rather than fed live from the demuxer's
//! subtitle pad. That is the difference between correct and
//! nearly-correct: display sets are sparse, so a session that starts
//! mid-set — every resume, every seek-restart — is fed nothing by a
//! live pad until the NEXT set arrives, and the subtitle on screen
//! simply vanishes for seconds (measured against mpv: present at
//! 25.5s when played from zero, absent after a flushing seek to the
//! same timestamp). A timeline knows what is on screen at any instant.
//!
//! Positions are canvas-relative: PGS authors against its own canvas
//! (commonly 1920x1080) which need not match the coded frame (that
//! same film is 3840x1600 scope), so every rectangle is scaled from
//! canvas space into the frame the encoder will see.

use anyhow::Result;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;

use crate::imagesubs::ImageObject;
use crate::remux::RemuxSource;

/// One screen state: valid from `start_ms` until `end_ms`.
struct Entry {
    start_ms: u64,
    end_ms: u64,
    // Width only: scaling is uniform by width ratio (see compose), so a
    // canvas height would be dead weight here.
    canvas_w: u32,
    objects: Vec<ImageObject>,
}

/// Every display set of one image subtitle track, in time order.
pub struct Timeline {
    entries: Vec<Entry>,
}

impl Timeline {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The set covering `ms`, if any. Binary search: called per frame.
    fn at(&self, ms: u64) -> Option<usize> {
        let i = self.entries.partition_point(|e| e.start_ms <= ms);
        let e = self.entries.get(i.checked_sub(1)?)?;
        (ms < e.end_ms && !e.objects.is_empty()).then_some(i - 1)
    }
}

/// Read and decode one image subtitle track into a timeline. `None`
/// when the container's index doesn't permit the sparse read or the
/// track isn't an image track — the caller then burns nothing and
/// says so, rather than encoding video for no reason.
pub fn timeline(
    src: &mut dyn RemuxSource,
    sub_index: usize,
    budget: std::time::Duration,
) -> Result<Option<Timeline>> {
    let Some(track) = crate::subindex::extract_image_track(src, sub_index, budget)? else {
        return Ok(None);
    };
    Ok(Some(build(
        &track.codec,
        track.codec_private.as_deref(),
        &track.blocks,
    )))
}

/// Decode raw display-set blocks into a timeline. Shared by both
/// sources of blocks: our own index walk and the mediahost's.
fn build(
    codec: &str,
    codec_private: Option<&[u8]>,
    blocks: &[(u64, Option<u64>, Vec<u8>)],
) -> Timeline {
    let is_pgs = codec.contains("PGS");
    // VobSub's .idx rides in CodecPrivate: it carries both the palette
    // and the canvas its coordinates are relative to. That canvas is
    // NOT always the video's resolution (a re-encode keeps the
    // original .idx), so scaling must know it — PGS states its own.
    let idx_text = codec_private.map(crate::subtitles::decode_text);
    let palette = idx_text
        .as_deref()
        .map(crate::imagesubs::vobsub_palette)
        .unwrap_or_default();
    let vob_canvas = idx_text.as_deref().and_then(crate::imagesubs::vobsub_size);

    let mut pgs = crate::imagesubs::PgsDecoder::default();
    let mut entries: Vec<Entry> = Vec::new();
    for (ms, dur, payload) in blocks {
        // A PGS block may define objects without composing a screen —
        // the decoder answers None until a set is complete.
        let (canvas, objects) = if is_pgs {
            match pgs.feed(payload) {
                Ok(Some(set)) => (set.canvas_w, set.objects),
                _ => continue,
            }
        } else {
            match crate::imagesubs::vobsub_decode(payload, &palette) {
                // No declared size: the coordinates are the frame's own
                // (scale 1.0 downstream), which is the historical
                // assumption and right for same-resolution rips.
                Ok(Some(obj)) => (vob_canvas.map(|(w, _)| w).unwrap_or(0), vec![obj]),
                _ => continue,
            }
        };
        // Each set ends where the next begins; an explicit duration
        // (VobSub) bounds it earlier. An empty set is a clear, kept as
        // an entry so it terminates its predecessor.
        entries.push(Entry {
            start_ms: *ms,
            end_ms: dur.map(|d| ms + d).unwrap_or(u64::MAX),
            canvas_w: canvas,
            objects,
        });
    }
    for i in 0..entries.len().saturating_sub(1) {
        let next = entries[i + 1].start_ms;
        entries[i].end_ms = entries[i].end_ms.min(next);
    }
    // A trailing set with no successor and no duration would otherwise
    // stay on screen forever.
    if let Some(last) = entries.last_mut()
        && last.end_ms == u64::MAX
    {
        last.end_ms = last.start_ms + 5_000;
    }
    Timeline { entries }
}

/// The blocks as a single file, for handing a worker a timeline it
/// cannot read itself. Deliberately trivial and self-describing: the
/// hub writes it, the worker reads it, nothing else parses it.
///
/// `KBS1` | codec (u16 len + utf8) | codec_private (u32 len + bytes)
/// then per block: start_ms u64 | duration_ms u64 | len u32 | payload.
/// All little-endian; duration 0 = undeclared.
/// One subtitle block as carried in the KBS1 sets file:
/// (start_ms, duration_ms, codec payload).
pub type SetBlock = (u64, Option<u64>, Vec<u8>);

pub fn encode_sets(codec: &str, codec_private: Option<&[u8]>, blocks: &[SetBlock]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(b"KBS1");
    out.extend_from_slice(&(codec.len() as u16).to_le_bytes());
    out.extend_from_slice(codec.as_bytes());
    let priv_bytes = codec_private.unwrap_or(&[]);
    out.extend_from_slice(&(priv_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(priv_bytes);
    for (start, dur, payload) in blocks {
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&dur.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
    }
    out
}

fn decode_sets(data: &[u8]) -> Result<(String, Option<Vec<u8>>, Vec<SetBlock>)> {
    let mut p = 0usize;
    fn take<'a>(data: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = p.checked_add(n).filter(|e| *e <= data.len());
        let Some(end) = end else {
            anyhow::bail!("display-set file truncated")
        };
        let out = &data[*p..end];
        *p = end;
        Ok(out)
    }
    anyhow::ensure!(take(data, &mut p, 4)? == b"KBS1", "not a display-set file");
    let n = u16::from_le_bytes(take(data, &mut p, 2)?.try_into().unwrap()) as usize;
    let codec = String::from_utf8_lossy(take(data, &mut p, n)?).to_string();
    let n = u32::from_le_bytes(take(data, &mut p, 4)?.try_into().unwrap()) as usize;
    let codec_private = (n > 0)
        .then(|| take(data, &mut p, n).map(<[u8]>::to_vec))
        .transpose()?;
    let mut blocks = Vec::new();
    while p < data.len() {
        let start = u64::from_le_bytes(take(data, &mut p, 8)?.try_into().unwrap());
        let dur = u64::from_le_bytes(take(data, &mut p, 8)?.try_into().unwrap());
        let n = u32::from_le_bytes(take(data, &mut p, 4)?.try_into().unwrap()) as usize;
        blocks.push((
            start,
            (dur > 0).then_some(dur),
            take(data, &mut p, n)?.to_vec(),
        ));
    }
    Ok((codec, codec_private, blocks))
}

/// Timeline from sets someone else read for us (the mediahost walks
/// the index on its own disk — see `subindex::extract_image_track`).
pub fn timeline_from_file(path: &std::path::Path) -> Result<Option<Timeline>> {
    let data = std::fs::read(path)?;
    let (codec, codec_private, blocks) = decode_sets(&data)?;
    Ok(Some(build(&codec, codec_private.as_deref(), &blocks)))
}

/// An element that blends `timeline` into every frame passing through
/// it, at the frame's own presentation time.
///
/// This blends EXPLICITLY rather than handing the composition to
/// `overlaycomposition`, whose contract is negotiated: it attaches the
/// composition as buffer metadata whenever downstream claims to
/// support it and only blends otherwise. The VA encoder claims it and
/// then ignores it, so subtitles vanished on one box while burning
/// correctly on another (NVENC, which makes no such claim) — a
/// difference no amount of reading the pipeline reveals. Blending here
/// is unconditional and needs nothing of the encoder.
pub fn blend_element(timeline: std::sync::Arc<Timeline>, start_ms: u64) -> Option<gst::Element> {
    let el = gst::ElementFactory::make("identity").build().ok()?;
    let pad = el.static_pad("src")?;
    // A display set spans dozens of frames and its rectangles are
    // immutable once scaled: build once per (set, frame size).
    let cache: std::sync::Mutex<Option<(usize, i32, i32, gst_video::VideoOverlayComposition)>> =
        std::sync::Mutex::new(None);
    // Say once what actually happened: a burn that silently does
    // nothing is the failure mode this tier keeps finding (a negotiated
    // overlay dropped by one encoder, a timeline that never matches a
    // frame time), and a debug-level line hides exactly that.
    // Whether frame timestamps are the file's own or rebased to the
    // seek point is not ours to decide: a session started at 15.5s
    // arrives as 15500ms on one box and as 0ms on another (measured,
    // same code, different plugin sets). So measure it on the first
    // frame instead of assuming — a wrong assumption puts every
    // subtitle at the wrong time, which is worse than none.
    let base = std::sync::atomic::AtomicU64::new(u64::MAX);
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let first_span = (
        timeline.entries.first().map(|e| e.start_ms).unwrap_or(0),
        timeline.entries.last().map(|e| e.end_ms).unwrap_or(0),
    );
    let say = move |what: &str| {
        let n = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 3 {
            tracing::info!(frame = n, outcome = what, timeline_ms = ?first_span,
                "burn-in: frame outcome");
        }
    };
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(pts) = buffer.pts() else {
            say("no pts");
            return gst::PadProbeReturn::Ok;
        };
        let Some(caps) = pad.current_caps() else {
            say("no caps");
            return gst::PadProbeReturn::Ok;
        };
        let Ok(vinfo) = gst_video::VideoInfo::from_caps(&caps) else {
            say("caps not video");
            return gst::PadProbeReturn::Ok;
        };
        let (fw, fh) = (vinfo.width() as i32, vinfo.height() as i32);
        let ms = pts.mseconds();
        let mut b = base.load(std::sync::atomic::Ordering::Relaxed);
        if b == u64::MAX {
            // Rebased streams start near zero; absolute ones start at
            // (or after) the offset the session asked for.
            b = if start_ms > 1_000 && ms + 1_000 < start_ms {
                start_ms
            } else {
                0
            };
            base.store(b, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(
                first_pts_ms = ms,
                start_ms,
                offset_ms = b,
                "burn-in: frame time base measured"
            );
        }
        let Some(idx) = timeline.at(ms + b) else {
            say(&format!("no set at {}ms", ms + b));
            return gst::PadProbeReturn::Ok;
        };

        let comp = {
            let mut cached = cache.lock().unwrap();
            match cached.as_ref() {
                Some((i, cw, ch, c)) if *i == idx && *cw == fw && *ch == fh => c.clone(),
                _ => match compose(&timeline.entries[idx], fw, fh) {
                    Some(c) => {
                        *cached = Some((idx, fw, fh, c.clone()));
                        c
                    }
                    None => {
                        say("composition empty");
                        return gst::PadProbeReturn::Ok;
                    }
                },
            }
        };
        let bufref = buffer.make_mut();
        match gst_video::VideoFrameRef::from_buffer_ref_writable(bufref, &vinfo) {
            Ok(mut frame) => match comp.blend(&mut frame) {
                Ok(()) => say("blended"),
                Err(e) => say(&format!("blend failed: {e}")),
            },
            Err(e) => say(&format!("frame not writable: {e}")),
        }
        gst::PadProbeReturn::Ok
    });
    Some(el)
}

/// One display set as a composition in FRAME coordinates.
fn compose(entry: &Entry, fw: i32, fh: i32) -> Option<gst_video::VideoOverlayComposition> {
    // Canvas → frame, UNIFORMLY by width. The canvas shares the
    // picture's width but not always its height: a 3840x1600 scope
    // film carries subtitles authored against 1920x1080, and scaling
    // the axes independently squashed the text by a quarter (measured
    // against mpv, which renders it 2:1). A zero canvas (VobSub with
    // no declared size) means the objects already speak frame
    // coordinates.
    let scale = if entry.canvas_w > 0 {
        f64::from(fw) / f64::from(entry.canvas_w)
    } else {
        1.0
    };
    let mut rects = Vec::with_capacity(entry.objects.len());
    for o in &entry.objects {
        if o.w == 0 || o.h == 0 {
            continue;
        }
        // Overlay rectangles take BGRA (unpremultiplied); our decoders
        // produce RGBA, so swap R and B.
        let mut bgra = o.rgba.clone();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let mut buf = gst::Buffer::from_mut_slice(bgra);
        {
            let bref = buf.get_mut()?;
            gst_video::VideoMeta::add(
                bref,
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                o.w,
                o.h,
            )
            .ok()?;
        }
        let rw = (f64::from(o.w) * scale).round().max(1.0) as u32;
        let rh = (f64::from(o.h) * scale).round().max(1.0) as u32;
        // A canvas taller than the picture (cropped scope) puts
        // bottom-anchored subtitles past the last row; keep them on
        // screen instead of off it.
        let rx = ((f64::from(o.x) * scale).round() as i32)
            .min(fw - rw as i32)
            .max(0);
        let ry = ((f64::from(o.y) * scale).round() as i32)
            .min(fh - rh as i32)
            .max(0);
        rects.push(gst_video::VideoOverlayRectangle::new_raw(
            &buf,
            rx,
            ry,
            rw,
            rh,
            gst_video::VideoOverlayFormatFlags::empty(),
        ));
    }
    (!rects.is_empty())
        .then(|| gst_video::VideoOverlayComposition::new(rects.iter()).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    /// The on-disk format must survive a round trip — the hub writes
    /// it, a worker on another machine reads it, and nothing else
    /// checks the two agree.
    #[test]
    fn sets_round_trip() {
        let blocks = vec![
            (100u64, None, vec![1u8, 2, 3]),
            (2_000, Some(3_000), vec![9u8; 40]),
        ];
        let bytes = super::encode_sets("S_HDMV/PGS", Some(b"size: 720x576"), &blocks);
        let (codec, private, out) = super::decode_sets(&bytes).unwrap();
        assert_eq!(codec, "S_HDMV/PGS");
        assert_eq!(private.as_deref(), Some(&b"size: 720x576"[..]));
        assert_eq!(out, blocks);
    }

    /// Manual: BURN_SETS=/path/to.sets cargo test -p kahawai-media \
    ///   sets_file_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn sets_file_from_env() {
        let Ok(path) = std::env::var("BURN_SETS") else {
            return;
        };
        let data = std::fs::read(&path).unwrap();
        let (codec, private, blocks) = super::decode_sets(&data).unwrap();
        println!(
            "codec {codec:?} · private {} bytes · {} blocks",
            private.as_ref().map_or(0, |p| p.len()),
            blocks.len()
        );
        let t = super::build(&codec, private.as_deref(), &blocks);
        println!("timeline entries: {}", t.len());
    }
}
