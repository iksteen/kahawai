use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Movies,
    Series,
    Anime,
    Music,
}
impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movies => "movies",
            Self::Series => "series",
            Self::Anime => "anime",
            Self::Music => "music",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "movies" => Self::Movies,
            "series" => Self::Series,
            "anime" => Self::Anime,
            "music" => Self::Music,
            _ => bail!("unknown media type {s}"),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Credit {
    pub name: String,
    pub role: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ChildMetadata {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub absolute: Option<u32>,
    #[serde(default)]
    pub artwork: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub rating: Option<f64>,
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub overview: Option<String>,
}
/// None means no answer. An explicit empty list is an answer, not a gap.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Description {
    pub overview: Option<String>,
    pub original_title: Option<String>,
    pub original_language: Option<String>,
    pub release_date: Option<String>,
    pub rating: Option<f64>,
    pub artwork: Option<Vec<String>>,
    pub genres: Option<Vec<String>>,
    pub cast: Option<Vec<Credit>>,
    pub children: Option<Vec<ChildMetadata>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProviderRecord {
    pub provider: String,
    pub namespace: String,
    pub external_id: String,
    pub language: String,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i32>,
    pub description: Description,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DetectedMetadata {
    pub title: String,
    pub year: Option<i32>,
    pub artist: Option<String>,
    pub description: Description,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EpisodeSpan {
    pub season: Option<u32>,
    pub episode: u32,
    /// Inclusive end; None is a single episode. No expansion during import.
    pub episode_end: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    Movie,
    Episode {
        episodes: Vec<EpisodeSpan>,
    },
    Track {
        disc: Option<u32>,
        track: Option<u32>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Part {
    pub file_id: String,
    pub ordinal: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NewEntry {
    pub occurrence: String,
    pub title: String,
    pub artist: Option<String>,
    pub kind: EntryKind,
    pub parts: Vec<Part>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NewOccurrence {
    pub collection_id: String,
    pub root_id: String,
    pub occurrence: String,
    pub detected: DetectedMetadata,
    pub entries: Vec<NewEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CollectionItem {
    pub id: String,
    pub library_item_id: String,
    pub collection_id: String,
    pub root_id: String,
    pub occurrence: String,
    pub detected: DetectedMetadata,
    pub selected_record: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MediaEntry {
    pub id: String,
    pub item_id: String,
    pub data: NewEntry,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FileInfo {
    pub id: String,
    pub root_id: String,
    pub root_token: String,
    pub path: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub head_hash: Option<u64>,
    pub tail_hash: Option<u64>,
    pub oshash: Option<u64>,
    #[schema(value_type=Object)]
    pub media: Option<kahawai_core::media::MediaInfo>,
    pub item_id: Option<String>,
    pub mapping_error: Option<String>,
}
/// Durable identity, available even when no collection supplies a copy.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LibraryItemRecord {
    pub id: String,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i32>,
    pub archived: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResolvedDescription {
    pub description: Description,
    /// Field name -> provider record ID, or "detected" for an unassigned copy.
    pub provenance: std::collections::BTreeMap<String, String>,
}
/// Playable shape, distinct from a collection's category (anime contains both).
/// Derived from visible physical entries, not provider descriptions or a stored flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LibraryItemKind {
    Movie,
    Series,
    Album,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LibraryItem {
    pub kind: LibraryItemKind,
    pub artist: Option<String>,
    pub media_type: MediaType,
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    /// Representative copy: manual or automatic assignment, weak candidate, or unmatched.
    pub match_confidence: Option<String>,
    pub representative_id: String,
    pub copy_ids: Vec<String>,
    pub metadata: ResolvedDescription,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Library {
    pub id: String,
    pub name: String,
    pub media_type: MediaType,
    pub collection_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Collection {
    pub id: String,
    pub mediahost_id: String,
    pub remote_id: String,
    pub media_type: MediaType,
    pub epoch: String,
    pub version: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Root {
    pub id: String,
    pub token: String,
    pub path: String,
    pub active: bool,
}

/// A requested catalogue entry has no visible representation in this scope.
/// Missing identities and identities with no copy in a library are equivalent.
#[derive(Debug)]
pub struct NotFound;
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("catalogue entry not found")
    }
}
impl std::error::Error for NotFound {}
