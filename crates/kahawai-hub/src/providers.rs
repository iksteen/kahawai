//! Provider description fields and default ordering. Scheduling and evidence live in mediadb.
/// What a provider has to say about an item's description.
#[derive(Debug, Default, Clone)]
pub struct Fields {
    pub title: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub rating: Option<f64>,
    pub premiered: Option<String>,
    pub original_language: Option<String>,
    /// JSON array, as stored.
    pub genres: Option<String>,
    /// JSON array of {name, character}, billing order, as stored.
    pub cast_json: Option<String>,
    /// This provider's identity for the credited Album Artist. Only music
    /// answers currently supply it; keeping it beside the release-group
    /// answer preserves the evidence needed to reject conflicting artists.
    pub provider_artist_id: Option<String>,
}

pub const MEDIA_TYPES: [&str; 4] = ["movies", "series", "anime", "music"];
pub fn chain_for(media_type: &str) -> &'static [&'static str] {
    match media_type {
        "anime" => &["anidb", "anilist", "tmdb", "tvdb"],
        "music" => &["musicbrainz"],
        _ => &["tmdb", "tvdb"],
    }
}
