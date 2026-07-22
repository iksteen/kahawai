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

/// capability → (any-of elements, essential, cost when missing)
const MATRIX: &[(&str, &[&str], bool, &str)] = &[
    ("typefind", &["typefind"], true, "media discovery is impossible"),
    ("stream parsing", &["parsebin"], true, "discovery and remux are impossible"),
    ("demux mkv", &["matroskademux"], true, "MKV/WebM sources unusable"),
    ("demux mp4", &["qtdemux"], true, "MP4/MOV sources unusable"),
    ("parse h264", &["h264parse"], true, "H.264 streams cannot be handled"),
    ("parse hevc", &["h265parse"], false, "HEVC streams cannot be parsed"),
    ("hls sink", &["hlssink2", "hlssink3"], false, "in-hub HLS remux unavailable"),
    (
        "mux fmp4/cmaf",
        &["cmafmux", "isofmp4mux"],
        false,
        "HLS uses TS segments only (install gst-plugins-rs for fMP4/CMAF)",
    ),
    (
        "decode h264",
        &["vah264dec", "nvh264dec", "avdec_h264", "openh264dec"],
        false,
        "H.264 sources cannot be transcoded (direct play only)",
    ),
    (
        "decode hevc",
        &["vah265dec", "nvh265dec", "avdec_h265"],
        false,
        "HEVC sources will always fail to transcode",
    ),
    (
        "encode h264",
        &["vah264enc", "vaapih264enc", "nvh264enc", "qsvh264enc", "x264enc", "openh264enc"],
        false,
        "no video transcoding to H.264",
    ),
    (
        "encode aac",
        &["fdkaacenc", "avenc_aac", "voaacenc"],
        false,
        "no audio transcoding to AAC",
    ),
    ("decode aac", &["fdkaacdec", "avdec_aac"], false, "AAC audio cannot be transcoded"),
    ("decode vorbis/opus", &["vorbisdec", "opusdec"], false, "ogg audio cannot be transcoded"),
    ("subtitle parse", &["subparse"], false, "text subtitle conversion unavailable"),
    ("ass burn-in", &["assrender"], false, "ASS burn-in unavailable (flatten only, HUB-32a)"),
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

    for (name, elements, essential, cost) in MATRIX {
        match elements.iter().find(|e| gst::ElementFactory::find(e).is_some()) {
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
    }
}
