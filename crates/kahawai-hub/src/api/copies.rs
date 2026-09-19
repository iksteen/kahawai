//! Public item reads use library item IDs. Collection copies are explicit context
//! for matching and source playback, never the unit of browse pagination.
use super::*;

#[derive(Serialize, ToSchema)]
pub(super) struct CollectionCopy {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
    pub artist: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub parent_library_item_id: Option<String>,
    pub module_id: Option<String>,
    pub host_name: Option<String>,
    pub collection_id: Option<String>,
    pub paths: Vec<String>,
    pub match_confidence: Option<String>,
    /// The selected provider record's title, without display-field fallbacks.
    pub matched_title: Option<String>,
    /// The selected provider record's release year, if supplied.
    pub matched_year: Option<i64>,
    pub assignment: CopyAssignment,
}

#[derive(Serialize, ToSchema)]
pub(super) struct CopyAssignment {
    pub collection_item_id: String,
    pub revision: i64,
    pub mode: String,
    pub library_item_ids: Vec<String>,
    pub conflict: Option<String>,
}
