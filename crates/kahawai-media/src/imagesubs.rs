//! Image-based subtitle decoding: PGS (Blu-ray, S_HDMV/PGS) and VobSub
//! (DVD, S_VOBSUB) into RGBA bitmaps, so clients render them on an
//! overlay canvas — no video transcoding involved.
//!
//! Both decoders consume the post-demux Matroska block payloads (gst
//! has already undone any track-level compression).

use anyhow::{bail, Context, Result};

/// One rendered subtitle object, positioned on the composition canvas.
pub struct ImageObject {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// RGBA8, w*h*4 bytes.
    pub rgba: Vec<u8>,
}

/// A display set: the complete screen state from this moment on.
/// Empty `objects` = clear the screen.
pub struct DisplaySet {
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub objects: Vec<ImageObject>,
}

// ---------- PGS ----------

/// Stateful PGS decoder: palettes and objects persist within an epoch,
/// so definitions may arrive in earlier display sets than the
/// composition that uses them.
#[derive(Default)]
pub struct PgsDecoder {
    palettes: std::collections::HashMap<u8, Vec<[u8; 4]>>, // id → 256 RGBA
    objects: std::collections::HashMap<u16, PgsObject>,
    /// Partially accumulated multi-fragment ODS.
    pending: std::collections::HashMap<u16, PgsObject>,
}

struct PgsObject {
    w: u32,
    h: u32,
    rle: Vec<u8>,
    declared_len: usize,
}

impl PgsDecoder {
    /// Feed one Matroska block payload (a full display set:
    /// `[type u8][size u16 BE][payload]`… ending with END). Returns the
    /// new screen state, or None when the set only carries definitions.
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<DisplaySet>> {
        let mut pos = 0usize;
        let mut composition: Option<(u32, u32, u8, Vec<(u16, u32, u32)>)> = None;
        while pos + 3 <= data.len() {
            let seg_type = data[pos];
            let size = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
            let body = data
                .get(pos + 3..pos + 3 + size)
                .context("PGS segment overruns block")?;
            pos += 3 + size;
            match seg_type {
                0x16 => {
                    // PCS: canvas + composition object references.
                    if body.len() < 11 {
                        bail!("short PCS");
                    }
                    let w = u16::from_be_bytes([body[0], body[1]]) as u32;
                    let h = u16::from_be_bytes([body[2], body[3]]) as u32;
                    let state = body[7];
                    if state & 0x80 != 0 {
                        // Epoch start: everything resets.
                        self.palettes.clear();
                        self.objects.clear();
                        self.pending.clear();
                    }
                    let palette_id = body[9];
                    let n = body[10] as usize;
                    let mut objs = Vec::new();
                    let mut o = 11usize;
                    for _ in 0..n {
                        if o + 8 > body.len() {
                            break;
                        }
                        let oid = u16::from_be_bytes([body[o], body[o + 1]]);
                        let cropped = body[o + 3] & 0x80 != 0;
                        let x = u16::from_be_bytes([body[o + 4], body[o + 5]]) as u32;
                        let y = u16::from_be_bytes([body[o + 6], body[o + 7]]) as u32;
                        objs.push((oid, x, y));
                        o += if cropped { 16 } else { 8 };
                    }
                    composition = Some((w, h, palette_id, objs));
                }
                0x14 => {
                    // PDS: palette entries (YCrCb+A → RGBA, BT.709).
                    if body.len() < 2 {
                        continue;
                    }
                    let palette = self
                        .palettes
                        .entry(body[0])
                        .or_insert_with(|| vec![[0u8; 4]; 256]);
                    for e in body[2..].chunks_exact(5) {
                        let (i, y, cr, cb, a) =
                            (e[0] as usize, e[1] as f32, e[2] as f32, e[3] as f32, e[4]);
                        // Limited-range BT.709 (Y 16..235 is video black..white).
                        let yl = 1.164 * (y - 16.0);
                        let r = yl + 1.793 * (cr - 128.0);
                        let g = yl - 0.213 * (cb - 128.0) - 0.533 * (cr - 128.0);
                        let b = yl + 2.112 * (cb - 128.0);
                        palette[i] = [
                            r.clamp(0.0, 255.0) as u8,
                            g.clamp(0.0, 255.0) as u8,
                            b.clamp(0.0, 255.0) as u8,
                            a,
                        ];
                    }
                }
                0x15 => {
                    // ODS: object bitmap, possibly fragmented.
                    if body.len() < 4 {
                        continue;
                    }
                    let oid = u16::from_be_bytes([body[0], body[1]]);
                    let seq = body[3];
                    if seq & 0x80 != 0 {
                        // First fragment: data_len(3) + w(2) + h(2).
                        if body.len() < 11 {
                            continue;
                        }
                        let declared =
                            (u32::from_be_bytes([0, body[4], body[5], body[6]]) as usize)
                                .saturating_sub(4); // w+h counted in data_len
                        let w = u16::from_be_bytes([body[7], body[8]]) as u32;
                        let h = u16::from_be_bytes([body[9], body[10]]) as u32;
                        self.pending.insert(
                            oid,
                            PgsObject { w, h, rle: body[11..].to_vec(), declared_len: declared },
                        );
                    } else if let Some(p) = self.pending.get_mut(&oid) {
                        p.rle.extend_from_slice(&body[4..]);
                    }
                    if seq & 0x40 != 0
                        && let Some(done) = self.pending.remove(&oid)
                    {
                        self.objects.insert(oid, done);
                    }
                }
                0x17 | 0x80 => {} // WDS / END: windows unused, END is a marker
                _ => {}
            }
        }

        let Some((w, h, palette_id, refs)) = composition else { return Ok(None) };
        let palette = self.palettes.get(&palette_id).cloned().unwrap_or_else(|| vec![[0; 4]; 256]);
        let mut objects = Vec::new();
        for (oid, x, y) in refs {
            let Some(obj) = self.objects.get(&oid) else { continue };
            if obj.rle.len() < obj.declared_len {
                continue; // incomplete fragment chain
            }
            if let Ok(rgba) = pgs_rle_decode(&obj.rle, obj.w, obj.h, &palette) {
                objects.push(ImageObject { x, y, w: obj.w, h: obj.h, rgba });
            }
        }
        Ok(Some(DisplaySet { canvas_w: w, canvas_h: h, objects }))
    }
}

/// PGS run-length decoding into RGBA via the palette.
fn pgs_rle_decode(rle: &[u8], w: u32, h: u32, palette: &[[u8; 4]]) -> Result<Vec<u8>> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 4];
    let (mut x, mut yline) = (0usize, 0usize);
    let mut i = 0usize;
    let put = |x: &mut usize, y: usize, color: u8, run: usize, out: &mut Vec<u8>| {
        let px = palette.get(color as usize).copied().unwrap_or([0; 4]);
        for _ in 0..run {
            if *x < w && y < h {
                let o = (y * w + *x) * 4;
                out[o..o + 4].copy_from_slice(&px);
            }
            *x += 1;
        }
    };
    while i < rle.len() && yline < h {
        let c = rle[i];
        i += 1;
        if c != 0 {
            put(&mut x, yline, c, 1, &mut out);
            continue;
        }
        let Some(&f) = rle.get(i) else { break };
        i += 1;
        match f {
            0 => {
                // End of line.
                yline += 1;
                x = 0;
            }
            _ => {
                let long = f & 0x40 != 0;
                let colored = f & 0x80 != 0;
                let mut run = (f & 0x3F) as usize;
                if long {
                    run = run << 8 | *rle.get(i).context("PGS RLE truncated")? as usize;
                    i += 1;
                }
                let color = if colored {
                    let c = *rle.get(i).context("PGS RLE truncated")?;
                    i += 1;
                    c
                } else {
                    0
                };
                put(&mut x, yline, color, run, &mut out);
            }
        }
    }
    Ok(out)
}

// ---------- VobSub ----------

/// Parse the 16-color palette from an S_VOBSUB CodecPrivate (the .idx
/// header text): `palette: 000000, 828282, …`.
pub fn vobsub_palette(codec_private: &str) -> Vec<[u8; 3]> {
    for line in codec_private.lines() {
        if let Some(rest) = line.trim().strip_prefix("palette:") {
            return rest
                .split(',')
                .filter_map(|c| u32::from_str_radix(c.trim(), 16).ok())
                .map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
                .collect();
        }
    }
    Vec::new()
}

/// The display size an S_VOBSUB CodecPrivate declares (`size: 720x576`).
/// VobSub coordinates are relative to THIS canvas, which need not be
/// the video's own resolution — a re-encode keeps the original .idx.
pub fn vobsub_size(codec_private: &str) -> Option<(u32, u32)> {
    codec_private.lines().find_map(|l| {
        let (w, h) = l.trim().strip_prefix("size:")?.trim().split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    })
}

/// Decode one VobSub SPU (post-demux Matroska block payload) into a
/// positioned bitmap. Returns None for empty/unshowable units.
pub fn vobsub_decode(spu: &[u8], palette16: &[[u8; 3]]) -> Result<Option<ImageObject>> {
    if spu.len() < 6 {
        return Ok(None);
    }
    let ctrl_at = u16::from_be_bytes([spu[2], spu[3]]) as usize;
    if ctrl_at >= spu.len() {
        bail!("SPU control offset out of range");
    }
    // Walk control sequences for palette/alpha/coords/data offsets.
    let (mut pal_idx, mut alpha) = ([0u8; 4], [0u8; 4]);
    let (mut x1, mut x2, mut y1, mut y2) = (0usize, 0usize, 0usize, 0usize);
    let (mut top_at, mut bot_at) = (0usize, 0usize);
    let mut pos = ctrl_at + 4; // skip date + next-seq offset of first block
    while pos < spu.len() {
        match spu[pos] {
            0x00 | 0x01 => pos += 1, // force/start display
            0x02 => break,           // stop display (timing comes from the block)
            0x03 => {
                pal_idx = [spu[pos + 1] >> 4, spu[pos + 1] & 0xF, spu[pos + 2] >> 4, spu[pos + 2] & 0xF];
                pos += 3;
            }
            0x04 => {
                alpha = [spu[pos + 1] >> 4, spu[pos + 1] & 0xF, spu[pos + 2] >> 4, spu[pos + 2] & 0xF];
                pos += 3;
            }
            0x05 => {
                let b = &spu[pos + 1..pos + 7];
                x1 = (b[0] as usize) << 4 | (b[1] >> 4) as usize;
                x2 = ((b[1] & 0xF) as usize) << 8 | b[2] as usize;
                y1 = (b[3] as usize) << 4 | (b[4] >> 4) as usize;
                y2 = ((b[4] & 0xF) as usize) << 8 | b[5] as usize;
                pos += 7;
            }
            0x06 => {
                top_at = u16::from_be_bytes([spu[pos + 1], spu[pos + 2]]) as usize;
                bot_at = u16::from_be_bytes([spu[pos + 3], spu[pos + 4]]) as usize;
                pos += 5;
            }
            0xFF => break,
            _ => pos += 1,
        }
    }
    if x2 < x1 || y2 < y1 || top_at == 0 {
        return Ok(None);
    }
    let (w, h) = (x2 - x1 + 1, y2 - y1 + 1);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return Ok(None);
    }

    // 4-bit RLE, two interlaced fields.
    let mut out = vec![0u8; w * h * 4];
    for (field, start) in [(0usize, top_at), (1, bot_at)] {
        let mut nib = start * 2; // nibble index into spu
        let read = |n: usize| -> u8 {
            let byte = spu.get(n / 2).copied().unwrap_or(0);
            if n.is_multiple_of(2) { byte >> 4 } else { byte & 0xF }
        };
        let mut y = field;
        let mut x = 0usize;
        while y < h && nib / 2 < spu.len() {
            let mut v = read(nib) as usize;
            nib += 1;
            if v < 0x4 {
                v = v << 4 | read(nib) as usize;
                nib += 1;
                if v < 0x10 {
                    v = v << 4 | read(nib) as usize;
                    nib += 1;
                    if v < 0x40 {
                        v = v << 4 | read(nib) as usize;
                        nib += 1;
                        if v < 4 {
                            // 0x000c: fill to end of line.
                            v |= ((w - x) << 2);
                        }
                    }
                }
            }
            let run = (v >> 2).min(w - x);
            let color = (v & 3);
            let pi = pal_idx[3 - color] as usize; // control order is reversed
            let a = alpha[3 - color];
            let rgb = palette16.get(pi).copied().unwrap_or([0, 0, 0]);
            for _ in 0..run {
                let o = (y * w + x) * 4;
                out[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], a << 4 | a]);
                x += 1;
            }
            if x >= w {
                x = 0;
                y += 2; // next line of this field
                nib = (nib + 1) & !1; // byte-align per line
            }
        }
    }
    Ok(Some(ImageObject { x: x1 as u32, y: y1 as u32, w: w as u32, h: h as u32, rgba: out }))
}

/// Encode an RGBA object as a PNG (for the JSONL tap stream).
pub fn to_png(obj: &ImageObject) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, obj.w, obj.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&obj.rgba)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(t: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![t];
        v.extend((body.len() as u16).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn pgs_display_set_roundtrip() {
        // Palette 0: entry 1 = opaque white.
        let pds = {
            let mut b = vec![0u8, 0]; // palette id, version
            b.extend_from_slice(&[1, 235, 128, 128, 255]); // idx1: Y=235 (white)
            b
        };
        // Object 7: 4x2, RLE: line = colored run of 4 (0x00 0x84 0x01), EOL.
        let ods = {
            let mut b = vec![0, 7, 0, 0xC0]; // oid, ver, first+last
            let rle: Vec<u8> = vec![0x00, 0x84, 0x01, 0x00, 0x00, 0x00, 0x84, 0x01, 0x00, 0x00];
            b.extend_from_slice(&(rle.len() as u32 + 4).to_be_bytes()[1..]); // data_len u24
            b.extend_from_slice(&4u16.to_be_bytes());
            b.extend_from_slice(&2u16.to_be_bytes());
            b.extend_from_slice(&rle);
            b
        };
        // PCS: 1920x1080, epoch start, palette 0, one object at (100,900).
        let pcs = {
            let mut b = Vec::new();
            b.extend_from_slice(&1920u16.to_be_bytes());
            b.extend_from_slice(&1080u16.to_be_bytes());
            b.push(0x10); // frame rate
            b.extend_from_slice(&1u16.to_be_bytes()); // comp number
            b.push(0x80); // epoch start
            b.push(0); // palette update
            b.push(0); // palette id
            b.push(1); // objects
            b.extend_from_slice(&7u16.to_be_bytes());
            b.push(0); // window id
            b.push(0); // flags
            b.extend_from_slice(&100u16.to_be_bytes());
            b.extend_from_slice(&900u16.to_be_bytes());
            b
        };
        let block =
            [seg(0x16, &pcs), seg(0x14, &pds), seg(0x15, &ods), seg(0x80, &[])].concat();

        let mut dec = PgsDecoder::default();
        let set = dec.feed(&block).unwrap().expect("display set");
        assert_eq!((set.canvas_w, set.canvas_h), (1920, 1080));
        assert_eq!(set.objects.len(), 1);
        let o = &set.objects[0];
        assert_eq!((o.x, o.y, o.w, o.h), (100, 900, 4, 2));
        // All 8 pixels near-white and opaque.
        for px in o.rgba.chunks(4) {
            assert!(px[0] > 240 && px[1] > 240 && px[2] > 240, "{px:?}");
            assert_eq!(px[3], 255);
        }
        // Clear: PCS with zero objects.
        let clear_pcs = {
            let mut b = pcs.clone();
            b[7] = 0x00; // normal case
            b[10] = 0; // no objects
            b.truncate(11);
            b
        };
        let block2 = [seg(0x16, &clear_pcs), seg(0x80, &[])].concat();
        let set2 = dec.feed(&block2).unwrap().expect("clear set");
        assert!(set2.objects.is_empty());
    }

    #[test]
    fn vobsub_size_parses() {
        let idx = "# comment\nsize: 720x576\npalette: 000000\n";
        assert_eq!(vobsub_size(idx), Some((720, 576)));
        assert_eq!(vobsub_size("palette: 000000\n"), None);
    }

    #[test]
    fn vobsub_palette_parses() {
        let idx = "# comment\nsize: 720x576\npalette: 000000, ff0000, 00ff00, 0000ff\n";
        let p = vobsub_palette(idx);
        assert_eq!(p.len(), 4);
        assert_eq!(p[1], [255, 0, 0]);
    }

    #[test]
    fn png_encodes() {
        let obj = ImageObject { x: 0, y: 0, w: 2, h: 2, rgba: vec![255; 16] };
        let png = to_png(&obj).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}
