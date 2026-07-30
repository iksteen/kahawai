//! HUB-32c: OCR of image subtitles (PGS/VobSub) into text tracks.
//!
//! Input is the HUB-32b display-set cache (the mediahost walked the
//! index; the hub holds the KBS1 file), decoded by the same
//! `kahawai-media` machinery the burn-in tier uses — so this module is
//! only preprocessing, Tesseract, and cue assembly. Engine is Tesseract
//! via `leptess` (MIT) for BOTH formats; `subtile-ocr` (GPL-3.0) is
//! deliberately not linked — its value is parsing `.idx/.sub` files we
//! already decode, and dropping it keeps `ocr`-enabled binaries free of
//! copyleft obligations (NFR-8).
//!
//! Results ride the `downloaded_subtitles` machinery with
//! `provider='ocr'`: stored, listed, served, selected, and deleted
//! exactly like a provider download, marked machine-derived in the API.
//! Cached per (source stream, model): regenerating first deletes.
//!
//! Quality, measured on real tracks (Babylon 5 PGS 1080p, conf 70–91;
//! a 2160p PGS track, conf 84–91): glyphs are bright-on-transparent,
//! so binarize to black-on-white at luma>100 ∧ alpha>128, upscale small
//! (DVD-height) bitmaps ×3, PSM 6 per display set (a set is one
//! subtitle: 1–3 uniform lines). ~16 ms/set → a feature film in ~30 s,
//! which is why generation is spawn_blocking and cached, never inline
//! in a session start.

use anyhow::{Context, Result};
use kahawai_media::subtitles::Cue;

/// Track language tag (ISO 639-1/2, as containers write it) → Tesseract
/// model name. Only tags whose mapping is not the identity; a 3-letter
/// tag not listed here is tried as a model name directly.
const LANG_MODELS: &[(&str, &str)] = &[
    ("en", "eng"),
    ("nl", "nld"),
    ("dut", "nld"),
    ("ja", "jpn"),
    ("de", "deu"),
    ("ger", "deu"),
    ("fr", "fra"),
    ("fre", "fra"),
    ("es", "spa"),
    ("it", "ita"),
    ("pt", "por"),
    ("ru", "rus"),
    ("zh", "chi_sim"),
    ("chi", "chi_sim"),
    ("zho", "chi_sim"),
    ("ko", "kor"),
    ("sv", "swe"),
    ("da", "dan"),
    ("no", "nor"),
    ("fi", "fin"),
    ("pl", "pol"),
    ("ro", "ron"),
    ("rum", "ron"),
    ("el", "ell"),
    ("gre", "ell"),
    ("cs", "ces"),
    ("cze", "ces"),
];

/// The Tesseract model for a track's language tag; `None` when the
/// model is not installed (the tier is then skipped, per HUB-32c's
/// graceful-degradation clause). An absent tag tries English — the
/// dominant case in this library, and the output is marked imperfect
/// either way. NOTE the tag itself can lie: a real track tagged `en`
/// carried Romanian, which OCRs readably under `eng` minus diacritics.
/// That is a metadata defect, not an OCR one; the result stays
/// reviewable and deletable.
pub fn model_for(lang: Option<&str>) -> Option<String> {
    let tag = lang.unwrap_or("en").to_ascii_lowercase();
    let model = LANG_MODELS
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, m)| (*m).to_string())
        .or_else(|| (tag.len() == 3).then_some(tag))?;
    model_installed(&model).then_some(model)
}

/// Asking Tesseract itself is the only probe that cannot disagree with
/// Tesseract (TESSDATA_PREFIX, per-distro data dirs...). Cached: a init
/// costs ~20 ms and the doctor asks about many models.
fn model_installed(model: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static SEEN: std::sync::OnceLock<Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(hit) = seen.lock().unwrap().get(model) {
        return *hit;
    }
    let ok = leptess::LepTess::new(None, model).is_ok();
    seen.lock().unwrap().insert(model.to_string(), ok);
    ok
}

/// Doctor row (OPS-3): is the engine usable, and with which of the
/// models this library's languages actually need?
pub fn doctor_check() -> kahawai_media::doctor::Check {
    use kahawai_media::doctor::Check;
    if !model_installed("eng") {
        return Check::warn(
            "ocr (HUB-32c)",
            "Tesseract unusable (library or eng.traineddata missing) — image \
             subtitles cannot become text; install tesseract + language data",
        );
    }
    let present: Vec<&str> = ["eng", "nld", "jpn", "deu", "fra", "spa"]
        .into_iter()
        .filter(|m| model_installed(m))
        .collect();
    Check::ok(
        "ocr (HUB-32c)",
        format!("tesseract usable; models: {}", present.join(", ")),
    )
}

/// OCR one cached display-set file into cues. Blocking (~16 ms per set,
/// seconds to minutes per track) — call from `spawn_blocking`.
pub fn ocr_sets_file(sets: &std::path::Path, model: &str) -> Result<Vec<Cue>> {
    let timeline = kahawai_media::burnin::timeline_from_file(sets)?
        .context("sets file has no readable timeline")?;
    let mut lt = leptess::LepTess::new(None, model)
        .map_err(|e| anyhow::anyhow!("tesseract init ({model}): {e}"))?;
    let mut cues: Vec<Cue> = Vec::new();
    for set in timeline.sets() {
        if set.objects.is_empty() || set.end_ms <= set.start_ms {
            continue; // screen clear, or a zero-length re-issue (seen live)
        }
        let mut lines: Vec<String> = Vec::new();
        for obj in set.objects {
            let (gray, w, h) = binarize(obj);
            let bmp = gray_bmp(&gray, w, h);
            if lt.set_image_from_mem(&bmp).is_err() {
                continue;
            }
            let Ok(text) = lt.get_utf8_text() else {
                continue;
            };
            let text = text.trim();
            if !text.is_empty() {
                lines.extend(text.lines().map(|l| l.trim().to_string()));
            }
        }
        if lines.is_empty() {
            continue;
        }
        let text = lines.join("\n");
        // PGS re-issues the same screen state (fades, splits); one cue.
        if let Some(last) = cues.last_mut()
            && last.text == text
            && set.start_ms <= last.end_ms
        {
            last.end_ms = last.end_ms.max(set.end_ms);
            continue;
        }
        cues.push(Cue {
            start_ms: set.start_ms,
            end_ms: set.end_ms,
            text,
        });
    }
    anyhow::ensure!(!cues.is_empty(), "OCR produced no text");
    Ok(cues)
}

/// Glyph ink → black on white, in Tesseract's terms. Bitmap subtitle
/// text is bright glyphs with a dark outline on transparency: ink is
/// what is both opaque and bright. Sub-40px bitmaps (DVD heights) are
/// upscaled ×3 — Tesseract wants ≥ ~30 px cap height.
fn binarize(o: &kahawai_media::imagesubs::ImageObject) -> (Vec<u8>, u32, u32) {
    let scale = if o.h < 40 { 3 } else { 1 };
    let (w, h) = (o.w * scale, o.h * scale);
    let mut out = vec![0xFFu8; (w as usize) * (h as usize)];
    for y in 0..h {
        for x in 0..w {
            let p = ((y / scale) as usize * o.w as usize + (x / scale) as usize) * 4;
            let (r, g, b, a) = (
                o.rgba[p] as u32,
                o.rgba[p + 1] as u32,
                o.rgba[p + 2] as u32,
                o.rgba[p + 3],
            );
            let luma = (r * 299 + g * 587 + b * 114) / 1000;
            if a > 128 && luma > 100 {
                out[(y * w + x) as usize] = 0;
            }
        }
    }
    (out, w, h)
}

/// 8-bit grayscale as an in-memory BMP — the one bitmap container that
/// is 30 lines by hand, and Leptonica reads it. No image crate needed.
fn gray_bmp(gray: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = w.div_ceil(4) * 4;
    let mut bmp = Vec::with_capacity(1078 + (row * h) as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(1078 + row * h).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&1078u32.to_le_bytes()); // pixel offset
    bmp.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    bmp.extend_from_slice(&w.to_le_bytes());
    bmp.extend_from_slice(&h.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
    bmp.extend_from_slice(&8u16.to_le_bytes()); // bpp
    bmp.extend_from_slice(&[0u8; 24]); // compression..colors, all zero
    for i in 0..=255u8 {
        bmp.extend_from_slice(&[i, i, i, 0]); // grayscale palette
    }
    for y in (0..h).rev() {
        let start = (y * w) as usize;
        bmp.extend_from_slice(&gray[start..start + w as usize]);
        bmp.extend(std::iter::repeat_n(0u8, (row - w) as usize));
    }
    bmp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_mapping_resolves_and_gates_on_installed_models() {
        if !model_installed("eng") {
            eprintln!("skipping: tesseract/eng not installed");
            return;
        }
        assert_eq!(model_for(Some("en")).as_deref(), Some("eng"));
        assert_eq!(model_for(None).as_deref(), Some("eng"), "absent tag → eng");
        // A 3-letter tag passes through as a model name.
        assert_eq!(model_for(Some("eng")).as_deref(), Some("eng"));
        // Unknown 2-letter tag has no mapping and is not a model name.
        assert_eq!(model_for(Some("xx")), None);
        assert_eq!(
            model_for(Some("qqq")),
            None,
            "not-installed model → tier off"
        );
    }

    /// The BMP writer is hand-rolled; the claim under test is that
    /// Leptonica parses it (dimensions, palette, row padding) — with a
    /// garbage negative control so "accepts anything" cannot pass.
    /// RECOGNITION quality is not testable from synthetic bars (no font
    /// renderer here); it was measured on real cached PGS tracks and is
    /// re-verified live whenever a track is generated.
    #[test]
    fn hand_rolled_bmp_is_readable_by_tesseract() {
        if !model_installed("eng") {
            eprintln!("skipping: tesseract/eng not installed");
            return;
        }
        let (w, h) = (61u32, 50u32); // odd width: row padding exercised
        let mut gray = vec![0xFFu8; (w * h) as usize];
        for y in 10..40 {
            for x in 20..26 {
                gray[(y * w + x) as usize] = 0;
            }
        }
        let bmp = gray_bmp(&gray, w, h);
        let mut lt = leptess::LepTess::new(None, "eng").unwrap();
        lt.set_image_from_mem(&bmp)
            .expect("Leptonica rejected our BMP");
        lt.get_utf8_text()
            .expect("recognition errored on a valid image");
        assert!(
            lt.set_image_from_mem(&bmp[..40]).is_err(),
            "truncated garbage was accepted — the positive case proves nothing"
        );
    }
}
