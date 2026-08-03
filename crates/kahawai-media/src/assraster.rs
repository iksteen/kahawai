//! HUB-32d: rasterise an ASS script server-side into the same
//! display-set shape PGS already produces, so an overlay-capable client
//! that cannot render ASS itself still gets the typesetting — with no
//! video encode anywhere.
//!
//! The output is deliberately `imagesubs::DisplaySet`, not a new type:
//! HUB-32b already decodes PGS to exactly that and the web client
//! already composites it. This tier is a second producer for a pipe
//! that exists.
//!
//! **Change-driven, not frame-driven.** `ass_render_frame` reports
//! through `detect_change` whether anything actually moved since the
//! last call, so sampling at the source frame rate costs CPU during
//! generation but emits a set only when the picture changes. A script
//! with no animation therefore costs about two sets per dialogue line
//! (one to draw, one to clear) no matter what rate it is sampled at;
//! only `\k` karaoke and `\t`/`\move` animation turn that into
//! per-frame output.
//!
//! **One rect per composition, not one per libass image.** libass emits
//! many overlapping fragments per line — fill, outline, shadow, one per
//! glyph run — and handing those to a canvas one at a time would
//! multiply both the JSON and the client's draw calls. Each changed
//! composition is flattened into a single RGBA rectangle over the union
//! of its fragments' bounding boxes.
//!
//! ## What it costs (measured 2026-08-03, before anything was wired up)
//!
//! OPS-6 never evicts, so this tier was gated on a number. Cost is the
//! product of two independent things — how often the composition
//! CHANGES, and how big each changed rectangle is — and only the first
//! varies much:
//!
//! | script                              | change rate | MB/min |
//! |-------------------------------------|-------------|--------|
//! | ordinary episode, no animation      |        2.5% |   0.64 |
//! | signs-heavy (833 `\pos`)            |        2.4% |   0.49 |
//! | real `\k` karaoke, song isolated    |        7.2% |   1.55 |
//! | synthetic `\kf` sweep + `\t`        |        100% |  57.52 |
//!
//! So a 24-minute episode costs ~15 MB, and ~18 MB if it has a sung
//! OP and ED. Real karaoke is 2.4x ordinary dialogue, not the 90x the
//! last row suggests: `\k` steps once per SYLLABLE (~11 changes a
//! line) while `\kf` sweeps and `\t` transforms change every frame.
//! Only the latter saturate, and this library holds 10 `\kf` tags and
//! ZERO `\t`/`\move` across 52,445 cached tracks — so sampling at the
//! source frame rate costs essentially nothing here, which is why the
//! cadence stayed as specified.
//!
//! Two things worth knowing before optimising:
//!
//! - PNG encoding dominates generation, not rendering (318 s against
//!   15 s on the saturated case). Default zlib on RGBA.
//! - The union-bbox flatten is pathological when one composition holds
//!   widely separated elements: 26% of the frame per rect on the
//!   synthetic case against 1-6% for dialogue. Cluster disjoint groups
//!   before believing any figure from animated content.
//!
//! Re-derive any of this with `raster_cost_from_env` (ignored test).
//!
//! No bindgen and no binding crate: ten `extern "C"` declarations
//! against the `libass.so` that `assrender` already forces onto every
//! box that can burn (HUB-32a). Adding a build-time dependency to reach
//! a library we already link transitively would be the expensive way.

use std::ffi::{CString, c_char, c_int, c_longlong, c_uchar, c_void};

use anyhow::{Context, Result, bail};

use crate::imagesubs::{DisplaySet, ImageObject};

#[repr(C)]
struct AssImage {
    w: c_int,
    h: c_int,
    stride: c_int,
    bitmap: *const c_uchar,
    /// RGBA, and the A is TRANSPARENCY — 0 means opaque. Getting this
    /// backwards yields a correctly-shaped, entirely invisible overlay.
    color: u32,
    dst_x: c_int,
    dst_y: c_int,
    next: *const AssImage,
    kind: c_int,
}

#[link(name = "ass")]
unsafe extern "C" {
    fn ass_library_init() -> *mut c_void;
    fn ass_library_done(lib: *mut c_void);
    fn ass_renderer_init(lib: *mut c_void) -> *mut c_void;
    fn ass_renderer_done(r: *mut c_void);
    fn ass_set_frame_size(r: *mut c_void, w: c_int, h: c_int);
    fn ass_set_storage_size(r: *mut c_void, w: c_int, h: c_int);
    fn ass_set_fonts(
        r: *mut c_void,
        default_font: *const c_char,
        default_family: *const c_char,
        dfp: c_int,
        config: *const c_char,
        update: c_int,
    );
    fn ass_read_memory(
        lib: *mut c_void,
        buf: *mut c_char,
        len: usize,
        codepage: *const c_char,
    ) -> *mut c_void;
    fn ass_free_track(track: *mut c_void);
    fn ass_render_frame(
        r: *mut c_void,
        track: *mut c_void,
        now: c_longlong,
        detect_change: *mut c_int,
    ) -> *const AssImage;
}

/// AUTODETECT: the box's own font stack. A standalone script names
/// fonts and relies on the host to have them, exactly as any other
/// player treats it (HUB-32a records the same rule for burn-in).
const FONTPROVIDER_AUTODETECT: c_int = 1;

pub struct Raster {
    lib: *mut c_void,
    renderer: *mut c_void,
    width: u32,
    height: u32,
}

impl Drop for Raster {
    fn drop(&mut self) {
        unsafe {
            ass_renderer_done(self.renderer);
            ass_library_done(self.lib);
        }
    }
}

impl Raster {
    /// Render at the video's CODED size. One size, not one per client:
    /// the player scales the overlay uniformly by width exactly as it
    /// does for PGS, so there is no per-resolution cache to multiply.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "zero raster size");
        unsafe {
            let lib = ass_library_init();
            if lib.is_null() {
                bail!("ass_library_init failed");
            }
            let renderer = ass_renderer_init(lib);
            if renderer.is_null() {
                ass_library_done(lib);
                bail!("ass_renderer_init failed");
            }
            ass_set_frame_size(renderer, width as c_int, height as c_int);
            ass_set_storage_size(renderer, width as c_int, height as c_int);
            let family = CString::new("Sans").unwrap();
            ass_set_fonts(
                renderer,
                std::ptr::null(),
                family.as_ptr(),
                FONTPROVIDER_AUTODETECT,
                std::ptr::null(),
                1,
            );
            Ok(Self {
                lib,
                renderer,
                width,
                height,
            })
        }
    }

    /// Every composition the script produces between 0 and `until_ms`,
    /// sampled at `fps`, as `(start_ms, set)` in time order. A set with
    /// no objects clears the screen — same convention as PGS.
    pub fn render(
        &mut self,
        script: &str,
        fps: (u32, u32),
        until_ms: u64,
    ) -> Result<Vec<(u64, DisplaySet)>> {
        let (num, den) = fps;
        anyhow::ensure!(num > 0 && den > 0, "zero frame rate");
        let mut buf = script.as_bytes().to_vec();
        let track = unsafe {
            let utf8 = CString::new("UTF-8").unwrap();
            ass_read_memory(
                self.lib,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
                utf8.as_ptr(),
            )
        };
        if track.is_null() {
            bail!("libass could not parse the script");
        }
        let mut out = Vec::new();
        let mut frame: u64 = 0;
        loop {
            // Integer arithmetic throughout: 24000/1001 accumulates
            // visible drift in f64 over a 24-minute episode.
            let ms = frame * 1000 * den as u64 / num as u64;
            if ms > until_ms {
                break;
            }
            let mut changed: c_int = 0;
            let head =
                unsafe { ass_render_frame(self.renderer, track, ms as c_longlong, &mut changed) };
            if changed != 0 {
                out.push((ms, self.flatten(head)));
            }
            frame += 1;
        }
        unsafe { ass_free_track(track) };
        Ok(out)
    }

    /// Composite one frame's fragment list into a single rectangle.
    fn flatten(&self, mut node: *const AssImage) -> DisplaySet {
        // Pass one: the union box, so pass two can blend into a buffer
        // that is already the right size.
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut n = node;
        while !n.is_null() {
            let img = unsafe { &*n };
            if img.w > 0 && img.h > 0 {
                x0 = x0.min(img.dst_x.max(0) as u32);
                y0 = y0.min(img.dst_y.max(0) as u32);
                x1 = x1.max((img.dst_x.max(0) as u32).saturating_add(img.w as u32));
                y1 = y1.max((img.dst_y.max(0) as u32).saturating_add(img.h as u32));
            }
            n = img.next;
        }
        if x0 == u32::MAX {
            // Nothing on screen: the clear-the-canvas set.
            return DisplaySet {
                canvas_w: self.width,
                canvas_h: self.height,
                objects: Vec::new(),
            };
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        // Pass two, in list order — libass emits shadow, then outline,
        // then glyph, and painting them out of order loses the outline.
        while !node.is_null() {
            let img = unsafe { &*node };
            blend(&mut rgba, w, x0, y0, img);
            node = img.next;
        }
        DisplaySet {
            canvas_w: self.width,
            canvas_h: self.height,
            objects: vec![ImageObject {
                x: x0,
                y: y0,
                w,
                h,
                rgba,
            }],
        }
    }
}

/// Source-over one libass fragment into the accumulator.
fn blend(dst: &mut [u8], dst_w: u32, ox: u32, oy: u32, img: &AssImage) {
    if img.w <= 0 || img.h <= 0 || img.bitmap.is_null() {
        return;
    }
    let (sr, sg, sb) = (
        (img.color >> 24) as u8,
        ((img.color >> 16) & 0xff) as u8,
        ((img.color >> 8) & 0xff) as u8,
    );
    // Inverted: the low byte is TRANSPARENCY.
    let opacity = 255 - (img.color & 0xff);
    if opacity == 0 {
        return;
    }
    let (w, h, stride) = (img.w as usize, img.h as usize, img.stride as usize);
    for row in 0..h {
        for col in 0..w {
            // The last row is not padded to stride; the guaranteed
            // allocation is stride*(h-1)+w, so index it exactly.
            let cov = unsafe { *img.bitmap.add(row * stride + col) } as u32;
            if cov == 0 {
                continue;
            }
            let sa = cov * opacity / 255;
            if sa == 0 {
                continue;
            }
            let dx = (img.dst_x.max(0) as u32 - ox) as usize + col;
            let dy = (img.dst_y.max(0) as u32 - oy) as usize + row;
            let i = (dy * dst_w as usize + dx) * 4;
            let da = dst[i + 3] as u32;
            let out_a = sa + da * (255 - sa) / 255;
            if out_a == 0 {
                continue;
            }
            for (k, s) in [sr, sg, sb].into_iter().enumerate() {
                let d = dst[i + k] as u32;
                dst[i + k] = ((s as u32 * sa + d * da * (255 - sa) / 255) / out_a).min(255) as u8;
            }
            dst[i + 3] = out_a as u8;
        }
    }
}

/// The on-disk cost of one rasterised track, in the shape it would
/// actually be stored (HUB-32b's NDJSON, one line per set).
pub struct RasterCost {
    pub sets: usize,
    pub png_bytes: u64,
    pub ndjson_bytes: u64,
}

/// When the script stops having anything to say: the last event end,
/// plus a second so the final clear-the-screen set is emitted. Sampling
/// past this only produces empty frames.
pub fn script_end_ms(script: &str) -> u64 {
    let (_, events) = crate::subtitles::ass_file_events(script);
    events.iter().map(|(_, end, _)| *end).max().unwrap_or(0) + 1_000
}

/// Serialise to the NDJSON the client already consumes for PGS: one
/// `{"s":ms,"cw":..,"ch":..,"o":[{"x","y","png"}…]}` per line, empty
/// `o` clearing the screen. Deliberately the SAME bytes the live
/// session tap writes (`remux::tap_image_track`) — this tier is a
/// second producer for a format that exists, not a new one.
pub fn to_ndjson(sets: &[(u64, DisplaySet)]) -> Result<Vec<u8>> {
    use base64::Engine;
    use std::io::Write;
    let mut out = Vec::new();
    for (ms, set) in sets {
        let mut objs = Vec::new();
        for o in &set.objects {
            let png = crate::imagesubs::to_png(o).context("png encode")?;
            objs.push(serde_json::json!({
                "x": o.x, "y": o.y,
                "png": base64::engine::general_purpose::STANDARD.encode(png),
            }));
        }
        let line = serde_json::json!({
            "s": ms, "cw": set.canvas_w, "ch": set.canvas_h, "o": objs
        });
        writeln!(out, "{line}")?;
    }
    Ok(out)
}

/// Measure without storing: HUB-32d was gated on this number, because
/// OPS-6 never evicts and a tier that costs gigabytes an episode is a
/// different proposition from one that costs megabytes.
pub fn measure(sets: &[(u64, DisplaySet)]) -> Result<RasterCost> {
    let mut png_bytes = 0u64;
    for (_, set) in sets {
        for o in &set.objects {
            png_bytes += crate::imagesubs::to_png(o).context("png encode")?.len() as u64;
        }
    }
    Ok(RasterCost {
        sets: sets.len(),
        png_bytes,
        ndjson_bytes: to_ndjson(sets)?.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// HUB-32d's gate: rasterise a real script and report what it would
    /// cost on disk, against a cache that never evicts (OPS-6).
    ///
    ///   ASS_SRC=/path/ep.ass ASS_W=1280 ASS_H=720 ASS_FPS=24000/1001 \
    ///   ASS_MS=1440000 cargo test -p kahawai-media raster_cost -- \
    ///     --ignored --nocapture
    #[test]
    #[ignore]
    fn raster_cost_from_env() {
        let Ok(src) = std::env::var("ASS_SRC") else {
            return;
        };
        let script = std::fs::read_to_string(&src).unwrap();
        let num = |k: &str, d: u64| -> u64 {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (num("ASS_W", 1280) as u32, num("ASS_H", 720) as u32);
        let fps = std::env::var("ASS_FPS").unwrap_or_else(|_| "24000/1001".into());
        let (fn_, fd) = fps.split_once('/').unwrap_or((fps.as_str(), "1"));
        let fps = (fn_.parse().unwrap(), fd.parse().unwrap());
        let until = num("ASS_MS", 1_440_000);

        let t0 = Instant::now();
        let mut r = Raster::new(w, h).unwrap();
        let sets = r.render(&script, fps, until).unwrap();
        let render_s = t0.elapsed().as_secs_f64();
        let t1 = Instant::now();
        let cost = measure(&sets).unwrap();
        let encode_s = t1.elapsed().as_secs_f64();

        let mins = until as f64 / 60_000.0;
        let events = script
            .lines()
            .filter(|l| l.starts_with("Dialogue:"))
            .count();
        let px: u64 = sets
            .iter()
            .flat_map(|(_, s)| s.objects.iter())
            .map(|o| (o.w * o.h) as u64)
            .sum();
        println!(
            "\n=== {} ({w}x{h} @ {}/{}, {mins:.1} min)",
            src, fps.0, fps.1
        );
        println!("  dialogue events   : {events}");
        println!(
            "  sets emitted      : {} ({:.1}/event)",
            cost.sets,
            cost.sets as f64 / events.max(1) as f64
        );
        println!("  rasterised pixels : {:.1} Mpx", px as f64 / 1e6);
        println!(
            "  png total         : {:.2} MB",
            cost.png_bytes as f64 / 1e6
        );
        println!(
            "  ndjson on disk    : {:.2} MB  ({:.2} MB/min)",
            cost.ndjson_bytes as f64 / 1e6,
            cost.ndjson_bytes as f64 / 1e6 / mins
        );
        println!("  generation        : {render_s:.1}s render + {encode_s:.1}s encode");
    }

    /// The blend maths, without libass: a fragment whose colour says
    /// "opaque white" must land as opaque white, and one whose alpha
    /// byte says "transparent" must land as nothing. The byte is
    /// TRANSPARENCY, and having it backwards produces a correctly
    /// shaped, entirely invisible overlay — which looks like the
    /// feature silently not working.
    #[test]
    fn the_alpha_byte_is_transparency_not_opacity() {
        let cov = [255u8, 255, 255, 255];
        let img = AssImage {
            w: 2,
            h: 2,
            stride: 2,
            bitmap: cov.as_ptr(),
            color: 0xFFFF_FF00, // white, alpha byte 0 = fully opaque
            dst_x: 0,
            dst_y: 0,
            next: std::ptr::null(),
            kind: 0,
        };
        let mut dst = vec![0u8; 2 * 2 * 4];
        blend(&mut dst, 2, 0, 0, &img);
        assert_eq!(&dst[0..4], &[255, 255, 255, 255], "opaque white expected");

        let clear = AssImage {
            color: 0xFFFF_FFFF, // alpha byte 255 = fully transparent
            ..img
        };
        let mut dst = vec![0u8; 2 * 2 * 4];
        blend(&mut dst, 2, 0, 0, &clear);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0], "transparent must draw nothing");
    }
}
