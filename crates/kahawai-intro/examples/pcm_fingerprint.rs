//! Fingerprint raw PCM from stdin: `pcm_fingerprint RATE CHANNELS`, signed
//! 16-bit little-endian, interleaved.
//!
//! The comparison rig uses this to take decoding out of the argument — feed the
//! same samples ffmpeg handed intro-skipper and see whether the fingerprints
//! still differ.

use std::io::Read;

use anyhow::{Context, Result};
use kahawai_intro::decode::AudioWindow;
use kahawai_intro::fingerprint;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rate: u32 = args
        .next()
        .context("usage: pcm_fingerprint RATE CHANNELS < raw.s16le")?
        .parse()?;
    let channels: u32 = args.next().unwrap_or_else(|| "2".into()).parse()?;

    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let window = AudioWindow {
        rate,
        channels,
        samples: bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect(),
    };

    for point in fingerprint::fingerprint_samples(&window)? {
        println!("{point}");
    }
    Ok(())
}
