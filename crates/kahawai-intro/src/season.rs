//! A season's worth of analysis: windows, the pairwise search, the credits
//! fallback, and the end refinement — intro-skipper's `ChromaprintAnalyzer`,
//! `BlackFrameAnalyzer` and `TimeAdjustmentHelper` wired together the way
//! `ScheduledTasks/BaseItemAnalyzerTask.cs` wires them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::blackframe::{self, BlackFrameParams, DecodeProbe};
use crate::chroma::{self, Range, SearchParams};
use crate::decode::Media;
use crate::{fingerprint, silence};

/// 1 ms, their `TimeAdjustmentHelper.Epsilon`.
const EPSILON: f64 = 1e-3;

/// Every knob intro-skipper exposes that changes a *timestamp*, with their
/// defaults (`Configuration/PluginConfiguration.cs`). The ones that only change
/// what gets stored or shown are not here.
///
/// Two default-ON upstream knobs are DELIBERATELY not ported, both about
/// trusting unnamed chapter marks: `AdjustIntroBasedOnChapters` (snap the
/// intro's edges to the nearest chapter boundary before silence/keyframe
/// refinement) and `UseChapterMarkersBlackFrame` (try the last chapter in
/// the credits window before the binary search). Kahawai's own chapter
/// analyzer answers where chapters are NAMED; an unnamed mark is somebody's
/// scene split and this port does not let it move a measured boundary. On
/// chaptered files the two implementations therefore disagree by design.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Share of an episode fingerprinted when hunting for an intro (0.25),
    /// applied only to episodes of at least 5 minutes.
    pub analysis_percent: f64,
    /// Hard cap on that window, in seconds (600).
    pub analysis_length_limit: f64,
    pub maximum_intro_duration: f64,
    /// Tail of the episode searched for credits, in seconds (450).
    pub maximum_credits_duration: f64,
    pub search: SearchParams,
    pub black: BlackFrameParams,
    pub silence_noise_db: f64,
    pub silence_minimum_duration: f64,
    /// End refinement window: how far back and forward to look (5 / 2).
    pub adjust_window_inward: f64,
    pub adjust_window_outward: f64,
    /// A start this close to zero, or an end this close to the file end, snaps
    /// (2).
    pub end_snap_threshold: f64,
    pub adjust_on_silence: bool,
    pub snap_to_keyframe: bool,
    /// Anime runs the fingerprint search before the black-frame search for
    /// credits; everything else runs it after.
    pub anime: bool,
    /// Look for a "previously on" recap at all. It costs a video scan for every
    /// episode that has a shared card, so it is worth being able to say no.
    pub scan_recap: bool,
    /// Shortest shared card that counts as the front of a recap (3).
    pub recap_card_minimum_duration: f64,
    /// A recap runs from zero to a black frame no earlier than this (15) and no
    /// later than `maximum_recap_duration` (120).
    pub minimum_recap_duration: f64,
    pub maximum_recap_duration: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            analysis_percent: 0.25,
            analysis_length_limit: 600.0,
            maximum_intro_duration: 120.0,
            maximum_credits_duration: 450.0,
            search: SearchParams::default(),
            black: BlackFrameParams::default(),
            silence_noise_db: -50.0,
            silence_minimum_duration: 0.33,
            adjust_window_inward: 5.0,
            adjust_window_outward: 2.0,
            end_snap_threshold: 2.0,
            adjust_on_silence: true,
            snap_to_keyframe: true,
            anime: false,
            scan_recap: true,
            recap_card_minimum_duration: 3.0,
            minimum_recap_duration: 15.0,
            maximum_recap_duration: 120.0,
        }
    }
}

/// One episode's bytes and running time, with the analysis windows their
/// `QueueManager` computes.
#[derive(Clone, Debug)]
pub struct Episode {
    pub media: Media,
    pub name: String,
    pub duration: f64,
    /// Whatever the caller needs to key the result back to: an item id in the
    /// hub, empty from the command line.
    pub id: String,
}

impl Episode {
    /// A local file, whose running time is read from the file itself.
    pub fn probe(path: &Path) -> Result<Self> {
        let info = kahawai_media::discover(path, std::time::Duration::from_secs(60))
            .with_context(|| format!("probing {}", path.display()))?;
        let duration =
            info.duration_ms
                .with_context(|| format!("{}: no duration", path.display()))? as f64
                / 1000.0;
        Ok(Self::new(
            Media::Path(path.to_path_buf()),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            duration,
        ))
    }

    /// Bytes from somewhere else, with a running time the caller already knows
    /// — the hub has it in the database and cannot open the file by name.
    pub fn new(media: Media, name: String, duration: f64) -> Self {
        Self {
            media,
            name,
            duration,
            id: String::new(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// `[0, min(duration × percent, limit)]`, the percentage only applying to
    /// episodes of five minutes or more.
    pub fn intro_window(&self, cfg: &Config) -> (f64, f64) {
        let by_percent = if self.duration >= 5.0 * 60.0 {
            self.duration * cfg.analysis_percent
        } else {
            self.duration
        };
        (0.0, by_percent.min(cfg.analysis_length_limit))
    }

    /// The tail searched for credits.
    pub fn credits_window(&self, cfg: &Config) -> (f64, f64) {
        let start = (self.duration - self.duration.min(cfg.maximum_credits_duration)).max(0.0);
        (start, self.duration)
    }
}

/// What one episode's analysis produced.
#[derive(Debug, serde::Serialize)]
pub struct EpisodeSegments {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub duration: f64,
    /// The "previously on" card, when the season shares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recap: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intro: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Range>,
    /// Which analyzer produced the credits: `chromaprint` or `blackframe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits_source: Option<&'static str>,
    /// A read of this episode's bytes failed somewhere — the lease would not
    /// open, or the decoder could not deliver a window. What was found (if
    /// anything) is incomplete, and above all the episode has NOT been
    /// analysed: a caller recording completed work must not record this one.
    /// An outage that reads as "analysed, nothing found" silences whole
    /// seasons for ever.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
}

/// A season's segments and what the whole pass cost. The cost is per season,
/// not per episode: the fingerprint search is a season-wide comparison and
/// splitting its time across episodes would be an invented number.
#[derive(Debug, serde::Serialize)]
pub struct SeasonReport {
    pub episodes: Vec<EpisodeSegments>,
    pub seconds: f64,
}

/// Called between episodes, wherever the analysis can be interrupted without
/// losing work. The hub blocks inside it while anything is playing: a season is
/// twenty minutes of reading and a viewer should not wait for it.
pub type Between<'a> = &'a (dyn Fn() + Sync);

/// Nothing to do between episodes — the command line's answer.
pub const STRAIGHT_THROUGH: Between<'static> = &|| {};

/// Analyze local files as one season.
pub fn analyze_paths(paths: &[PathBuf], cfg: &Config) -> Result<SeasonReport> {
    // One file the prober cannot read costs its own row, not the season —
    // the same containment the decode path promises. It is reported as
    // UNREADABLE rather than dropped: a zero-byte or still-downloading file
    // is a fact worth seeing in the output.
    let mut unprobeable: Vec<(usize, EpisodeSegments)> = Vec::new();
    let episodes: Vec<Episode> = paths
        .iter()
        .enumerate()
        .filter_map(|(index, p)| match Episode::probe(p) {
            Ok(episode) => Some(episode),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = format!("{e:#}"), "probe failed");
                unprobeable.push((
                    index,
                    EpisodeSegments {
                        name: p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        id: String::new(),
                        duration: 0.0,
                        recap: None,
                        intro: None,
                        credits: None,
                        credits_source: None,
                        unreadable: true,
                    },
                ));
                None
            }
        })
        .collect();
    if episodes.is_empty() {
        anyhow::bail!("no file in the season could be probed");
    }
    let mut report = analyze(&episodes, cfg, STRAIGHT_THROUGH)?;
    // Back at their input positions, not appended: the report reads in
    // broadcast order, and a mid-season file that would not probe used to
    // print after the finale. Ascending indices keep each insert honest
    // about the shifts the ones before it caused.
    for (index, row) in unprobeable {
        report
            .episodes
            .insert(index.min(report.episodes.len()), row);
    }
    Ok(report)
}

/// Analyze a season: episodes in broadcast order, each opened however its
/// `Media` says to.
pub fn analyze(episodes: &[Episode], cfg: &Config, between: Between<'_>) -> Result<SeasonReport> {
    let started = Instant::now();

    let all: Vec<usize> = (0..episodes.len()).collect();
    // Episodes whose bytes failed to read anywhere along the way. Carried on
    // the report so the caller can refuse to mark them analysed.
    let mut unreadable: HashSet<usize> = HashSet::new();
    // One pass over the opening window of every episode, answering two
    // questions: where the shared opening is, and where a shared recap card is.
    //
    // A season of ONE has nothing to pair against, so no fingerprint is worth
    // decoding — upstream's analyzer returns before fingerprinting a queue of
    // one. The hub never sends one (its query requires two episodes), but the
    // CLI pointed at a single file paid up to 600 s of audio decode for a
    // search that could not run. The empty print map makes every pairwise
    // search below answer "nothing", which is the truthful result; the
    // black-frame credits pass still runs, because it compares nothing.
    let single = episodes.len() < 2;
    let intro_windows = windows_for(episodes, cfg, Mode::Intro);
    let intro_prints = if single {
        HashMap::new()
    } else {
        fingerprints(episodes, &intro_windows, &all, between, &mut unreadable)
    };
    let intros = search_regions(
        episodes,
        cfg,
        Mode::Intro,
        &intro_windows,
        &intro_prints,
        search_queue(episodes, &all),
        &cfg.search,
        chroma::Select::Longest,
        true,
        &mut unreadable,
    )?;
    let recaps = if cfg.scan_recap {
        recaps(
            episodes,
            cfg,
            &intro_windows,
            &intro_prints,
            &intros,
            between,
            &mut unreadable,
        )?
    } else {
        HashMap::new()
    };
    let mut credits = HashMap::new();
    let mut credits_source: HashMap<usize, &'static str> = HashMap::new();

    // Anime: fingerprints first, black frames for whatever is left. Everything
    // else the other way round (BaseItemAnalyzerTask). Each analyzer only looks
    // at the episodes the one before it did not answer — that is their
    // `NeedsAnalysis` filter, and it is most of the difference in cost.
    let credits_windows = windows_for(episodes, cfg, Mode::Credits);
    let credits_regions =
        |wanted: &[usize], unreadable: &mut HashSet<usize>| -> Result<HashMap<usize, Range>> {
            if wanted.is_empty() || single {
                return Ok(HashMap::new());
            }
            let queue = search_queue(episodes, wanted);
            let prints = fingerprints(episodes, &credits_windows, &queue, between, unreadable);
            search_regions(
                episodes,
                cfg,
                Mode::Credits,
                &credits_windows,
                &prints,
                queue,
                &cfg.search,
                chroma::Select::Longest,
                true,
                unreadable,
            )
        };

    if cfg.anime {
        record(
            credits_regions(&all, &mut unreadable)?,
            "chromaprint",
            &mut credits,
            &mut credits_source,
        );
    }
    // Their black-frame analyzer walks the season with one running search
    // position: where the credits started in the previous episode is where the
    // next episode's binary search begins. Dropping that carry-over changes
    // which black run the search converges on, by minutes on some episodes.
    let mut search_start = 0.0;
    for (i, episode) in episodes.iter().enumerate() {
        if credits.contains_key(&i) {
            continue;
        }
        between();
        // A file the decoder cannot read costs its own episode and no more.
        // One truncated download — a Matroska whose index is promised past the
        // end of the file, so every seek fails — used to take its whole season
        // with it, and the season came back unanalysed on every sweep.
        match black_frame_credits(episode, cfg, &mut search_start) {
            Ok(Some(range)) => {
                credits.insert(i, range);
                credits_source.insert(i, "blackframe");
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(episode = %episode.name, error = %e, "black-frame scan failed");
                unreadable.insert(i);
            }
        }
    }
    if !cfg.anime {
        let remaining: Vec<usize> = all
            .iter()
            .copied()
            .filter(|i| !credits.contains_key(i))
            .collect();
        record(
            credits_regions(&remaining, &mut unreadable)?,
            "chromaprint",
            &mut credits,
            &mut credits_source,
        );
    }

    Ok(SeasonReport {
        episodes: episodes
            .iter()
            .enumerate()
            .map(|(i, e)| EpisodeSegments {
                name: e.name.clone(),
                id: e.id.clone(),
                duration: e.duration,
                recap: recaps.get(&i).copied(),
                intro: intros.get(&i).copied(),
                credits: credits.get(&i).copied(),
                credits_source: credits_source.get(&i).copied(),
                unreadable: unreadable.contains(&i),
            })
            .collect(),
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// "Previously on": the shared card at the front of a recap, then the black
/// frame it ends on.
///
/// intro-skipper finds the card by fingerprint (the earliest shared region of
/// at least three seconds) and then takes the recap to run from zero to the
/// *last* black frame before the boundary — the recap's own content differs
/// every episode, so only its edges repeat. The boundary is 120 s, or the start
/// of the intro when there is one.
///
/// The black-frame scan decodes up to two minutes of video, so it only runs for
/// episodes where a shared card was actually found.
///
/// Four DELIBERATE divergences from upstream's recap flow, kept because the
/// simpler shape measured better on the corpus (4/6 hand-labelled recaps
/// found vs their 2/6 — `docs/intro-detection-results.md`):
/// - the black-frame filter uses the fixed `minimum_percentage` instead of
///   upstream's adaptive `NormalizeThreshold` against the content's own
///   darkness distribution;
/// - the stored recap is the raw `0 → last black frame` — no silence
///   pull-back or keyframe snap, since a recap's end is a hard cut to
///   black, not a theme fading out;
/// - the pairwise search stops at the first episode pair that yields a
///   card (upstream keeps trying later pairs when the black-frame build
///   fails), and the card's end is used un-ceiled as the scan floor;
/// - the boundary scan reads two seconds past the intro start and clamps
///   the result back (inline comment at the scan), where upstream stops
///   the window exactly at it.
fn recaps(
    episodes: &[Episode],
    cfg: &Config,
    windows: &[(f64, f64)],
    prints: &HashMap<usize, Vec<u32>>,
    intros: &HashMap<usize, Range>,
    between: Between<'_>,
    unreadable: &mut HashSet<usize>,
) -> Result<HashMap<usize, Range>> {
    let card_params = SearchParams {
        min_region_duration: cfg.recap_card_minimum_duration,
        ..cfg.search
    };
    let cards = search_regions(
        episodes,
        cfg,
        Mode::Intro,
        windows,
        prints,
        search_queue(episodes, &(0..episodes.len()).collect::<Vec<_>>()),
        &card_params,
        chroma::Select::Earliest,
        false,
        unreadable,
    )?;

    tracing::debug!(cards = cards.len(), "recap cards found");
    let mut found = HashMap::new();
    for (i, card) in cards {
        between();
        let episode = &episodes[i];
        let intro_start = intros.get(&i).map(|r| r.start);
        // Scan a little past the intro's start: the fade the recap ends on
        // straddles the cut, and a detected start can sit a hop or two early.
        // The recap is clamped back below so the two never overlap.
        let boundary = episode.duration.min(cfg.maximum_recap_duration).min(
            intro_start
                .map(|s| s + cfg.end_snap_threshold)
                .unwrap_or(f64::MAX),
        );
        let minimum = cfg.minimum_recap_duration.max(card.end);
        if boundary <= minimum {
            continue;
        }

        let frames =
            match crate::decode::luma_window(&episode.media, 0.0, boundary, cfg.black.threshold) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::warn!(episode = %episode.name, error = %e, "recap scan failed");
                    unreadable.insert(i);
                    continue;
                }
            };
        let last_black = frames
            .iter()
            .filter(|f| {
                f.black_percentage >= cfg.black.minimum_percentage
                    && f.time >= minimum
                    && f.time <= boundary
            })
            .map(|f| f.time)
            .fold(None::<f64>, |best, t| Some(best.map_or(t, |b| b.max(t))));

        tracing::debug!(
            episode = %episode.name, ?card, boundary, minimum,
            frames = frames.len(), ?last_black, "recap scan"
        );
        if let Some(end) = last_black {
            // Never past the intro: two segments that overlap would put two
            // skip buttons on screen at once.
            let end = intro_start.map(|s| end.min(s)).unwrap_or(end);
            if end > cfg.minimum_recap_duration {
                found.insert(i, Range::new(0.0, end));
            }
        }
    }
    Ok(found)
}

/// Store what a search found, WITHOUT overwriting an episode that already
/// has an answer.
///
/// The search is asked about the episodes still missing credits, but a
/// single remaining episode has nobody to compare against, so `search_queue`
/// lends it a neighbour — and the search reports a region for both sides of
/// any pair it matches. Overwriting there replaced a neighbour's black-frame
/// credits (where the picture goes dark) with a chromaprint range (where the
/// music starts) and relabelled its source, for an episode nothing had asked
/// about. The anime path guards the same way round at its black-frame pass.
fn record(
    found: HashMap<usize, Range>,
    source: &'static str,
    credits: &mut HashMap<usize, Range>,
    sources: &mut HashMap<usize, &'static str>,
) {
    for (i, range) in found {
        if credits.contains_key(&i) {
            continue;
        }
        credits.insert(i, range);
        sources.insert(i, source);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Intro,
    Credits,
}

/// Window bounds per episode for one mode.
fn windows_for(episodes: &[Episode], cfg: &Config, mode: Mode) -> Vec<(f64, f64)> {
    episodes
        .iter()
        .map(|e| match mode {
            Mode::Intro => e.intro_window(cfg),
            Mode::Credits => e.credits_window(cfg),
        })
        .collect()
}

/// Fingerprint one window of every episode in `wanted`.
///
/// Kept apart from the search because the same points answer two questions —
/// where the opening is, and where the recap's card is — and fingerprinting a
/// window twice is the most expensive mistake available here: over a mediahost
/// lease it is a quarter of the episode's bytes, again.
fn fingerprints(
    episodes: &[Episode],
    windows: &[(f64, f64)],
    wanted: &[usize],
    between: Between<'_>,
    unreadable: &mut HashSet<usize>,
) -> HashMap<usize, Vec<u32>> {
    let mut prints: HashMap<usize, Vec<u32>> = HashMap::new();
    for &i in wanted {
        between();
        let (start, end) = windows[i];
        // A file we cannot fingerprint is not a reason to abandon the season;
        // theirs substitutes an empty fingerprint and carries on. But the
        // failure is REMEMBERED, not merely logged: an episode that was never
        // read must not end this pass marked analysed, or a mediahost outage
        // records whole seasons as "nothing found" for ever.
        let points = fingerprint::fingerprint(&episodes[i].media, start, end).unwrap_or_else(|e| {
            tracing::warn!(episode = %episodes[i].name, error = %e, "fingerprinting failed");
            unreadable.insert(i);
            Vec::new()
        });
        prints.insert(i, points);
    }
    prints
}

/// The queue the pairwise search walks: the episodes asked for, plus a
/// neighbour when only one was asked for and it would have nothing to compare
/// against.
fn search_queue(episodes: &[Episode], wanted: &[usize]) -> Vec<usize> {
    let mut queue: Vec<usize> = wanted.to_vec();
    if queue.len() == 1 {
        let only = queue[0];
        queue.extend((0..episodes.len()).filter(|i| *i != only && i.abs_diff(only) <= 1));
    }
    queue
}

/// The pairwise season search, popping episodes off the front of a queue and
/// comparing each against everything still behind it — theirs, including the
/// `break` after the first pair that yields a region.
#[allow(clippy::too_many_arguments)]
fn search_regions(
    episodes: &[Episode],
    cfg: &Config,
    mode: Mode,
    windows: &[(f64, f64)],
    prints: &HashMap<usize, Vec<u32>>,
    mut queue: Vec<usize>,
    params: &SearchParams,
    select: chroma::Select,
    refine: bool,
    unreadable: &mut HashSet<usize>,
) -> Result<HashMap<usize, Range>> {
    let mut found: HashMap<usize, Range> = HashMap::new();
    while !queue.is_empty() {
        let current = queue.remove(0);
        for &remaining in &queue {
            let (mut lhs, mut rhs) =
                chroma::compare_with(&prints[&current], &prints[&remaining], params, select);

            let maximum = match mode {
                Mode::Intro if select == chroma::Select::Earliest => cfg.maximum_recap_duration,
                Mode::Intro => cfg.maximum_intro_duration,
                // Never accept a perfect match: two files whose whole credits
                // window agrees are duplicates, not credits.
                Mode::Credits => episodes[remaining].duration - windows[remaining].0 - 1.0,
            };
            if !rhs.valid() || rhs.duration() > maximum {
                continue;
            }

            if mode == Mode::Credits {
                // The fingerprint's clock starts at the window, not the file.
                lhs.start += windows[current].0;
                lhs.end += windows[current].0;
                rhs.start += windows[remaining].0;
                rhs.end += windows[remaining].0;
            }

            for (index, range) in [(current, lhs), (remaining, rhs)] {
                // Credits live in the BACK of an episode. For anything longer
                // than ~15 minutes the 450s window says so by construction,
                // but a short's window covers the whole file and the shared
                // OPENING theme — usually the longest shared region — was
                // reported as the credits, starting at zero. A deliberate
                // divergence from upstream, which shares the hole.
                if mode == Mode::Credits && range.start < episodes[index].duration / 2.0 {
                    continue;
                }
                match found.get(&index) {
                    Some(saved) if saved.duration() >= range.duration() => {}
                    _ => {
                        found.insert(index, range);
                    }
                }
            }
            break;
        }

        if refine && let Some(range) = found.get(&current).copied() {
            found.insert(
                current,
                adjust(&episodes[current], range, cfg, unreadable, current)?,
            );
        }
    }
    Ok(found)
}

/// `search_start` is a distance from the end of the file, carried from one
/// episode to the next and updated only when this one found credits — theirs.
fn black_frame_credits(
    episode: &Episode,
    cfg: &Config,
    search_start: &mut f64,
) -> Result<Option<Range>> {
    if episode.duration <= 0.0 {
        return Ok(None);
    }
    let (credits_start, _) = episode.credits_window(cfg);
    let mut probe = DecodeProbe {
        media: &episode.media,
        params: cfg.black,
    };

    // A longer previous episode can leave a position this one cannot reach,
    // which would invert the binary search's bracket.
    if *search_start > episode.duration - credits_start {
        *search_start = 0.0;
    }
    if *search_start < cfg.black.minimum_credits_duration {
        *search_start =
            blackframe::find_search_start(episode.duration, credits_start, &cfg.black, &mut probe)?;
    }

    let found = blackframe::find_credits(
        episode.duration,
        credits_start,
        *search_start,
        &cfg.black,
        &mut probe,
    )?;

    if let Some(credit) = found {
        *search_start = episode.duration - credit.start + cfg.black.minimum_credits_duration;
    }
    Ok(found)
}

/// Pull an end back to the pause after the theme and onto a keyframe, and snap
/// the edges that are within a couple of seconds of the file's own edges.
fn adjust(
    episode: &Episode,
    region: Range,
    cfg: &Config,
    unreadable: &mut HashSet<usize>,
    index: usize,
) -> Result<Range> {
    let duration = episode.duration;
    let mut start = region.start;
    if start < 0.0 || start <= cfg.end_snap_threshold + EPSILON {
        start = 0.0;
    }

    let mut end = region.end;
    if end >= duration - cfg.end_snap_threshold - EPSILON {
        end = duration;
    } else {
        let window = Range::new(
            (end - cfg.adjust_window_inward).max(0.0),
            (end + cfg.adjust_window_outward).min(duration),
        );

        if cfg.adjust_on_silence {
            match silence::detect(
                &episode.media,
                window.start,
                window.end,
                cfg.silence_noise_db,
                0.1,
            ) {
                Ok(quiet) => {
                    tracing::debug!(
                        window = ?window, found = quiet.len(), first = ?quiet.first(),
                        "silence in the refinement window"
                    );
                    if let Some(first) = quiet.iter().find(|r| {
                        r.start < window.end
                            && window.start < r.end
                            && r.duration() >= cfg.silence_minimum_duration
                            && r.start >= window.start
                    }) {
                        end = first.start;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "silence detection failed; leaving the end alone");
                    // The boundary survives unrefined, but the episode must
                    // not be recorded as fully analysed: frozen at the raw
                    // end — off by up to the refinement window — it would
                    // never be revisited.
                    unreadable.insert(index);
                }
            }
        }

        if cfg.snap_to_keyframe {
            match crate::decode::keyframes_window(&episode.media, window.start, window.end) {
                Ok(keyframes) => {
                    tracing::debug!(window = ?window, end, ?keyframes, "keyframes in the refinement window");
                    if let Some(nearest) = keyframes.into_iter().min_by(|a, b| {
                        (a - end)
                            .abs()
                            .partial_cmp(&(b - end).abs())
                            .expect("timestamps are never NaN")
                    }) {
                        end = nearest;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "keyframe scan failed; leaving the end alone");
                    unreadable.insert(index);
                }
            }
        }
    }

    // Their guard: an adjustment that inverts the segment is discarded whole.
    Ok(if start >= end {
        region
    } else {
        Range::new(start, end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the flag: a season nobody could read comes back
    /// with every episode saying so, not as "analysed, nothing found".
    #[test]
    fn an_unreadable_file_is_reported_not_absorbed() {
        let gone = |name: &str| {
            Episode::new(
                crate::decode::Media::Remote {
                    name: name.into(),
                    open: std::sync::Arc::new(|| anyhow::bail!("host is away")),
                },
                name.into(),
                600.0,
            )
        };
        let report = analyze(
            &[gone("e1"), gone("e2")],
            &Config::default(),
            STRAIGHT_THROUGH,
        )
        .expect("per-episode failures do not abort the season");
        assert_eq!(report.episodes.len(), 2);
        assert!(report.episodes.iter().all(|e| e.unreadable));
        assert!(
            report
                .episodes
                .iter()
                .all(|e| e.intro.is_none() && e.credits.is_none())
        );
    }

    /// A season's last unanswered episode borrows a neighbour to compare
    /// against, and the search reports both sides of the pair. The neighbour
    /// already has its credits from the black-frame pass, and that answer is
    /// the one the non-anime path prefers.
    #[test]
    fn a_borrowed_neighbour_keeps_the_answer_it_had() {
        let mut credits = HashMap::from([(
            4,
            Range {
                start: 1300.0,
                end: 1400.0,
            },
        )]);
        let mut sources = HashMap::from([(4, "blackframe")]);
        let found = HashMap::from([
            (
                5,
                Range {
                    start: 1280.0,
                    end: 1390.0,
                },
            ),
            (
                4,
                Range {
                    start: 1290.0,
                    end: 1395.0,
                },
            ),
        ]);

        record(found, "chromaprint", &mut credits, &mut sources);

        assert_eq!(credits[&4].start, 1300.0, "the neighbour is left alone");
        assert_eq!(sources[&4], "blackframe");
        assert_eq!(
            credits[&5].start, 1280.0,
            "the episode asked about is answered"
        );
        assert_eq!(sources[&5], "chromaprint");
    }

    fn episode(duration: f64) -> Episode {
        Episode::new(
            Media::Path(PathBuf::from("/nonexistent.mkv")),
            "test".into(),
            duration,
        )
    }

    #[test]
    fn windows_match_the_plugins() {
        let cfg = Config::default();
        // 24 minutes: a quarter of it, under the ten-minute cap.
        let e = episode(24.0 * 60.0);
        assert_eq!(e.intro_window(&cfg), (0.0, 360.0));
        assert_eq!(e.credits_window(&cfg), (24.0 * 60.0 - 450.0, 24.0 * 60.0));

        // A feature-length file is capped at ten minutes.
        let long = episode(2.0 * 3600.0);
        assert_eq!(long.intro_window(&cfg), (0.0, 600.0));

        // Anything under five minutes is fingerprinted whole.
        let short = episode(200.0);
        assert_eq!(short.intro_window(&cfg), (0.0, 200.0));
        assert_eq!(short.credits_window(&cfg), (0.0, 200.0));
    }

    #[test]
    fn an_end_near_the_file_end_snaps_to_it() {
        let cfg = Config::default();
        let e = episode(1440.0);
        let adjusted =
            adjust(&e, Range::new(1300.0, 1439.5), &cfg, &mut HashSet::new(), 0).unwrap();
        assert_eq!(adjusted.end, 1440.0);
        // The start is far from zero, so it stays put.
        assert_eq!(adjusted.start, 1300.0);
    }

    #[test]
    fn a_start_inside_the_snap_threshold_goes_to_zero() {
        let cfg = Config {
            adjust_on_silence: false,
            snap_to_keyframe: false,
            ..Config::default()
        };
        let adjusted = adjust(
            &episode(1440.0),
            Range::new(1.5, 90.0),
            &cfg,
            &mut HashSet::new(),
            0,
        )
        .unwrap();
        assert_eq!(adjusted, Range::new(0.0, 90.0));
    }
}
