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
    canvas_w: u32,
    canvas_h: u32,
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
    let is_pgs = track.codec.contains("PGS");
    // VobSub's .idx rides in CodecPrivate: it carries both the palette
    // and the canvas its coordinates are relative to. That canvas is
    // NOT always the video's resolution (a re-encode keeps the
    // original .idx), so scaling must know it — PGS states its own.
    let idx_text = track.codec_private.as_deref().map(crate::subtitles::decode_text);
    let palette = idx_text
        .as_deref()
        .map(crate::imagesubs::vobsub_palette)
        .unwrap_or_default();
    let vob_canvas = idx_text.as_deref().and_then(crate::imagesubs::vobsub_size);

    let mut pgs = crate::imagesubs::PgsDecoder::default();
    let mut entries: Vec<Entry> = Vec::new();
    for (ms, dur, payload) in &track.blocks {
        // A PGS block may define objects without composing a screen —
        // the decoder answers None until a set is complete.
        let (canvas, objects) = if is_pgs {
            match pgs.feed(payload) {
                Ok(Some(set)) => ((set.canvas_w, set.canvas_h), set.objects),
                _ => continue,
            }
        } else {
            match crate::imagesubs::vobsub_decode(payload, &palette) {
                // No declared size: the coordinates are the frame's own
                // (scale 1.0 downstream), which is the historical
                // assumption and right for same-resolution rips.
                Ok(Some(obj)) => (vob_canvas.unwrap_or((0, 0)), vec![obj]),
                _ => continue,
            }
        };
        // Each set ends where the next begins; an explicit duration
        // (VobSub) bounds it earlier. An empty set is a clear, kept as
        // an entry so it terminates its predecessor.
        entries.push(Entry {
            start_ms: *ms,
            end_ms: dur.map(|d| ms + d).unwrap_or(u64::MAX),
            canvas_w: canvas.0,
            canvas_h: canvas.1,
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
    Ok(Some(Timeline { entries }))
}

/// Build the overlay element and wire its per-frame draw to `timeline`.
/// Returns `None` when the element is unavailable (the caller then
/// keeps the plan but reports that nothing was burned).
pub fn overlay_element(timeline: std::sync::Arc<Timeline>) -> Option<gst::Element> {
    let overlay = gst::ElementFactory::make("overlaycomposition").build().ok()?;
    // Cache the built composition: a display set spans dozens to
    // hundreds of frames and the rectangles are immutable once scaled.
    let cache: std::sync::Mutex<Option<(usize, i32, i32, gst_video::VideoOverlayComposition)>> =
        std::sync::Mutex::new(None);
    overlay.connect("draw", false, move |args| {
        // The signal is typed: it must ALWAYS hand back a composition
        // value, and a frame with no subtitle returns a null one. A
        // Rust `None` here is "no return value at all" and aborts the
        // process from an FFI frame that cannot unwind.
        let comp: Option<gst_video::VideoOverlayComposition> = (|| {
        let sample = args[1].get::<gst::Sample>().ok()?;
        let buffer = sample.buffer()?;
        let pts = buffer.pts()?;
        let caps = sample.caps()?;
        let info = gst_video::VideoInfo::from_caps(caps).ok()?;
        let (fw, fh) = (info.width() as i32, info.height() as i32);
        let idx = timeline.at(pts.mseconds())?;

        let mut cached = cache.lock().unwrap();
        if let Some((i, cw, ch, comp)) = cached.as_ref()
            && *i == idx
            && *cw == fw
            && *ch == fh
        {
            return Some(comp.clone());
        }
        let entry = &timeline.entries[idx];
        // Canvas → frame, UNIFORMLY by width. The canvas shares the
        // picture's width but not always its height: this film is
        // 3840x1600 scope with subtitles authored against a 1920x1080
        // master, and scaling x and y independently squashed the text
        // by a quarter (measured against mpv, which renders it 2:1).
        // A zero canvas (VobSub without a declared size) means the
        // objects already speak frame coordinates.
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
            // Overlay rectangles take BGRA (unpremultiplied); our
            // decoders produce RGBA, so swap R and B in place.
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
            // A canvas taller than the picture (the cropped-scope case)
            // puts bottom-anchored subtitles past the last row; keep
            // them on screen instead of off it.
            let rx = (f64::from(o.x) * scale).round() as i32;
            let ry = (f64::from(o.y) * scale).round() as i32;
            let rx = rx.min(fw - rw as i32).max(0);
            let ry = ry.min(fh - rh as i32).max(0);
            let rect = gst_video::VideoOverlayRectangle::new_raw(
                &buf,
                rx,
                ry,
                rw,
                rh,
                gst_video::VideoOverlayFormatFlags::empty(),
            );
            rects.push(rect);
        }
        if rects.is_empty() {
            return None;
        }
        let comp = gst_video::VideoOverlayComposition::new(rects.iter()).ok()?;
        *cached = Some((idx, fw, fh, comp.clone()));
        Some(comp)
        })();
        Some(comp.to_value())
    });
    Some(overlay)
}
