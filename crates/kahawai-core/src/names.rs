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
    pub season: u32,
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
    let (idx, season, episode) =
        tokens.iter().enumerate().find_map(|(i, t)| parse_sxxeyy(t).map(|(s, e)| (i, s, e)))?;

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
        season,
        episode,
        episode_title: (!ep_title.is_empty()).then_some(ep_title),
    })
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
            assert_eq!((g.season, g.episode), (s, e), "{path}");
            assert_eq!(g.episode_title.as_deref(), ep_title, "{path}");
        }
        assert!(parse_episode("Movies/Heat (1995).mkv").is_none());
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
