//! Season covers are provider evidence, independent of episode catalogues.
//! Native anime entries are deliberately not assigned an invented season index.
use super::*;
use kahawai_mediadb as m;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SeasonPage {
    pub episodes: Vec<EpisodeData>,
    pub cover: m::ProviderSeasonArtwork,
}
fn numbered(number: u32, id: Option<String>, url: Option<String>) -> m::ProviderSeasonArtwork {
    m::ProviderSeasonArtwork {
        provider_id: id,
        scope: m::SeasonArtworkScope::Season { number },
        artwork: url.into_iter().collect(),
    }
}
pub(super) fn native_cover(record: &m::ProviderRecord) -> m::ProviderSeasonArtwork {
    m::ProviderSeasonArtwork {
        provider_id: Some(record.external_id.clone()),
        scope: m::SeasonArtworkScope::NativeEntry,
        artwork: record.description.artwork.clone().unwrap_or_default(),
    }
}
fn id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}
fn image(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
pub(super) fn tmdb_seasons(value: &Value) -> Vec<(i64, i64)> {
    let mut seasons: Vec<_> = value["seasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let n = v["season_number"].as_i64()?;
            (n >= 0).then_some((n, v["episode_count"].as_i64().unwrap_or(0)))
        })
        .collect();
    seasons.sort_unstable();
    seasons.dedup_by_key(|s| s.0);
    seasons
}
pub(super) fn tmdb_page(value: &Value, season: i64) -> Result<SeasonPage> {
    let number = u32::try_from(season)?;
    // TMDB season details retain the provider's selected poster_path and id:
    // https://developer.themoviedb.org/reference/tv-season-details (2026-10-13).
    let cover = numbered(
        number,
        id(&value["id"]),
        image(&value["poster_path"]).map(|p| format!("https://image.tmdb.org/t/p/w500{p}")),
    );
    let episodes = value["episodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            Ok(EpisodeData {
                provider_id: id(&e["id"]).context("TMDB episode id missing")?,
                season: Some(season),
                episode: e["episode_number"]
                    .as_i64()
                    .context("TMDB episode number missing")?,
                absolute: None,
                title: image(&e["name"]),
                overview: image(&e["overview"]),
                image: image(&e["still_path"]),
                aired: image(&e["air_date"]),
                rating: e["vote_average"].as_f64(),
            })
        })
        .collect::<Result<_>>()?;
    Ok(SeasonPage { episodes, cover })
}
pub(super) fn tmdb_poster(images: &Value) -> Option<String> {
    let selected = images["posters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| image(&p["file_path"]).is_some())
        .min_by(|a, b| {
            language_rank(&a["iso_639_1"])
                .cmp(&language_rank(&b["iso_639_1"]))
                .then_with(|| score(&b["vote_average"]).total_cmp(&score(&a["vote_average"])))
                .then_with(|| b["vote_count"].as_u64().cmp(&a["vote_count"].as_u64()))
                .then_with(|| image(&a["file_path"]).cmp(&image(&b["file_path"])))
        })?;
    image(&selected["file_path"]).map(|p| format!("https://image.tmdb.org/t/p/w500{p}"))
}
fn language_rank(value: &Value) -> u8 {
    match value.as_str().unwrap_or("") {
        "en" | "eng" | "en-US" => 0,
        "" | "00" | "null" => 1,
        _ => 2,
    }
}
fn score(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}
fn choose<'a>(
    images: impl Iterator<Item = &'a Value>,
    language: &str,
    ranking: &str,
) -> Option<&'a Value> {
    images.min_by(|a, b| {
        language_rank(&a[language])
            .cmp(&language_rank(&b[language]))
            .then_with(|| score(&b[ranking]).total_cmp(&score(&a[ranking])))
            .then_with(|| id(&a["id"]).cmp(&id(&b["id"])))
            .then_with(|| {
                image(&a["url"])
                    .or_else(|| image(&a["image"]))
                    .cmp(&image(&b["url"]).or_else(|| image(&b["image"])))
            })
    })
}
fn poster_types(types: &Value) -> BTreeSet<u64> {
    types["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| {
            v["recordType"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("season"))
                && v["name"]
                    .as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("poster"))
        })
        .filter_map(|v| v["id"].as_u64())
        .collect()
}
fn tvdb_seasons(series: &Value) -> Vec<&Value> {
    let mut seasons: Vec<_> = series["seasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| {
            series["defaultSeasonType"]
                .as_u64()
                .is_some_and(|t| s["type"]["id"].as_u64() == Some(t))
        })
        .filter(|s| {
            s["number"]
                .as_u64()
                .is_some_and(|n| u32::try_from(n).is_ok())
        })
        .collect();
    seasons.sort_by_key(|s| (s["number"].as_u64(), s["id"].as_u64()));
    seasons.dedup_by_key(|s| s["number"].as_u64());
    seasons
}
fn tvdb_cover(season: &Value, types: &BTreeSet<u64>) -> m::ProviderSeasonArtwork {
    let default = season["imageType"]
        .as_u64()
        .filter(|t| types.contains(t))
        .and_then(|_| image(&season["image"]));
    let selected = default.or_else(|| {
        choose(
            season["artwork"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| {
                    v["type"].as_u64().is_some_and(|t| types.contains(&t))
                        && image(&v["image"]).is_some()
                }),
            "language",
            "score",
        )
        .and_then(|v| image(&v["image"]))
    });
    numbered(
        season["number"].as_u64().unwrap_or(0) as u32,
        id(&season["id"]),
        selected,
    )
}
fn fanart_covers(value: &Value) -> Vec<m::ProviderSeasonArtwork> {
    let mut seasons: BTreeMap<u32, Vec<&Value>> = BTreeMap::new();
    for image in value["seasonposter"].as_array().into_iter().flatten() {
        let number = image["season"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .or_else(|| image["season"].as_u64().and_then(|v| u32::try_from(v).ok()));
        if let Some(number) = number
            && reqwest::Url::parse(image["url"].as_str().unwrap_or("")).is_ok_and(|u| {
                u.scheme() == "https"
                    && u.host_str()
                        .is_some_and(|h| h == "fanart.tv" || h.ends_with(".fanart.tv"))
            })
        {
            seasons.entry(number).or_default().push(image);
        }
    }
    seasons
        .into_iter()
        .filter_map(|(number, images)| {
            let chosen = choose(images.into_iter(), "lang", "likes")?;
            Some(numbered(number, id(&chosen["id"]), image(&chosen["url"])))
        })
        .collect()
}
impl Enricher {
    pub(super) async fn tvdb_season_covers(
        &self,
        registry: &Registry,
        token: &str,
        series: &Value,
        lease: &crate::gate::CredentialLease,
    ) -> Result<Vec<m::ProviderSeasonArtwork>> {
        let seasons = tvdb_seasons(series);
        if seasons.is_empty() {
            return Ok(vec![]);
        }
        // Read type IDs from the provider, never assume IDs or alternate season
        // orders. The official v4 schema links imageType/type to /artwork/types.
        // https://github.com/thetvdb/v4-api/blob/master/docs/swagger.yml (2026-10-13).
        let types: Value = if let Some(cached) = registry
            .catalogue()
            .recent_cache_answer(
                "tvdb-pages",
                "season-artwork-types-v1",
                catalogue::now() - 7 * 24 * 3600,
            )
            .await?
        {
            serde_json::from_str(&cached)?
        } else {
            let types: Value = self
                .http
                .send_current(
                    self.http
                        .get("https://api4.thetvdb.com/v4/artwork/types")
                        .bearer_auth(token),
                    lease.clone(),
                )
                .await?
                .status_checked()?
                .json()
                .await?;
            registry
                .catalogue()
                .put_cache_answer(
                    "tvdb-pages",
                    "season-artwork-types-v1",
                    &serde_json::to_string(&types)?,
                    catalogue::now(),
                )
                .await?;
            types
        };
        let types = poster_types(&types);
        let mut out = vec![];
        for season in seasons {
            let mut cover = tvdb_cover(season, &types);
            if cover.artwork.is_empty() {
                let season_id = id(&season["id"]).context("TVDB season id missing")?;
                let question = format!("season-artwork-v1:{season_id}");
                let value: Value = if let Some(cached) = registry
                    .catalogue()
                    .recent_cache_answer("tvdb-pages", &question, catalogue::now() - 7 * 24 * 3600)
                    .await?
                {
                    serde_json::from_str(&cached)?
                } else {
                    let value: Value = self
                        .http
                        .send_current(
                            self.http
                                .get(format!(
                                    "https://api4.thetvdb.com/v4/seasons/{season_id}/extended"
                                ))
                                .bearer_auth(token),
                            lease.clone(),
                        )
                        .await?
                        .status_checked()?
                        .json()
                        .await?;
                    registry
                        .catalogue()
                        .put_cache_answer(
                            "tvdb-pages",
                            &question,
                            &serde_json::to_string(&value)?,
                            catalogue::now(),
                        )
                        .await?;
                    value
                };
                // Keep identity/number from the matching default-order base record.
                let mut extended = value["data"].clone();
                extended["id"] = season["id"].clone();
                extended["number"] = season["number"].clone();
                cover = tvdb_cover(&extended, &types);
            }
            out.push(cover);
        }
        Ok(out)
    }
    pub(super) async fn catalogue_fanart_seasons(
        &self,
        registry: &Registry,
        input: &m::EnrichmentInput,
        force: bool,
    ) -> Result<m::EnrichmentAnswer> {
        let Some((_, selected)) = &input.selected else {
            return Ok(Default::default());
        };
        let tvdb = if selected.provider == "tvdb" && selected.namespace == "show" {
            Some(selected.external_id.as_str())
        } else {
            input
                .links
                .iter()
                .find(|l| l.provider == "tvdb" && l.namespace == "show")
                .map(|l| l.external_id.as_str())
        };
        let Some(tvdb) = tvdb else {
            return Ok(Default::default());
        };
        let question = format!("season-artwork-v1:tvdb:{tvdb}");
        let covers: Vec<m::ProviderSeasonArtwork> = if !force
            && let Some(cached) = registry
                .catalogue()
                .cache_answer("fanart", &question)
                .await?
        {
            serde_json::from_str(&cached)?
        } else {
            catalogue::provider_ready(registry.catalogue(), "fanart").await?;
            let key = fanart_client_key(registry)
                .await?
                .ok_or(catalogue::Unconfigured)?;
            // Fanart TV v3 TV requests use the verified TVDB series ID. Reuse the
            // existing personal-key header and host gate used by artist artwork.
            // https://api.fanart.tv/ and https://github.com/fanart-tv/fanart.tv-api (2026-10-13).
            let response = self
                .http
                .send_current(
                    self.http
                        .get_no_redirect(format!("https://webservice.fanart.tv/v3.2/tv/{tvdb}"))
                        .header("client-key", key),
                    self.provider_lease(FANART),
                )
                .await?;
            let covers = if response.status() == reqwest::StatusCode::NOT_FOUND {
                vec![]
            } else {
                fanart_covers(&response.status_checked()?.json().await?)
            };
            registry
                .catalogue()
                .put_cache_answer(
                    "fanart",
                    &question,
                    &serde_json::to_string(&covers)?,
                    catalogue::now(),
                )
                .await?;
            covers
        };
        let record = m::ProviderRecord {
            provider: "fanart".into(),
            namespace: "show".into(),
            external_id: tvdb.into(),
            title: input.title.clone(),
            year: input.year,
            media_type: m::MediaType::Series,
            language: "en".into(),
            description: Default::default(),
            children: None,
            season_artwork: Some(covers),
        };
        // Auxiliary evidence has no descriptive identity or episode catalogue.
        Ok(m::EnrichmentAnswer {
            artwork_supplement: Some(record),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn tmdb_retains_specials_and_poster_without_inventing_absolute_numbers() {
        let value =
            json!({"id":42,"poster_path":"/poster.jpg", "episodes":[{"id":7,"episode_number":1}]});
        let page = tmdb_page(&value, 0).unwrap();
        assert_eq!(page.cover.provider_id.as_deref(), Some("42"));
        assert_eq!(
            page.cover.scope,
            m::SeasonArtworkScope::Season { number: 0 }
        );
        assert_eq!(page.episodes[0].absolute, None);
        assert_eq!(
            page.cover.artwork,
            ["https://image.tmdb.org/t/p/w500/poster.jpg"]
        );
        assert_eq!(
            tmdb_seasons(
                &json!({"seasons":[{"season_number":2},{"season_number":0},{"season_number":1}]})
            ),
            [(0, 0), (1, 0), (2, 0)]
        );
    }
    #[test]
    fn tvdb_only_uses_default_order_posters_and_prefers_provider_default() {
        let value = json!({"defaultSeasonType":9,"seasons":[{"id":1,"number":1,"type":{"id":8}},{"id":2,"number":0,"type":{"id":9}}]});
        assert_eq!(tvdb_seasons(&value).len(), 1);
        let types = poster_types(
            &json!({"data":[{"id":99,"recordType":"season","name":"Season Poster"},{"id":11,"recordType":"season","name":"Season Banner"}]}),
        );
        let season = json!({"id":2,"number":0,"imageType":99,"image":"default","artwork":[{"id":1,"type":99,"language":"eng","image":"english","score":100}]});
        assert_eq!(tvdb_cover(&season, &types).artwork, ["default"]);
        let mut season = season;
        season["imageType"] = json!(11);
        assert_eq!(tvdb_cover(&season, &types).artwork, ["english"]);
    }
    #[test]
    fn fanart_filters_non_seasons_and_selects_language_then_likes_stably() {
        let value = json!({"seasonposter":[
            {"id":"1","season":"all","url":"https://assets.fanart.tv/all.jpg"},
            {"id":"2","season":"0","url":"https://assets.fanart.tv/special.jpg"},
            {"id":"3","season":"1","lang":"fr","likes":"99","url":"https://assets.fanart.tv/fr.jpg"},
            {"id":"5","season":"1","lang":"en","likes":"1","url":"https://assets.fanart.tv/en5.jpg"},
            {"id":"4","season":"1","lang":"en","likes":"1","url":"https://assets.fanart.tv/en4.jpg"}
        ]});
        let covers = fanart_covers(&value);
        assert_eq!(covers.len(), 2);
        assert_eq!(covers[1].artwork, ["https://assets.fanart.tv/en4.jpg"]);
    }
}
