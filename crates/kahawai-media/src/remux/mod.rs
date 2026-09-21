//! In-hub remuxer (AR-10, §4.6): repackage supported streams into HLS with
//! **no re-encoding and no transcoder** — `appsrc ! parsebin ! <hls sink>`.
//! Parsing and repackaging elementary streams costs a few % CPU.
//!
//! The HLS sink follows the plugin-fallback strategy (see HLS_SINKS):
//! hlssink3 when installed, hlssink2 otherwise. TS segments either way
//! (the HLS baseline, HUB-17); fMP4/CMAF via cmafmux is a future upgrade.
//!
//! Split by concern, all of it one namespace: `probe` (what this box
//! can decode, encode and mux, dry-run verified), `plan` (the plan types
//! every supervisor and the wire speak), `route` (parsed-pad plumbing and
//! seeks), `taps` (subtitle side channels out of a live pipeline), `video`
//! and `audio` (the encode chains), `source` (the seekable byte feed),
//! `sink` (the segmenter and pacing) and `job` (the pipeline lifecycle).

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gstreamer_video as gst_video;
use std::path::Path;
use std::sync::{Arc, Mutex};

mod audio;
mod job;
mod plan;
mod probe;
mod route;
mod sink;
mod source;
mod taps;
#[cfg(test)]
mod tests;
mod video;

pub use audio::*;
pub use job::*;
pub use plan::*;
pub use probe::*;
use route::*;
pub use sink::*;
pub use source::*;
use taps::*;
pub(crate) use video::*;
