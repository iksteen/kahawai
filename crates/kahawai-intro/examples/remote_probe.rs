//! Run the analyzers over the *remote* source path against a local file:
//! `remote_probe FILE`.
//!
//! The hub does not open episodes by name — it reads them through a mediahost
//! lease, which reaches GStreamer as an appsrc rather than a filesrc. That is a
//! different source element with a different seek path, so it gets its own
//! check, backed here by a plain file so a stall is the source and not the
//! network.

use std::sync::Arc;

use anyhow::{Context, Result};
use kahawai_intro::decode::Media;
use kahawai_intro::{decode, fingerprint};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: remote_probe FILE")?;
    let media = Media::Remote {
        name: path.clone(),
        open: Arc::new(move || {
            Ok(Box::new(kahawai_media::remux::FileSource::open(
                std::path::Path::new(&path),
            )?)
                as Box<dyn kahawai_media::remux::RemuxSource>)
        }),
    };

    // Windows from the command line, so a file that fails one can be asked
    // about the others: `remote_probe FILE [START END]`.
    let mut rest = std::env::args().skip(2);
    let start: f64 = rest.next().unwrap_or_else(|| "0".into()).parse()?;
    let end: f64 = rest.next().unwrap_or_else(|| "60".into()).parse()?;

    match fingerprint::fingerprint(&media, start, end) {
        Ok(points) => println!("fingerprint [{start}, {end}): {} points", points.len()),
        Err(e) => println!("fingerprint [{start}, {end}): FAILED {e:#}"),
    }
    match decode::luma_window(&media, start, start + 2.0, 28) {
        Ok(frames) => println!("luma [{start}, {}): {} frames", start + 2.0, frames.len()),
        Err(e) => println!("luma [{start}, {}): FAILED {e:#}", start + 2.0),
    }
    match decode::keyframes_window(&media, start, start + 10.0) {
        Ok(times) => println!("keyframes [{start}, {}): {times:?}", start + 10.0),
        Err(e) => println!("keyframes [{start}, {}): FAILED {e:#}", start + 10.0),
    }
    Ok(())
}
