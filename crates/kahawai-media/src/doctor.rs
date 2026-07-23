//! Environment self-check (OPS-3): GStreamer inventory against the feature
//! matrix, naming exactly which capability each missing plugin costs.

use gstreamer as gst;
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
        Self { name: name.into(), status: Status::Ok, detail: detail.into(), essential: false }
    }
    pub fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Warn, detail: detail.into(), essential: false }
    }
    pub fn fail(name: impl Into<String>, detail: impl Into<String>, essential: bool) -> Self {
        Self { name: name.into(), status: Status::Fail, detail: detail.into(), essential }
    }
}

/// capability → (elements in preference order, essential, cost when missing,
/// recommend-preferred: if matched by a fallback, suggest installing the
/// first-listed element)
const MATRIX: &[(&str, &[&str], bool, &str, bool)] = &[
    ("typefind", &["typefind"], true, "media discovery is impossible", false),
    ("stream parsing", &["parsebin"], true, "discovery and remux are impossible", false),
    ("demux mkv", &["matroskademux"], true, "MKV/WebM sources unusable", false),
    ("demux mp4", &["qtdemux"], true, "MP4/MOV sources unusable", false),
    ("parse h264", &["h264parse"], true, "H.264 streams cannot be handled", false),
    ("parse hevc", &["h265parse"], false, "HEVC streams cannot be parsed", false),
    ("hls sink", &["hlssink3", "hlssink2"], false, "in-hub HLS remux unavailable", true),
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
        &["cmafmux", "isofmp4mux"],
        false,
        "HLS uses TS segments only (install gst-plugins-rs for fMP4/CMAF)",
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
        &["vah264enc", "vaapih264enc", "nvh264enc", "qsvh264enc", "x264enc", "openh264enc"],
        false,
        "no video transcoding to H.264",
        false,
     ),
    (
        "encode aac",
        &["fdkaacenc", "avenc_aac", "voaacenc"],
        false,
        "no audio transcoding to AAC",
        false,
     ),
    ("decode aac", &["fdkaacdec", "avdec_aac"], false, "AAC audio cannot be transcoded", false),
    ("decode ac3", &["a52dec", "avdec_ac3"], false, "AC-3 audio cannot be transcoded", false),
    (
        "decode eac3",
        &["avdec_eac3"],
        false,
        "E-AC-3 audio cannot be transcoded (silent in browsers) — install gst-libav",
        false,
    ),
    ("decode dts", &["dcadec", "avdec_dca"], false, "DTS audio cannot be transcoded", false),
    (
        "decode truehd",
        &["avdec_truehd"],
        false,
        "TrueHD audio cannot be transcoded — install gst-libav",
        false,
    ),
    ("decode vorbis/opus", &["vorbisdec", "opusdec"], false, "ogg audio cannot be transcoded", false),
    ("subtitle parse", &["subparse"], false, "text subtitle conversion unavailable", false),
    ("ass burn-in", &["assrender"], false, "ASS burn-in unavailable (flatten only, HUB-32a)", false),
];

/// GStreamer version + feature-matrix inventory. Reused by `doctor` and by
/// module startup (warnings logged, essential failures fatal).
pub fn gstreamer_checks() -> Vec<Check> {
    let mut out = Vec::new();
    if let Err(e) = crate::init() {
        out.push(Check::fail("gstreamer", format!("{e:#}"), true));
        return out;
    }
    let (maj, min, micro, _) = gst::version();
    out.push(Check::ok("gstreamer", format!("{maj}.{min}.{micro}")));

    for (name, elements, essential, cost, recommend) in MATRIX {
        match elements.iter().find(|e| gst::ElementFactory::find(e).is_some()) {
            Some(found) if *recommend && *found != elements[0] => out.push(Check::ok(
                *name,
                format!("via {found} — {} preferred, consider installing it", elements[0]),
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

    // TC-1: encoders that will actually run sessions are dry-run-verified,
    // not just present — a broken element surfaces here, not mid-session.
    if let Some(c) = out.iter_mut().find(|c| c.name == "encode aac")
        && c.status == Status::Ok
    {
        match crate::remux::aac_encoder() {
            Some(name) => c.detail = format!("via {name} (dry-run verified)"),
            None => {
                c.status = Status::Warn;
                c.detail = "installed but dry-run failed — audio transcode disabled".into();
            }
        }
    }
    out
}

/// True if any essential check failed.
pub fn has_essential_failure(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail && c.essential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_gst_install_passes_essentials() {
        let checks = gstreamer_checks();
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
