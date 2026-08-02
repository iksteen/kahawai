//! Environment self-check (OPS-3): GStreamer inventory against the feature
//! matrix, naming exactly which capability each missing plugin costs.

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Essential failures should abort startup; warnings only degrade.
    pub essential: bool,
}

impl Check {
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            detail: detail.into(),
            essential: false,
        }
    }
    pub fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
            essential: false,
        }
    }
    pub fn fail(name: impl Into<String>, detail: impl Into<String>, essential: bool) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
            essential,
        }
    }
}

/// capability → (elements in preference order, essential, cost when missing,
/// recommend-preferred: if matched by a fallback, suggest installing the
/// first-listed element)
const MATRIX: &[(&str, &[&str], bool, &str, bool)] = &[
    (
        "typefind",
        &["typefind"],
        true,
        "media discovery is impossible",
        false,
    ),
    (
        "stream parsing",
        &["parsebin"],
        true,
        "discovery and remux are impossible",
        false,
    ),
    (
        "demux mkv",
        &["matroskademux"],
        true,
        "MKV/WebM sources unusable",
        false,
    ),
    (
        "demux mp4",
        &["qtdemux"],
        true,
        "MP4/MOV sources unusable",
        false,
    ),
    (
        "parse h264",
        &["h264parse"],
        true,
        "H.264 streams cannot be handled",
        false,
    ),
    (
        "parse hevc",
        &["h265parse"],
        false,
        "HEVC streams cannot be parsed",
        false,
    ),
    (
        "hls sink",
        &["hlssink3", "hlssink2"],
        false,
        "in-hub HLS remux unavailable",
        true,
    ),
    (
        "h264 dts fix",
        &["h264timestamper"],
        false,
        "remuxed H.264 with B-frames will corrupt in browser (hls.js), OK in mpv",
        false,
    ),
    (
        "hevc dts fix",
        &["h265timestamper"],
        false,
        "remuxed HEVC with B-frames will corrupt in browser (hls.js), OK in mpv",
        false,
    ),
    (
        "mux fmp4/cmaf",
        &["isofmp4mux"],
        false,
        "non-h264 encode targets unavailable — HLS uses TS segments only \
         (install gst-plugins-rs fmp4 for the HUB-15b fMP4 path)",
        false,
    ),
    (
        "decode h264",
        &["vah264dec", "nvh264dec", "avdec_h264", "openh264dec"],
        false,
        "H.264 sources cannot be transcoded (direct play only)",
        false,
    ),
    (
        "decode hevc",
        &["vah265dec", "nvh265dec", "avdec_h265"],
        false,
        "HEVC sources will always fail to transcode",
        false,
    ),
    (
        "encode h264",
        &[
            "vah264enc",
            "vaapih264enc",
            "nvh264enc",
            "qsvh264enc",
            "vtenc_h264_hw",
            "vtenc_h264",
            "x264enc",
            "openh264enc",
        ],
        false,
        "no video transcoding to H.264",
        false,
    ),
    (
        "encode hevc",
        &[
            "vah265enc",
            "vaapih265enc",
            "nvh265enc",
            "qsvh265enc",
            "vtenc_h265_hw",
            "vtenc_h265",
            "x265enc",
        ],
        false,
        "no hevc encode target — clients without h264 fall back to refusal",
        false,
    ),
    (
        "encode av1",
        &[
            "vaav1enc",
            "nvav1enc",
            "qsvav1enc",
            "svtav1enc",
            "rav1enc",
            "av1enc",
        ],
        false,
        "no av1 encode target",
        false,
    ),
    (
        "encode aac",
        &["fdkaacenc", "avenc_aac", "voaacenc"],
        false,
        "no audio transcoding to AAC",
        false,
    ),
    (
        "encode opus",
        &["opusenc"],
        false,
        "no opus encode target — clients without aac fall back to refusal",
        false,
    ),
    (
        "decode aac",
        &["fdkaacdec", "avdec_aac"],
        false,
        "AAC audio cannot be transcoded",
        false,
    ),
    (
        "decode ac3",
        &["a52dec", "avdec_ac3"],
        false,
        "AC-3 audio cannot be transcoded",
        false,
    ),
    (
        "decode eac3",
        &["avdec_eac3"],
        false,
        "E-AC-3 audio cannot be transcoded (silent in browsers) — install gst-libav",
        false,
    ),
    (
        "decode dts",
        &["avdec_dca", "dcadec", "dtsdec"],
        false,
        "DTS audio cannot be transcoded",
        false,
    ),
    (
        "decode truehd",
        &["avdec_truehd"],
        false,
        "TrueHD audio cannot be transcoded — install gst-libav",
        false,
    ),
    (
        "decode vorbis/opus",
        &["vorbisdec", "opusdec"],
        false,
        "ogg audio cannot be transcoded",
        false,
    ),
    (
        "subtitle parse",
        &["subparse"],
        false,
        "text subtitle conversion unavailable",
        false,
    ),
    (
        "ass burn-in",
        &["assrender"],
        false,
        "ASS burn-in unavailable (flatten only, HUB-32a)",
        false,
    ),
];

/// GStreamer version + feature-matrix inventory. Reused by `doctor` and by
/// module startup (warnings logged, essential failures fatal).
/// `bench_cache`: where this box keeps its HUB-36 measurements, so the
/// encoder rows can state speed. The doctor never benchmarks itself —
/// that is a ~40 s job owned by the hub/satellite background task.
pub fn gstreamer_checks(bench_cache: Option<&std::path::Path>) -> Vec<Check> {
    let mut out = Vec::new();
    if let Err(e) = crate::init() {
        out.push(Check::fail("gstreamer", format!("{e:#}"), true));
        return out;
    }
    let (maj, min, micro, _) = gst::version();
    out.push(Check::ok("gstreamer", format!("{maj}.{min}.{micro}")));

    for (name, elements, essential, cost, recommend) in MATRIX {
        match elements
            .iter()
            .find(|e| gst::ElementFactory::find(e).is_some())
        {
            Some(found) if *recommend && *found != elements[0] => out.push(Check::ok(
                *name,
                format!(
                    "via {found} — {} preferred, consider installing it",
                    elements[0]
                ),
            )),
            Some(found) => out.push(Check::ok(*name, format!("via {found}"))),
            None if *essential => out.push(Check::fail(
                *name,
                format!("missing {} — {cost}", elements.join("/")),
                true,
            )),
            None => out.push(Check::warn(
                *name,
                format!("missing {} — {cost}", elements.join("/")),
            )),
        }
    }

    // hlssink3 has a known panic class (see remux.rs) that the session
    // starter escapes by retrying on hlssink2 — so the FALLBACK sink is
    // load-bearing on its own. Verify it instantiates, not merely that
    // "hls sink" passed: a half-upgraded box (libgsthls.so present but a
    // shared lib missing — seen live: nettle on silence) drops hlssink2
    // from the registry while hlssink3 keeps the matrix row green.
    out.push(match gst::ElementFactory::make("hlssink2").build() {
        Ok(el) => {
            let ready = el.set_state(gst::State::Ready).is_ok();
            let _ = el.set_state(gst::State::Null);
            if ready {
                Check::ok("hls fallback sink", "hlssink2 instantiates")
            } else {
                Check::warn(
                    "hls fallback sink",
                    "hlssink2 present but cannot reach READY — files hitting the \
                     known hlssink3 panic will fail to start",
                )
            }
        }
        Err(_) => Check::warn(
            "hls fallback sink",
            "hlssink2 unavailable — files hitting the known hlssink3 panic cannot \
             start; check gst-plugins-bad and its libraries \
             (ldd /usr/lib/gstreamer-1.0/libgsthls.so)",
        ),
    });

    // The decode-dts row above says whether DTS decodes at all; this one
    // says WHICH decoder decodebin will pick, because that decides how
    // much of a DTS-HD track survives. libdca (dtsdec/dcadec) ships at
    // rank primary and only decodes the lossy 5.1 core — the lossless
    // XLL extension (and its 7.1) is discarded with nothing in any log.
    // avdec_dca (rank marginal) decodes all of it. Found live: the same
    // file came out 5.1-core on the box with libdca and 7.1 on the box
    // without. Ranks are read AFTER config demotions apply, so a box
    // fixed via [transcoder] demote_decoders reports ok here.
    out.push(dts_hd_check().0);

    // TC-1: encoders that will actually run sessions are dry-run-verified,
    // not just present — a broken element (or a hw element without its
    // driver) surfaces here, not mid-session.
    // HUB-15a: HDR→SDR is a GL shader segment, not a matrix row — all
    // five elements must be present together or the tier is absent.
    let bench = bench_cache.and_then(crate::bench::load);
    // Absent measurements say so; a measured-but-dreadful number is
    // printed as the number, which is the point of the distinction.
    let speeds = |s: Option<crate::bench::Speeds>| {
        let one = |v: Option<f32>| match v {
            Some(v) => format!("{v:.2}x"),
            None => "n/a".to_string(),
        };
        match s {
            Some(s) if s.s1080.is_some() || s.s2160.is_some() => {
                format!(", {} @1080p / {} @2160p", one(s.s1080), one(s.s2160))
            }
            _ => ", speed not yet measured".to_string(),
        }
    };
    out.push(if crate::remux::tonemap_available() {
        Check::ok(
            "hdr tone-map",
            format!(
                "GL shader segment (glshader + capssetter){}",
                speeds(bench.as_ref().and_then(|b| b.tonemap))
            ),
        )
    } else {
        Check::warn(
            "hdr tone-map",
            "GL segment incomplete — HDR sources transcode without tone-mapping \
             (washed-out colors); check gst-plugins-base GL and capssetter \
             (gst-plugins-bad)",
        )
    });

    for (row, verified, disabled) in [
        (
            "encode aac",
            crate::remux::aac_encoder(),
            "audio transcode disabled",
        ),
        (
            "encode h264",
            crate::remux::h264_encoder(),
            "video transcode disabled",
        ),
        (
            "encode hevc",
            crate::remux::hevc_encoder(),
            "hevc target disabled",
        ),
        (
            "encode av1",
            crate::remux::av1_encoder(),
            "av1 target disabled",
        ),
        (
            "encode opus",
            crate::remux::opus_encoder(),
            "opus target disabled",
        ),
    ] {
        if let Some(c) = out.iter_mut().find(|c| c.name == row)
            && c.status == Status::Ok
        {
            match verified {
                Some(name) => {
                    // Audio encoders run hundreds of times realtime
                    // everywhere; only video speeds are measured.
                    let s = bench.as_ref().and_then(|b| b.encoders.get(name).copied());
                    let note = if s.is_some() {
                        speeds(s)
                    } else {
                        String::new()
                    };
                    c.detail = format!("via {name} (dry-run verified){note}");
                }
                None => {
                    c.status = Status::Warn;
                    c.detail = format!("installed but dry-run failed — {disabled}");
                }
            }
        }
    }
    out
}

/// OPS-9: one calibration pass over this box's decoder ranks — the
/// checks a human reads, and the demotions `--fix` writes.
#[derive(Debug, Default)]
pub struct Calibration {
    pub checks: Vec<Check>,
    /// (element, why) for every demotion this box needs. Ordered and
    /// deduplicated by the caller that writes them.
    pub demote: Vec<(String, String)>,
}

/// OPS-9. TIMED, so only the `doctor` command calls this — the same
/// checks run at every module startup via `gstreamer_checks`, and a
/// boot must not pay seconds of decoding to say something a human is
/// not there to read.
///
/// Two classes, and presence checks see neither:
///
/// (a) **Pathologically slow hardware decode.** On Gemini Lake
/// `vah265dec` decodes at ~6 fps where `avdec_h265` does ~121 — the
/// element is present, advertises the codec, and works. Only a
/// measurement tells them apart, so each decoder outranking the
/// software one is timed against the same reference clip.
///
/// (b) **Decoders that see less of the stream.** A fixed known-bad
/// list, not a measurement: `dtsdec` decodes only the lossy DTS core,
/// which no amount of timing reveals because it is fast and wrong.
pub fn calibrate() -> Calibration {
    let mut out = Calibration::default();
    for codec in crate::bench::Codec::ALL {
        out.checks.push(decoder_speed_check(codec, &mut out.demote));
    }
    // Class (b) is a fixed list, so `gstreamer_checks` already prints
    // this row on every startup and every doctor run. Take only its
    // demotion — printing it twice would say the calibration found
    // something the cheap checks did not.
    out.demote.extend(dts_hd_check().1);
    out
}

/// Is this element name a hardware decoder? A NAME heuristic, and
/// deliberately so: GStreamer exposes no "this is fixed-function" bit,
/// and every vendor's elements are named after their API. The ceiling
/// is that a hardware decoder under an unlisted prefix is not
/// examined — which the "unmeasured" row would then also miss, so the
/// failure is silence rather than a wrong finding.
///
/// The filter matters in both directions. Without it the calibration
/// times software siblings against each other (`vp9dec` versus
/// `avdec_vp9`, both libvpx-class) and reports whichever lost as a
/// pathology, which would demote a perfectly good decoder — and it
/// floods the unmeasured row with elements nobody should compare,
/// training the reader to skip the one row that matters.
fn is_hardware_decoder(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "va",    // vah264dec, vaapih265dec (VA-API)
        "nv",    // nvh264dec, nvh265sldec (NVDEC)
        "v4l2",  // v4l2h264dec (embedded)
        "msdk",  // Intel Media SDK
        "qsv",   // Intel Quick Sync
        "d3d11", // Windows
        "d3d12",
        "amf", // AMD
        "vt",  // vtdec, vtdec_hw (VideoToolbox)
        "applemedia",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The software decoders a hardware one has to beat, best first.
///
/// A LIST because "the software decoder" is not one element: gst-libav
/// covers most codecs, but AV1's is `dav1ddec` (libdav1d) and there is
/// no `avdec_av1` on a normal install — naming only the libav one made
/// the av1 row report "no software decoder to compare against" on a box
/// that has two.
fn software_decoders(codec: crate::bench::Codec) -> &'static [&'static str] {
    match codec {
        crate::bench::Codec::H264 => &["avdec_h264"],
        crate::bench::Codec::H265 => &["avdec_h265"],
        crate::bench::Codec::Av1 => &["dav1ddec", "avdec_av1", "av1dec"],
        crate::bench::Codec::Vp9 => &["avdec_vp9", "vp9dec"],
        crate::bench::Codec::Vp8 => &["avdec_vp8", "vp8dec"],
        crate::bench::Codec::Mpeg2 => &["avdec_mpeg2video", "mpeg2dec"],
    }
}

/// The one this box will actually be measured against.
fn software_decoder(codec: crate::bench::Codec) -> Option<&'static str> {
    software_decoders(codec)
        .iter()
        .copied()
        .find(|n| gst::ElementFactory::find(n).is_some())
}

fn decoder_speed_check(codec: crate::bench::Codec, demote: &mut Vec<(String, String)>) -> Check {
    let name = format!("{} decode rank", codec.label());
    let rank = |n: &str| {
        gst::ElementFactory::find(n)
            .filter(|f| f.rank() > gst::Rank::NONE)
            .map(|f| f.rank())
    };
    let Some(sw) = software_decoder(codec) else {
        return Check::warn(
            name,
            format!(
                "none of {:?} present — no software decoder to compare against",
                software_decoders(codec)
            ),
        );
    };
    let Some(sw_rank) = rank(sw) else {
        return Check::warn(
            name,
            format!("{sw} is demoted out of autoplug; nothing to compare"),
        );
    };
    // Candidates: anything that decodes this codec, is in autoplug, and
    // GStreamer would reach for BEFORE the software decoder. A lower
    // rank is already losing, so timing it decides nothing.
    let caps = codec.caps();
    let mut candidates: Vec<String> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO,
        gst::Rank::MARGINAL,
    )
    .into_iter()
    .filter(|f| {
        f.name() != sw
            && f.rank() >= sw_rank
            && is_hardware_decoder(&f.name())
            && f.can_sink_any_caps(&caps)
    })
    .map(|f| f.name().to_string())
    .collect();
    candidates.sort();
    if candidates.is_empty() {
        return Check::ok(&name, format!("{sw} leads; nothing outranks it"));
    }

    let Some(sw_fps) = crate::bench::decode_fps(sw, codec) else {
        return Check::warn(
            name,
            format!(
                "{sw} would not decode the reference clip — cannot calibrate {}",
                codec.label()
            ),
        );
    };
    let mut slower: Vec<String> = Vec::new();
    let mut detail: Vec<String> = vec![format!("{sw} {sw_fps:.0} fps")];
    for cand in &candidates {
        match crate::bench::decode_fps(cand, codec) {
            // Timed and slower: the finding. Name BOTH figures — "slow"
            // is unactionable, "6 against 121" is not.
            Some(fps) if fps < sw_fps => {
                detail.push(format!("{cand} {fps:.0} fps"));
                slower.push(cand.clone());
                demote.push((
                    cand.clone(),
                    format!(
                        "decodes {} at {fps:.0} fps where {sw} does {sw_fps:.0}",
                        codec.label()
                    ),
                ));
            }
            Some(fps) => detail.push(format!("{cand} {fps:.0} fps")),
            // Present, outranking, and it would not decode the clip at
            // all. Not a speed finding, and not silently dropped either.
            None => detail.push(format!("{cand} would not decode the clip")),
        }
    }
    let detail = detail.join(" · ");
    if slower.is_empty() {
        Check::ok(name, detail)
    } else {
        Check::warn(
            name,
            format!(
                "{detail} — {} outranks {sw} and is slower; \
                 add it to [transcoder] demote_decoders (or run `doctor --fix`)",
                slower.join(", ")
            ),
        )
    }
}

fn dts_hd_check() -> (Check, Vec<(String, String)>) {
    const NAME: &str = "dts-hd full decode";
    let rank = |name: &str| {
        gst::ElementFactory::find(name)
            .filter(|f| f.rank() > gst::Rank::NONE) // rank NONE = out of autoplug
            .map(|f| f.rank())
    };
    let Some(full) = rank("avdec_dca") else {
        return (
            Check::warn(
                NAME,
                "avdec_dca unavailable — DTS-HD sources decode only the lossy 5.1 core \
                 (or not at all); install gst-libav",
            ),
            Vec::new(),
        );
    };
    let shadow = ["dtsdec", "dcadec"].iter().find(|n| rank(n) >= Some(full));
    match shadow {
        Some(name) => (
            Check::warn(
                NAME,
                format!(
                    "{name} (libdca, core-only) outranks avdec_dca — DTS-HD tracks lose \
                     their lossless extension and decode as lossy 5.1, and a scan run \
                     this way FILES them that way; add \"{name}\" to [transcoder] and \
                     [mediahost] demote_decoders"
                ),
            ),
            vec![(
                (*name).to_string(),
                "libdca decodes only the lossy DTS core, so DTS-HD MA files \
                 decode AND get filed as 5.1"
                    .to_string(),
            )],
        ),
        None => (
            Check::ok(NAME, "via avdec_dca (core + lossless XLL extension)"),
            Vec::new(),
        ),
    }
}

/// True if any essential check failed.
pub fn has_essential_failure(checks: &[Check]) -> bool {
    checks
        .iter()
        .any(|c| c.status == Status::Fail && c.essential)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OPS-9. The calibration TIMES decoders, so this is the one test
    /// that pays for it — and what it asserts are the two ways the
    /// check could be actively harmful rather than merely wrong.
    #[test]
    fn calibration_never_demotes_the_thing_it_measures_against() {
        crate::init().unwrap();
        let cal = calibrate();
        // A row per codec, so a box with no finding still says what was
        // examined — "no warnings" and "nothing was checked" must not
        // look alike.
        for codec in crate::bench::Codec::ALL {
            let name = format!("{} decode rank", codec.label());
            let row = cal.checks.iter().find(|c| c.name == name);
            assert!(
                row.is_some(),
                "no row for {}: {:?}",
                codec.label(),
                cal.checks
            );
        }
        // Demoting the software reference would remove the fallback the
        // demotion exists to fall back TO, leaving the box with no
        // decoder for that codec at all.
        for (element, _) in &cal.demote {
            assert!(
                !element.starts_with("avdec_"),
                "would demote the software reference: {element}"
            );
        }
        // Every demotion carries its measurement: `--fix` prints these
        // into the operator's terminal, and "it was slow" is not a
        // reason anyone can check later.
        for (element, why) in &cal.demote {
            assert!(
                why.contains("fps") || why.contains("core"),
                "demotion of {element} has no evidence: {why}"
            );
        }
    }

    #[test]
    fn full_gst_install_passes_essentials() {
        let checks = gstreamer_checks(None);
        assert!(checks.len() > 10);
        assert!(
            !has_essential_failure(&checks),
            "essential failure on dev box: {checks:?}"
        );
        // Every non-ok check names its cost.
        for c in &checks {
            if c.status != Status::Ok {
                assert!(c.detail.contains('—'), "no cost message: {c:?}");
            }
        }

        // The DTS-HD row must answer for THIS box's effective ranks:
        // ok only when avdec_dca actually wins the autoplug.
        let dts = checks
            .iter()
            .find(|c| c.name == "dts-hd full decode")
            .unwrap();
        let shadowed = ["dtsdec", "dcadec"].iter().any(|n| {
            gst::ElementFactory::find(n).is_some_and(|f| {
                f.rank() > gst::Rank::NONE
                    && gst::ElementFactory::find("avdec_dca")
                        .is_some_and(|full| f.rank() >= full.rank())
            })
        });
        assert_eq!(
            dts.status,
            if shadowed { Status::Warn } else { Status::Ok },
            "dts-hd check disagrees with the registry: {dts:?}"
        );

        // Fallback rows are OK but recommend the preferred element.
        let hls = checks.iter().find(|c| c.name == "hls sink").unwrap();
        assert_eq!(hls.status, Status::Ok);
        let has_preferred = gst::ElementFactory::find("hlssink3").is_some();
        if !has_preferred {
            assert!(
                hls.detail.contains("hlssink3 preferred"),
                "fallback should recommend the preferred element: {hls:?}"
            );
        }
    }
}
