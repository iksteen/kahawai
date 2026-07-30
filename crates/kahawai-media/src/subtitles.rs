//! Text subtitle handling (HUB-15 subtitle conversion, HUB-27 selection):
//! parse SRT/ASS/VTT into cues, serialize cues to WebVTT, and extract
//! embedded text tracks from a media source without decoding A/V.
//!
//! ASS tracks are extracted faithfully (script header + re-timed
//! Dialogue lines) for client-side rendering (HUB-32); the flattened
//! cues remain available for the HUB-32a fallback path.

use anyhow::{Context, Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Formats this module can turn into WebVTT.
pub fn is_text_format(format: &str) -> bool {
    matches!(
        format,
        "srt" | "subrip" | "ass" | "ssa" | "webvtt" | "vtt" | "text"
    )
}

/// Decode subtitle file bytes: UTF-8 (with or without BOM), falling back
/// to Latin-1 — the overwhelmingly common non-UTF-8 sidecar encoding.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Parse sidecar content by format name (see `is_text_format`).
pub fn parse(format: &str, content: &str) -> Result<Vec<Cue>> {
    match format {
        "srt" | "subrip" => Ok(parse_srt(content)),
        "ass" | "ssa" => Ok(parse_ass(content)),
        "webvtt" | "vtt" => Ok(parse_vtt(content)),
        other => bail!("unsupported text subtitle format: {other}"),
    }
}

/// Serialize cues as WebVTT, shifting timestamps by `shift_ms` (negative
/// when the player's timeline starts mid-file). Cues that end before the
/// shifted zero are dropped.
pub fn to_vtt(cues: &[Cue], shift_ms: i64) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        let start = cue.start_ms as i64 + shift_ms;
        let end = cue.end_ms as i64 + shift_ms;
        if end <= 0 {
            continue;
        }
        let ts = |ms: i64| {
            let ms = ms.max(0) as u64;
            format!(
                "{:02}:{:02}:{:02}.{:03}",
                ms / 3_600_000,
                ms / 60_000 % 60,
                ms / 1000 % 60,
                ms % 1000
            )
        };
        out.push_str(&format!("{} --> {}\n{}\n\n", ts(start), ts(end), cue.text));
    }
    out
}

fn parse_timestamp(s: &str) -> Option<u64> {
    // HH:MM:SS,mmm or HH:MM:SS.mmm or MM:SS.mmm (VTT short form)
    let s = s.trim();
    let (hms, ms_part) = s.rsplit_once([',', '.'])?;
    let ms: u64 = ms_part.get(..3)?.parse().ok()?;
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, sec): (u64, u64, u64) = match parts.as_slice() {
        [h, m, s] => (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?),
        [m, s] => (0, m.parse().ok()?, s.parse().ok()?),
        _ => return None,
    };
    Some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

/// Strip markup a VTT renderer would choke on or show literally: keep
/// i/b/u tags, drop font/anything else, unescape ASS line breaks.
pub(crate) fn clean_cue_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // ASS override block {\...}: drop entirely.
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            '<' => {
                let mut tag = String::new();
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                    tag.push(c);
                }
                let name = tag.trim_start_matches('/');
                if matches!(name, "i" | "b" | "u") {
                    out.push('<');
                    out.push_str(&tag);
                    out.push('>');
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

fn parse_srt(content: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    for block in content.replace('\r', "").split("\n\n") {
        let mut lines = block.lines().peekable();
        // Optional numeric counter line.
        if lines
            .peek()
            .is_some_and(|l| l.trim().parse::<u64>().is_ok())
        {
            lines.next();
        }
        let Some(timing) = lines.next() else { continue };
        let Some((start, end)) = timing.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        let text = clean_cue_text(&lines.collect::<Vec<_>>().join("\n"));
        if !text.is_empty() {
            cues.push(Cue {
                start_ms: start,
                end_ms: end,
                text,
            });
        }
    }
    cues
}

fn parse_vtt(content: &str) -> Vec<Cue> {
    // Same cue shape as SRT (dot timestamps, optional cue ids/settings);
    // skip NOTE/STYLE/REGION blocks and the header.
    let mut cues = Vec::new();
    for block in content.replace('\r', "").split("\n\n") {
        let block = block.trim();
        if block.is_empty()
            || block.starts_with("WEBVTT")
            || block.starts_with("NOTE")
            || block.starts_with("STYLE")
            || block.starts_with("REGION")
        {
            continue;
        }
        let mut lines = block.lines().peekable();
        // Optional cue identifier line (anything without "-->").
        if lines.peek().is_some_and(|l| !l.contains("-->")) {
            lines.next();
        }
        let Some(timing) = lines.next() else { continue };
        let Some((start, rest)) = timing.split_once("-->") else {
            continue;
        };
        // Cue settings may follow the end timestamp.
        let end = rest.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        let text = clean_cue_text(&lines.collect::<Vec<_>>().join("\n"));
        if !text.is_empty() {
            cues.push(Cue {
                start_ms: start,
                end_ms: end,
                text,
            });
        }
    }
    cues
}

/// ASS/SSA: [Events] section, Format: line names the fields, Dialogue:
/// lines carry them (Text is always last and may contain commas).
fn parse_ass(content: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut in_events = false;
    let mut fields: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().starts_with('[') {
            in_events = line.trim().eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(fmt) = line.strip_prefix("Format:") {
            fields = fmt.split(',').map(|f| f.trim().to_lowercase()).collect();
            continue;
        }
        let Some(dialogue) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        if fields.is_empty() {
            continue;
        }
        let parts: Vec<&str> = dialogue.splitn(fields.len(), ',').collect();
        let field = |name: &str| {
            fields
                .iter()
                .position(|f| f == name)
                .and_then(|i| parts.get(i))
                .copied()
        };
        let (Some(start), Some(end), Some(text)) = (field("start"), field("end"), field("text"))
        else {
            continue;
        };
        // ASS timestamps are H:MM:SS.cc (centiseconds).
        let ts = |s: &str| -> Option<u64> {
            let (hms, cc) = s.trim().rsplit_once('.')?;
            let cc: u64 = cc.parse().ok()?;
            let p: Vec<&str> = hms.split(':').collect();
            let [h, m, sec] = p.as_slice() else {
                return None;
            };
            Some(
                ((h.parse::<u64>().ok()? * 60 + m.parse::<u64>().ok()?) * 60
                    + sec.parse::<u64>().ok()?)
                    * 1000
                    + cc * 10,
            )
        };
        let (Some(start), Some(end)) = (ts(start), ts(end)) else {
            continue;
        };
        let text = clean_cue_text(text);
        if !text.is_empty() {
            cues.push(Cue {
                start_ms: start,
                end_ms: end,
                text,
            });
        }
    }
    cues.sort_by_key(|c| c.start_ms);
    cues
}

/// Extraction result: flattened cues (VTT serving) plus, for ASS
/// tracks, the faithful reconstruction — original script header from
/// the container's codec_data and re-timed Dialogue lines (HUB-32:
/// styling is never discarded at extraction time).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extracted {
    pub cues: Vec<Cue>,
    pub ass: Option<String>,
}

/// Rebuild an ASS Dialogue line from a matroska block payload
/// ("ReadOrder,Layer,Style,Name,ML,MR,MV,Effect,Text") and its timing.
pub(crate) fn ass_dialogue(raw: &str, start_ms: u64, end_ms: u64) -> Option<String> {
    let (_read_order, rest) = raw.split_once(',')?;
    let (layer, rest) = rest.split_once(',')?;
    let ts = |ms: u64| {
        format!(
            "{}:{:02}:{:02}.{:02}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000 / 10
        )
    };
    Some(format!(
        "Dialogue: {layer},{},{},{rest}",
        ts(start_ms),
        ts(end_ms)
    ))
}

/// One streamed extraction event (HUB-32 streaming: subtitles usable
/// while the demux pass is still running). ASS tracks only.
pub enum SubStreamEvent {
    /// Script header (+ a completed [Events]/Format section), once, early.
    Header(String),
    /// One re-timed Dialogue line, in demux (≈chronological) order.
    Dialogue(String),
}

/// The script header normalized for appending Dialogue lines.
pub(crate) fn compose_header(h: &str) -> String {
    let mut out = h.trim_end().to_string();
    if !out.to_lowercase().contains("[events]") {
        out.push_str("\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text");
    }
    out.push('\n');
    out
}

/// Extract the `index`-th text subtitle track (0-based, counting only
/// subtitle pads in demux order) from a media source. No A/V decoding —
/// everything except text subtitle tracks goes to fakesinks. One demux
/// pass extracts ALL tracks; this returns the requested one.
pub fn extract_embedded(
    source: Box<dyn crate::remux::RemuxSource>,
    index: usize,
) -> Result<Extracted> {
    let all = extract_embedded_core(source, usize::MAX, |_| {})?;
    let ex = all
        .into_iter()
        .find(|(i, _)| *i == index)
        .map(|(_, ex)| ex)
        .filter(|ex| !ex.cues.is_empty());
    ex.with_context(|| format!("no cues extracted (track {index} missing or not a text track)"))
}

/// [`extract_embedded_all`], additionally streaming the ASS material
/// of track `stream_index` through `sink` as it is demuxed — the
/// caller can serve that track long before EOS while every other text
/// track is collected in the same pass.
pub fn extract_embedded_stream(
    source: Box<dyn crate::remux::RemuxSource>,
    stream_index: usize,
    sink: impl FnMut(SubStreamEvent),
) -> Result<Vec<(usize, Extracted)>> {
    extract_embedded_core(source, stream_index, sink)
}

fn raw_from_sample(sample: &gst::Sample) -> Option<(u64, u64, String)> {
    let buffer = sample.buffer()?;
    let start_ms = buffer.pts()?.mseconds();
    let end_ms = start_ms + buffer.duration().map(|d| d.mseconds()).unwrap_or(3000);
    let map = buffer.map_readable().ok()?;
    Some((start_ms, end_ms, decode_text(map.as_slice())))
}

/// Extract EVERY text subtitle track in one demux pass: (track index,
/// result) pairs, indexes matching discovery/tap `e{n}` keys. Used by
/// the mediahost extraction facility, where the source is a local file
/// and one sequential walk is the whole cost.
pub fn extract_embedded_all(
    source: Box<dyn crate::remux::RemuxSource>,
) -> Result<Vec<(usize, Extracted)>> {
    extract_embedded_core(source, usize::MAX, |_| {})
}

/// The one demux pass behind every embedded extraction: taps every text
/// subtitle pad; the tap at `stream_index` (if any, and ASS) also
/// streams header + Dialogue lines through `out`.
fn extract_embedded_core(
    source: Box<dyn crate::remux::RemuxSource>,
    stream_index: usize,
    mut out: impl FnMut(SubStreamEvent),
) -> Result<Vec<(usize, Extracted)>> {
    crate::init()?;

    struct Tap {
        idx: usize,
        sink: gstreamer_app::AppSink,
        is_ass: bool,
        header: Option<String>,
        streamed_header: bool,
        cues: Vec<Cue>,
        raw_events: Vec<(u64, u64, String)>,
    }

    let pipeline = gst::Pipeline::new();
    let appsrc = crate::remux::seekable_appsrc(source);
    let parsebin = gst::ElementFactory::make("parsebin").build()?;
    pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;
    gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;

    let taps: std::sync::Arc<std::sync::Mutex<Vec<Tap>>> = Default::default();
    let sub_seen = std::sync::atomic::AtomicUsize::new(0);
    let pipeline_pa = pipeline.clone();
    let taps_pa = taps.clone();
    parsebin.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().or_else(|| pad.allowed_caps());
        let caps_name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        let is_text = caps_name.starts_with("application/x-subtitle")
            || caps_name.starts_with("application/x-ssa")
            || caps_name.starts_with("application/x-ass")
            || caps_name.starts_with("text/");
        let is_sub = is_text || caps_name.starts_with("subpicture/");
        if is_sub && is_text {
            let idx = sub_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let sink = gstreamer_app::AppSink::builder().sync(false).build();
            sink.set_property("async", false);
            let header = caps
                .as_ref()
                .and_then(|c| c.structure(0))
                .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
                .and_then(|b| b.map_readable().ok().map(|m| decode_text(m.as_slice())));
            pipeline_pa.add(sink.upcast_ref::<gst::Element>()).ok();
            sink.sync_state_with_parent().ok();
            if pad.link(&sink.static_pad("sink").unwrap()).is_ok() {
                taps_pa.lock().unwrap().push(Tap {
                    idx,
                    sink,
                    is_ass: caps_name.contains("ssa") || caps_name.contains("ass"),
                    header,
                    streamed_header: false,
                    cues: Vec::new(),
                    raw_events: Vec::new(),
                });
                return;
            }
        } else if is_sub {
            sub_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let fake = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline_pa.add(&fake).ok();
        fake.sync_state_with_parent().ok();
        pad.link(&fake.static_pad("sink").unwrap()).ok();
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("starting subtitle extraction")?;
    let bus = pipeline.bus().unwrap();
    let mut result = Ok(());
    let mut drain = |taps: &mut Vec<Tap>| {
        for tap in taps.iter_mut() {
            while let Some(sample) = tap.sink.try_pull_sample(gst::ClockTime::ZERO) {
                if let Some((start, end, raw)) = raw_from_sample(&sample) {
                    let text = if tap.is_ass {
                        if tap.idx == stream_index {
                            if !tap.streamed_header
                                && let Some(h) = &tap.header
                            {
                                out(SubStreamEvent::Header(compose_header(h)));
                                tap.streamed_header = true;
                            }
                            if tap.streamed_header
                                && let Some(line) = ass_dialogue(&raw, start, end)
                            {
                                out(SubStreamEvent::Dialogue(line));
                            }
                        }
                        tap.raw_events.push((start, end, raw.clone()));
                        clean_cue_text(raw.splitn(9, ',').last().unwrap_or(""))
                    } else {
                        clean_cue_text(&raw)
                    };
                    if !text.is_empty() {
                        tap.cues.push(Cue {
                            start_ms: start,
                            end_ms: end,
                            text,
                        });
                    }
                }
            }
        }
    };
    'outer: loop {
        drain(&mut taps.lock().unwrap());
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break 'outer,
            gst::MessageView::Error(e) => {
                result = Err(anyhow::anyhow!("extraction failed: {}", e.error()));
                break 'outer;
            }
            _ => {}
        }
    }
    // The pad-added closure keeps a clone of `taps` alive as long as
    // the pipeline exists; take the contents out under the lock.
    let mut final_taps = {
        let mut guard = taps.lock().unwrap();
        drain(&mut guard);
        std::mem::take(&mut *guard)
    };
    drain(&mut final_taps);
    pipeline.set_state(gst::State::Null).ok();
    result?;

    Ok(final_taps
        .into_iter()
        .map(|mut tap| {
            tap.cues.sort_by_key(|c| c.start_ms);
            let ass = tap.header.filter(|_| !tap.raw_events.is_empty()).map(|h| {
                let mut out = compose_header(&h);
                tap.raw_events.sort_by_key(|(s, _, _)| *s);
                for (s, e, raw) in &tap.raw_events {
                    if let Some(line) = ass_dialogue(raw, *s, *e) {
                        out.push_str(&line);
                        out.push('\n');
                    }
                }
                out
            });
            (
                tap.idx,
                Extracted {
                    cues: tap.cues,
                    ass,
                },
            )
        })
        .collect())
}

/// Font attachments from a matroska source (HUB-32): matroskademux
/// exposes attachments as `attachment` tag samples during header parse,
/// so a short preroll suffices — no full read.
pub fn extract_fonts(source: Box<dyn crate::remux::RemuxSource>) -> Result<Vec<(String, Vec<u8>)>> {
    crate::init()?;
    let pipeline = gst::Pipeline::new();
    let appsrc = crate::remux::seekable_appsrc(source);
    let demux = gst::ElementFactory::make("matroskademux").build()?;
    pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &demux])?;
    gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &demux])?;
    let pipe2 = pipeline.clone();
    demux.connect_pad_added(move |_, pad| {
        let fake = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipe2.add(&fake).ok();
        fake.sync_state_with_parent().ok();
        pad.link(&fake.static_pad("sink").unwrap()).ok();
    });
    pipeline
        .set_state(gst::State::Playing)
        .context("starting font extraction")?;
    let bus = pipeline.bus().unwrap();
    let mut fonts = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            if !fonts.is_empty() {
                break; // attachments arrive with the headers; done
            }
            continue;
        };
        match msg.view() {
            gst::MessageView::Tag(t) => {
                let tags = t.tags();
                for i in 0..tags.size_by_name("attachment") {
                    let Some(v) = tags.index_generic("attachment", i) else {
                        continue;
                    };
                    let Ok(sample) = v.get::<gst::Sample>() else {
                        continue;
                    };
                    let is_font = sample
                        .caps()
                        .and_then(|c| c.structure(0))
                        .map(|s| {
                            let n = s.name();
                            n.contains("font") || n.contains("truetype") || n.contains("opentype")
                        })
                        .unwrap_or(false);
                    if !is_font {
                        continue;
                    }
                    let name = sample
                        .info()
                        .and_then(|s| s.get::<String>("filename").ok())
                        .unwrap_or_else(|| format!("font-{}.ttf", fonts.len()));
                    if let Some(buf) = sample.buffer()
                        && let Ok(map) = buf.map_readable()
                    {
                        // The same attachment tag repeats on every stream's
                        // TAG message; keep one copy per filename.
                        if !fonts.iter().any(|(n, _): &(String, Vec<u8>)| *n == name) {
                            fonts.push((name, map.as_slice().to_vec()));
                        }
                    }
                }
            }
            gst::MessageView::Eos(_) | gst::MessageView::Error(_) => break,
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).ok();
    Ok(fonts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_roundtrip() {
        let srt = "1\n00:00:01,500 --> 00:00:03,000\nHello <i>world</i>\n\n\
                   2\n00:01:00,000 --> 00:01:02,250\nSecond line\nwraps\n\n";
        let cues = parse("srt", srt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0],
            Cue {
                start_ms: 1500,
                end_ms: 3000,
                text: "Hello <i>world</i>".into()
            }
        );
        assert_eq!(cues[1].text, "Second line\nwraps");
        let vtt = to_vtt(&cues, 0);
        assert!(vtt.starts_with("WEBVTT\n\n00:00:01.500 --> 00:00:03.000\nHello <i>world</i>\n"));
    }

    #[test]
    fn vtt_shift_drops_prehistory() {
        let cues = vec![
            Cue {
                start_ms: 1000,
                end_ms: 2000,
                text: "gone".into(),
            },
            Cue {
                start_ms: 5000,
                end_ms: 7000,
                text: "kept".into(),
            },
        ];
        let vtt = to_vtt(&cues, -3000);
        assert!(!vtt.contains("gone"));
        assert!(vtt.contains("00:00:02.000 --> 00:00:04.000\nkept"));
    }

    #[test]
    fn ass_flattening() {
        let ass = "[Script Info]\nTitle: x\n\n[Events]\n\
            Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
            Dialogue: 0,0:00:05.00,0:00:07.50,Default,,0,0,0,,{\\an8\\i1}Sign text\\Nline two\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Earlier, with, commas\n";
        let cues = parse("ass", ass).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0],
            Cue {
                start_ms: 1000,
                end_ms: 2000,
                text: "Earlier, with, commas".into()
            }
        );
        assert_eq!(
            cues[1],
            Cue {
                start_ms: 5000,
                end_ms: 7500,
                text: "Sign text\nline two".into()
            }
        );
    }

    #[test]
    fn vtt_parse_skips_notes_and_settings() {
        let vtt = "WEBVTT\n\nNOTE a comment\n\nid-1\n00:01.000 --> 00:02.000 line:0\nCue one\n\n\
                   00:00:03.000 --> 00:00:04.000\n<font color=\"red\">styled</font>\n\n";
        let cues = parse("vtt", vtt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0],
            Cue {
                start_ms: 1000,
                end_ms: 2000,
                text: "Cue one".into()
            }
        );
        assert_eq!(cues[1].text, "styled"); // font tags stripped
    }

    #[test]
    fn latin1_fallback() {
        let bytes = b"caf\xe9";
        assert_eq!(decode_text(bytes), "café");
        assert_eq!(decode_text("café".as_bytes()), "café");
        assert_eq!(decode_text(b"\xef\xbb\xbfbom"), "bom");
    }

    #[test]
    fn extracts_embedded_srt_from_mkv() {
        // Build a tiny MKV with a video track and an SRT subtitle track.
        let dir = tempfile::tempdir().unwrap();
        let srt_path = dir.path().join("in.srt");
        std::fs::write(
            &srt_path,
            "1\n00:00:00,500 --> 00:00:01,500\nFirst cue\n\n2\n00:00:02,000 --> 00:00:03,000\nSecond cue\n\n",
        )
        .unwrap();
        let mkv = dir.path().join("out.mkv");
        let launch = format!(
            "videotestsrc num-buffers=90 ! video/x-raw,width=64,height=48 ! x264enc ! h264parse ! mux. \
             filesrc location={} ! subparse ! mux. \
             matroskamux name=mux ! filesink location={}",
            srt_path.display(),
            mkv.display()
        );
        crate::init().unwrap();
        let pipe = gst::parse::launch(&launch).unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        let bus = pipe.bus().unwrap();
        let msg = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipe.set_state(gst::State::Null).unwrap();
        match msg.map(|m| m.type_()) {
            Some(gst::MessageType::Eos) => {}
            other => panic!("mkv fixture build failed: {other:?}"),
        }

        let source = crate::remux::FileSource::open(&mkv).unwrap();
        let ex = extract_embedded(Box::new(source), 0).unwrap();
        assert_eq!(ex.cues.len(), 2, "{ex:?}");
        assert_eq!(ex.cues[0].text, "First cue");
        assert_eq!(ex.cues[0].start_ms, 500);
        assert_eq!(ex.cues[1].text, "Second cue");
        assert!(ex.ass.is_none(), "srt track must not fabricate ASS");
    }

    #[test]
    fn reconstructs_ass_dialogue_lines() {
        let line = ass_dialogue("17,0,Default,,0,0,0,,{\\an8}Sign", 61_500, 63_750).unwrap();
        assert_eq!(
            line,
            "Dialogue: 0,0:01:01.50,0:01:03.75,Default,,0,0,0,,{\\an8}Sign"
        );
    }
}
