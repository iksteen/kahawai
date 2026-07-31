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
pub fn gstreamer_checks() -> Vec<Check> {
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
    out.push(dts_hd_check());

    // TC-1: encoders that will actually run sessions are dry-run-verified,
    // not just present — a broken element (or a hw element without its
    // driver) surfaces here, not mid-session.
    // HUB-15a: HDR→SDR is a GL shader segment, not a matrix row — all
    // five elements must be present together or the tier is absent.
    out.push(if crate::remux::tonemap_available() {
        Check::ok("hdr tone-map", "GL shader segment (glshader + capssetter)")
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
                Some(name) => c.detail = format!("via {name} (dry-run verified)"),
                None => {
                    c.status = Status::Warn;
                    c.detail = format!("installed but dry-run failed — {disabled}");
                }
            }
        }
    }
    out
}

fn dts_hd_check() -> Check {
    const NAME: &str = "dts-hd full decode";
    let rank = |name: &str| {
        gst::ElementFactory::find(name)
            .filter(|f| f.rank() > gst::Rank::NONE) // rank NONE = out of autoplug
            .map(|f| f.rank())
    };
    let Some(full) = rank("avdec_dca") else {
        return Check::warn(
            NAME,
            "avdec_dca unavailable — DTS-HD sources decode only the lossy 5.1 core \
             (or not at all); install gst-libav",
        );
    };
    let shadow = ["dtsdec", "dcadec"].iter().find(|n| rank(n) >= Some(full));
    match shadow {
        Some(name) => Check::warn(
            NAME,
            format!(
                "{name} (libdca, core-only) outranks avdec_dca — DTS-HD tracks lose \
                 their lossless extension and decode as lossy 5.1; add \"{name}\" to \
                 [transcoder] demote_decoders"
            ),
        ),
        None => Check::ok(NAME, "via avdec_dca (core + lossless XLL extension)"),
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
