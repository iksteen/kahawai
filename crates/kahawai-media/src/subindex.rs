//! Sparse subtitle extraction (efficiency ladder step 2b): read only
//! what the container's own index makes reachable, without GStreamer.
//!
//! Access classes, best first:
//!  - **Exact** — MP4 sample tables always; MKV when the Cues index the
//!    subtitle track with relative positions. Reads = the subtitle
//!    payloads themselves.
//!  - **Header walk** (MKV) — hop cluster to cluster by declared sizes,
//!    read block headers, skip every non-subtitle payload by its
//!    length. Reads a few percent of the file, no full demux.
//!  - **None** — unsupported container / parse trouble: the caller
//!    falls back to the sequential GStreamer pass.
//!
//! Track indexing counts every subtitle track (image ones too) in
//! container order, matching discovery's `e{n}` keys.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::remux::RemuxSource;
use crate::subtitles::{Cue, Extracted, ass_dialogue, clean_cue_text, compose_header, decode_text};

/// Extract all text subtitle tracks via index-driven reads, or `None`
/// when this file's structure doesn't permit it (caller falls back).
pub fn extract_sparse(path: &Path) -> Result<Option<Vec<(usize, Extracted)>>> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 12];
    if file.read_exact(&mut magic).is_err() {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0))?;
    drop(file);
    let mut src = crate::remux::FileSource::open(path)?;
    if magic[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return mkv_extract(&mut src).map(Some).or_else(|e| {
            tracing::debug!(
                error = format!("{e:#}"),
                "mkv sparse parse failed; falling back"
            );
            Ok(None)
        });
    }
    if &magic[4..8] == b"ftyp" {
        return mp4_extract(&mut src).map(Some).or_else(|e| {
            tracing::debug!(
                error = format!("{e:#}"),
                "mp4 sparse parse failed; falling back"
            );
            Ok(None)
        });
    }
    Ok(None)
}

// ---------- shared low-level reader ----------

/// The parser reads only at offsets it computes, never sequentially,
/// so any random-access source will do: a local file for the
/// mediahost's extraction pass, and the session's own lease-backed
/// reader when a pipeline worker needs the same index (burn-in builds
/// its display-set timeline that way — see `extract_image_track`).
struct Reader<'a> {
    src: &'a mut dyn RemuxSource,
    len: u64,
    /// Walks are index-driven and finish in milliseconds on local
    /// disk, but every read is a round trip when the source is a
    /// session lease (measured: ~4 KB/s hub->mediahost->NAS, which is
    /// minutes for one film). Callers that sit in front of a live
    /// session give the walk a budget and degrade when it runs out.
    deadline: Option<std::time::Instant>,
    /// Readahead window: header walks issue thousands of tiny reads;
    /// over network filesystems each would be a round trip.
    win_start: u64,
    win: Vec<u8>,
}

const READAHEAD: usize = 256 * 1024;

impl<'a> Reader<'a> {
    fn new(src: &'a mut dyn RemuxSource) -> Self {
        let len = src.size();
        Self {
            src,
            len,
            win_start: 0,
            win: Vec::new(),
            deadline: None,
        }
    }

    fn with_budget(src: &'a mut dyn RemuxSource, budget: std::time::Duration) -> Self {
        let mut r = Self::new(src);
        r.deadline = Some(std::time::Instant::now() + budget);
        r
    }

    fn read_at(&mut self, off: u64, n: usize) -> Result<Vec<u8>> {
        if let Some(d) = self.deadline
            && std::time::Instant::now() > d
        {
            bail!("index walk exceeded its read budget");
        }
        let end = off + n as u64;
        let in_window = off >= self.win_start && end <= self.win_start + self.win.len() as u64;
        if !in_window {
            let want = n
                .max(READAHEAD)
                .min((self.len.saturating_sub(off)) as usize);
            if want < n {
                bail!("read past eof");
            }
            let mut buf = vec![0u8; want];
            let mut got = 0usize;
            while got < want {
                let k = self.src.read_at(off + got as u64, &mut buf[got..])?;
                if k == 0 {
                    bail!("short read at {off}");
                }
                got += k;
            }
            self.win_start = off;
            self.win = buf;
        }
        let s = (off - self.win_start) as usize;
        Ok(self.win[s..s + n].to_vec())
    }
}

// ---------- Matroska ----------

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CUE_RELATIVE_POSITION: u32 = 0xF0;
const ATTACHMENTS: u32 = 0x1941_A469;
const ATTACHED_FILE: u32 = 0x61A7;
const FILE_NAME: u32 = 0x466E;
const FILE_MIME: u32 = 0x4660;
const FILE_DATA: u32 = 0x465C;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CONTENT_ENCODING: u32 = 0x6240;
const CONTENT_COMPRESSION: u32 = 0x5034;
const CONTENT_COMP_ALGO: u32 = 0x4254;
const CONTENT_COMP_SETTINGS: u32 = 0x4255;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;

/// EBML element id (raw, marker bit kept) from a byte slice.
/// Returns (id, id_len).
fn ebml_id(buf: &[u8]) -> Result<(u32, usize)> {
    let first = *buf.first().context("eof at id")?;
    let len = (first.leading_zeros() as usize) + 1;
    if len > 4 || buf.len() < len {
        bail!("bad EBML id");
    }
    let mut id = 0u32;
    for b in &buf[..len] {
        id = (id << 8) | u32::from(*b);
    }
    Ok((id, len))
}

/// EBML size (marker masked). Returns (size, len); size None = unknown.
fn ebml_size(buf: &[u8]) -> Result<(Option<u64>, usize)> {
    let first = *buf.first().context("eof at size")?;
    if first == 0 {
        bail!("bad EBML size");
    }
    let len = (first.leading_zeros() as usize) + 1;
    if len > 8 || buf.len() < len {
        bail!("bad EBML size len");
    }
    let mask = 0xFFu8.checked_shr(len as u32).unwrap_or(0);
    let mut val = u64::from(first & mask);
    for b in &buf[1..len] {
        val = (val << 8) | u64::from(*b);
    }
    // All-ones payload = unknown size (streamed files).
    let all_ones = (1u64 << (7 * len)) - 1;
    Ok(((val != all_ones).then_some(val), len))
}

fn uint(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

/// Iterate child elements of `data`, calling `f(id, body)`; `f` returns
/// false to stop.
fn walk_children(data: &[u8], mut f: impl FnMut(u32, &[u8]) -> Result<bool>) -> Result<()> {
    let mut pos = 0usize;
    while pos < data.len() {
        let (id, il) = ebml_id(&data[pos..])?;
        let (size, sl) = ebml_size(&data[pos + il..])?;
        let size = size.context("unknown-size child")? as usize;
        let body_at = pos + il + sl;
        if body_at + size > data.len() {
            bail!("child overruns parent");
        }
        if !f(id, &data[body_at..body_at + size])? {
            return Ok(());
        }
        pos = body_at + size;
    }
    Ok(())
}

/// Matroska per-track content compression (ContentEncodings). Common
/// on subtitle tracks — this library's PGS is zlib — and applied to
/// every block payload before anything can parse it.
#[derive(Clone, PartialEq)]
enum Compression {
    Zlib,
    /// Header stripping: these bytes were removed from every payload.
    HeaderStrip(Vec<u8>),
}

struct MkvTrack {
    /// Container track number (block headers reference this).
    number: u64,
    /// Position among all subtitle tracks — the `e{n}` key index.
    sub_index: usize,
    /// Matroska CodecID: `S_TEXT/*`, `S_HDMV/PGS`, `S_VOBSUB`.
    codec: String,
    is_ass: bool,
    header: Option<String>,
    /// CodecPrivate — the VobSub .idx text (palette + display size).
    private: Option<Vec<u8>>,
    compression: Option<Compression>,
}

impl MkvTrack {
    fn is_text(&self) -> bool {
        self.codec.starts_with("S_TEXT/")
    }
}

struct MkvIndex {
    segment_start: u64,
    timestamp_scale: u64,
    tracks: Vec<MkvTrack>,
    /// (time_ticks, cluster_pos_rel, relative_pos, track) — subtitle cues.
    sub_cues: Vec<(u64, u64, Option<u64>, u64)>,
    /// First cluster position (absolute), for the header walk.
    first_cluster: Option<u64>,
}

/// ContentEncodings → the one compression we must undo. Anything
/// exotic (encryption, unknown algo) yields None and the track's
/// payloads are left alone — garbled is better than wrong-and-silent,
/// and the caller's decoder will simply find nothing.
fn read_compression(data: &[u8]) -> Option<Compression> {
    let mut out = None;
    walk_children(data, |id, v| {
        if id == CONTENT_ENCODING {
            walk_children(v, |id, v| {
                if id == CONTENT_COMPRESSION {
                    let (mut algo, mut settings) = (0u64, Vec::new());
                    walk_children(v, |id, v| {
                        match id {
                            CONTENT_COMP_ALGO => algo = uint(v),
                            CONTENT_COMP_SETTINGS => settings = v.to_vec(),
                            _ => {}
                        }
                        Ok(true)
                    })
                    .ok();
                    out = match algo {
                        0 => Some(Compression::Zlib),
                        3 => Some(Compression::HeaderStrip(settings)),
                        _ => None,
                    };
                }
                Ok(true)
            })
            .ok();
        }
        Ok(true)
    })
    .ok();
    out
}

fn decompress(payload: &[u8], how: Option<&Compression>) -> Vec<u8> {
    match how {
        Some(Compression::Zlib) => {
            use std::io::Read;
            let mut out = Vec::new();
            match flate2::read::ZlibDecoder::new(payload).read_to_end(&mut out) {
                Ok(_) => out,
                Err(e) => {
                    tracing::debug!(error = %e, "zlib block did not inflate");
                    payload.to_vec()
                }
            }
        }
        Some(Compression::HeaderStrip(prefix)) => {
            let mut out = Vec::with_capacity(prefix.len() + payload.len());
            out.extend_from_slice(prefix);
            out.extend_from_slice(payload);
            out
        }
        None => payload.to_vec(),
    }
}

fn mkv_read_index(r: &mut Reader) -> Result<MkvIndex> {
    // EBML header.
    let head = r.read_at(0, 32.min(r.len as usize))?;
    let (id, il) = ebml_id(&head)?;
    anyhow::ensure!(id == EBML_HEADER, "not matroska");
    let (hsize, hsl) = ebml_size(&head[il..])?;
    let pos = il as u64 + hsl as u64 + hsize.context("unknown EBML header size")?;

    // Segment.
    let seg = r.read_at(pos, 16)?;
    let (id, il) = ebml_id(&seg)?;
    anyhow::ensure!(id == SEGMENT, "no segment");
    let (seg_size, sl) = ebml_size(&seg[il..])?;
    let segment_start = pos + il as u64 + sl as u64;
    let segment_end = seg_size.map(|s| segment_start + s).unwrap_or(r.len);

    let mut idx = MkvIndex {
        segment_start,
        timestamp_scale: 1_000_000,
        tracks: Vec::new(),
        sub_cues: Vec::new(),
        first_cluster: None,
    };

    // Top-level walk. SeekHead lets us jump straight to Tracks/Cues even
    // when they sit behind the clusters; the walk itself hops clusters
    // by declared size, so a missing SeekHead costs only header reads.
    let mut pos = segment_start;
    let mut pending: Vec<u64> = Vec::new(); // absolute positions from SeekHead
    let mut visited = std::collections::HashSet::new();
    while pos < segment_end.min(r.len) {
        if !visited.insert(pos) {
            break;
        }
        let head = match r.read_at(pos, 16) {
            Ok(h) => h,
            Err(_) => break,
        };
        let Ok((id, il)) = ebml_id(&head) else { break };
        let Ok((size, sl)) = ebml_size(&head[il..]) else {
            break;
        };
        let body = pos + il as u64 + sl as u64;
        let Some(size) = size else { break };
        match id {
            SEEK_HEAD => {
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, seek| {
                    if id == SEEK {
                        let mut target = 0u32;
                        let mut position = None;
                        walk_children(seek, |id, v| {
                            match id {
                                SEEK_ID => target = uint(v) as u32,
                                SEEK_POSITION => position = Some(uint(v)),
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if matches!(target, TRACKS | CUES | INFO)
                            && let Some(p) = position
                        {
                            pending.push(segment_start + p);
                        }
                    }
                    Ok(true)
                })?;
            }
            INFO => {
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, v| {
                    if id == TIMESTAMP_SCALE {
                        // (fields are ordered; a plain assign is fine)
                    }
                    if id == TIMESTAMP_SCALE {
                        idx.timestamp_scale = uint(v).max(1);
                    }
                    Ok(true)
                })?;
            }
            TRACKS => {
                let data = r.read_at(body, size as usize)?;
                let mut sub_seen = 0usize;
                walk_children(&data, |id, entry| {
                    if id == TRACK_ENTRY {
                        let (mut number, mut ttype, mut codec, mut private) =
                            (0u64, 0u64, String::new(), None::<Vec<u8>>);
                        let mut compression = None;
                        walk_children(entry, |id, v| {
                            match id {
                                TRACK_NUMBER => number = uint(v),
                                TRACK_TYPE => ttype = uint(v),
                                CODEC_ID => codec = String::from_utf8_lossy(v).to_string(),
                                CODEC_PRIVATE => private = Some(v.to_vec()),
                                CONTENT_ENCODINGS => compression = read_compression(v),
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if ttype == 0x11 {
                            // Subtitle track: always consumes an e{n} slot.
                            // Image tracks are indexed too — burn-in reads
                            // their blocks through the same walk — and the
                            // block collection filters per caller so a text
                            // extraction never drags PGS payloads along.
                            let is_ass = codec.contains("ASS") || codec.contains("SSA");
                            idx.tracks.push(MkvTrack {
                                number,
                                sub_index: sub_seen,
                                codec: codec.clone(),
                                is_ass,
                                header: private.as_deref().filter(|_| is_ass).map(decode_text),
                                private: private.clone(),
                                compression: compression.clone(),
                            });
                            sub_seen += 1;
                        }
                    }
                    Ok(true)
                })?;
            }
            CUES => {
                let data = r.read_at(body, size as usize)?;
                let wanted: Vec<u64> = idx.tracks.iter().map(|t| t.number).collect();
                walk_children(&data, |id, point| {
                    if id == CUE_POINT {
                        let mut time = 0u64;
                        walk_children(point, |id, v| {
                            match id {
                                CUE_TIME => time = uint(v),
                                CUE_TRACK_POSITIONS => {
                                    let (mut track, mut cluster, mut rel) =
                                        (0u64, 0u64, None::<u64>);
                                    walk_children(v, |id, w| {
                                        match id {
                                            CUE_TRACK => track = uint(w),
                                            CUE_CLUSTER_POSITION => cluster = uint(w),
                                            CUE_RELATIVE_POSITION => rel = Some(uint(w)),
                                            _ => {}
                                        }
                                        Ok(true)
                                    })?;
                                    if wanted.contains(&track) {
                                        idx.sub_cues.push((time, cluster, rel, track));
                                    }
                                }
                                _ => {}
                            }
                            Ok(true)
                        })?;
                    }
                    Ok(true)
                })?;
            }
            CLUSTER if idx.first_cluster.is_none() => {
                idx.first_cluster = Some(pos);
            }
            // Tracks/Cues may still be ahead (SeekHead pending covers
            // the usual layouts); keep hopping — header reads only.
            _ => {}
        }
        // Jump to any SeekHead-promised sections we haven't visited,
        // otherwise continue linearly.
        pos = body + size;
        if id == CLUSTER
            && !idx.tracks.is_empty()
            && (idx.first_cluster.is_some())
            && !idx.sub_cues.is_empty()
        {
            // Have everything an Exact pass needs.
            if pending.iter().all(|p| visited.contains(p)) {
                break;
            }
        }
        if let Some(next) = pending.iter().find(|p| !visited.contains(*p)) {
            // Prefer promised sections over linear scanning when the
            // linear position has entered cluster territory.
            if idx.first_cluster.is_some() {
                pos = *next;
            }
        }
    }
    Ok(idx)
}

/// MH-4: declare embedded attachments (name, mime, payload byte range)
/// without ever reading a payload. Header reads only; jumps straight to
/// the Attachments element when the SeekHead promises one, otherwise
/// hops top-level elements by declared size. Non-matroska → empty.
pub fn declare_attachments(path: &Path) -> Result<Vec<kahawai_core::media::Attachment>> {
    let mut src = crate::remux::FileSource::open(path)?;
    let len = src.size();
    let mut r = Reader::new(&mut src);

    let head = r.read_at(0, 32.min(len as usize))?;
    let Ok((id, il)) = ebml_id(&head) else {
        return Ok(Vec::new());
    };
    if id != EBML_HEADER {
        return Ok(Vec::new());
    }
    let (hsize, hsl) = ebml_size(&head[il..])?;
    let pos = il as u64 + hsl as u64 + hsize.context("unknown EBML header size")?;
    let seg = r.read_at(pos, 16)?;
    let (id, il) = ebml_id(&seg)?;
    anyhow::ensure!(id == SEGMENT, "no segment");
    let (seg_size, sl) = ebml_size(&seg[il..])?;
    let segment_start = pos + il as u64 + sl as u64;
    let segment_end = seg_size.map(|s| segment_start + s).unwrap_or(r.len);

    let mut pos = segment_start;
    let mut pending: Vec<u64> = Vec::new();
    let mut saw_seekhead = false;
    let mut visited = std::collections::HashSet::new();
    while pos < segment_end.min(r.len) {
        if !visited.insert(pos) {
            break;
        }
        let Ok(head) = r.read_at(pos, 16) else { break };
        let Ok((id, il)) = ebml_id(&head) else { break };
        let Ok((size, sl)) = ebml_size(&head[il..]) else {
            break;
        };
        let body = pos + il as u64 + sl as u64;
        let Some(size) = size else { break };
        match id {
            SEEK_HEAD => {
                saw_seekhead = true;
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, seek| {
                    if id == SEEK {
                        let (mut target, mut position) = (0u32, None);
                        walk_children(seek, |id, v| {
                            match id {
                                SEEK_ID => target = uint(v) as u32,
                                SEEK_POSITION => position = Some(uint(v)),
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if target == ATTACHMENTS
                            && let Some(p) = position
                        {
                            pending.push(segment_start + p);
                        }
                    }
                    Ok(true)
                })?;
            }
            ATTACHMENTS => return read_attached_files(&mut r, body, size),
            // Trust a present SeekHead: it indexes the top-level
            // elements, so no Attachments entry by the time clusters
            // start means there are none — skip the (long) cluster-hop
            // walk. Partial SeekHeads lose the declaration and fall
            // back to the gst rung, which is acceptable.
            CLUSTER if saw_seekhead && pending.is_empty() => break,
            _ => {}
        }
        pos = body + size;
        if let Some(next) = pending.iter().find(|p| !visited.contains(*p)) {
            pos = *next;
        }
    }
    Ok(Vec::new())
}

/// Sparse walk of an Attachments element: child headers are read,
/// FileData payloads are skipped by declared size.
fn read_attached_files(
    r: &mut Reader,
    body: u64,
    size: u64,
) -> Result<Vec<kahawai_core::media::Attachment>> {
    let mut out = Vec::new();
    let end = body + size;
    let mut pos = body;
    while pos < end {
        let head = r.read_at(pos, 16)?;
        let (id, il) = ebml_id(&head)?;
        let (esize, sl) = ebml_size(&head[il..])?;
        let ebody = pos + il as u64 + sl as u64;
        let esize = esize.context("unsized element in Attachments")?;
        if id == ATTACHED_FILE {
            let (mut name, mut mime) = (String::new(), String::new());
            let (mut off, mut dlen) = (0u64, 0u64);
            let cend = ebody + esize;
            let mut cpos = ebody;
            while cpos < cend {
                let h = r.read_at(cpos, 16)?;
                let (cid, cil) = ebml_id(&h)?;
                let (csize, csl) = ebml_size(&h[cil..])?;
                let cbody = cpos + cil as u64 + csl as u64;
                let csize = csize.context("unsized element in AttachedFile")?;
                match cid {
                    FILE_NAME => {
                        name = String::from_utf8_lossy(&r.read_at(cbody, csize as usize)?)
                            .into_owned();
                    }
                    FILE_MIME => {
                        mime = String::from_utf8_lossy(&r.read_at(cbody, csize as usize)?)
                            .into_owned();
                    }
                    FILE_DATA => {
                        (off, dlen) = (cbody, csize); // declared, never read
                    }
                    _ => {}
                }
                cpos = cbody + csize;
            }
            if dlen > 0 {
                out.push(kahawai_core::media::Attachment {
                    file_name: name,
                    mime_type: mime,
                    offset: off,
                    size: dlen,
                });
            }
        }
        pos = ebody + esize;
    }
    Ok(out)
}

/// A parsed subtitle block: (track number, time ticks rel cluster, payload, duration ticks).
struct SubBlock {
    track: u64,
    rel_time: i16,
    payload: Vec<u8>,
    duration: Option<u64>,
}

/// Parse a Block/SimpleBlock payload header; None when laced (subtitle
/// tracks never lace in practice) or not a wanted track.
fn parse_block(data: &[u8], wanted: &[u64]) -> Option<(u64, i16, usize)> {
    let (track, tl) = {
        let first = *data.first()?;
        let len = (first.leading_zeros() as usize) + 1;
        if len > 8 || data.len() < len {
            return None;
        }
        let mask = 0xFFu8.checked_shr(len as u32).unwrap_or(0);
        let mut val = u64::from(first & mask);
        for b in &data[1..len] {
            val = (val << 8) | u64::from(*b);
        }
        (val, len)
    };
    if !wanted.contains(&track) || data.len() < tl + 3 {
        return None;
    }
    let rel = i16::from_be_bytes([data[tl], data[tl + 1]]);
    let flags = data[tl + 2];
    if flags & 0x06 != 0 {
        return None; // laced
    }
    Some((track, rel, tl + 3))
}

/// Track number claimed by a block payload prefix (peek-sized slice).
fn block_track(data: &[u8]) -> Option<u64> {
    let first = *data.first()?;
    let len = (first.leading_zeros() as usize) + 1;
    if len > 8 || data.len() < len {
        return None;
    }
    let mask = 0xFFu8.checked_shr(len as u32).unwrap_or(0);
    let mut val = u64::from(first & mask);
    for b in &data[1..len] {
        val = (val << 8) | u64::from(*b);
    }
    Some(val)
}

/// Header-walk one cluster WITHOUT reading its payloads: element
/// headers are peeked, non-subtitle payloads skipped by declared size.
/// Only blocks of wanted tracks are actually read.
fn walk_cluster_sparse(
    r: &mut Reader,
    body_start: u64,
    body_len: u64,
    wanted: &[u64],
    out: &mut Vec<(u64, SubBlock)>,
) -> Result<()> {
    let mut ts = 0u64;
    let mut p = body_start;
    let end = body_start + body_len;
    while p < end {
        let peek_n = 48usize.min((end - p) as usize).min((r.len - p) as usize);
        if peek_n < 2 {
            break;
        }
        let head = r.read_at(p, peek_n)?;
        let Ok((id, il)) = ebml_id(&head) else { break };
        let Ok((size, sl)) = ebml_size(&head[il..]) else {
            break;
        };
        let Some(size) = size else { break };
        let body = p + (il + sl) as u64;
        match id {
            CLUSTER_TIMESTAMP => {
                if let Some(v) = head.get(il + sl..il + sl + size as usize) {
                    ts = uint(v);
                } else {
                    ts = uint(&r.read_at(body, size as usize)?);
                }
            }
            SIMPLE_BLOCK => {
                let prefix = &head[(il + sl).min(head.len())..];
                let is_ours = block_track(prefix)
                    .map(|t| wanted.contains(&t))
                    .unwrap_or(true); // can't tell from peek: read it
                if is_ours {
                    let data = r.read_at(body, size as usize)?;
                    if let Some((track, rel, off)) = parse_block(&data, wanted) {
                        out.push((
                            ts,
                            SubBlock {
                                track,
                                rel_time: rel,
                                payload: data[off..].to_vec(),
                                duration: None,
                            },
                        ));
                    }
                }
            }
            BLOCK_GROUP => {
                // Peek the first child (Block) and its payload prefix.
                let inner = &head[(il + sl).min(head.len())..];
                let ours = (|| -> Option<bool> {
                    let (cid, cil) = ebml_id(inner).ok()?;
                    if cid != BLOCK {
                        return None; // unusual layout: read to be safe
                    }
                    let (_, csl) = ebml_size(&inner[cil..]).ok()?;
                    let t = block_track(inner.get(cil + csl..)?)?;
                    Some(wanted.contains(&t))
                })()
                .unwrap_or(true);
                if ours {
                    let data = r.read_at(body, size as usize)?;
                    let synthetic = encode_element(BLOCK_GROUP, &data);
                    let mut tmp = Vec::new();
                    let frame = [
                        encode_element(CLUSTER_TIMESTAMP, &encode_uint(ts)),
                        synthetic,
                    ]
                    .concat();
                    scan_cluster(&frame, wanted, &mut tmp)?;
                    out.extend(tmp);
                }
            }
            _ => {}
        }
        p = body + size;
    }
    Ok(())
}

/// Walk one cluster's children collecting wanted subtitle blocks.
fn scan_cluster(data: &[u8], wanted: &[u64], out: &mut Vec<(u64, SubBlock)>) -> Result<()> {
    let mut cluster_ts = 0u64;
    walk_children(data, |id, body| {
        match id {
            CLUSTER_TIMESTAMP => cluster_ts = uint(body),
            SIMPLE_BLOCK => {
                if let Some((track, rel, off)) = parse_block(body, wanted) {
                    out.push((
                        cluster_ts,
                        SubBlock {
                            track,
                            rel_time: rel,
                            payload: body[off..].to_vec(),
                            duration: None,
                        },
                    ));
                }
            }
            BLOCK_GROUP => {
                let mut blk: Option<SubBlock> = None;
                let mut duration = None;
                walk_children(body, |id, v| {
                    match id {
                        BLOCK => {
                            if let Some((track, rel, off)) = parse_block(v, wanted) {
                                blk = Some(SubBlock {
                                    track,
                                    rel_time: rel,
                                    payload: v[off..].to_vec(),
                                    duration: None,
                                });
                            }
                        }
                        BLOCK_DURATION => duration = Some(uint(v)),
                        _ => {}
                    }
                    Ok(true)
                })?;
                if let Some(mut b) = blk {
                    b.duration = duration;
                    out.push((cluster_ts, b));
                }
            }
            _ => {}
        }
        Ok(true)
    })
}

/// One image subtitle track's raw display-set blocks, in file order.
/// The caller decodes them (`imagesubs`) — this layer only finds the
/// bytes without demuxing the whole file.
pub struct ImageTrack {
    /// Matroska CodecID (`S_HDMV/PGS`, `S_VOBSUB`).
    pub codec: String,
    /// CodecPrivate: the VobSub `.idx` text (palette, display size).
    pub codec_private: Option<Vec<u8>>,
    /// (presentation ms, declared duration ms, payload) per block.
    /// VobSub carries its own lifetime; PGS ends at the next set.
    pub blocks: Vec<(u64, Option<u64>, Vec<u8>)>,
}

/// Index-driven read of ONE image subtitle track (`e{sub_index}`).
/// `None` when the container or its index doesn't permit the sparse
/// path, or that track is not an image track — callers then have no
/// timeline and skip burn-in rather than guessing.
pub fn extract_image_track(
    src: &mut dyn RemuxSource,
    sub_index: usize,
    budget: std::time::Duration,
) -> Result<Option<ImageTrack>> {
    let mut r = Reader::with_budget(src, budget);
    let magic = r.read_at(0, 4)?;
    if magic != [0x1A, 0x45, 0xDF, 0xA3] {
        return Ok(None); // mp4 image subtitles do not occur in practice
    }
    match mkv_walk(r, Some(sub_index)) {
        Ok(out) => Ok(out.image),
        Err(e) => {
            tracing::debug!(error = format!("{e:#}"), "mkv sparse image walk failed");
            Ok(None)
        }
    }
}

struct MkvOut {
    text: Vec<(usize, Extracted)>,
    image: Option<ImageTrack>,
}

fn mkv_extract(src: &mut dyn RemuxSource) -> Result<Vec<(usize, Extracted)>> {
    Ok(mkv_walk(Reader::new(src), None)?.text)
}

/// The shared walk. `image = Some(sub_index)` collects that one image
/// track's blocks; `None` collects every text track's.
fn mkv_walk(mut r: Reader<'_>, image: Option<usize>) -> Result<MkvOut> {
    let idx = mkv_read_index(&mut r)?;
    if idx.tracks.is_empty() {
        return Ok(MkvOut {
            text: Vec::new(),
            image: None,
        });
    }
    // Collect blocks only for the tracks this caller will assemble: a
    // text extraction must not read (or hold) a film's worth of PGS.
    let wanted: Vec<u64> = match image {
        Some(want) => idx
            .tracks
            .iter()
            .filter(|t| t.sub_index == want && !t.is_text())
            .map(|t| t.number)
            .collect(),
        None => idx
            .tracks
            .iter()
            .filter(|t| t.is_text())
            .map(|t| t.number)
            .collect(),
    };
    if wanted.is_empty() {
        return Ok(MkvOut {
            text: Vec::new(),
            image: None,
        });
    }
    let mut blocks: Vec<(u64, SubBlock)> = Vec::new();

    let exact = !idx.sub_cues.is_empty() && idx.sub_cues.iter().all(|(_, _, rel, _)| rel.is_some());
    if exact {
        // Exact: every subtitle block is cue-addressed. Read each
        // cluster's timestamp once, then just the BlockGroups.
        tracing::debug!(cues = idx.sub_cues.len(), "sparse mkv: exact cue reads");
        let mut cluster_ts_cache: std::collections::HashMap<u64, u64> = Default::default();
        for (_, cluster_rel, rel, _) in &idx.sub_cues {
            let cluster_pos = idx.segment_start + cluster_rel;
            let ts = match cluster_ts_cache.get(&cluster_pos) {
                Some(t) => *t,
                None => {
                    let head = r.read_at(cluster_pos, 64)?;
                    let (id, il) = ebml_id(&head)?;
                    anyhow::ensure!(id == CLUSTER, "cue does not point at a cluster");
                    let (_, sl) = ebml_size(&head[il..])?;
                    // First child is Timestamp in every real muxer.
                    let mut ts = 0u64;
                    walk_children(&head[il + sl..il + sl + 16], |id, v| {
                        if id == CLUSTER_TIMESTAMP {
                            ts = uint(v);
                            return Ok(false);
                        }
                        Ok(true)
                    })
                    .ok();
                    cluster_ts_cache.insert(cluster_pos, ts);
                    ts
                }
            };
            // CueRelativePosition is relative to the cluster's first
            // child (i.e. past the cluster header).
            let head = r.read_at(cluster_pos, 16)?;
            let (_, il) = ebml_id(&head)?;
            let (_, sl) = ebml_size(&head[il..])?;
            let block_at = cluster_pos + il as u64 + sl as u64 + rel.unwrap();
            let bh = r.read_at(block_at, 16)?;
            let (bid, bil) = ebml_id(&bh)?;
            let (bsize, bsl) = ebml_size(&bh[bil..])?;
            let bsize = bsize.context("unknown block size")? as usize;
            let body = r.read_at(block_at + (bil + bsl) as u64, bsize)?;
            let mut tmp = Vec::new();
            match bid {
                BLOCK_GROUP => {
                    let synthetic = [
                        // re-wrap as a cluster fragment: timestamp + group
                        &encode_element(CLUSTER_TIMESTAMP, &encode_uint(ts))[..],
                        &encode_element(BLOCK_GROUP, &body)[..],
                    ]
                    .concat();
                    scan_cluster(&synthetic, &wanted, &mut tmp)?;
                }
                SIMPLE_BLOCK => {
                    let synthetic = [
                        &encode_element(CLUSTER_TIMESTAMP, &encode_uint(ts))[..],
                        &encode_element(SIMPLE_BLOCK, &body)[..],
                    ]
                    .concat();
                    scan_cluster(&synthetic, &wanted, &mut tmp)?;
                }
                _ => {}
            }
            blocks.extend(tmp);
        }
    } else {
        // Header walk: hop every cluster, skip non-subtitle payloads.
        let Some(mut pos) = idx.first_cluster else {
            return Ok(MkvOut {
                text: Vec::new(),
                image: None,
            });
        };
        tracing::debug!("sparse mkv: cluster header walk");
        while pos < r.len {
            let head = match r.read_at(pos, 16.min((r.len - pos) as usize)) {
                Ok(h) => h,
                Err(_) => break,
            };
            let Ok((id, il)) = ebml_id(&head) else { break };
            let Ok((size, sl)) = ebml_size(&head[il..]) else {
                break;
            };
            let Some(size) = size else { break };
            if id == CLUSTER {
                walk_cluster_sparse(&mut r, pos + (il + sl) as u64, size, &wanted, &mut blocks)?;
            }
            pos += (il + sl) as u64 + size;
        }
    }

    // Assemble per track.
    let scale = idx.timestamp_scale;
    let to_ms = |ticks: u64| ticks.saturating_mul(scale) / 1_000_000;

    // Image track: hand back the raw display-set blocks, timed.
    if let Some(want) = image {
        let Some(t) = idx
            .tracks
            .iter()
            .find(|t| t.sub_index == want && !t.is_text())
        else {
            return Ok(MkvOut {
                text: Vec::new(),
                image: None,
            });
        };
        let mut out: Vec<(u64, Option<u64>, Vec<u8>)> = blocks
            .iter()
            .filter(|(_, b)| b.track == t.number)
            .map(|(cluster_ts, b)| {
                let ticks = cluster_ts.saturating_add_signed(i64::from(b.rel_time));
                (
                    to_ms(ticks),
                    b.duration.map(&to_ms),
                    decompress(&b.payload, t.compression.as_ref()),
                )
            })
            .collect();
        out.sort_by_key(|(ms, _, _)| *ms);
        return Ok(MkvOut {
            text: Vec::new(),
            image: Some(ImageTrack {
                codec: t.codec.clone(),
                codec_private: t.private.clone(),
                blocks: out,
            }),
        });
    }

    let mut out = Vec::new();
    for t in idx.tracks.iter().filter(|t| t.is_text()) {
        let mut cues: Vec<Cue> = Vec::new();
        let mut raw_events: Vec<(u64, u64, String)> = Vec::new();
        for (cluster_ts, b) in blocks.iter().filter(|(_, b)| b.track == t.number) {
            let start_ticks = cluster_ts.saturating_add_signed(i64::from(b.rel_time));
            let start = to_ms(start_ticks);
            let end = b
                .duration
                .map(|d| to_ms(start_ticks + d))
                .unwrap_or(start + 3000);
            let raw = decode_text(&decompress(&b.payload, t.compression.as_ref()));
            let text = if t.is_ass {
                raw_events.push((start, end, raw.clone()));
                clean_cue_text(raw.splitn(9, ',').last().unwrap_or(""))
            } else {
                clean_cue_text(&raw)
            };
            if !text.is_empty() {
                cues.push(Cue {
                    start_ms: start,
                    end_ms: end,
                    text,
                });
            }
        }
        cues.sort_by_key(|c| c.start_ms);
        let ass = t
            .header
            .clone()
            .filter(|_| !raw_events.is_empty())
            .map(|h| {
                let mut s = compose_header(&h);
                raw_events.sort_by_key(|(a, _, _)| *a);
                for (st, en, raw) in &raw_events {
                    if let Some(line) = ass_dialogue(raw, *st, *en) {
                        s.push_str(&line);
                        s.push('\n');
                    }
                }
                s
            });
        out.push((t.sub_index, Extracted { cues, ass }));
    }
    Ok(MkvOut {
        text: out,
        image: None,
    })
}

// EBML writers (Exact-path rewrapping + tests).
fn encode_uint(v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
    bytes[skip..].to_vec()
}

fn encode_size(n: u64) -> Vec<u8> {
    // 8-byte form: unambiguous for any realistic size.
    let mut out = vec![0x01];
    out.extend_from_slice(&n.to_be_bytes()[1..]);
    out
}

fn encode_element(id: u32, body: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().take_while(|b| **b == 0).count();
    let mut out = id_bytes[skip..].to_vec();
    out.extend(encode_size(body.len() as u64));
    out.extend_from_slice(body);
    out
}

// ---------- MP4 ----------

fn mp4_extract(src: &mut dyn RemuxSource) -> Result<Vec<(usize, Extracted)>> {
    let mut r = Reader::new(src);
    // Find moov at top level.
    let mut pos = 0u64;
    let mut moov: Option<Vec<u8>> = None;
    while pos + 8 <= r.len {
        let head = r.read_at(pos, 16)?;
        let size32 = u32::from_be_bytes(head[0..4].try_into().unwrap()) as u64;
        let kind = &head[4..8];
        let (size, hdr) = if size32 == 1 {
            (u64::from_be_bytes(head[8..16].try_into().unwrap()), 16u64)
        } else if size32 == 0 {
            (r.len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        if size < hdr {
            bail!("bad box size");
        }
        if kind == b"moov" {
            moov = Some(r.read_at(pos + hdr, (size - hdr) as usize)?);
            break;
        }
        pos += size;
    }
    let moov = moov.context("no moov box")?;

    let mut out = Vec::new();
    let mut sub_seen = 0usize;
    walk_boxes(&moov, |kind, body| {
        if kind != *b"trak" {
            return Ok(());
        }
        let Some(mdia) = find_box(body, b"mdia") else {
            return Ok(());
        };
        let Some(hdlr) = find_box(mdia, b"hdlr") else {
            return Ok(());
        };
        if hdlr.len() < 12 {
            return Ok(());
        }
        let handler = &hdlr[8..12];
        // Subtitle-ish handlers: 3GPP text, Apple subtitles, generic subt.
        if !matches!(handler, b"text" | b"sbtl" | b"subt") {
            return Ok(());
        }
        let idx = sub_seen;
        sub_seen += 1;

        let Some(mdhd) = find_box(mdia, b"mdhd") else {
            return Ok(());
        };
        let timescale = if mdhd[0] == 1 {
            u32::from_be_bytes(mdhd[20..24].try_into().unwrap())
        } else {
            u32::from_be_bytes(mdhd[12..16].try_into().unwrap())
        } as u64;
        let Some(minf) = find_box(mdia, b"minf") else {
            return Ok(());
        };
        let Some(stbl) = find_box(minf, b"stbl") else {
            return Ok(());
        };

        // Sample format: only plain text codecs here.
        let format = find_box(stbl, b"stsd")
            .and_then(|s| s.get(12..16))
            .map(|f| f.to_vec())
            .unwrap_or_default();
        if !matches!(&format[..], b"tx3g" | b"text") {
            return Ok(());
        }

        let Some(samples) = mp4_sample_table(stbl, timescale) else {
            return Ok(());
        };
        let mut cues = Vec::new();
        for (off, size, start_ms, dur_ms) in samples {
            if !(2..=1 << 20).contains(&size) {
                continue;
            }
            let data = match r.read_at(off, size as usize) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // tx3g/text: 2-byte big-endian text length, then UTF-8.
            let tlen = u16::from_be_bytes([data[0], data[1]]) as usize;
            if tlen == 0 || data.len() < 2 + tlen {
                continue;
            }
            let text = clean_cue_text(&decode_text(&data[2..2 + tlen]));
            if !text.is_empty() {
                cues.push(Cue {
                    start_ms,
                    end_ms: start_ms + dur_ms.max(1),
                    text,
                });
            }
        }
        cues.sort_by_key(|c| c.start_ms);
        out.push((idx, Extracted { cues, ass: None }));
        Ok(())
    })?;
    Ok(out)
}

/// (offset, size, start_ms, duration_ms) for every sample of the track.
fn mp4_sample_table(stbl: &[u8], timescale: u64) -> Option<Vec<(u64, u64, u64, u64)>> {
    let stts = find_box(stbl, b"stts")?;
    let stsz = find_box(stbl, b"stsz")?;
    let stsc = find_box(stbl, b"stsc")?;
    let (co64, chunk_offsets): (bool, &[u8]) = match find_box(stbl, b"co64") {
        Some(b) => (true, b),
        None => (false, find_box(stbl, b"stco")?),
    };

    // Sizes.
    let uniform = u32::from_be_bytes(stsz.get(4..8)?.try_into().ok()?) as u64;
    let count = u32::from_be_bytes(stsz.get(8..12)?.try_into().ok()?) as usize;
    let size_of = |i: usize| -> Option<u64> {
        if uniform != 0 {
            Some(uniform)
        } else {
            stsz.get(12 + i * 4..16 + i * 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as u64)
        }
    };

    // Times.
    let mut times = Vec::with_capacity(count);
    {
        let n = u32::from_be_bytes(stts.get(4..8)?.try_into().ok()?) as usize;
        let mut t = 0u64;
        for e in 0..n {
            let base = 8 + e * 8;
            let cnt = u32::from_be_bytes(stts.get(base..base + 4)?.try_into().ok()?) as u64;
            let delta = u32::from_be_bytes(stts.get(base + 4..base + 8)?.try_into().ok()?) as u64;
            for _ in 0..cnt {
                times.push((t * 1000 / timescale.max(1), delta * 1000 / timescale.max(1)));
                t += delta;
            }
        }
    }

    // Chunk map.
    let nchunk_entries = u32::from_be_bytes(stsc.get(4..8)?.try_into().ok()?) as usize;
    let stsc_entry = |i: usize| -> Option<(u64, u64)> {
        let base = 8 + i * 12;
        Some((
            u32::from_be_bytes(stsc.get(base..base + 4)?.try_into().ok()?) as u64,
            u32::from_be_bytes(stsc.get(base + 4..base + 8)?.try_into().ok()?) as u64,
        ))
    };
    let nchunks = u32::from_be_bytes(chunk_offsets.get(4..8)?.try_into().ok()?) as usize;
    let chunk_off = |i: usize| -> Option<u64> {
        if co64 {
            chunk_offsets
                .get(8 + i * 8..16 + i * 8)
                .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        } else {
            chunk_offsets
                .get(8 + i * 4..12 + i * 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as u64)
        }
    };

    let mut result = Vec::with_capacity(count);
    let mut sample = 0usize;
    let mut entry = 0usize;
    for chunk in 0..nchunks {
        while entry + 1 < nchunk_entries
            && stsc_entry(entry + 1)
                .map(|(first, _)| first <= chunk as u64 + 1)
                .unwrap_or(false)
        {
            entry += 1;
        }
        let per_chunk = stsc_entry(entry)?.1 as usize;
        let mut off = chunk_off(chunk)?;
        for _ in 0..per_chunk {
            if sample >= count || sample >= times.len() {
                break;
            }
            let size = size_of(sample)?;
            let (start, dur) = times[sample];
            result.push((off, size, start, dur));
            off += size;
            sample += 1;
        }
    }
    Some(result)
}

fn walk_boxes(data: &[u8], mut f: impl FnMut([u8; 4], &[u8]) -> Result<()>) -> Result<()> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let (size, hdr) = if size32 == 1 {
            if pos + 16 > data.len() {
                break;
            }
            (
                u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize,
                16,
            )
        } else if size32 == 0 {
            (data.len() - pos, 8)
        } else {
            (size32, 8)
        };
        if size < hdr || pos + size > data.len() {
            break;
        }
        f(kind, &data[pos + hdr..pos + size])?;
        pos += size;
    }
    Ok(())
}

fn find_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut found = None;
    let _ = walk_boxes(data, |k, body| {
        if &k == kind && found.is_none() {
            // Safety: body borrows from data; extend lifetime via indices.
            let start = body.as_ptr() as usize - data.as_ptr() as usize;
            found = Some(&data[start..start + body.len()]);
        }
        Ok(())
    });
    found
}

#[cfg(test)]
mod tests {
    /// Manual: IMGSUB_SRC=/path/to/file.mkv cargo test -p kahawai-media \
    ///   image_track_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn image_track_from_env() {
        let Ok(path) = std::env::var("IMGSUB_SRC") else {
            return;
        };
        let idx: usize = std::env::var("IMGSUB_IDX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut src = crate::remux::FileSource::open(std::path::Path::new(&path)).unwrap();
        let t0 = std::time::Instant::now();
        let track = super::extract_image_track(&mut src, idx, std::time::Duration::from_secs(60))
            .unwrap()
            .expect("no image track");
        let bytes: usize = track.blocks.iter().map(|(_, _, b)| b.len()).sum();
        println!(
            "codec {} · {} blocks · {} KiB · {:.2}s",
            track.codec,
            track.blocks.len(),
            bytes / 1024,
            t0.elapsed().as_secs_f64()
        );
        // Decode them the way burn-in will, and report the timeline.
        let mut dec = crate::imagesubs::PgsDecoder::default();
        let mut sets = 0usize;
        let mut first = None;
        for (ms, _dur, payload) in &track.blocks {
            if let Ok(Some(set)) = dec.feed(payload) {
                sets += 1;
                if first.is_none() && !set.objects.is_empty() {
                    let o = &set.objects[0];
                    first = Some((
                        *ms,
                        set.canvas_w,
                        set.canvas_h,
                        format!("rect {}x{}+{}+{}", o.w, o.h, o.x, o.y),
                    ));
                }
            }
        }
        println!("decoded display sets: {sets} · first with objects: {first:?}");
    }

    use super::*;
    use std::io::Write;

    fn ebml(id: u32, body: &[u8]) -> Vec<u8> {
        encode_element(id, body)
    }

    /// A minimal in-memory MKV: header, segment(info, tracks(1 ass sub
    /// track), cluster(ts + one BlockGroup dialogue), optional cues).
    fn tiny_mkv(with_sub_cues: bool) -> Vec<u8> {
        let info = ebml(
            INFO,
            &ebml(TIMESTAMP_SCALE, &1_000_000u64.to_be_bytes()[5..]),
        );
        let track_entry = [
            ebml(TRACK_NUMBER, &[3]),
            ebml(TRACK_TYPE, &[0x11]),
            ebml(CODEC_ID, b"S_TEXT/ASS"),
            ebml(
                CODEC_PRIVATE,
                b"[Script Info]\nTitle: t\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text",
            ),
        ]
        .concat();
        let tracks = ebml(TRACKS, &ebml(TRACK_ENTRY, &track_entry));

        // Block: track 3 (varint 0x83), rel time 500, flags 0, ass payload.
        let mut block = vec![0x83, 0x01, 0xF4, 0x00];
        block.extend_from_slice(b"1,0,Default,,0,0,0,,Hello sparse");
        let group = ebml(
            BLOCK_GROUP,
            &[ebml(BLOCK, &block), ebml(BLOCK_DURATION, &[0x07, 0xD0])].concat(),
        );
        let cluster_body = [ebml(CLUSTER_TIMESTAMP, &[0x27, 0x10]), group.clone()].concat();
        let cluster = ebml(CLUSTER, &cluster_body);

        // Segment body layout: info, tracks, cluster, cues.
        let mut segment_body = Vec::new();
        segment_body.extend_from_slice(&info);
        segment_body.extend_from_slice(&tracks);
        let cluster_pos_rel = segment_body.len() as u64;
        segment_body.extend_from_slice(&cluster);
        if with_sub_cues {
            // Relative position of the BlockGroup within the cluster body:
            // past the Timestamp element.
            let rel = ebml(CLUSTER_TIMESTAMP, &[0x27, 0x10]).len() as u64;
            let positions = [
                ebml(CUE_TRACK, &[3]),
                ebml(CUE_CLUSTER_POSITION, &encode_uint(cluster_pos_rel)),
                ebml(CUE_RELATIVE_POSITION, &encode_uint(rel)),
            ]
            .concat();
            let point = [
                ebml(CUE_TIME, &[0x29, 0xF4]),
                ebml(CUE_TRACK_POSITIONS, &positions),
            ]
            .concat();
            segment_body.extend_from_slice(&ebml(CUES, &ebml(CUE_POINT, &point)));
        }

        let mut out = ebml(EBML_HEADER, &[]);
        out.extend(ebml(SEGMENT, &segment_body));
        out
    }

    fn extract_from_bytes(bytes: &[u8]) -> Vec<(usize, Extracted)> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        extract_sparse(f.path()).unwrap().unwrap()
    }

    #[test]
    fn mkv_header_walk_extracts_ass() {
        let tracks = extract_from_bytes(&tiny_mkv(false));
        assert_eq!(tracks.len(), 1);
        let (idx, ex) = &tracks[0];
        assert_eq!(*idx, 0);
        assert_eq!(ex.cues.len(), 1);
        // cluster ts 10000 + rel 500 = 10500ms, duration 2000ms
        assert_eq!(ex.cues[0].start_ms, 10500);
        assert_eq!(ex.cues[0].end_ms, 12500);
        assert_eq!(ex.cues[0].text, "Hello sparse");
        let ass = ex.ass.as_ref().unwrap();
        assert!(ass.contains("Dialogue: 0,0:00:10.50,0:00:12.50,Default,,0,0,0,,Hello sparse"));
    }

    #[test]
    fn mkv_exact_cue_reads_match_header_walk() {
        let coarse = extract_from_bytes(&tiny_mkv(false));
        let exact = extract_from_bytes(&tiny_mkv(true));
        assert_eq!(coarse[0].1.cues, exact[0].1.cues);
        assert_eq!(coarse[0].1.ass, exact[0].1.ass);
    }

    #[test]
    fn attachments_declared_without_payload_reads() {
        // Segment: SeekHead → Attachments(2 fonts), then a cluster.
        let font1: &[u8] = b"\x00\x01\x00\x00fake-truetype-bytes";
        let font2: &[u8] = b"OTTOfake-opentype";
        let attached = |name: &str, mime: &str, data: &[u8]| {
            ebml(
                ATTACHED_FILE,
                &[
                    ebml(FILE_NAME, name.as_bytes()),
                    ebml(FILE_MIME, mime.as_bytes()),
                    ebml(FILE_DATA, data),
                ]
                .concat(),
            )
        };
        let attachments = ebml(
            ATTACHMENTS,
            &[
                attached("Font1.ttf", "font/ttf", font1),
                attached("Font2.otf", "font/otf", font2),
            ]
            .concat(),
        );
        // SeekHead promising the Attachments position (computed below).
        let seek = |pos: u64| {
            ebml(
                SEEK_HEAD,
                &ebml(
                    SEEK,
                    &[
                        ebml(SEEK_ID, &ATTACHMENTS.to_be_bytes()),
                        ebml(SEEK_POSITION, &encode_uint(pos)),
                    ]
                    .concat(),
                ),
            )
        };
        let seek_len = seek(0).len() as u64; // position encoding is fixed-size via encode_uint? guard below
        let segment_body = [
            seek(seek_len),
            attachments.clone(),
            ebml(CLUSTER, &[0u8; 8]),
        ]
        .concat();
        assert_eq!(
            seek(seek_len).len() as u64,
            seek_len,
            "seek element size must be stable"
        );

        let mut file = ebml(EBML_HEADER, &[]);
        file.extend(ebml(SEGMENT, &segment_body));

        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&file).unwrap();
        f.flush().unwrap();
        let atts = declare_attachments(f.path()).unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].file_name, "Font1.ttf");
        assert_eq!(atts[0].mime_type, "font/ttf");
        assert_eq!(atts[1].file_name, "Font2.otf");
        // The declared ranges must slice out exactly the payloads.
        assert_eq!(
            &file[atts[0].offset as usize..(atts[0].offset + atts[0].size) as usize],
            font1
        );
        assert_eq!(
            &file[atts[1].offset as usize..(atts[1].offset + atts[1].size) as usize],
            font2
        );

        // Non-matroska input declares nothing.
        let mut junk = tempfile::NamedTempFile::new().unwrap();
        junk.write_all(b"\x00\x00\x00\x20ftypisommp4-not-mkv-junk-padding")
            .unwrap();
        junk.flush().unwrap();
        assert!(declare_attachments(junk.path()).unwrap().is_empty());
    }

    /// Real-file check: KAHAWAI_ATTACH_CHECK=/path/to/file — declared
    /// ranges must carry font magic where the mime says font.
    #[test]
    #[ignore]
    fn corpus_attachments() {
        use std::io::{Read as _, Seek as _};
        let path = std::path::PathBuf::from(std::env::var("KAHAWAI_ATTACH_CHECK").unwrap());
        let atts = declare_attachments(&path).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        for a in &atts {
            let mut magic = [0u8; 4];
            f.seek(std::io::SeekFrom::Start(a.offset)).unwrap();
            f.read_exact(&mut magic).unwrap();
            println!(
                "{} ({}) offset={} size={} magic={magic:02x?}",
                a.file_name, a.mime_type, a.offset, a.size
            );
            if a.mime_type.contains("font") || a.mime_type.contains("truetype") {
                assert!(
                    matches!(&magic, b"\x00\x01\x00\x00" | b"OTTO" | b"true" | b"ttcf"),
                    "{}: not font magic: {magic:02x?}",
                    a.file_name
                );
            }
        }
        println!("{} attachments declared", atts.len());
    }

    /// Corpus equivalence: KAHAWAI_SPARSE_CHECK=/path/to/file — sparse
    /// output must match the sequential GStreamer pass cue for cue.
    #[test]
    #[ignore]
    fn corpus_equivalence() {
        let path = std::path::PathBuf::from(std::env::var("KAHAWAI_SPARSE_CHECK").unwrap());
        let t0 = std::time::Instant::now();
        let sparse = extract_sparse(&path)
            .unwrap()
            .expect("file should be sparse-readable");
        println!("sparse pass: {:?}", t0.elapsed());
        if std::env::var("KAHAWAI_SPARSE_ONLY").is_ok() {
            return;
        }
        let source = crate::remux::FileSource::open(&path).unwrap();
        let gst = crate::subtitles::extract_embedded_all(Box::new(source)).unwrap();
        assert_eq!(sparse.len(), gst.len(), "track count");
        for ((si, sx), (gi, gx)) in sparse.iter().zip(gst.iter()) {
            assert_eq!(si, gi, "track index");
            assert_eq!(sx.cues.len(), gx.cues.len(), "cue count on track {si}");
            for (a, b) in sx.cues.iter().zip(gx.cues.iter()) {
                assert_eq!(a, b);
            }
            assert_eq!(sx.ass, gx.ass, "ass reconstruction on track {si}");
        }
        println!(
            "OK: {} tracks, {} cues",
            sparse.len(),
            sparse.iter().map(|(_, e)| e.cues.len()).sum::<usize>()
        );
    }

    #[test]
    fn non_container_falls_back() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"definitely not a media file").unwrap();
        assert!(extract_sparse(f.path()).unwrap().is_none());
    }
}

/// The longest gap between keyframes, in milliseconds — read from the
/// container's own index, never by decoding.
///
/// What it is FOR: our segmenters (`splitmuxsink`, `isofmp4mux`) close
/// a fragment at the first keyframe past the fragment target, and a
/// stream copy has no encoder to request one from — so on a copy the
/// longest keyframe gap bounds the longest segment, and declaring
/// below it violates RFC 8216 §4.3.3.1, which strict clients act on.
///
/// Worth being exact, because the wrong version of this sentence sent
/// a whole night sideways: HLS does not require segments to start at
/// keyframes (§6.2.1 "SHOULD", and §3 allows leading frames to be
/// "downloaded but possibly discarded"). A segmenter cutting on a time
/// grid would make the declared duration a constant of our choosing
/// and this measurement unnecessary.
///
/// `None` means "unknown, assume nothing": an unsupported container
/// (AVI), an index that isn't there, or a file whose every frame is a
/// keyframe (no `stss` box — MP4 says so by omission). Callers must
/// treat unknown as "could be long" rather than as zero.
///
/// Sources are conservative on purpose. Matroska Cues are a hint, not a
/// contract — a muxer may index only some keyframes — so a missed one
/// makes the measured gap LONGER than the truth, which errs toward a
/// larger declared duration. That is the safe direction.
pub fn max_keyframe_interval_ms(path: &Path) -> Result<Option<u32>> {
    let mut src = crate::remux::FileSource::open(path)?;
    let mut magic = [0u8; 12];
    {
        let mut f = std::fs::File::open(path)?;
        use std::io::Read;
        if f.read(&mut magic)? < 12 {
            return Ok(None);
        }
    }
    if magic[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        mkv_max_keyframe_gap(&mut src)
    } else if &magic[4..8] == b"ftyp" {
        mp4_max_keyframe_gap(&mut src)
    } else if &magic[..4] == b"RIFF" && &magic[8..12] == b"AVI " {
        avi_max_keyframe_gap(&mut src)
    } else {
        Ok(None)
    }
}

/// AVI: `idx1` is a flat table of 16-byte entries, one per chunk, with
/// `AVIIF_KEYFRAME` in the flags — the same fact as Cues and `stss`,
/// differently spelled.
///
/// Worth having despite the container's age: these files are MPEG-4
/// Part 2, which Android lists as a mandatory decoder on every version,
/// so a phone can be handed the video stream COPIED rather than
/// re-encoded — and then the source's keyframe spacing decides segment
/// length exactly as it does for a modern container. Browsers cannot
/// decode Part 2 and never take that path, which is why this looked
/// skippable until the reporting client turned out to be ExoPlayer.
///
/// Costs megabytes rather than kilobytes: a feature-length `idx1` runs
/// to ~3 MB because it indexes every chunk, not every keyframe. Still
/// an index read, still no decoding, and read in windows rather than
/// whole.
fn avi_max_keyframe_gap(src: &mut dyn RemuxSource) -> Result<Option<u32>> {
    const AVIIF_KEYFRAME: u32 = 0x10;
    let mut r = Reader::new(src);
    // Top-level RIFF walk: find the video stream's number and its frame
    // duration in `hdrl`, then the `idx1` table.
    let (mut pos, mut idx1) = (12u64, None);
    let (mut video_stream, mut secs_per_frame) = (None::<usize>, None::<f64>);
    let mut stream_no = 0usize;
    while pos + 8 <= r.len {
        let head = r.read_at(pos, 8)?;
        let id = &head[0..4];
        let size = u32::from_be_bytes([head[7], head[6], head[5], head[4]]) as u64;
        if id == b"LIST" {
            let kind = r.read_at(pos + 8, 4)?;
            if &kind[..] == b"hdrl" || &kind[..] == b"strl" {
                pos += 12; // descend
                continue;
            }
            pos += 8 + size + (size & 1); // movi and friends: skip whole
            continue;
        }
        if id == b"strh" {
            let b = r.read_at(pos + 8, 32.min(size as usize))?;
            if b.len() >= 32 {
                let is_video = &b[0..4] == b"vids";
                let scale = u32::from_le_bytes(b[20..24].try_into().unwrap()) as f64;
                let rate = u32::from_le_bytes(b[24..28].try_into().unwrap()) as f64;
                if is_video && video_stream.is_none() {
                    video_stream = Some(stream_no);
                    if rate > 0.0 {
                        secs_per_frame = Some(scale / rate);
                    }
                }
            }
            stream_no += 1;
        }
        if id == b"idx1" {
            idx1 = Some((pos + 8, size));
            break;
        }
        pos += 8 + size + (size & 1);
    }
    let (Some((body, size)), Some(stream), Some(spf)) = (idx1, video_stream, secs_per_frame) else {
        return Ok(None);
    };
    // The chunk id of this stream's video data: "00dc"/"00db" for
    // stream 0, "01dc" for stream 1, and so on.
    let tag = format!("{:02}", stream);
    let (mut frames, mut keyframes) = (0u64, Vec::new());
    let mut off = 0u64;
    while off + 16 <= size {
        let n = (16 * 4096).min((size - off) as usize) / 16 * 16;
        let buf = r.read_at(body + off, n)?;
        for e in buf.chunks_exact(16) {
            let ck = &e[0..4];
            if ck[0..2] != tag.as_bytes()[0..2] || !matches!(&ck[2..4], b"dc" | b"db") {
                continue;
            }
            let flags = u32::from_le_bytes(e[4..8].try_into().unwrap());
            if flags & AVIIF_KEYFRAME != 0 {
                keyframes.push(frames);
            }
            frames += 1;
        }
        off += n as u64;
    }
    Ok(gap_ms(&mut keyframes, move |f| (f as f64 * spf * 1000.0) as u64))
}

/// Matroska: the Cues element indexes keyframes by timestamp. Reached
/// through the SeekHead exactly as `declare_attachments` does, so a
/// file whose SeekHead promises no Cues costs only the header hops.
fn mkv_max_keyframe_gap(src: &mut dyn RemuxSource) -> Result<Option<u32>> {
    let mut r = Reader::new(src);
    let head = r.read_at(0, 32.min(r.len as usize))?;
    let Ok((id, il)) = ebml_id(&head) else {
        return Ok(None);
    };
    if id != EBML_HEADER {
        return Ok(None);
    }
    let (hsize, hsl) = ebml_size(&head[il..])?;
    let pos = il as u64 + hsl as u64 + hsize.context("unknown EBML header size")?;
    let seg = r.read_at(pos, 16)?;
    let (id, il) = ebml_id(&seg)?;
    if id != SEGMENT {
        return Ok(None);
    }
    let (seg_size, sl) = ebml_size(&seg[il..])?;
    let segment_start = pos + il as u64 + sl as u64;
    let segment_end = seg_size.map(|s| segment_start + s).unwrap_or(r.len);

    let mut scale = 1_000_000u64; // ns per tick; matroska's default
    let mut video_track: Option<u64> = None;
    let mut cues: Option<(u64, u64)> = None; // (body, size)
    let mut pending: Vec<u64> = Vec::new();
    let mut saw_seekhead = false;
    let mut visited = std::collections::HashSet::new();
    let mut pos = segment_start;
    while pos < segment_end.min(r.len) {
        if !visited.insert(pos) {
            break;
        }
        let Ok(head) = r.read_at(pos, 16) else { break };
        let Ok((id, il)) = ebml_id(&head) else { break };
        let Ok((size, sl)) = ebml_size(&head[il..]) else {
            break;
        };
        let body = pos + il as u64 + sl as u64;
        let Some(size) = size else { break };
        match id {
            SEEK_HEAD => {
                saw_seekhead = true;
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, seek| {
                    if id == SEEK {
                        let (mut target, mut position) = (0u32, None);
                        walk_children(seek, |id, v| {
                            match id {
                                SEEK_ID => target = uint(v) as u32,
                                SEEK_POSITION => position = Some(uint(v)),
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if let Some(p) = position
                            && matches!(target, CUES | TRACKS | INFO)
                        {
                            pending.push(segment_start + p);
                        }
                    }
                    Ok(true)
                })?;
            }
            INFO => {
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, v| {
                    if id == TIMESTAMP_SCALE {
                        scale = uint(v).max(1);
                    }
                    Ok(true)
                })?;
            }
            TRACKS => {
                let data = r.read_at(body, size as usize)?;
                walk_children(&data, |id, entry| {
                    if id == TRACK_ENTRY {
                        let (mut num, mut kind) = (None, 0u64);
                        walk_children(entry, |id, v| {
                            match id {
                                TRACK_NUMBER => num = Some(uint(v)),
                                TRACK_TYPE => kind = uint(v),
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        // 1 = video, and the FIRST one: the same track
                        // negotiation plans around.
                        if kind == 1 && video_track.is_none() {
                            video_track = num;
                        }
                    }
                    Ok(true)
                })?;
            }
            CUES => cues = Some((body, size)),
            // Clusters mean the index is behind us; with a SeekHead
            // that promised nothing more, stop rather than hop the
            // whole file.
            CLUSTER if saw_seekhead && pending.is_empty() => break,
            _ => {}
        }
        if cues.is_some() && video_track.is_some() {
            break;
        }
        pos = body + size;
        if let Some(next) = pending.iter().find(|p| !visited.contains(*p)) {
            pos = *next;
        }
    }

    let Some((body, size)) = cues else {
        return Ok(None);
    };
    let data = r.read_at(body, size as usize)?;
    let mut times: Vec<u64> = Vec::new();
    walk_children(&data, |id, point| {
        if id == CUE_POINT {
            let (mut time, mut track) = (None, None);
            walk_children(point, |id, v| {
                match id {
                    CUE_TIME => time = Some(uint(v)),
                    CUE_TRACK_POSITIONS => {
                        walk_children(v, |id, w| {
                            if id == CUE_TRACK {
                                track = Some(uint(w));
                            }
                            Ok(true)
                        })?;
                    }
                    _ => {}
                }
                Ok(true)
            })?;
            // Cues for a subtitle or audio track say nothing about
            // where the video can be cut.
            let ours = match (track, video_track) {
                (Some(t), Some(v)) => t == v,
                _ => true,
            };
            if let Some(t) = time
                && ours
            {
                times.push(t);
            }
        }
        Ok(true)
    })?;
    Ok(gap_ms(&mut times, |ticks| ticks * scale / 1_000_000))
}

/// MP4: `stss` lists the sync samples and `stts` gives every sample's
/// duration, so the keyframe timeline is exact. No `stss` at all means
/// every sample is a sync sample (an all-intra file) — a zero-length
/// gap, not an unknown one.
fn mp4_max_keyframe_gap(src: &mut dyn RemuxSource) -> Result<Option<u32>> {
    let mut r = Reader::new(src);
    let mut pos = 0u64;
    let mut moov: Option<Vec<u8>> = None;
    while pos + 8 <= r.len {
        let head = r.read_at(pos, 16)?;
        let size32 = u32::from_be_bytes(head[0..4].try_into().unwrap()) as u64;
        let kind = &head[4..8];
        let (size, hdr) = if size32 == 1 {
            (u64::from_be_bytes(head[8..16].try_into().unwrap()), 16u64)
        } else if size32 == 0 {
            (r.len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        if size < hdr {
            return Ok(None);
        }
        if kind == b"moov" {
            moov = Some(r.read_at(pos + hdr, (size - hdr) as usize)?);
            break;
        }
        pos += size;
    }
    let Some(moov) = moov else { return Ok(None) };

    let mut out: Option<Option<u32>> = None;
    walk_boxes(&moov, |kind, body| {
        if kind != *b"trak" || out.is_some() {
            return Ok(());
        }
        let Some(mdia) = find_box(body, b"mdia") else {
            return Ok(());
        };
        let Some(hdlr) = find_box(mdia, b"hdlr") else {
            return Ok(());
        };
        if hdlr.len() < 12 || &hdlr[8..12] != b"vide" {
            return Ok(());
        }
        let Some(mdhd) = find_box(mdia, b"mdhd") else {
            return Ok(());
        };
        let timescale = if mdhd[0] == 1 {
            u32::from_be_bytes(mdhd[20..24].try_into().unwrap())
        } else {
            u32::from_be_bytes(mdhd[12..16].try_into().unwrap())
        } as u64;
        let Some(stbl) = find_box(mdia, b"minf").and_then(|m| find_box(m, b"stbl")) else {
            return Ok(());
        };
        let Some(stts) = find_box(stbl, b"stts") else {
            return Ok(());
        };
        let entries = u32::from_be_bytes(stts[4..8].try_into().unwrap()) as usize;
        // FRAGMENTED mp4: the moov describes no samples at all (the
        // fragments carry their own tables), so an absent `stss` here
        // means "not written down", not "no keyframes to write down".
        // Reading it as all-intra would declare that a cut is possible
        // anywhere and put us straight back to under-declaring.
        if entries == 0 || find_box(&moov, b"mvex").is_some() {
            return Ok(());
        }
        // No stss in a non-fragmented file: every sample IS a sync
        // sample, so a cut is possible anywhere. Known and zero.
        let Some(stss) = find_box(stbl, b"stss") else {
            out = Some(Some(0));
            return Ok(());
        };
        let ts = timescale.max(1);
        let n = u32::from_be_bytes(stss[4..8].try_into().unwrap()) as usize;
        let syncs: std::collections::HashSet<u32> = (0..n)
            .filter_map(|i| stss.get(8 + i * 4..12 + i * 4))
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
            .collect();
        let mut times: Vec<u64> = Vec::new();
        let (mut t, mut sample) = (0u64, 1u32);
        for e in 0..entries {
            let base = 8 + e * 8;
            let (Some(cnt), Some(delta)) = (stts.get(base..base + 4), stts.get(base + 4..base + 8))
            else {
                break;
            };
            let cnt = u32::from_be_bytes(cnt.try_into().unwrap());
            let delta = u32::from_be_bytes(delta.try_into().unwrap()) as u64;
            for _ in 0..cnt {
                if syncs.contains(&sample) {
                    times.push(t);
                }
                t += delta;
                sample += 1;
            }
        }
        out = Some(gap_ms(&mut times, |t| t * 1000 / ts));
        Ok(())
    })?;
    Ok(out.flatten())
}

/// Largest adjacent gap in a keyframe timeline, via `to_ms`. Fewer than
/// two keyframes is not a measurement — one keyframe says nothing about
/// spacing — so it reads as unknown.
fn gap_ms(times: &mut Vec<u64>, to_ms: impl Fn(u64) -> u64) -> Option<u32> {
    if times.len() < 2 {
        return None;
    }
    times.sort_unstable();
    let max = times
        .windows(2)
        .map(|w| to_ms(w[1]) - to_ms(w[0]))
        .max()
        .unwrap_or(0);
    Some(max.min(u32::MAX as u64) as u32)
}

#[cfg(test)]
mod keyframe_tests {
    use super::*;

    /// Both index formats, against a keyframe interval we CHOSE. A
    /// parser that silently returns something plausible is the failure
    /// mode here — the number decides a spec-bound value, so the test
    /// pins it to a GOP the encoder was told to produce.
    #[test]
    fn keyframe_interval_read_from_mkv_and_mp4_indexes() {
        if !crate::testutil::has_element("x264enc") {
            eprintln!("skipped: no x264enc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 25 fps, a keyframe every 50 frames = 2.0 s, 6 s long.
        for (name, mux) in [
            ("gop.mkv", "matroskamux"),
            ("gop.mp4", "mp4mux faststart=true"),
        ] {
            let path = dir.path().join(name);
            crate::testutil::render(&format!(
                "videotestsrc num-buffers=150 ! video/x-raw,framerate=25/1,width=320,height=240 \
                 ! x264enc key-int-max=50 speed-preset=ultrafast ! h264parse \
                 ! {mux} ! filesink location={}",
                path.display()
            ));
            let got = max_keyframe_interval_ms(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: no interval read"));
            assert!(
                (1900..=2100).contains(&got),
                "{name}: expected ~2000 ms, got {got}"
            );
        }
    }

    /// A FRAGMENTED mp4 must read as unknown. Its moov describes no
    /// samples — the fragments carry their own tables — so the absent
    /// `stss` means "not written here", and taking it for all-intra
    /// would declare cuts possible anywhere: the exact under-declaration
    /// this whole measurement exists to stop.
    #[test]
    fn a_fragmented_mp4_is_unknown_not_all_intra() {
        if !crate::testutil::has_element("isofmp4mux") || !crate::testutil::has_element("x264enc") {
            eprintln!("skipped: no isofmp4mux/x264enc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frag.mp4");
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=150 ! video/x-raw,framerate=25/1,width=320,height=240 \
             ! x264enc key-int-max=50 speed-preset=ultrafast ! h264parse \
             ! isofmp4mux ! filesink location={}",
            path.display()
        ));
        assert_eq!(
            max_keyframe_interval_ms(&path).unwrap(),
            None,
            "a fragmented mp4 has no sample table to read"
        );
    }

    /// AVI carries the same fact in `idx1`, and it is not a museum
    /// piece here: these files are MPEG-4 Part 2, which Android
    /// decodes on every version, so a phone gets the stream COPIED and
    /// the source's keyframe spacing decides its segment length.
    ///
    /// Ground-truthed against ffprobe on a real file rather than only
    /// against itself: `Last Resort - S01E04` reads 10010 ms here and
    /// ffprobe walks 893 keyframes to the same 10.010 s.
    #[test]
    fn keyframe_interval_read_from_an_avi_index() {
        if !crate::testutil::has_element("avimux") || !crate::testutil::has_element("x264enc") {
            eprintln!("skipped: no avimux/x264enc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gop.avi");
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=150 ! video/x-raw,framerate=25/1,width=320,height=240 \
             ! x264enc key-int-max=50 speed-preset=ultrafast ! h264parse \
             ! avimux ! filesink location={}",
            path.display()
        ));
        let got = max_keyframe_interval_ms(&path)
            .unwrap()
            .expect("no interval read from idx1");
        assert!(
            (1900..=2100).contains(&got),
            "expected ~2000 ms from idx1, got {got}"
        );
    }

    /// Unknown must stay unknown. A container we cannot index reads as
    /// `None` so the caller falls back to a conservative bound — the one
    /// thing it must never do is read as 0, which would declare that
    /// cuts are possible anywhere.
    #[test]
    fn an_unindexable_file_is_unknown_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("not-media.bin");
        std::fs::write(&junk, vec![0u8; 4096]).unwrap();
        assert_eq!(max_keyframe_interval_ms(&junk).unwrap(), None);

        let empty = dir.path().join("empty.mkv");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(max_keyframe_interval_ms(&empty), Ok(None) | Err(_)));
    }
}
