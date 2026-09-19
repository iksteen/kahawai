//! Source-bound playback segment boundaries, supplied by mediadb.
use serde::Serialize;
use utoipa::ToSchema;

/// One boundary pair for one episode.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Segment {
    /// `recap`, `intro` or `credits`.
    pub kind: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Which analyzer answered: `chapter`, `chromaprint` or `blackframe`.
    pub source: String,
}
