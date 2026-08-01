//! Filename → candidate identity parsing (HUB-4), v1: movies.
//! The fansub tokenizer for anime (HUB-30) is a separate, later variant.

#[derive(Debug, Clone, PartialEq)]
pub struct MovieGuess {
    pub title: String,
    pub year: Option<u16>,
    /// CD1/CD2-era multi-part rips: 1-based part number, None = whole film.
    pub part: Option<u32>,
}

/// Parse a movie filename: `The.Matrix.1999.1080p.x264-GRP.mkv` →
/// title "The Matrix", year 1999. The *last* plausible year wins, so
/// `2001 A Space Odyssey (1968)` keeps its numeric title.
/// Strip trailing parenthetical RELEASE tags from a name: "(Dual-Audio)",
/// "(Eng.-Dub)", "(Uncut)", "(OVA)". A borrowed collection titled every
/// directory this way, and the tag rode into show titles, poisoning
/// provider matching and deduplication — "Hellsing Ultimate (Dual-Audio)"
/// is not a different show from "Hellsing Ultimate". A parenthetical is
/// stripped only when one of its words is unambiguously release-speak,
/// so "(Director's Cut)" and a bare year survive.
pub fn strip_release_tags(name: &str) -> String {
    const TAGS: &[&str] = &[
        "dual",
        "audio",
        "dub",
        "dubbed",
        "sub",
        "subbed",
        "subs",
        "eng",
        "uncut",
        "ova",
        "ona",
        "remaster",
        "remastered",
        "batch",
        "uncensored",
    ];
    let mut out = name.trim_end().to_string();
    while let Some(open) = out.rfind('(') {
        if !out.ends_with(')') {
            break;
        }
        let inner = &out[open + 1..out.len() - 1];
        let releaseish = inner
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .any(|w| TAGS.contains(&w.to_ascii_lowercase().as_str()));
        if !releaseish {
            break;
        }
        out.truncate(open);
        out = out.trim_end().to_string();
    }
    out
}

pub fn parse_movie(filename: &str) -> MovieGuess {
    let filename = &strip_release_tags(filename);
    // Strip the suffix only when it plausibly IS a file extension:
    // 2-4 alphanumerics with at least one letter. Show directories like
    // "Mr. Robot" reach here too — " Robot" is a title, not an ext, and
    // "Show.2004"'s year must survive.
    let stem = match filename.rsplit_once('.') {
        Some((s, ext))
            if (2..=4).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic()) =>
        {
            s
        }
        _ => filename,
    };
    parse_movie_stem(stem)
}

/// Same parse as `parse_movie`, minus the trailing-`.xxx`-looks-like-an-
/// extension strip — for directory names and other non-file strings,
/// which never have a real extension to drop. Without this, a dot-glued
/// two-word directory name ("30.Rock") loses its second word: "Rock" is
/// 4 alphanumeric characters with a letter in it, indistinguishable from
/// a plausible extension by the heuristic above.
fn parse_movie_dir(name: &str) -> MovieGuess {
    parse_movie_stem(&strip_release_tags(name))
}

fn parse_movie_stem(stem: &str) -> MovieGuess {
    let cleaned: String = stem
        .chars()
        .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
        .collect();

    let mut tokens: Vec<&str> = cleaned.split_whitespace().collect();
    // Part token: "CD1", "cd 2", "Disc1" — as its own token or
    // marker+digit pair. Deliberately ONLY the cd/disc family:
    // "Part 2" is a legitimate title suffix (Deathly Hallows), and
    // merging those would be catastrophic.
    let mut part = None;
    let part_of = |tok: &str| -> Option<u32> {
        let t = tok.to_ascii_lowercase();
        for marker in ["cd", "disc", "disk"] {
            if let Some(rest) = t.strip_prefix(marker)
                && !rest.is_empty()
                && rest.len() <= 2
                && rest.chars().all(|c| c.is_ascii_digit())
            {
                return rest.parse().ok();
            }
        }
        None
    };
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].trim_matches(['(', ')', '[', ']', '-']);
        if let Some(n) = part_of(tok) {
            part = Some(n);
            tokens.remove(i);
            continue;
        }
        // Split form: "cd 2" / "disc 1".
        if matches!(tok.to_ascii_lowercase().as_str(), "cd" | "disc" | "disk")
            && i + 1 < tokens.len()
            && tokens[i + 1].len() <= 2
            && tokens[i + 1].chars().all(|c| c.is_ascii_digit())
        {
            part = tokens[i + 1].parse().ok();
            tokens.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }

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
    MovieGuess { title, year, part }
}

/// Movie identity for a file that has NO episode shape at all — the
/// last thing tried for an anime collection, where a film sits beside
/// the series (HUB-30). Yearless is fine: "Akira.mkv" is as much a film
/// as "Akira (1988).mkv", and requiring a year left 23 of them bare.
///
/// The filename names the film, with one exception that matters: a bare
/// `partN` names a PIECE of one. `Nescaflowne (Eng,-Audio)/part1.mp4`
/// through `part7.mp4` is a single film in seven parts, and taking the
/// filename literally would mint seven — so identity falls to the
/// directory and the number becomes the part, which is the same shape
/// as the CD1/CD2 rips `parse_movie` already folds.
///
/// The directory is used ONLY then. A flat `Movies/Akira.mkv` would
/// otherwise resolve to a film called "Movies".
pub fn parse_movie_file(path_rel: &str) -> Option<MovieGuess> {
    let filename = path_rel.rsplit('/').next()?;
    let mg = parse_movie(filename);
    if let Some(n) = bare_part(&mg.title) {
        let dir = path_rel.rsplit('/').nth(1)?;
        let d = parse_movie_dir(dir);
        if d.title.is_empty() {
            return None;
        }
        return Some(MovieGuess {
            title: d.title,
            year: d.year,
            part: Some(n),
        });
    }
    (!mg.title.is_empty()).then_some(mg)
}

/// `part3` / `Part 3` standing alone as the whole title — a piece, not
/// a film. Deliberately only the bare form: "Part 2" as a title SUFFIX
/// ("Deathly Hallows Part 2") is a real film and must survive, which is
/// why `parse_movie` refuses to fold the part family in the first place.
fn bare_part(title: &str) -> Option<u32> {
    let t = title.trim().to_ascii_lowercase().replace(' ', "");
    let n = t.strip_prefix("part")?;
    if n.is_empty() || n.len() > 2 || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
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
pub struct MusicGuess {
    pub artist: String,
    pub album: String,
    pub album_year: Option<i64>,
    pub disc: Option<u32>,
    pub track: u32,
    pub title: String,
}

/// Filename fallback for untagged music, matching the Lidarr layout:
/// `Artist/Album (Year)/[Artist - Album - ][D-]NN [- ]Title.ext`.
/// Tags always win — this only fires when they're missing.
pub fn parse_music(path_rel: &str) -> Option<MusicGuess> {
    let mut parts = path_rel.rsplitn(3, '/');
    let file = parts.next()?;
    let album_dir = parts.next()?;
    let artist_dir = parts.next().unwrap_or("");

    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    // Album dir: "Name (1997)" or bare "Name".
    let (album, album_year) = match album_dir.rfind('(') {
        Some(i) if album_dir.ends_with(')') => {
            let inner = &album_dir[i + 1..album_dir.len() - 1];
            match inner.parse::<i64>() {
                Ok(y) if (1900..2100).contains(&y) => (album_dir[..i].trim(), Some(y)),
                _ => (album_dir.trim(), None),
            }
        }
        _ => (album_dir.trim(), None),
    };
    if album.is_empty() {
        return None;
    }

    // Track number = first standalone number segment; everything after
    // it is the title. Segments split on " - ".
    let segs: Vec<&str> = stem.split(" - ").collect();
    let num_at = segs.iter().position(|s| {
        let t = s.trim();
        !t.is_empty() && t.len() <= 4 && t.chars().all(|c| c.is_ascii_digit())
    })?;
    let track: u32 = segs[num_at].trim().parse().ok()?;
    let title = segs[num_at + 1..].join(" - ");
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let artist = if artist_dir.is_empty() {
        segs.first()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())?
            .to_string()
    } else {
        artist_dir.trim().to_string()
    };
    Some(MusicGuess {
        artist,
        album: album.to_string(),
        album_year,
        disc: None,
        track,
        title: title.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeGuess {
    pub show_title: String,
    pub show_year: Option<u16>,
    /// None = absolute numbering (anime): the episode number is the
    /// whole identity, season views are a later projection (HUB-31).
    pub season: Option<u32>,
    pub episode: u32,
    /// Batch marker (HUB-30): the file SPANS `episode..=episode_end`
    /// ("OVA 1-2", "S01E01-E02"). None = a single episode. The scan
    /// creates one item per number, all sharing the file.
    pub episode_end: Option<u32>,
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
    let cleaned = split_glued_marker(&cleaned);
    let cleaned = join_split_marker(&cleaned);
    let cleaned = split_trailing_marker(&cleaned);
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let lettered = tokens.iter().enumerate().find_map(|(i, t)| {
        parse_sxxeyy(t)
            .or_else(|| parse_nnxnn(t).map(|(s, e)| (s, e, None)))
            .map(|(s, e, end)| (i, s, e, end))
    });
    // Compact scene numbering is tried ONLY when nothing lettered
    // matched, which is what bounds its blast radius to files that
    // resolve to nothing today.
    let (idx, season, episode, episode_end) = match lettered {
        Some(found) => found,
        None => {
            let (i, s, e) = parse_scene_compact(stem, &tokens)?;
            (i, s, e, None)
        }
    };

    // Show identity: the top-level directory when there is one (skipping
    // season dirs), else the filename tokens before SxxEyy.
    let show_dir = parts
        .iter()
        .rev()
        .skip(1) // filename
        .find(|d| !is_season_dir(d))
        .copied();
    let show_guess = match show_dir {
        Some(dir) => parse_movie_dir(dir),
        None => parse_movie_dir(&tokens[..idx].join(" ")),
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
        episode_end,
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
            let tg = parse_movie_dir(top);
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
            _ if depth == 0 => cleaned.push(if matches!(c, '.' | '_') { ' ' } else { c }),
            _ => {}
        }
    }
    let tokens: Vec<&str> = cleaned.split_whitespace().filter(|t| *t != "-").collect();

    // HUB-30 release designations: NCOP/NCED (creditless op/ed), OVA/
    // OAV/ONA/SP/SPECIAL, and MOVIE. Consulted only in THIS branch — a
    // SxxEyy name never reaches it, which is what keeps "Houndoom's
    // Special Delivery" and "Zorua The Movie!" (real episode titles in
    // the live collection) out of the specials bin. The designator's
    // index is its attached digits (NCOP2) or the immediately following
    // number ("OVA 2" — consumed, so it cannot double as an episode
    // number), defaulting to 1. A remaining standalone episode number
    // still wins: "[Grp] Show - 05 Special Training" is episode 5.
    // Among several designator tokens, one carrying an EXPLICIT index
    // (attached digits, a following number/range/roman) outranks an
    // earlier indexless one: in "Kite Special Edition Uncut OVA 1-2"
    // the adjective "Special" must not shadow the real "OVA 1-2".
    let designator = {
        let mut cands = tokens.iter().enumerate().filter_map(|(i, t)| {
            let split = t.len() - t.chars().rev().take_while(|c| c.is_ascii_digit()).count();
            let (base, digits) = t.split_at(split);
            let band = match base.to_ascii_uppercase().as_str() {
                "NCOP" => Some(Some(100u32)),
                "NCED" => Some(Some(120)),
                "OVA" | "OAV" | "ONA" | "SP" | "SPECIAL" | "SPECIALS" => Some(Some(0)),
                "MOVIE" | "GEKIJOUBAN" => Some(None),
                _ => None,
            }?;
            let (index, consumed, span_end) = if !digits.is_empty() {
                (digits.parse().ok(), None, None)
            } else {
                match tokens.get(i + 1) {
                    Some(n)
                        if n.len() <= 4
                            && n.chars().all(|c| c.is_ascii_digit())
                            && !(1900..=2099).contains(&n.parse::<u32>().unwrap_or(0)) =>
                    {
                        (n.parse().ok(), Some(i + 1), None)
                    }
                    // "OVA 1-2": one file spanning a range of the band —
                    // the position right after a designator is context
                    // enough to trust the dash (HUB-30 batch markers).
                    Some(r) if num_range(r).is_some() => {
                        let (a, b) = num_range(r).unwrap();
                        (Some(a), Some(i + 1), Some(b))
                    }
                    // "OVA II": fansub packs number in roman as often as
                    // arabic — uppercase only, or the English word "I" in
                    // an episode title would become an index.
                    Some(r) if roman(r).is_some() => (roman(r), Some(i + 1), None),
                    _ => (Some(1), None, None),
                }
            };
            let explicit = !digits.is_empty() || consumed.is_some();
            Some((i, band, index.unwrap_or(1), consumed, span_end, explicit))
        });
        let all: Vec<_> = cands.by_ref().collect();
        all.iter()
            .find(|c| c.5)
            .or(all.first())
            .map(|&(i, band, index, consumed, span, _)| (i, band, index, consumed, span))
    };

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
    let episode_number = e_token.map(|(i, n)| (i, n, None)).or_else(|| {
        tokens.iter().enumerate().rev().find_map(|(i, t)| {
            // A number consumed as a designator's index is not an
            // episode candidate.
            if let Some((_, _, _, Some(consumed), _)) = designator
                && i == consumed
            {
                return None;
            }
            // A trailing batch marker ("Show - 01-02") spans a range.
            // FINAL token only: mid-name dashed numbers are usually
            // title ("Ranma 1-2 Special" must not become a span).
            if i == tokens.len() - 1
                && let Some((a, b)) = num_range(t)
            {
                return Some((i, a, Some(b)));
            }
            let num = t.split(['v', 'V']).next()?;
            if num.is_empty() || num.len() > 4 || !num.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: u32 = num.parse().ok()?;
            if (1900..=2099).contains(&n) {
                return None; // year, not an episode
            }
            Some((i, n, None))
        })
    });
    let (idx, episode, episode_end) = match (episode_number, designator) {
        // A designator with an EXPLICIT index — attached digits or the
        // number right after it — outranks any other number in the name:
        // "Cyber City Oedo 808 Ova 02" is the second OVA, and the 808 is
        // title. An INDEXLESS designator loses to a real episode number:
        // "[Grp] Show - 05 Special Training" is episode 5.
        (_, Some((i, band, index, consumed, span)))
            if consumed.is_some()
                || tokens[i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_digit()) =>
        {
            let b = band?;
            let title = if i == 0 {
                top_dir(path_rel).unwrap_or("Unknown Show").to_string()
            } else {
                tokens[..i].join(" ")
            };
            let show_guess = match top_dir(path_rel) {
                Some(top) => parse_movie_dir(top),
                None => parse_movie_dir(&title),
            };
            return Some(EpisodeGuess {
                show_title: if show_guess.title.is_empty() {
                    title
                } else {
                    show_guess.title
                },
                show_year: show_guess.year,
                season: Some(0),
                episode: b + index,
                episode_end: span.map(|s| b + s),
                episode_title: None,
            });
        }
        (Some(triple), _) => triple,
        // MOVIE: not an episode of anything. Bail out entirely so the
        // caller's movie path (a credible "Title (Year)") or, failing
        // that, exact hash identification takes it.
        (None, Some((_, None, _, _, _))) => return None,
        (None, Some((i, Some(band), index, _, span))) => {
            let title = if i == 0 {
                top_dir(path_rel).unwrap_or("Unknown Show").to_string()
            } else {
                tokens[..i].join(" ")
            };
            let show_guess = match top_dir(path_rel) {
                Some(top) => parse_movie_dir(top),
                None => parse_movie_dir(&title),
            };
            return Some(EpisodeGuess {
                show_title: if show_guess.title.is_empty() {
                    title
                } else {
                    show_guess.title
                },
                show_year: show_guess.year,
                // The same season-0 bands the hash binder uses; if AniDB
                // knows the file, the hash refines the slot later.
                season: Some(0),
                episode: band + index,
                episode_end: span.map(|s| band + s),
                episode_title: None,
            });
        }
        (None, None) => return None,
    };

    let mut title = tokens[..idx].join(" ");
    if title.is_empty() {
        title = top_dir(path_rel).unwrap_or("Unknown Show").to_string();
    }
    let show_guess = match top_dir(path_rel) {
        Some(top) => parse_movie_dir(top),
        None => parse_movie_dir(&title),
    };
    let ep_title = tokens[idx + 1..].join(" ");
    Some(EpisodeGuess {
        show_title: if show_guess.title.is_empty() {
            title
        } else {
            show_guess.title
        },
        show_year: show_guess.year,
        season: None, // absolute numbering is authoritative
        episode,
        episode_end,
        episode_title: (!ep_title.is_empty()).then_some(ep_title),
    })
}

/// A batch-marker token: `1-2`, `01-02`, `1-26` — two 1-3 digit numbers,
/// strictly increasing. Four digits would be years ("1997-2001"), and
/// equality ("5-5") is junk, not a span.
fn num_range(t: &str) -> Option<(u32, u32)> {
    let (a, b) = t.split_once('-')?;
    if a.is_empty() || b.is_empty() || a.len() > 3 || b.len() > 3 {
        return None;
    }
    if !a.chars().all(|c| c.is_ascii_digit()) || !b.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
    (b > a).then_some((a, b))
}

/// Uppercase roman numerals I..XIII — the range fansub packs use.
fn roman(t: &str) -> Option<u32> {
    Some(match t {
        "I" => 1,
        "II" => 2,
        "III" => 3,
        "IV" => 4,
        "V" => 5,
        "VI" => 6,
        "VII" => 7,
        "VIII" => 8,
        "IX" => 9,
        "X" => 10,
        "XI" => 11,
        "XII" => 12,
        "XIII" => 13,
        _ => return None,
    })
}

fn top_dir(path_rel: &str) -> Option<&str> {
    let mut parts = path_rel.split('/');
    let first = parts.next()?;
    parts.next().map(|_| first) // only when there IS a directory
}

/// A hyphen gluing the show name straight to the episode marker with no
/// surrounding space ("1883-S01E01", "30 Rock-S01E01") survives the
/// '.'/'_' cleanup above as one token, so `parse_sxxeyy`/`parse_nnxnn` —
/// which require the marker to lead its token — never see it. Insert the
/// missing space wherever a '-' is immediately followed by a token they
/// *would* accept. A '-' inside a marker itself ("S01E01-E02", "S02E05-
/// 06") or one that already has a space on either side is left alone.
fn split_glued_marker(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' && i > 0 {
            let rest: String = chars[i + 1..].iter().collect();
            let marker = &rest[..rest.find(' ').unwrap_or(rest.len())];
            if parse_sxxeyy(marker).is_some() || parse_nnxnn(marker).is_some() {
                out.push(' ');
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// The mirror of `split_glued_marker`: a season and episode written as
/// two SEPARATE tokens ("Madam Secretary - S05 E05 - Ghosts"), which
/// `parse_sxxeyy` cannot see because it needs both halves in one token.
/// Join them back into "S05E05" and the ordinary parse takes over —
/// including its multi-episode handling, since "S05 E05-E06" joins to a
/// form it already understands.
///
/// Both halves must match EXACTLY, with no surrounding punctuation to
/// trim: the pattern is narrow on purpose, because "S" plus digits
/// followed by "E" plus digits is a shape a title could otherwise
/// stumble into. 144 files in one library, all of Madam Secretary and
/// Humans, scanned but never resolved to an episode because of this.
fn join_split_marker(s: &str) -> String {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if i + 1 < toks.len() && is_season_token(toks[i]) && is_episode_token(toks[i + 1]) {
            out.push(format!("{}{}", toks[i], toks[i + 1]));
            i += 2;
        } else {
            out.push(toks[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

/// Scene compact numbering: `helix.213.hdtv-lol.mp4` — one digit of
/// season, two of episode, no `S`/`E` letters anywhere.
///
/// Three bare digits are far more ambiguous than `s##e##`, so the digits
/// alone can never be the evidence. Everything that could be confused
/// with them is a DIFFERENT SHAPE of name, and that is what this gates
/// on — the release convention, not the number:
///
///   * anime absolute numbering (`[AnimeRG] Dragon Ball Super - 110
///     [720p]`) would read as S01E10, and it would reach here because
///     `parse_anime` tries `parse_episode` first. Fansub names carry
///     brackets and spaces; a scene name has neither.
///   * titles that ARE numbers (`Cyber City Oedo 808 Ova 01`, three
///     files in this library) would read as S08E08. Spaces again, and
///     a title-number sits at the FRONT — hence `i > 0`.
///   * years used for disambiguation are four digits, and `The 4400`
///     is four digits, so `exactly three` excludes both outright.
///
/// A scene name is dot-separated with no whitespace and no brackets,
/// and ends in a `-group` tag; the marker is never the first token and
/// always has the source/codec run after it. Callers reach this only
/// when no lettered marker matched anywhere, so a file that parses
/// today cannot change.
fn parse_scene_compact(stem: &str, tokens: &[&str]) -> Option<(usize, u32, u32)> {
    if stem.contains(char::is_whitespace)
        || stem.contains(['[', ']', '(', ')'])
        || !stem.contains('.')
    {
        return None;
    }
    // `-group` suffix: the last dot-separated segment carries it.
    if !stem
        .rsplit('.')
        .next()
        .is_some_and(|last| last.contains('-'))
    {
        return None;
    }
    tokens.iter().enumerate().skip(1).find_map(|(i, t)| {
        let b = t.as_bytes();
        // Exactly three digits, season 1-9: a leading zero is anime
        // absolute numbering ("- 010 -"), never a season.
        if b.len() != 3 || !b.iter().all(|c| c.is_ascii_digit()) || b[0] == b'0' {
            return None;
        }
        // Something must follow — a scene name always runs on into the
        // source and codec, and a trailing number is not a marker.
        if i + 1 >= tokens.len() {
            return None;
        }
        let s = (b[0] - b'0') as u32;
        let e = ((b[1] - b'0') * 10 + (b[2] - b'0')) as u32;
        Some((i, s, e))
    })
}

/// The third gluing: a marker welded to the END of the stem with no
/// separator at all, not even a hyphen — `teneighty-mfs04e01.mkv`, a
/// release group's tag running straight into the numbering. Neither
/// `split_glued_marker` (needs a hyphen to cut at) nor
/// `join_split_marker` (needs two tokens) can see it.
///
/// Anchored at the END, and only `s##e##`: that is what keeps it safe.
/// A six-character run of exactly that shape, finishing the stem,
/// preceded by an alphanumeric — anything looser would start guessing
/// inside titles. Verified against every episode path in a 9610-file
/// library: 8 files newly resolve, no other parse changes.
fn split_trailing_marker(s: &str) -> String {
    let b = s.as_bytes();
    let n = b.len();
    if n < 7 {
        return s.to_string();
    }
    let m = &b[n - 6..];
    let marker = (m[0] | 0x20) == b's'
        && m[1].is_ascii_digit()
        && m[2].is_ascii_digit()
        && (m[3] | 0x20) == b'e'
        && m[4].is_ascii_digit()
        && m[5].is_ascii_digit();
    // Glued only. With a separator already there the tokenizer copes,
    // and a non-ASCII byte before it is left well alone.
    if marker && b[n - 7].is_ascii_alphanumeric() {
        let mut out = String::with_capacity(n + 1);
        out.push_str(&s[..n - 6]);
        out.push(' ');
        out.push_str(&s[n - 6..]);
        return out;
    }
    s.to_string()
}

/// `S5` / `S05` and nothing else.
fn is_season_token(t: &str) -> bool {
    matches!(t.strip_prefix(['s', 'S']),
        Some(d) if (1..=2).contains(&d.len()) && d.chars().all(|c| c.is_ascii_digit()))
}

/// `E05`, and the range forms `E05-E06` / `E05-06` that `parse_sxxeyy`
/// accepts once glued to a season.
fn is_episode_token(t: &str) -> bool {
    let Some(rest) = t.strip_prefix(['e', 'E']) else {
        return false;
    };
    let digits = |d: &str| (1..=3).contains(&d.len()) && d.chars().all(|c| c.is_ascii_digit());
    match rest.split_once('-') {
        Some((first, tail)) => {
            digits(first) && digits(tail.strip_prefix(['e', 'E']).unwrap_or(tail))
        }
        None => digits(rest),
    }
}

fn parse_sxxeyy(tok: &str) -> Option<(u32, u32, Option<u32>)> {
    let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let rest = t.strip_prefix(['s', 'S'])?;
    let e_pos = rest.find(['e', 'E'])?;
    let (s, e_part) = rest.split_at(e_pos);
    let e = &e_part[1..];
    let e_first: String = e.chars().take_while(|c| c.is_ascii_digit()).collect();
    if s.is_empty() || e_first.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let first: u32 = e_first.parse().ok()?;
    // Multi-episode: S01E01E02 / S01E01-E02 / S01E01-02 — the file
    // spans a range; a non-increasing tail is junk, not a range.
    let tail = &e[e_first.len()..];
    let tail = tail.strip_prefix('-').unwrap_or(tail);
    let tail = tail.strip_prefix(['e', 'E']).unwrap_or(tail);
    let end_digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    let end = end_digits.parse::<u32>().ok().filter(|n| *n > first);
    Some((s.parse().ok()?, first, end))
}

fn is_season_dir(dir: &str) -> bool {
    let d = dir.to_ascii_lowercase();
    d.starts_with("season")
        || d.starts_with("staffel")
        || d.starts_with("series ")
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

/// Release revision from a FILENAME: 1 for a plain release, higher for
/// a corrected one (HUB-30 generalized).
///
/// Two conventions, one number. Anime fansub `NNv2` means "second
/// version of this episode's release"; scene `REPACK`/`PROPER`/`RERIP`
/// mean "this release corrects a defective one" and each adds one. The
/// number exists for source ranking: within the same quality tier the
/// corrected release must win, and byte size cannot say so — a v2 is
/// often SMALLER than the broken encode it replaces.
///
/// Precision over recall, because titles lie. This collection's own
/// files include "Proper Preparation and Planning", "Intellectual
/// Property" and "(Proper Night Out mix)" — so tags must be standalone
/// delimiter-separated tokens, and PROPER (an English word) only counts
/// in ALL CAPS, while REPACK/RERIP (not words) match case-insensitively,
/// which is what still catches a P2P-styled "Repack". Only the basename
/// is read: directory names hold titles like "Version 2.0".
pub fn release_revision(path_rel: &str) -> u32 {
    let name = path_rel.rsplit('/').next().unwrap_or(path_rel);
    let mut version: u32 = 1;
    let (mut repack, mut proper, mut rerip) = (false, false, false);
    for token in name.split(['.', '_', '-', '[', ']', '(', ')', ' ']) {
        if token.eq_ignore_ascii_case("repack") {
            repack = true;
        } else if token.eq_ignore_ascii_case("rerip") {
            rerip = true;
        } else if token == "PROPER" {
            proper = true;
        } else if let Some((ep, v)) = token.split_once(['v', 'V']) {
            // `05v2`: both halves numeric, so "x264" and plain words
            // cannot match.
            if !ep.is_empty()
                && ep.chars().all(|c| c.is_ascii_digit())
                && !v.is_empty()
                && v.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = v.parse::<u32>()
            {
                version = version.max(n);
            }
        }
    }
    version + repack as u32 + proper as u32 + rerip as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The anime last resort: no episode shape means a film. Yearless
    /// counts, and a bare `partN` is a PIECE of a film, not one of its
    /// own — seven such files in this library are one movie.
    fn anime_films_resolve_without_a_year() {
        let g = parse_movie_file("Akira.mkv").expect("bare film");
        assert_eq!((g.title.as_str(), g.year, g.part), ("Akira", None, None));

        let g = parse_movie_file("Neo Tokyo (Dual-Audio)/Neo Tokyo.mkv").expect("film in a dir");
        assert_eq!((g.title.as_str(), g.part), ("Neo Tokyo", None));

        // Seven parts, one film: identity from the directory, number
        // from the filename, release tag stripped off the title.
        for (n, file) in [(1u32, "part1.mp4"), (7, "part7.mp4")] {
            let g = parse_movie_file(&format!("Nescaflowne (Eng,-Audio)/{file}"))
                .expect("multi-part film");
            assert_eq!((g.title.as_str(), g.part), ("Nescaflowne", Some(n)));
        }

        // A "Part N" SUFFIX is a real title and must not be mistaken
        // for a piece.
        let g = parse_movie_file("Harry Potter and the Deathly Hallows Part 2 (2011).mkv")
            .expect("titled part");
        assert_eq!(g.year, Some(2011));
        assert!(g.title.contains("Part 2"), "lost a real title: {}", g.title);

        // Nothing to name it by stays unresolved rather than guessed.
        assert!(parse_movie_file("part3.mp4").is_none());
    }

    #[test]
    /// Three bare digits are the most dangerous shape in this file, so
    /// the negatives matter more than the positive. Each of these was a
    /// real worry, and two of them are real files.
    fn compact_scene_numbering_stays_inside_its_shape() {
        // What it is for: scene form, one digit season + two episode.
        let g = parse_episode("Helix/Season 02/helix.213.hdtv-lol.mp4").expect("helix");
        assert_eq!(
            (g.show_title.as_str(), g.season, g.episode),
            ("Helix", Some(2), 13)
        );

        // A title that IS a number. 808 must never read as S08E08 —
        // space-separated, and the number leads the title.
        let g =
            parse_anime("Cyber City Oedo 808/Cyber City Oedo 808 Ova 01 Memories of the Past.mkv");
        assert_ne!(
            g.as_ref().map(|g| (g.season, g.episode)),
            Some((Some(8), 8)),
            "a numeric title was read as a season/episode"
        );

        // Anime absolute numbering reaches parse_episode FIRST (via
        // parse_anime), so 110 must not become S01E10.
        assert!(
            parse_episode("[AnimeRG] Dragon Ball Super - 110 [720p] [x264].mkv").is_none(),
            "fansub absolute numbering matched the compact form"
        );

        // Four digits are never a compact marker: disambiguation years,
        // and shows whose title is a four-digit number.
        assert!(parse_episode("The.4400.2004.1080p.WEB.x264-GRP.mkv").is_none());

        // A leading title-number is not a marker (i > 0).
        assert!(parse_episode("300.2006.1080p.BluRay.x264-GRP.mkv").is_none());
    }

    #[test]
    fn parses_episodes() {
        let cases = [
            (
                "Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv",
                "Andor",
                None,
                1,
                2,
                Some("That Would Be Me"),
            ),
            (
                "The Wire (2002)/Season 3/The.Wire.S03E11.Middle.Ground.720p.mkv",
                "The Wire",
                Some(2002),
                3,
                11,
                Some("Middle Ground 720p"),
            ),
            ("Alphas/alphas s02e05.mkv", "Alphas", None, 2, 5, None),
            (
                "Show/Specials/Show - S00E01 - Pilot.mkv",
                "Show",
                None,
                0,
                1,
                Some("Pilot"),
            ),
            (
                "Lost/Season 1/Lost - S01E01E02 - Pilot.mkv",
                "Lost",
                None,
                1,
                1,
                Some("Pilot"),
            ),
            // Show name glued straight to the marker by a bare hyphen,
            // no surrounding space (the on-disk shape that shipped with
            // zero series ever resolving into items).
            ("1883/Season01/1883-S01E01.mkv", "1883", None, 1, 1, None),
            (
                "30.Rock/Season01/30.Rock-S01E02.mkv",
                "30 Rock",
                None,
                1,
                2,
                None,
            ),
            // Season and episode as separate tokens — the whole of
            // Madam Secretary and Humans on this library, scanned but
            // never resolved. Exact on-disk shapes.
            (
                "Madam Secretary/Madam Secretary - S05 E05 - Ghosts (720p - AMZN Web-DL).mp4",
                "Madam Secretary",
                None,
                5,
                5,
                Some("Ghosts (720p - AMZN Web-DL)"),
            ),
            // Marker welded to the end of the stem, no separator at
            // all — a release tag running straight into the numbering.
            (
                "Misfits/Season 4/teneighty-mfs04e01.mkv",
                "Misfits",
                None,
                4,
                1,
                None,
            ),
            (
                "Humans/HUMANS - S03 E01 - Episode 01 (1080p - BluRay).mp4",
                // Identity comes from the DIRECTORY, not the shoutier
                // filename — the parser already prefers it, and should.
                "Humans",
                None,
                3,
                1,
                Some("Episode 01 (1080p - BluRay)"),
            ),
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

    /// HUB-30 batch markers: one file spanning a range of episodes.
    #[test]
    fn batch_markers_span_a_range_and_titles_with_dashes_do_not() {
        // The live Kite file: designator + range in tag-stripped tail.
        let g = parse_anime(
            "Kite OVAs & Liberator (Dual-Audio)/Kite Special Edition Uncut OVA 1-2 (Eng.-Dub).mkv",
        )
        .unwrap();
        assert_eq!((g.season, g.episode, g.episode_end), (Some(0), 1, Some(2)));

        // Trailing bare range.
        let g = parse_anime("Show/[Grp] Show - 01-02.mkv").unwrap();
        assert_eq!((g.episode, g.episode_end), (1, Some(2)));

        // SxxEyy ranges, both spellings.
        let g = parse_episode("Show/Show - S01E01-E03 - Double.mkv").unwrap();
        assert_eq!((g.season, g.episode, g.episode_end), (Some(1), 1, Some(3)));
        let g = parse_episode("Show/Show - S02E05-06.mkv").unwrap();
        assert_eq!((g.season, g.episode, g.episode_end), (Some(2), 5, Some(6)));

        // A dashed number INSIDE a title is not a span ("Ranma 1-2"),
        // and a real episode number still wins over it.
        let g = parse_anime("Ranma/[Grp] Ranma 1-2 - 05.mkv").unwrap();
        assert_eq!((g.episode, g.episode_end), (5, None));
        let g = parse_anime("Ranma/[Grp] Ranma 1-2 Special.mkv").unwrap();
        assert_eq!((g.season, g.episode, g.episode_end), (Some(0), 1, None));

        // Year ranges are never spans.
        assert!(num_range("1997-2001").is_none());
        // Non-increasing is junk, not a span.
        assert!(num_range("5-5").is_none());
        assert!(num_range("10-2").is_none());
    }

    #[test]
    fn parses_anime_shapes() {
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, &str, Option<u16>, Option<u32>, u32)] = &[
            (
                "Ao No Exorcist/Ao no Exorcist (720p, BluRay) [Coalgirls]/[Coalgirls]_Ao_no_Exorcist_11_(1280x720_Blu-Ray_FLAC)_[865A19CF].mkv",
                "Ao No Exorcist",
                None,
                None,
                11,
            ),
            (
                "Dragon Ball Super/[AnimeRG] Dragon Ball Super - 001 [720p] [x264] [pseudo].mkv",
                "Dragon Ball Super",
                None,
                None,
                1,
            ),
            (
                "Hellsing Ultimate/[CBM]_Hellsing_Ultimate_-_01_-_[1080p-AC3]_[7B4A1D84].mkv",
                "Hellsing Ultimate",
                None,
                None,
                1,
            ),
            (
                "Pokemon/Season 01/Pokemon 01x01 Pokemon! I Choose You!.mkv",
                "Pokemon",
                None,
                Some(1),
                1,
            ),
            (
                "Rozen Maiden (2013)/Rozen Maiden (2013) - S01E01 - Alice Game.mkv",
                "Rozen Maiden",
                Some(2013),
                Some(1),
                1,
            ),
            (
                "Serial Experiments Lain/Serial.Experiments.Lain.E01.1080p.Bluray.AV1.Opus.DualAudio-AeTHER.mkv",
                "Serial Experiments Lain",
                None,
                None,
                1,
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
            (
                "The.Matrix.1999.1080p.BluRay.x264-GRP.mkv",
                "The Matrix",
                Some(1999),
            ),
            ("Moana 2 (2024).mkv", "Moana 2", Some(2024)),
            (
                "2001 A Space Odyssey (1968).mkv",
                "2001 A Space Odyssey",
                Some(1968),
            ),
            ("Primer.mkv", "Primer", None),
            (
                "Blade_Runner_[1982]_Final_Cut.mkv",
                "Blade Runner",
                Some(1982),
            ),
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

    #[test]
    fn dotted_abbreviations_survive() {
        // Directories (no extension) with abbreviation dots.
        assert_eq!(parse_movie("Mr. Robot").title, "Mr Robot");
        assert_eq!(parse_movie("Mrs. Davis").title, "Mrs Davis");
        // Numeric suffix is a year, not an extension.
        let g = parse_movie("Archer.2009");
        assert_eq!((g.title.as_str(), g.year), ("Archer", Some(2009)));
        // Real extensions still strip.
        assert_eq!(parse_movie("Mr. Brooks (2007).mkv").title, "Mr Brooks");
        // Episode paths pick up the full show name.
        let g =
            parse_episode("Mr. Robot/Season 01/Mr. Robot - S01E01 - eps1.0_hellofriend.mov.mp4")
                .unwrap();
        assert_eq!(g.show_title, "Mr Robot");
        assert_eq!((g.season, g.episode), (Some(1), 1));
    }

    #[test]
    fn parses_multipart_movies() {
        let g = parse_movie("12 Monkeys - CD1.avi");
        assert_eq!(
            (g.title.as_str(), g.year, g.part),
            ("12 Monkeys", None, Some(1))
        );
        let g = parse_movie("300 - CD2.avi");
        assert_eq!((g.title.as_str(), g.part), ("300", Some(2)));
        let g = parse_movie("Alexander.2004.Disc 2.DVDRip.avi");
        assert_eq!(
            (g.title.as_str(), g.year, g.part),
            ("Alexander", Some(2004), Some(2))
        );
        // "Part N" is a TITLE, never a part marker.
        let g = parse_movie("Harry Potter and the Deathly Hallows Part 2 (2011).mkv");
        assert_eq!(g.part, None);
        assert!(g.title.contains("Part 2"));
        // discography noise doesn't trip it: "cd" needs a digit suffix.
        let g = parse_movie("The Cd Collector (1999).mkv");
        assert_eq!(g.part, None);
    }

    #[test]
    fn parses_lidarr_music_layout() {
        let g = parse_music(
            "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 01 - Thou Art Blind.flac",
        )
        .unwrap();
        assert_eq!(
            g,
            MusicGuess {
                artist: "Rotting Christ".into(),
                album: "Khronos".into(),
                album_year: Some(2000),
                disc: None,
                track: 1,
                title: "Thou Art Blind".into(),
            }
        );
        // No year, no artist prefix in the filename.
        let g = parse_music("Garbage/Version 2.0/03 - When I Grow Up.mp3").unwrap();
        assert_eq!(g.album, "Version 2.0");
        assert_eq!(g.album_year, None);
        assert_eq!(g.track, 3);
        assert_eq!(g.title, "When I Grow Up");
        assert_eq!(g.artist, "Garbage");
        // Title containing " - " stays whole after the number.
        let g = parse_music("X/Y (1999)/X - Y - 07 - Foo - Bar.flac").unwrap();
        assert_eq!(g.title, "Foo - Bar");
        // Unparseable: no track number.
        assert!(parse_music("X/Y/cover.jpg").is_none());
        assert!(parse_music("X/Y/liner-notes.flac").is_none());
    }

    #[test]
    fn release_revision_reads_tags_not_titles() {
        use super::release_revision as rev;
        // The real corrected releases in the live collection.
        assert_eq!(
            rev("Avengers.Infinity.War.2018.BDRip.1080p.PROPER.X265.Ac3-GANJAMAN.mkv"),
            2
        );
        assert_eq!(
            rev("Captain.America.Civil.War.2016.PROPER.REMASTERED.1080p.BluRay.x265.mp4"),
            2
        );
        assert_eq!(
            rev("Kingsman.The.Golden.Circle.2017.REPACK.1080p.BluRay.DD.7.1.X265-Ralphy.mkv"),
            2
        );
        assert_eq!(rev("Obsession.[2026].[1080p.BluRay.x265].[REPACK].mkv"), 2);
        assert_eq!(
            rev("The.Chronicles.of.Riddick.Dark.Fury.2004.Repack.1080p.BRRip.mkv"),
            2
        );
        assert_eq!(rev("Mr.Robot.S02E06.PROPER.HDTV.x264-KILLERS[ettv].mkv"), 2);
        // Titles that merely contain the words — every one from the same
        // collection, every one a plain release.
        assert_eq!(
            rev("The Boys (2019) - S02E02 - Proper Preparation and Planning (1080p).mkv"),
            1
        );
        assert_eq!(
            rev("Atypical - S01E08 - The Silencing Properties of Snow.mkv"),
            1
        );
        assert_eq!(
            rev("Silicon Valley - S04E03 - Intellectual Property.mkv"),
            1
        );
        assert_eq!(
            rev("Republica - Republica - 12 - Out of This World (Proper Night Out mix).flac"),
            1
        );
        assert_eq!(rev("Judas Priest - Turbo - 03 - Private Property.flac"), 1);
        // Anime versions, attached to the episode number.
        assert_eq!(rev("[Grp] Show - 05v2 [720p][A1B2C3D4].mkv"), 2);
        assert_eq!(rev("[Grp]_Show_-_12V3_(1080p).mkv"), 3);
        assert_eq!(rev("[Grp] Show - 05 [720p].mkv"), 1);
        // A version AND a repack compound.
        assert_eq!(rev("[Grp] Show - 05v2 REPACK.mkv"), 3);
        // Directory names hold titles; only the basename is read.
        assert_eq!(rev("Garbage/Version 2.0/03 - When I Grow Up.mp3"), 1);
        // Codec/channel tokens cannot match.
        assert_eq!(rev("Film.2020.1080p.x264.DD5.1.mkv"), 1);
    }

    #[test]
    fn designations_slot_into_season_zero_and_titles_stay_episodes() {
        use super::parse_anime;
        let slot = |p: &str| parse_anime(p).map(|g| (g.season, g.episode));
        // The Coalgirls NC files that sat bare and invisible for want of
        // exactly this.
        assert_eq!(
            slot("Ao No Exorcist/x/[Coalgirls]_Ao_no_Exorcist_NCOP_(1280x720)_[B97DE8EE].mkv"),
            Some((Some(0), 101))
        );
        assert_eq!(
            slot("Ao No Exorcist/x/[Coalgirls]_Ao_no_Exorcist_NCOP2_(1280x720)_[EA9A5C59].mkv"),
            Some((Some(0), 102))
        );
        assert_eq!(
            slot("Ao No Exorcist/x/[Coalgirls]_Ao_no_Exorcist_NCED_(1280x720)_[8A18F47C].mkv"),
            Some((Some(0), 121))
        );
        // A designator with an explicit index beats a title number; the
        // real thing, from a borrowed collection.
        assert_eq!(
            slot("X/Cyber City Oedo 808 Ova 02 The Decoy Program.mkv"),
            Some((Some(0), 2))
        );
        // An indexless designator AFTER a real episode number loses.
        assert_eq!(
            slot("Macross Plus (Dual-Audio)/Mcross + - 02 - OVA.mkv"),
            Some((None, 2))
        );
        // Roman-numbered OVA packs, from a borrowed collection where all
        // four Hellsings piled onto slot 1.
        assert_eq!(slot("X/Hellsing Ultimate OVA I.mkv"), Some((Some(0), 1)));
        assert_eq!(slot("X/Hellsing Ultimate OVA IV.mkv"), Some((Some(0), 4)));
        // Specials, attached and adjacent index forms.
        assert_eq!(slot("[Grp] Show - OVA [720p].mkv"), Some((Some(0), 1)));
        assert_eq!(slot("[Grp] Show - OVA 2 [720p].mkv"), Some((Some(0), 2)));
        assert_eq!(slot("[Grp] Show - SP03.mkv"), Some((Some(0), 3)));
        assert_eq!(
            slot("Show/Specials/Show - Special 2.mkv"),
            Some((Some(0), 2))
        );
        // A real episode number outranks a designator in the title.
        assert_eq!(
            slot("[Grp] Show - 05 Special Training.mkv"),
            Some((None, 5))
        );
        // SxxEyy names never reach the fansub branch at all — the shield
        // for the live collection's episode titles.
        assert_eq!(
            slot("Pokemon/Season 04/Pokemon 04x23 Houndoom's Special Delivery.mkv"),
            Some((Some(4), 23))
        );
        assert_eq!(
            slot("Pokemon/Season 14/Pokemon 14x40 Zorua The Movie! Legend.mkv"),
            Some((Some(14), 40))
        );
        // MOVIE bails out to the movie/hash path rather than inventing
        // an episode from a stray number.
        assert_eq!(slot("[Grp] Show - Movie 2 [1080p].mkv"), None);
        // "Show OVA - 04" is undecidable by tokens alone: episode 4 of an
        // OVA series, or the fourth OVA? The adjacency rule reads it as
        // OVA #4 — season 0 either way presents it as extra material,
        // and the hash refines the slot when AniDB knows the file. The
        // year in parens is stripped and can never become an index.
        assert_eq!(slot("[Grp] Show OVA (1997) - 04.mkv"), Some((Some(0), 4)));
    }

    #[test]
    fn release_tag_parentheticals_leave_titles_alone() {
        use super::strip_release_tags as strip;
        assert_eq!(strip("Hellsing Ultimate (Dual-Audio)"), "Hellsing Ultimate");
        assert_eq!(strip("8 Man After (Eng.-Dub)"), "8 Man After");
        assert_eq!(strip("Mezzo Forte (Uncut) (Dual Audio)"), "Mezzo Forte");
        assert_eq!(
            strip("BALDR FORCE EXE Resolution (OVA)"),
            "BALDR FORCE EXE Resolution"
        );
        // Not release-speak: kept.
        assert_eq!(
            strip("Blade Runner (Director's Cut)"),
            "Blade Runner (Director's Cut)"
        );
        assert_eq!(strip("Fate/stay night (2014)"), "Fate/stay night (2014)");
        assert_eq!(strip("Akira"), "Akira");
        // And the parse itself now sheds the tag from a directory name.
        assert_eq!(
            super::parse_movie("1001 Nights (Dual-Audio)").title,
            "1001 Nights"
        );
        assert_eq!(
            super::parse_movie("Armitage Dual Matrix [2002] (Dual-Audio)").year,
            Some(2002)
        );
    }
}
