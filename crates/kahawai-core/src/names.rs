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
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let (idx, season, episode, episode_end) = tokens.iter().enumerate().find_map(|(i, t)| {
        parse_sxxeyy(t)
            .or_else(|| parse_nnxnn(t).map(|(s, e)| (s, e, None)))
            .map(|(s, e, end)| (i, s, e, end))
    })?;

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
                Some(top) => parse_movie(top),
                None => parse_movie(&title),
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
                Some(top) => parse_movie(top),
                None => parse_movie(&title),
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
        Some(top) => parse_movie(top),
        None => parse_movie(&title),
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
