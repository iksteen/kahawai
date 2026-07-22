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
