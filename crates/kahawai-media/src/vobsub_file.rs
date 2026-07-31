//! VobSub sidecar files (`.idx` + `.sub`): the DVD-rip era's external
//! image subtitles. The `.idx` is a text index — palette, canvas,
//! per-track language, and (timestamp, filepos) pairs; the `.sub` is
//! the raw MPEG-2 Program Stream those offsets point into, each pack
//! carrying SPU fragments for private stream 1.
//!
//! Matroska muxing strips the PS layer and stores bare SPUs with the
//! idx text as CodecPrivate — which is exactly the shape the rest of
//! this codebase already consumes (`imagesubs::vobsub_*`, the KBS1
//! sets file, burn-in, OCR). This module reproduces that shape from
//! the sidecar pair: parse the idx, depacketize the sub, hand back
//! (idx text, blocks). Nothing downstream can tell a sidecar from an
//! embedded track, which is the point.

use anyhow::{Context, Result};

/// One track of an `.idx` file: `id: en, index: 0` and the timestamp
/// lines that follow it (until the next `id:`).
pub struct IdxTrack {
    /// The `index:` value — the PES substream is `0x20 + id`.
    pub id: u32,
    /// The `id:` language token, verbatim ("en").
    pub language: Option<String>,
    /// (start_ms, byte offset into the .sub), in file order.
    pub entries: Vec<(u64, u64)>,
}

/// Parse the track list out of idx text. Ignores everything global
/// (palette, size — read by `imagesubs::vobsub_*` from the same text).
pub fn parse_idx(text: &str) -> Vec<IdxTrack> {
    let mut tracks: Vec<IdxTrack> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id:") {
            // "en, index: 0"
            let mut lang = None;
            let mut id = tracks.len() as u32; // fallback: positional
            for part in rest.split(',') {
                let part = part.trim();
                if let Some(n) = part.strip_prefix("index:") {
                    if let Ok(n) = n.trim().parse() {
                        id = n;
                    }
                } else if !part.is_empty() && lang.is_none() {
                    lang = Some(part.to_lowercase());
                }
            }
            tracks.push(IdxTrack {
                id,
                language: lang,
                entries: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("timestamp:") {
            // "00:03:20:000, filepos: 000000000" — ms after the last colon.
            let Some(track) = tracks.last_mut() else {
                continue;
            };
            let Some((ts, fp)) = rest.split_once(',') else {
                continue;
            };
            let Some(pos) = fp.trim().strip_prefix("filepos:") else {
                continue;
            };
            let t: Vec<&str> = ts.trim().split(':').collect();
            let [h, m, s, ms] = t[..] else { continue };
            let (Ok(h), Ok(m), Ok(s), Ok(ms)) = (
                h.parse::<u64>(),
                m.parse::<u64>(),
                s.parse::<u64>(),
                ms.parse::<u64>(),
            ) else {
                continue;
            };
            let Ok(pos) = u64::from_str_radix(pos.trim(), 16) else {
                continue;
            };
            track
                .entries
                .push((((h * 60 + m) * 60 + s) * 1000 + ms, pos));
        }
    }
    tracks
}

/// MPEG-PS pack size: fixed for DVD-authored streams, which VobSub is.
const PACK: usize = 2048;

/// Extract one track's SPUs from a `.sub` byte slice, as the same
/// (start_ms, duration, payload) blocks a Matroska demux would yield.
/// Duration comes from the SPU's own stop-display control sequence.
pub fn extract_track(
    idx_text: &str,
    sub: &[u8],
    track_id: u32,
) -> Result<Vec<crate::burnin::SetBlock>> {
    let tracks = parse_idx(idx_text);
    let track = tracks
        .iter()
        .find(|t| t.id == track_id)
        .with_context(|| format!("no track index {track_id} in idx"))?;
    let substream = 0x20u8 + (track_id as u8 & 0x1F);
    let mut blocks = Vec::with_capacity(track.entries.len());
    for &(start_ms, filepos) in &track.entries {
        match assemble_spu(sub, filepos as usize, substream) {
            Ok(spu) => {
                let dur = spu_stop_ms(&spu);
                blocks.push((start_ms, dur, spu));
            }
            // A torn tail or authoring glitch loses one subtitle, not
            // the track; the idx has thousands of entries.
            Err(e) => {
                tracing::debug!(offset = filepos, error = %e, "skipping unreadable SPU");
            }
        }
    }
    anyhow::ensure!(!blocks.is_empty(), "no readable SPUs for track {track_id}");
    Ok(blocks)
}

/// Reassemble one SPU starting at `at`: walk consecutive PS packs,
/// collect PES payloads for our substream (skipping interleaved
/// tracks), until the size the SPU header declares is complete.
fn assemble_spu(sub: &[u8], mut at: usize, substream: u8) -> Result<Vec<u8>> {
    let mut spu: Vec<u8> = Vec::new();
    let mut want = 0usize;
    // A 32 KB SPU spans 16 packs; 64 packs is a generous walk cap.
    for _ in 0..64 {
        if spu.len() >= want && want > 0 {
            break;
        }
        let pack = sub.get(at..at + PACK).context("pack out of range")?;
        anyhow::ensure!(pack[..4] == [0, 0, 1, 0xBA], "not a PS pack at {at}");
        // Pack header: 14 bytes + stuffing (low 3 bits of byte 13).
        let mut p = 14 + (pack[13] & 0x07) as usize;
        while p + 6 <= PACK {
            if pack[p..p + 3] != [0, 0, 1] {
                break;
            }
            let sid = pack[p + 3];
            let len = u16::from_be_bytes([pack[p + 4], pack[p + 5]]) as usize;
            let body = pack.get(p + 6..p + 6 + len).context("PES overruns pack")?;
            p += 6 + len;
            if sid != 0xBD {
                continue; // padding (0xBE), system headers, other streams
            }
            // PES header: flags, flags, header_data_length, then data.
            let hdr = 3 + *body.get(2).context("PES header short")? as usize;
            let data = body.get(hdr..).context("PES data short")?;
            let (Some(&ss), rest) = (data.first(), &data[1.min(data.len())..]) else {
                continue;
            };
            if ss != substream {
                continue;
            }
            if spu.is_empty() {
                anyhow::ensure!(rest.len() >= 2, "SPU fragment too short");
                want = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            }
            spu.extend_from_slice(rest);
        }
        at += PACK;
    }
    anyhow::ensure!(
        want > 0 && spu.len() >= want,
        "SPU incomplete ({}/{want} bytes)",
        spu.len()
    );
    spu.truncate(want);
    Ok(spu)
}

/// The SPU's own display duration: control blocks are (date, next)
/// headers followed by commands, and the block whose commands include
/// 0x02 (stop display) dates the stop in 90kHz/1024 ticks.
fn spu_stop_ms(spu: &[u8]) -> Option<u64> {
    let ctrl = u16::from_be_bytes([*spu.get(2)?, *spu.get(3)?]) as usize;
    let mut block = ctrl;
    for _ in 0..64 {
        let date = u16::from_be_bytes([*spu.get(block)?, *spu.get(block + 1)?]) as u64;
        let next = u16::from_be_bytes([*spu.get(block + 2)?, *spu.get(block + 3)?]) as usize;
        let mut p = block + 4;
        loop {
            match spu.get(p)? {
                0x00 | 0x01 => p += 1,
                0x02 => return Some(date * 1024 / 90),
                0x03 | 0x04 => p += 3,
                0x05 => p += 7,
                0x06 => p += 5,
                0xFF => break,
                _ => return None, // unknown command: stop guessing
            }
        }
        if next == block {
            return None; // last block, no stop command
        }
        block = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idx_parses_tracks_and_timestamps() {
        let idx = "# comment\nsize: 1920x1080\npalette: 000000, f0f0f0\n\
                   langidx: 0\nid: en, index: 0\n\
                   timestamp: 00:03:20:000, filepos: 000000000\n\
                   timestamp: 01:02:03:456, filepos: 00000b800\n\
                   id: nl, index: 1\n\
                   timestamp: 00:00:01:000, filepos: 000001000\n";
        let tracks = parse_idx(idx);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, 0);
        assert_eq!(tracks[0].language.as_deref(), Some("en"));
        assert_eq!(tracks[0].entries, vec![(200_000, 0), (3_723_456, 0xb800)]);
        assert_eq!(tracks[1].id, 1);
        assert_eq!(tracks[1].language.as_deref(), Some("nl"));
    }

    /// A hand-built two-pack PS stream: one SPU split across two packs,
    /// with an interleaved pack for another substream between them —
    /// the assembler must skip it and still complete the SPU.
    #[test]
    fn spu_reassembles_across_packs() {
        let spu_payload = {
            // Minimal valid SPU: size u16, ctrl offset u16, then a
            // control block at offset 4: date=90, next=self, cmds
            // [0x02 stop, 0xFF end].
            let body = [0u8, 90, 0, 4, 0x02, 0xFF];
            let total = (4 + body.len()) as u16;
            let mut s = total.to_be_bytes().to_vec();
            s.extend_from_slice(&[0, 4]); // control block at offset 4
            s.extend_from_slice(&body);
            s
        };
        // Split the SPU into two fragments over packs 0 and 2; pack 1
        // belongs to substream 0x21 and must be ignored.
        let frag_a = &spu_payload[..4];
        let frag_b = &spu_payload[4..];
        let mut sub = Vec::new();
        for (ss, frag) in [(0x20u8, frag_a), (0x21, &b"garbage"[..]), (0x20, frag_b)] {
            let mut pack = vec![0u8, 0, 1, 0xBA];
            pack.extend_from_slice(&[0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0xF8]); // no stuffing
            let mut pes = vec![0u8, 0, 1, 0xBD];
            let body_len = 3 + 1 + frag.len(); // flags+hdrlen, substream, frag
            pes.extend_from_slice(&(body_len as u16).to_be_bytes());
            pes.extend_from_slice(&[0x81, 0x80, 0x00]); // flags, no PTS
            pes.push(ss);
            pes.extend_from_slice(frag);
            pack.extend_from_slice(&pes);
            pack.resize(PACK, 0);
            sub.extend_from_slice(&pack);
        }
        let idx = "id: en, index: 0\ntimestamp: 00:00:05:000, filepos: 000000000\n";
        let blocks = extract_track(idx, &sub, 0).unwrap();
        assert_eq!(blocks.len(), 1);
        let (start, dur, spu) = &blocks[0];
        assert_eq!(*start, 5_000);
        assert_eq!(*spu, spu_payload);
        assert_eq!(
            *dur,
            Some(90 * 1024 / 90),
            "stop-display date must become duration"
        );
    }

    /// Manual, against a real pair:
    /// IDX=/path/movie.idx cargo test -p kahawai-media vobsub_pair \
    ///   -- --ignored --nocapture
    #[test]
    #[ignore]
    fn vobsub_pair_from_env() {
        let Ok(idx_path) = std::env::var("IDX") else {
            return;
        };
        let idx = std::fs::read_to_string(&idx_path).unwrap();
        let sub = std::fs::read(idx_path.replace(".idx", ".sub")).unwrap();
        for t in parse_idx(&idx) {
            let blocks = extract_track(&idx, &sub, t.id).unwrap();
            let with_dur = blocks.iter().filter(|(_, d, _)| d.is_some()).count();
            println!(
                "track {} ({:?}): {} entries -> {} SPUs ({} with duration)",
                t.id,
                t.language,
                t.entries.len(),
                blocks.len(),
                with_dur
            );
            let palette = crate::imagesubs::vobsub_palette(&idx);
            let decoded = blocks
                .iter()
                .take(50)
                .filter_map(|(_, _, spu)| {
                    crate::imagesubs::vobsub_decode(spu, &palette)
                        .ok()
                        .flatten()
                })
                .count();
            println!("  first 50: {decoded} decode to bitmaps");
        }
    }
}
