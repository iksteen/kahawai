//! ED2K hashing (MH-9): 9,728,000-byte chunks, MD4 per chunk, MD4 over the
//! concatenated chunk digests when there is more than one. eMule/AniDB
//! variant: a file whose size is an exact multiple of the chunk size gets
//! a terminating empty-chunk digest — AniDB's file identity expects this.
//!
//! The same read pass verifies a CRC32 carried in the filename (the
//! `[ABCD1234]` fansub convention) when one is present.

use md4::{Digest, Md4};

pub const CHUNK: usize = 9_728_000;

/// Streaming ED2K state: feed arbitrary slices, finish to a hex digest.
pub struct Ed2k {
    chunk_hasher: Md4,
    /// Bytes fed into the current (partial) chunk.
    chunk_fill: usize,
    /// Digests of completed chunks.
    chunks: Vec<[u8; 16]>,
}

impl Default for Ed2k {
    fn default() -> Self {
        Self { chunk_hasher: Md4::new(), chunk_fill: 0, chunks: Vec::new() }
    }
}

impl Ed2k {
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = data.len().min(CHUNK - self.chunk_fill);
            self.chunk_hasher.update(&data[..take]);
            self.chunk_fill += take;
            data = &data[take..];
            if self.chunk_fill == CHUNK {
                let done = std::mem::replace(&mut self.chunk_hasher, Md4::new());
                self.chunks.push(done.finalize().into());
                self.chunk_fill = 0;
            }
        }
    }

    pub fn finish(mut self) -> String {
        let digest: [u8; 16] = if self.chunks.is_empty() {
            // Single partial (or empty) chunk: its MD4 is the hash.
            self.chunk_hasher.finalize().into()
        } else {
            // eMule variant: the trailing chunk digest is always appended,
            // even when it is the digest of zero bytes (exact multiple).
            self.chunks.push(self.chunk_hasher.finalize().into());
            let mut root = Md4::new();
            for c in &self.chunks {
                root.update(c);
            }
            root.finalize().into()
        };
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A CRC32 claimed by the filename: the last `[xxxxxxxx]` or `(xxxxxxxx)`
/// group of 8 hex digits, per fansub convention.
pub fn filename_crc32(name: &str) -> Option<u32> {
    let bytes = name.as_bytes();
    let mut best = None;
    let mut i = 0;
    while i + 10 <= bytes.len() {
        let open = bytes[i];
        if (open == b'[' || open == b'(')
            && bytes[i + 9] == (if open == b'[' { b']' } else { b')' })
            && bytes[i + 1..i + 9].iter().all(u8::is_ascii_hexdigit)
        {
            best = u32::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 9]).unwrap(), 16).ok();
        }
        i += 1;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed2k(data: &[u8]) -> String {
        let mut h = Ed2k::default();
        h.update(data);
        h.finish()
    }

    #[test]
    fn md4_direct_below_one_chunk() {
        // MD4 test vectors (RFC 1320): a sub-chunk file IS its MD4.
        assert_eq!(ed2k(b""), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(ed2k(b"abc"), "a448017aaf21d8525fc10ae87aa6729d");
    }

    #[test]
    fn exact_chunk_multiple_uses_emule_null_chunk() {
        // Kimundi/ed2k "Red" vector: 9,728,000 bytes of 0x55.
        assert_eq!(ed2k(&vec![0x55u8; CHUNK]), "49e80f377b7e4e706dbd3ecc89f39306");
    }

    #[test]
    fn multi_chunk_matches_hash_of_hashes() {
        let data = vec![0xabu8; CHUNK + 17];
        let expected: [u8; 16] = {
            let a: [u8; 16] = Md4::digest(&data[..CHUNK]).into();
            let b: [u8; 16] = Md4::digest(&data[CHUNK..]).into();
            let mut root = Md4::new();
            root.update(a);
            root.update(b);
            root.finalize().into()
        };
        let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(ed2k(&data), expected_hex);
        // Split feeding must agree with one-shot feeding.
        let mut split = Ed2k::default();
        for part in data.chunks(1_000_003) {
            split.update(part);
        }
        assert_eq!(split.finish(), expected_hex);
    }

    #[test]
    fn filename_crc_extraction() {
        assert_eq!(
            filename_crc32("[Coalgirls]_Ao_no_Exorcist_01_(1280x720)_[66D6AE9D].mkv"),
            Some(0x66D6AE9D)
        );
        assert_eq!(filename_crc32("Show - 01 (DEADBEEF).mkv"), Some(0xDEADBEEF));
        // The LAST tag wins; resolution groups aren't 8 hex digits.
        assert_eq!(filename_crc32("[ABCD1234] then [12345678].mkv"), Some(0x12345678));
        assert_eq!(filename_crc32("no tag here (1280x720).mkv"), None);
    }
}
