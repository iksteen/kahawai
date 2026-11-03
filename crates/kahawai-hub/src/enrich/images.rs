//! Selected title and cast images. Provider identity stays with the enclosing
//! record; artwork never supplies an identity match. Selection is deterministic.
use kahawai_mediadb::{Credit, Description};
use serde_json::Value;

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn language(value: &Value) -> u8 {
    match value.as_str().unwrap_or("") {
        "en" | "eng" | "en-US" => 0,
        "" | "00" | "null" => 1,
        _ => 2,
    }
}

fn score(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

fn best<'a>(
    values: impl Iterator<Item = &'a Value>,
    lang: &str,
    rating: &str,
) -> Option<&'a Value> {
    values.min_by(|a, b| {
        language(&a[lang])
            .cmp(&language(&b[lang]))
            .then_with(|| score(&b[rating]).total_cmp(&score(&a[rating])))
            .then_with(|| b["vote_count"].as_u64().cmp(&a["vote_count"].as_u64()))
            .then_with(|| a["file_path"].as_str().cmp(&b["file_path"].as_str()))
            .then_with(|| a["url"].as_str().cmp(&b["url"].as_str()))
            .then_with(|| a["image"].as_str().cmp(&b["image"].as_str()))
    })
}

pub(super) fn tmdb_image(path: &Value, size: &str) -> Option<String> {
    let mut path = text(path)?;
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    // TMDB's image service provides PNG counterparts for SVG logos.
    // https://developer.themoviedb.org/docs/image-basics (2026-11-03).
    if path.ends_with(".svg") {
        path.truncate(path.len() - 4);
        path.push_str(".png");
    }
    Some(format!("https://image.tmdb.org/t/p/{size}{path}"))
}

pub(super) fn tmdb(description: &mut Description, value: &Value) {
    // /images returns logos and backdrops; appended credits include profile_path.
    // https://developer.themoviedb.org/reference/movie-images
    // https://developer.themoviedb.org/reference/tv-series-images (2026-11-03).
    let select = |kind: &str, size: &str| {
        best(
            value["images"][kind]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| tmdb_image(&v["file_path"], size).is_some()),
            "iso_639_1",
            "vote_average",
        )
        .and_then(|v| tmdb_image(&v["file_path"], size))
    };
    description.logo = select("logos", "original");
    description.backdrop =
        select("backdrops", "original").or_else(|| tmdb_image(&value["backdrop_path"], "original"));
    description.genres = value["genres"]
        .as_array()
        .map(|values| values.iter().filter_map(|v| text(&v["name"])).collect());
    description.cast = value["credits"]["cast"].as_array().map(|values| {
        values
            .iter()
            .filter_map(|v| {
                Some(Credit {
                    name: text(&v["name"])?,
                    role: text(&v["character"]),
                    id: v["id"]
                        .as_u64()
                        .filter(|id| *id > 0)
                        .map(|id| format!("tmdb:{id}")),
                    image: tmdb_image(&v["profile_path"], "w500"),
                })
            })
            .collect()
    });
}

pub(super) fn tvdb(description: &mut Description, value: &Value, types: &Value, kind: &str) {
    // Artwork type IDs are provider data, never hard-coded. Headshots use the
    // personImgURL from character records, not an invented person lookup.
    // https://github.com/thetvdb/v4-api/blob/main/docs/swagger.yml (2026-11-03).
    let record_type = if kind == "movie" { "movie" } else { "series" };
    let select = |needle: &str| {
        let ids: std::collections::BTreeSet<_> = types["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| {
                v["recordType"]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case(record_type))
                    && v["name"]
                        .as_str()
                        .is_some_and(|n| n.to_ascii_lowercase().contains(needle))
            })
            .filter_map(|v| v["id"].as_u64())
            .collect();
        best(
            value["artworks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| {
                    v["type"].as_u64().is_some_and(|id| ids.contains(&id))
                        && text(&v["image"]).is_some()
                }),
            "language",
            "score",
        )
        .and_then(|v| text(&v["image"]))
    };
    description.logo = select("logo");
    description.banner = select("banner");
    description.backdrop = select("background");
    description.genres = value["genres"]
        .as_array()
        .map(|values| values.iter().filter_map(|v| text(&v["name"])).collect());
    description.cast = value["characters"].as_array().map(|values| {
        values
            .iter()
            .filter(|v| {
                v["peopleType"].as_str().is_none_or(|t| {
                    matches!(
                        t.to_ascii_lowercase().as_str(),
                        "actor" | "guest star" | "gueststar"
                    )
                })
            })
            .filter_map(|v| {
                Some(Credit {
                    name: text(&v["personName"])?,
                    role: text(&v["name"]),
                    id: v["peopleId"]
                        .as_u64()
                        .filter(|id| *id > 0)
                        .map(|id| format!("tvdb:{id}")),
                    image: text(&v["personImgURL"]),
                })
            })
            .collect()
    });
}

pub(super) fn fanart(value: &Value, movie: bool) -> Description {
    // Fanart v3.2 returns all of these image roles in one title lookup.
    // https://github.com/fanart-tv/fanart.tv-api (2026-11-03).
    let select = |kinds: &[&str]| {
        best(
            kinds
                .iter()
                .flat_map(|kind| value[*kind].as_array().into_iter().flatten())
                .filter(|v| text(&v["url"]).is_some()),
            "lang",
            "likes",
        )
        .and_then(|v| text(&v["url"]))
    };
    Description {
        logo: select(if movie {
            &["hdmovielogo", "movielogo"]
        } else {
            &["hdtvlogo", "clearlogo"]
        }),
        banner: select(if movie {
            &["moviebanner"]
        } else {
            &["tvbanner"]
        }),
        backdrop: select(if movie {
            &["moviebackground", "movie4kbackground"]
        } else {
            &["showbackground", "show4kbackground"]
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tmdb_images_keep_people_identity_and_choose_language_before_votes() {
        let mut d = Description::default();
        tmdb(
            &mut d,
            &json!({
                "images": {"logos": [
                    {"iso_639_1":"fr","file_path":"/french.png","vote_average":10},
                    {"iso_639_1":"en","file_path":"/logo.svg","vote_average":1}],
                    "backdrops":[{"file_path":"/back.jpg","iso_639_1":null}]},
                "credits":{"cast":[{"id":7,"name":"Actor","character":"Role","profile_path":"/actor.jpg"}, {"id":8,"name":"No photo"}]}
            }),
        );
        assert_eq!(
            d.logo.as_deref(),
            Some("https://image.tmdb.org/t/p/original/logo.png")
        );
        assert_eq!(
            d.backdrop.as_deref(),
            Some("https://image.tmdb.org/t/p/original/back.jpg")
        );
        let cast = d.cast.unwrap();
        assert_eq!(cast[0].id.as_deref(), Some("tmdb:7"));
        assert_eq!(
            cast[0].image.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/actor.jpg")
        );
        assert!(cast[1].image.is_none());
    }

    #[test]
    fn tvdb_uses_supplied_artwork_types_and_person_headshots() {
        let types = json!({"data":[{"id":901,"recordType":"series","name":"ClearLogo"},
            {"id":902,"recordType":"series","name":"Background"},
            {"id":903,"recordType":"season","name":"ClearLogo"}]});
        let mut d = Description::default();
        tvdb(
            &mut d,
            &json!({"artworks":[{"type":903,"image":"season-logo","score":99},
            {"type":901,"image":"series-logo","language":"eng"},
            {"type":902,"image":"background","language":null}],
            "characters":[{"peopleId":10,"personName":"Actor","name":"Role","peopleType":"Actor","personImgURL":"headshot","image":"costume"}]}),
            &types,
            "show",
        );
        assert_eq!(d.logo.as_deref(), Some("series-logo"));
        assert_eq!(d.backdrop.as_deref(), Some("background"));
        let cast = d.cast.unwrap();
        assert_eq!(cast[0].id.as_deref(), Some("tvdb:10"));
        assert_eq!(cast[0].image.as_deref(), Some("headshot"));
    }

    #[test]
    fn fanart_uses_distinct_movie_and_tv_roles_and_stable_ranking() {
        let value = json!({"hdmovielogo":[{"url":"movie","lang":"en","likes":"2"}],
            "hdtvlogo":[{"url":"tv","lang":"en","likes":"1"}],
            "moviebanner":[{"url":"banner-z","lang":"en","likes":"4"},{"url":"banner-a","lang":"en","likes":"4"}],
            "showbackground":[{"url":"tv-background","lang":"00"}]});
        let movie = fanart(&value, true);
        assert_eq!(movie.logo.as_deref(), Some("movie"));
        assert_eq!(movie.banner.as_deref(), Some("banner-a"));
        let tv = fanart(&value, false);
        assert_eq!(tv.logo.as_deref(), Some("tv"));
        assert_eq!(tv.backdrop.as_deref(), Some("tv-background"));
        assert!(tv.artwork.is_none());
    }

    #[test]
    fn logo_language_preference_applies_across_standard_and_hd_variants() {
        let value = json!({"hdmovielogo":[{"url":"french-hd","lang":"fr","likes":"100"}],
            "movielogo":[{"url":"english","lang":"en","likes":"1"}]});
        assert_eq!(fanart(&value, true).logo.as_deref(), Some("english"));
    }
}
