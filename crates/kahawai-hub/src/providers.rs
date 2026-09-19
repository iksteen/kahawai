//! Provider description fields and default ordering. Scheduling and evidence live in mediadb.
pub const MEDIA_TYPES: [&str; 4] = ["movies", "series", "anime", "music"];
pub fn chain_for(media_type: &str) -> &'static [&'static str] {
    match media_type {
        "anime" => &["anidb", "anilist", "tmdb", "tvdb"],
        "music" => &["musicbrainz"],
        _ => &["tmdb", "tvdb"],
    }
}
