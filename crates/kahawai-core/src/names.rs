//! Filename → candidate identity parsing (HUB-4), v1: movies.
//! The fansub tokenizer for anime (HUB-30) is a separate, later variant.

#[derive(Debug, Clone, PartialEq)]
pub struct MovieGuess {
    pub title: String,
    pub year: Option<u16>,
}

/// Parse a movie filename: `The.Matrix.1999.1080p.x264-GRP.mkv` →
/// title "The Matrix", year 1999. The *last* plausible year wins, so
/// `2001 A Space Odyssey (1968)` keeps its numeric title.
pub fn parse_movie(filename: &str) -> MovieGuess {
    let stem = filename.rsplit_once('.').map_or(filename, |(s, _)| s);
    let cleaned: String = stem
        .chars()
        .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
        .collect();

    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let mut year = None;
    let mut title_end = tokens.len();
    for (i, tok) in tokens.iter().enumerate().rev() {
        if let Some(y) = parse_year_token(tok) {
            // A year as the very first token is a title, not a release year.
            if i > 0 {
                year = Some(y);
                title_end = i;
                break;
            }
        }
    }

    let mut title = tokens[..title_end].join(" ");
    // Strip unclosed release junk like a trailing "(" or "[".
    while title.ends_with(['(', '[', '-', ' ']) {
        title.pop();
    }
    if title.is_empty() {
        title = tokens.join(" ");
    }
    MovieGuess { title, year }
}

fn parse_year_token(tok: &str) -> Option<u16> {
    let t = tok.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']'));
    if t.len() != 4 {
        return None;
    }
    let y: u16 = t.parse().ok()?;
    (1900..=2099).contains(&y).then_some(y)
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeGuess {
    pub show_title: String,
    pub show_year: Option<u16>,
    /// None = absolute numbering (anime): the episode number is the
    /// whole identity, season views are a later projection (HUB-31).
    pub season: Option<u32>,
    pub episode: u32,
    pub episode_title: Option<String>,
}

/// Parse a series path (`Show/Season 1/Show - S01E02 - Name.mkv`) into
/// show + episode identity. SxxEyy in the filename is authoritative
/// (multi-episode files keep the first number — ponytail: multi-episode
/// items later); the show directory names the show (fansub-free), the
/// filename is the fallback. None → unparseable, goes unresolved (the
/// review queue's job, later).
pub fn parse_episode(path_rel: &str) -> Option<EpisodeGuess> {
    let parts: Vec<&str> = path_rel.split('/').collect();
    let filename = parts.last()?;
    let stem = filename.rsplit_once('.').map_or(*filename, |(s, _)| s);
    let cleaned: String = stem
        .chars()
        .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
        .collect();
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let (idx, season, episode) = tokens
        .iter()
        .enumerate()
        .find_map(|(i, t)| parse_sxxeyy(t).or_else(|| parse_nnxnn(t)).map(|(s, e)| (i, s, e)))?;

    // Show identity: the top-level directory when there is one (skipping
    // season dirs), else the filename tokens before SxxEyy.
    let show_dir = parts
        .iter()
        .rev()
        .skip(1) // filename
        .find(|d| !is_season_dir(d))
        .copied();
    let show_guess = match show_dir {
        Some(dir) => parse_movie(dir),
        None => parse_movie(&tokens[..idx].join(" ")),
    };
    let show_title = if show_guess.title.is_empty() {
        // e.g. bare "S01E01.mkv" at top level
        "Unknown Show".to_string()
    } else {
        show_guess.title
    };

    // Episode title: whatever follows SxxEyy, minus separators/junk.
    let mut ep_title = tokens[idx + 1..].join(" ");
    while ep_title.starts_with(['-', ' ']) {
        ep_title.remove(0);
    }
    while ep_title.ends_with(['(', '[', '-', ' ']) {
        ep_title.pop();
    }
    Some(EpisodeGuess {
        show_title,
        show_year: show_guess.year,
        season: Some(season),
        episode,
        episode_title: (!ep_title.is_empty()).then_some(ep_title),
    })
}

/// `01x02` / `1x02` season-x-episode (Pokemon-style library naming).
fn parse_nnxnn(tok: &str) -> Option<(u32, u32)> {
    let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let (s, e) = t.split_once(['x', 'X'])?;
    if s.is_empty() || e.is_empty() || s.len() > 2 || e.len() > 3 {
        return None;
    }
    Some((s.parse().ok()?, e.parse().ok()?))
}

/// Fansub tokenizer (HUB-30, first slice): `[Group]_Title_-_01v2_(tags)_
/// [CRC].mkv` → title + absolute episode. Falls back to the standard
/// series parser first (plenty of anime is named SxxEyy). Providers and
/// the season-view mapping come later; group/version/CRC are parsed
/// past, not yet stored.
pub fn parse_anime(path_rel: &str) -> Option<EpisodeGuess> {
    if let Some(mut g) = parse_episode(path_rel) {
        // Standard-named anime, but prefer the top-level dir identity
        // (release subdirs like "Title (720p) [Group]" mislead).
        if let Some(top) = top_dir(path_rel) {
            let tg = parse_movie(top);
            g.show_title = tg.title;
            g.show_year = tg.year.or(g.show_year);
        }
        return Some(g);
    }

    let parts: Vec<&str> = path_rel.split('/').collect();
    let filename = parts.last()?;
    let stem = filename.rsplit_once('.').map_or(*filename, |(s, _)| s);

    // Strip bracket/paren tag groups: [Group] [720p] [A1B2C3D4] (BD FLAC).
    let mut cleaned = String::with_capacity(stem.len());
    let mut depth = 0u32;
    for c in stem.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                cleaned.push(if matches!(c, '.' | '_') { ' ' } else { c })
            }
            _ => {}
        }
    }
    let tokens: Vec<&str> = cleaned.split_whitespace().filter(|t| *t != "-").collect();

    // Absolute episode: an explicit E01/EP01 token wins; otherwise the
    // LAST standalone number (optional vN) that isn't a plausible year.
    let e_token = tokens.iter().enumerate().find_map(|(i, t)| {
        let rest = t
            .strip_prefix(['e', 'E'])
            .map(|r| r.strip_prefix(['p', 'P']).unwrap_or(r))?;
        if rest.is_empty() || rest.len() > 4 || !rest.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some((i, rest.parse().ok()?))
    });
    let (idx, episode) = e_token.or_else(|| {
        tokens.iter().enumerate().rev().find_map(|(i, t)| {
            let num = t.split(['v', 'V']).next()?;
            if num.is_empty() || num.len() > 4 || !num.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: u32 = num.parse().ok()?;
            if (1900..=2099).contains(&n) {
                return None; // year, not an episode
            }
            Some((i, n))
        })
    })?;

    let mut title = tokens[..idx].join(" ");
    if title.is_empty() {
        title = top_dir(path_rel).unwrap_or("Unknown Show").to_string();
    }
    let show_guess = match top_dir(path_rel) {
        Some(top) => parse_movie(top),
        None => parse_movie(&title),
    };
    let ep_title = tokens[idx + 1..].join(" ");
    Some(EpisodeGuess {
        show_title: if show_guess.title.is_empty() { title } else { show_guess.title },
        show_year: show_guess.year,
        season: None, // absolute numbering is authoritative
        episode,
        episode_title: (!ep_title.is_empty()).then_some(ep_title),
    })
}

fn top_dir(path_rel: &str) -> Option<&str> {
    let mut parts = path_rel.split('/');
    let first = parts.next()?;
    parts.next().map(|_| first) // only when there IS a directory
}

fn parse_sxxeyy(tok: &str) -> Option<(u32, u32)> {
    let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let rest = t.strip_prefix(['s', 'S'])?;
    let e_pos = rest.find(['e', 'E'])?;
    let (s, e_part) = rest.split_at(e_pos);
    let e = &e_part[1..];
    // Multi-episode: S01E01E02 / S01E01-E02 — first episode wins.
    let e_first: String = e.chars().take_while(|c| c.is_ascii_digit()).collect();
    if s.is_empty() || e_first.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((s.parse().ok()?, e_first.parse().ok()?))
}

fn is_season_dir(dir: &str) -> bool {
    let d = dir.to_ascii_lowercase();
    d.starts_with("season") || d.starts_with("staffel") || d.starts_with("series ")
        || d.trim().parse::<u32>().is_ok()
        || d.starts_with("specials")
}

/// Normalization key for dedup (HUB-3): lowercase alphanumerics, single
/// spaces.
pub fn normalize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_space = true;
    for c in title.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_episodes() {
        let cases = [
            (
                "Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv",
                "Andor", None, 1, 2, Some("That Would Be Me"),
            ),
            (
                "The Wire (2002)/Season 3/The.Wire.S03E11.Middle.Ground.720p.mkv",
                "The Wire", Some(2002), 3, 11, Some("Middle Ground 720p"),
            ),
            ("Alphas/alphas s02e05.mkv", "Alphas", None, 2, 5, None),
            ("Show/Specials/Show - S00E01 - Pilot.mkv", "Show", None, 0, 1, Some("Pilot")),
            ("Lost/Season 1/Lost - S01E01E02 - Pilot.mkv", "Lost", None, 1, 1, Some("Pilot")),
        ];
        for (path, show, year, s, e, ep_title) in cases {
            let g = parse_episode(path).unwrap_or_else(|| panic!("no parse: {path}"));
            assert_eq!(g.show_title, show, "{path}");
            assert_eq!(g.show_year, year, "{path}");
            assert_eq!((g.season, g.episode), (Some(s), e), "{path}");
            assert_eq!(g.episode_title.as_deref(), ep_title, "{path}");
        }
        assert!(parse_episode("Movies/Heat (1995).mkv").is_none());
    }

    #[test]
    fn parses_anime_shapes() {
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, &str, Option<u16>, Option<u32>, u32)] = &[
            (
                "Ao No Exorcist/Ao no Exorcist (720p, BluRay) [Coalgirls]/[Coalgirls]_Ao_no_Exorcist_11_(1280x720_Blu-Ray_FLAC)_[865A19CF].mkv",
                "Ao No Exorcist", None, None, 11,
            ),
            (
                "Dragon Ball Super/[AnimeRG] Dragon Ball Super - 001 [720p] [x264] [pseudo].mkv",
                "Dragon Ball Super", None, None, 1,
            ),
            (
                "Hellsing Ultimate/[CBM]_Hellsing_Ultimate_-_01_-_[1080p-AC3]_[7B4A1D84].mkv",
                "Hellsing Ultimate", None, None, 1,
            ),
            (
                "Pokemon/Season 01/Pokemon 01x01 Pokemon! I Choose You!.mkv",
                "Pokemon", None, Some(1), 1,
            ),
            (
                "Rozen Maiden (2013)/Rozen Maiden (2013) - S01E01 - Alice Game.mkv",
                "Rozen Maiden", Some(2013), Some(1), 1,
            ),
            (
                "Serial Experiments Lain/Serial.Experiments.Lain.E01.1080p.Bluray.AV1.Opus.DualAudio-AeTHER.mkv",
                "Serial Experiments Lain", None, None, 1,
            ),
            // Episode version markers.
            ("Show/[Grp] Show - 05v2 [720p].mkv", "Show", None, None, 5),
        ];
        for (path, show, year, season, ep) in cases {
            let g = parse_anime(path).unwrap_or_else(|| panic!("no parse: {path}"));
            assert_eq!(g.show_title, *show, "{path}");
            assert_eq!(g.show_year, *year, "{path}");
            assert_eq!(g.season, *season, "{path}");
            assert_eq!(g.episode, *ep, "{path}");
        }
    }

    #[test]
    fn parses_common_shapes() {
        let cases = [
            ("Heat (1995).mkv", "Heat", Some(1995)),
            ("The.Matrix.1999.1080p.BluRay.x264-GRP.mkv", "The Matrix", Some(1999)),
            ("Moana 2 (2024).mkv", "Moana 2", Some(2024)),
            ("2001 A Space Odyssey (1968).mkv", "2001 A Space Odyssey", Some(1968)),
            ("Primer.mkv", "Primer", None),
            ("Blade_Runner_[1982]_Final_Cut.mkv", "Blade Runner", Some(1982)),
        ];
        for (input, title, year) in cases {
            let g = parse_movie(input);
            assert_eq!((g.title.as_str(), g.year), (title, year), "input: {input}");
        }
    }

    #[test]
    fn normalizes_for_dedup() {
        assert_eq!(normalize_title("The Matrix"), "the matrix");
        assert_eq!(normalize_title("Heat!"), "heat");
        assert_eq!(
            normalize_title("Léon: The Professional"),
            normalize_title("léon the professional")
        );
    }
}
