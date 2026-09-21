//! Playback mechanics shared by everything that runs a pipeline.
//!
//! The hub's local remux, the transcoder's dispatched sessions and the
//! `remux-worker` child process used to each carry their own copy of the
//! same three things: what a pipeline run IS (`job`), how to supervise one
//! (`executor`) and what its output directory means (`playlist`, `bundle`).
//! The copies drifted — the readiness runway, the run-directory layout, the
//! log capture and the diagnostics bundle each ended up subtly different on
//! the two sides of the transcoder link. This crate is the one place they
//! live now.
//!
//! It also holds the pure half of placement (`placement`): ranking a fleet
//! snapshot against what a session needs, which the hub's registry wraps in
//! its locks and its reservation.
//!
//! What it deliberately does NOT know: sessions, users, catalogues, leases,
//! the registry, HTTP. Those stay in the hub. The dependency rule that
//! follows from the transcoder daemon depending on this crate is in
//! `Cargo.toml`.

pub mod bundle;
pub mod executor;
pub mod job;
pub mod placement;
pub mod playlist;
pub mod seek;
pub mod worker;
