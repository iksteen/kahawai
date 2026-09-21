use super::*;

pub(super) fn vobsub_palette_event(event: &gst::EventRef) -> Option<Vec<[u8; 3]>> {
    if !matches!(event.view(), gst::EventView::CustomDownstream(_)) {
        return None;
    }
    let structure = event.structure()?;
    if structure.name() != "application/x-gst-dvd"
        || structure.get::<String>("event").ok()? != "dvd-spu-clut-change"
    {
        return None;
    }
    let clut: Option<Vec<u32>> = (0..16)
        .map(|i| {
            structure
                .get::<i32>(&format!("clut{i:02}"))
                .ok()
                .map(|value| value as u32)
        })
        .collect();
    Some(crate::imagesubs::vobsub_dvd_clut(&clut?))
}

pub(super) fn vobsub_canvas(
    declared: Option<(u32, u32)>,
    video: Option<(u32, u32)>,
    object: &crate::imagesubs::ImageObject,
) -> (u32, u32) {
    if let Some(canvas) = declared {
        return canvas;
    }
    // Ordinary VobSub is authored in a 720-wide DVD canvas. Some MP4
    // muxers instead rewrite SPU coordinates into the video's HD canvas but
    // carry no size at all. Crossing either DVD bound proves this is the HD
    // form; use the video caps qtdemux already supplied instead of scaling it
    // once more from 720 (Men.In.Black.1997.mp4: 1920-wide coordinates were
    // rendered 2.67x too large).
    if object.x.saturating_add(object.w) > 720 || object.y.saturating_add(object.h) > 576 {
        return video.unwrap_or((object.x + object.w, object.y + object.h));
    }
    (720, 576)
}

/// Tee a text subtitle stream into the session dir as it is demuxed:
/// ASS/SSA → `subs-e{idx}.ass` (composed script header immediately —
/// codec_data carries it — then re-timed Dialogue lines); every other
/// text codec → `subs-e{idx}.jsonl` (one `{"s","e","t"}` cue per
/// line). Sparse and text-sized — a flush per line is nothing.
pub(super) fn tap_text_track(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    caps: &gst::Caps,
    dir: &std::path::Path,
    idx: usize,
    caps_name: &str,
) {
    let is_ass = caps_name.contains("ssa") || caps_name.contains("ass");
    let path = dir.join(format!(
        "subs-e{idx}.{}",
        if is_ass { "ass" } else { "jsonl" }
    ));
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "subtitle tap file failed");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = from.link(&fake.static_pad("sink").unwrap());
            return;
        }
    };
    use std::io::Write;
    if is_ass {
        let header = caps
            .structure(0)
            .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
            .and_then(|b| {
                b.map_readable()
                    .ok()
                    .map(|m| crate::subtitles::decode_text(m.as_slice()))
            })
            .unwrap_or_default();
        let _ = file.write_all(crate::subtitles::compose_header(&header).as_bytes());
    }
    let file = std::sync::Mutex::new(file);

    let appsink = gstreamer_app::AppSink::builder().sync(false).build();
    appsink.set_property("async", false);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(sample) = sink.pull_sample()
                    && let Some(buffer) = sample.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    let start = pts.mseconds();
                    let end = start + buffer.duration().map(|d| d.mseconds()).unwrap_or(3000);
                    let raw = crate::subtitles::decode_text(map.as_slice());
                    if is_ass {
                        if let Some(line) = crate::subtitles::ass_dialogue(&raw, start, end) {
                            let mut f = file.lock().unwrap();
                            let _ = writeln!(f, "{line}");
                        }
                    } else {
                        let text = crate::subtitles::clean_cue_text(&raw);
                        if !text.is_empty() {
                            let line = serde_json::json!({"s": start, "e": end, "t": text});
                            let mut f = file.lock().unwrap();
                            let _ = writeln!(f, "{line}");
                        }
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipe.add(appsink.upcast_ref::<gst::Element>()).unwrap();
    appsink.sync_state_with_parent().unwrap();
    if let Err(e) = from.link(&appsink.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "subtitle tap link failed");
    }
    tracing::info!(path = %path.display(), "tapping text subtitle track");
}

/// Tee an image subtitle stream (PGS / VobSub) into
/// `subs-e{idx}.jsonl`: one display-set line per event —
/// `{"s":ms,"cw":..,"ch":..,"o":[{"x","y","png":base64}…]}` — decoded
/// to RGBA server-side so any client can draw them on an overlay
/// canvas. Empty "o" clears the screen.
pub(super) fn tap_image_track(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    caps: &gst::Caps,
    dir: &std::path::Path,
    idx: usize,
    caps_name: &str,
    video_canvas: &Arc<Mutex<Option<(u32, u32)>>>,
) {
    use base64::Engine;
    let path = dir.join(format!("subs-e{idx}.jsonl"));
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "image sub tap file failed");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = from.link(&fake.static_pad("sink").unwrap());
            return;
        }
    };
    let file = std::sync::Mutex::new(file);
    let is_pgs = caps_name.contains("pgs");
    let mut pgs = crate::imagesubs::PgsDecoder::default();
    // VobSub: 16-color palette + display size ride the codec_data (.idx text).
    let (vob_palette, vob_size) = caps
        .structure(0)
        .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
        .and_then(|b| {
            b.map_readable().ok().map(|m| {
                let text = crate::subtitles::decode_text(m.as_slice());
                let size = crate::imagesubs::vobsub_size(&text);
                (crate::imagesubs::vobsub_palette(&text), size)
            })
        })
        .unwrap_or_default();
    let vob_palette = Arc::new(Mutex::new(vob_palette));
    let event_palette = vob_palette.clone();
    from.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let Some(palette) = vobsub_palette_event(event)
        {
            *event_palette.lock().unwrap_or_else(|e| e.into_inner()) = palette;
        }
        gst::PadProbeReturn::Ok
    });
    let video_canvas = video_canvas.clone();

    let write_set = move |file: &std::sync::Mutex<std::fs::File>,
                          ms: u64,
                          cw: u32,
                          ch: u32,
                          objects: &[crate::imagesubs::ImageObject]| {
        use std::io::Write;
        let objs: Vec<serde_json::Value> = objects
            .iter()
            .filter_map(|o| {
                let png = crate::imagesubs::to_png(o).ok()?;
                Some(serde_json::json!({
                    "x": o.x, "y": o.y,
                    "png": base64::engine::general_purpose::STANDARD.encode(png),
                }))
            })
            .collect();
        let line = serde_json::json!({"s": ms, "cw": cw, "ch": ch, "o": objs});
        let mut f = file.lock().unwrap();
        let _ = writeln!(f, "{line}");
    };

    let appsink = gstreamer_app::AppSink::builder().sync(false).build();
    appsink.set_property("async", false);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(sample) = sink.pull_sample()
                    && let Some(buffer) = sample.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    let ms = pts.mseconds();
                    // Belt and braces around the decoders (issue #1):
                    // this callback is invoked by C, so a panic here
                    // cannot unwind and ABORTS the worker — killing the
                    // session, and killing the sink-fallback retry with
                    // it, since this runs upstream of the sink. The
                    // decoders are bounds-checked; this makes the NEXT
                    // parser bug a dropped subtitle instead of a dead
                    // session. Same reasoning as guard_pts.
                    let guard = |f: &mut dyn FnMut()| {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                            tracing::error!(
                                pts_ms = ms,
                                "image subtitle decoder panicked; dropping this packet"
                            );
                        }
                    };
                    if is_pgs {
                        let mut fed = None;
                        guard(&mut || fed = pgs.feed(map.as_slice()).ok().flatten());
                        if let Some(set) = fed {
                            write_set(&file, ms, set.canvas_w, set.canvas_h, &set.objects);
                        }
                    } else if let Some(obj) = {
                        let mut decoded = None;
                        guard(&mut || {
                            let palette = vob_palette.lock().unwrap_or_else(|e| e.into_inner());
                            decoded = crate::imagesubs::vobsub_decode(map.as_slice(), &palette)
                                .ok()
                                .flatten();
                        });
                        decoded
                    } {
                        let end = ms + buffer.duration().map(|d| d.mseconds()).unwrap_or(5000);
                        let canvas = vobsub_canvas(
                            vob_size,
                            *video_canvas.lock().unwrap_or_else(|e| e.into_inner()),
                            &obj,
                        );
                        write_set(&file, ms, canvas.0, canvas.1, std::slice::from_ref(&obj));
                        write_set(&file, end, canvas.0, canvas.1, &[]);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipe.add(appsink.upcast_ref::<gst::Element>()).unwrap();
    appsink.sync_state_with_parent().unwrap();
    if let Err(e) = from.link(&appsink.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "image sub tap link failed");
    }
    tracing::info!(path = %path.display(), pgs = is_pgs, "tapping image subtitle track");
}
