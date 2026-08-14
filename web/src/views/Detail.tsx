import { useEffect, useState } from 'react'
import Failed from '../Failed'
import { fetchItem } from '../item-query'
import {
  artworkSrcSet,
  artworkUrl,
  fetchChildren,
  fetchLibraries,
  prefsOrNone,
  resolveTracks,
  pickSubtitle,
  type Pref,
  searchSubtitles,
  downloadSubtitle,
  deleteSubtitle,
  quotaLabel,
  putPref,
  type SubtitleQuota,
  type Subtitle,
  type SubtitleCandidate,
  type Item,
  type ItemDetail,
  type Source,
  isAdmin,
  downloadWithAuth,
  setWatched,
} from '../api'
import CapabilityDebug from './CapabilityDebug'
import { loadMask, maskSummary } from '../capabilities'
import Icon from '../icons'
import {
  episodeOf,
  projecting,
  seLabel,
  seasonLabel,
  seasonOf,
  resumeMsFor,
  watchedPct,
} from '../label'
import { notify } from '../toast'
import { deliveryPlan as plan } from '../delivery'
import tmdbLogo from '../assets/tmdb.svg'

const GB = 1024 * 1024 * 1024

function fmtDuration(ms?: number) {
  if (!ms) return null
  const m = Math.round(ms / 60000)
  return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`
}

type SubStream = { format: string; language?: string | null }

/// "3 subs · en nl" for a handful, "26 subs · 5 formats" past that. The
/// full list goes in the tooltip, where length costs nothing.
function subChipLabel(subs: SubStream[]) {
  const langs = [...new Set(subs.map((t) => t.language).filter(Boolean))]
  const n = `${subs.length} sub${subs.length === 1 ? '' : 's'}`
  if (langs.length > 0 && langs.length <= 6) return `${n} · ${langs.join(' ')}`
  const formats = [...new Set(subs.map((t) => t.format))]
  return `${n} · ${formats.join(' ')}`
}

function subChipTitle(subs: SubStream[]) {
  return subs.map((t) => [t.language, t.format].filter(Boolean).join(' ')).join(', ')
}

function Chips({ s }: { s: Source }) {
  const st = s.streams
  if (!st) return null
  return (
    <span className="chips">
      {st.container && <span className="chip">{st.container}</span>}
      {st.video?.map((v, i) => (
        <span className="chip" key={`v${i}`}>
          {v.codec} {v.height ? `${v.height}p` : ''}
        </span>
      ))}
      {st.audio?.map((a, i) => (
        <span className="chip" key={`a${i}`}>
          {a.codec}
          {a.language ? ` ${a.language}` : ''}
        </span>
      ))}
      {/* One chip for the subtitles, not one per track. A file with 26
          embedded tracks produced 26 chips all reading "text", which said
          nothing 26 times and pushed the size and the offline mark off
          the row. */}
      {st.subtitles && st.subtitles.length > 0 && (
        <span className="chip dim" title={subChipTitle(st.subtitles)}>
          {subChipLabel(st.subtitles)}
        </span>
      )}
      <span className="chip dim">{(s.size / GB).toFixed(1)} GB</span>
      {!s.available && <span className="chip warn">offline</span>}
    </span>
  )
}

/// The verdict, as the hub words it. Never re-derived here: the whole
/// point of asking the item what it would serve is that the answer comes
/// from the code that will serve it.
///
/// Keyed on `cost`, NOT on `mode`. On this endpoint `mode` is only ever
/// `direct` or `remux` — it says whether bytes are served as they are,
/// not whether anything is re-encoded (`sessions.rs`: `if sp.direct
/// { "direct" } else { "remux" }`). A `remux` with `cost: video_encode`
/// re-encodes the video, so a chip reading REMUX over that row would be
/// telling you the opposite of what the row says.
///
/// The chip carries it, and no gloss goes with it. This panel lists every
/// stream's verdict directly underneath, so a sentence beside the chip said
/// the same thing twice — and worse than the rows do, because one chip cannot
/// describe two streams. `video_encode` is reached whenever the VIDEO is
/// encoded, whatever the audio is doing (negotiate.rs checks video first), so
/// any gloss naming one stream is silent about the other.
///
/// UNPLAYABLE keeps its note: it is the one verdict that describes no work at
/// all but a refusal, and there is no stream row to read it off.
/// Deliveries that mean something is being done TO the subtitles rather
/// than them being handed over as they are. Worth a colour, because each
/// one costs something: a burn restarts the video encode.
const LOUD_DELIVERY: Record<string, 'warn' | 'sand'> = {
  none: 'warn',
  burn: 'sand',
  ocr: 'sand',
}

/// Which subtitle track you would actually get, and why that one.
///
/// Resolved with `resolveTracks` and `pickSubtitle` — the same two
/// functions the player calls when it starts. A panel that answered this
/// its own way would be a second opinion, and the interesting case is
/// exactly when the two disagree.
function chosenSubtitle(
  item: ItemDetail,
  subs: Subtitle[],
  prefs: Pref[],
  mediaType: string,
): { verdict: string; tone?: 'warn' | 'sand' } {
  const want = resolveTracks(
    prefs,
    item.parent_id ?? item.id,
    item.id,
    mediaType,
    item.metadata?.original_language,
    // Audio streams decide the audio track, which is not what is being
    // asked here.
    [],
  )
  // An exact track pinned on this item outranks the language order — but only
  // while that track still EXISTS AND CAN BE SERVED, which is the condition the
  // player applies (`initialSubtitle`, and `pickSubtitle` below excludes the
  // same). Without the delivery half, masking off graphics_overlay in the panel
  // at the foot of this row left the plan naming a pinned image track: it read
  // "eng pgs · none", which does say unavailable and says it in the warn tone —
  // so the row was not silent, it was answering a different question from the
  // one it exists for. Not "is this track available" but "what will I get",
  // which is whatever the language order finds instead.
  const pinned =
    want.subTrack != null
      ? subs.find((s) => s.id === want.subTrack && s.delivery !== 'none')
      : undefined
  const track = pinned ?? pickSubtitle(want.subs, subs)
  // No trailing reason, in any of the forms it took — "pinned on this
  // title", "your order: en, nl", "nothing here is in en, nl". The row
  // answers WHICH track you get; why that one is a question about your own
  // settings, and the answer was the same on every item page you opened.
  if (!track) return want.subs.length === 0 ? { verdict: 'off' } : { verdict: 'none', tone: 'sand' }
  const name = [track.language ?? '?', track.format].filter(Boolean).join(' ')
  // An ASS track delivered as ASS does not need saying twice; a text
  // track delivered as a burn very much does.
  const how = track.delivery === track.format ? '' : ` · ${track.delivery}`
  return { verdict: `${name}${how}`, tone: LOUD_DELIVERY[track.delivery] }
}

/// One stream's verdict: what happens to it, then why. The label column
/// is fixed so the reasons line up down the panel and can be read as a
/// column rather than as sentences.
function PlanRow({
  label,
  verdict,
  tone,
}: {
  label: string
  verdict: string
  tone?: 'warn' | 'sand'
}) {
  if (!verdict) return null
  // "copy" / "dts → aac (transcoded) · 7.1 → 5.1" — the hub puts the
  // action first and the reasoning after a dash, so splitting on the
  // first one colours the action without parsing the sentence.
  const [action, ...why] = verdict.split(' — ')
  const acted = tone ?? (/(copy|direct|text)/i.test(action) ? 'teal' : 'sand')
  return (
    <div className="plan-row mono">
      <span className="dim">{label}</span>
      <span>
        <span className={acted}>{action}</span>
        {why.length > 0 && <span className="dim"> — {why.join(' — ')}</span>}
      </span>
    </div>
  )
}

/// The head every item page opens with: artwork on the left, everything
/// that identifies the item on the right, and the actions under it.
///
/// One shape for all four kinds rather than three near-copies — only the
/// artwork's proportions differ, and they follow what the artwork IS: a
/// square sleeve, a 16:9 episode still, a poster.
function DetailHead({
  item,
  subline,
  progress,
  children,
}: {
  item: ItemDetail
  subline: React.ReactNode
  /// Percent watched, or null when it has not been started.
  progress: number | null
  /// The action row(s) — different per kind, so the caller supplies them.
  children?: React.ReactNode
}) {
  // Follows what the artwork IS, not what the page is about: a track's
  // art is its album's square sleeve, an episode's is a 16:9 still, and
  // everything else has a poster.
  const square = item.kind === 'album' || item.kind === 'track'
  const ratio = square ? '1' : item.kind === 'episode' ? '16 / 9' : '2 / 3'
  const width = square ? '180px' : item.kind === 'episode' ? '320px' : '190px'
  const rating = item.metadata?.rating
  return (
    <div className="detail-top">
      <span
        className="detail-artbox card-artbox"
        style={{ '--art-w': width, '--card-ratio': ratio } as React.CSSProperties}
      >
        <img
          className="detail-art"
          src={artworkUrl(item.id, item.art_version, 'card')}
          srcSet={artworkSrcSet(item.id, item.art_version)}
          alt=""
          onError={(e) => e.currentTarget.classList.add('art-failed')}
        />
      </span>
      <div className="detail-meta">
        <h1>
          {item.title} {item.year && <span className="year">({item.year})</span>}
        </h1>
        <div className="detail-sub mono">{subline}</div>
        {progress !== null && (
          <span className="waterline">
            <span className="waterline-fill" style={{ width: `${progress}%` }} />
          </span>
        )}
        {item.metadata?.overview && <p className="overview">{item.metadata.overview}</p>}
        {/* The facts nobody needs in a heading but everybody checks. */}
        <div className="detail-facts mono">
          {item.metadata?.premiered && <span>{item.metadata.premiered}</span>}
          {rating != null && <span className="sand">★ {rating.toFixed(1)}</span>}
          {item.metadata?.confidence === 'weak' && (
            <span className="sand" title="The metadata match was not certain">
              uncertain match
            </span>
          )}
          {item.play_count > 0 && <span className="teal">seen ×{item.play_count}</span>}
        </div>
        {children}
      </div>
    </div>
  )
}

/// The tick on an episode row. A sibling of the row's own button rather
/// than inside it: a button within a button is invalid, and a click that
/// both ticked the episode and opened it would be neither.
function EpisodeSeen({ episode, onDone }: { episode: Item; onDone: () => void }) {
  const [busy, setBusy] = useState(false)
  return (
    <button
      className={`ep-seen${episode.played ? ' on' : ''}`}
      disabled={busy}
      title={episode.played ? 'Mark as unwatched' : 'Mark as watched'}
      onClick={async () => {
        setBusy(true)
        try {
          await setWatched(episode.id, !episode.played)
          onDone()
        } catch (e) {
          notify(`Could not change the watched mark: ${e}`)
        } finally {
          setBusy(false)
        }
      }}
    >
      <Icon name="check" size={15} />
    </button>
  )
}

/// Tick it off, or take the tick back. Its own component so the busy and
/// failed states stay local to the button that caused them.
function MarkWatched({ item, onDone }: { item: Item; onDone: () => void }) {
  const [busy, setBusy] = useState(false)
  return (
    <button
      className={`btn ghost small mark${item.played ? ' on' : ''}`}
      disabled={busy}
      title={item.played ? 'Mark as unwatched' : 'Mark as watched without playing it'}
      onClick={async () => {
        setBusy(true)
        try {
          await setWatched(item.id, !item.played)
          onDone()
        } catch (e) {
          notify(`Could not change the watched mark: ${e}`)
        } finally {
          setBusy(false)
        }
      }}
    >
      <Icon name="check" />
      {item.played ? 'Watched' : 'Mark watched'}
    </button>
  )
}

export default function Detail({
  id,
  fromLib,
  onPlay,
  onOpenEpisode,
  onOpenLibrary,
  onOpenSeason,
  onPlayAlbum,
  onEnqueueAlbum,
  onEnqueueTrack,
  playingId,
}: {
  id: string
  fromLib: string
  /// Open the player for this item. Acquiring the session belongs to that
  /// route now — pressing Play is a navigation, and the wait, the offline
  /// host and the refusal are all shown by the page that owns them.
  onPlay: (id: string, fromStart?: boolean) => void
  onOpenEpisode: (id: string) => void
  onOpenLibrary: (id: string) => void
  onOpenSeason: (season: number | null) => void
  /// Hand the album to the app-level queue, which outlives this page.
  /// Playing a record replaces the queue; adding one leaves what is
  /// playing alone. An album levels by album gain, a single track by its
  /// own — the app decides that, this page only says which it is.
  onPlayAlbum: (tracks: Item[], at: number) => void
  onEnqueueAlbum: (tracks: Item[]) => void
  onEnqueueTrack: (track: Item) => void
  /// Which track the queue is on, so this page can mark it. The queue is
  /// no longer this page's state, and it may be playing an album you are
  /// not currently looking at.
  playingId?: string | null
}) {
  const [item, setItem] = useState<ItemDetail | null>(null)
  /// `null` until the child list answers: a count of zero is a FACT, and
  /// printing it before asking made every album read "0 tracks" with both
  /// actions disabled for a round trip — disabled because the data was absent,
  /// which is the one thing a disabled control must not mean.
  const [episodes, setEpisodes] = useState<Item[] | null>(null)
  /// The child list's own failure. Separate from `error`, which the album
  /// branch never rendered at all, so a track list that failed left a page
  /// that looked complete and two buttons that could not be pressed.
  const [childError, setChildError] = useState('')
  /// Bumped by Try again on that failure. The fetch keys on the item, which
  /// does not change when a retry is what you want.
  const [childAttempt, setChildAttempt] = useState(0)
  const [animeView, setAnimeView] = useState<'seasons' | 'native'>('seasons')
  const [mediaType, setMediaType] = useState('')
  // HUB-21/24: subtitle tracks + external search results.
  const [subs, setSubs] = useState<Subtitle[]>([])
  const [subCands, setSubCands] = useState<SubtitleCandidate[] | null>(null)
  // HUB-33 subtitle-language preference for this library's media type;
  // empty = no preference, search every language.
  const [subLangs, setSubLangs] = useState<string[]>([])
  /// This title's standing subtitle choice: 'off', 'any', or a language.
  const [titleSub, setTitleSub] = useState('')
  /// The whole pref set, because the plan panel resolves the chosen
  /// subtitle with the same function the player uses.
  const [prefs, setPrefs] = useState<Pref[]>([])
  const [subBusy, setSubBusy] = useState(false)
  const [subNote, setSubNote] = useState('')
  const [subQuota, setSubQuota] = useState<SubtitleQuota | null>(null)
  /// The item itself would not load: there is no page without it, so this is
  /// the one that takes the screen.
  const [loadError, setLoadError] = useState('')
  /// Something you asked for did not work — a play that was refused, a
  /// subtitle that would not delete. The page is intact and you are still
  /// looking at it, so this is a line on it, not a replacement for it.
  const [error, setError] = useState('')
  /// Bumped by Try again; the loads below depend on it, so asking again is
  /// re-running exactly what failed rather than a second code path.
  const [attempt, setAttempt] = useState(0)
  const [showCaps, setShowCaps] = useState(false)
  // The badge reads the stored mask, which the panel edits underneath
  // it; the counter repaints it on an edit instead of leaving the
  // previous mask on screen.
  const [, setCapsRev] = useState(0)
  const masked = maskSummary(loadMask())

  useEffect(() => {
    setItem(null)
    setEpisodes(null)
    setChildError('')
    setSubs([])
    setSubCands(null)
    setSubNote('')
    // The action error goes too. Without this, a failed episode load kept its
    // `Failed` screen over the show page you pressed Back to — the early
    // return below runs before any of the state this effect refills, so the
    // page underneath was loaded and invisible. `loadError` is NOT cleared
    // here: it is what the retry is still standing on. Clearing it up front
    // put `item === null` behind an empty screen, so pressing Try again
    // against a wedged hub blanked the page for the whole request and then
    // brought the error back — which reads as the button having worked, once,
    // wrongly. It goes when the load actually succeeds.
    setError('')
    // `App` keys this view on the item id, so an id change is a fresh mount and
    // the resets above are belt rather than braces. The guard below still earns
    // its place for the OTHER entry to this effect: two quick Try again clicks
    // on the same id, where the instance persists and the slower answer would
    // otherwise win. Do not remove the key on the strength of that — the
    // departure guard in `play()` is a ref flipped by unmount, and without the
    // key it never fires.
    let current = true
    fetchItem(id)
      .then((d) => {
        if (!current) return
        setLoadError('')
        setItem(d)
        setSubs(d.negotiated?.subtitles ?? [])
      })
      .catch((e) => current && setLoadError(String(e)))
    return () => {
      current = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, attempt])
  // The subtitle list rides on the item now — one question, one
  // answer. After a download or a delete that means re-asking the
  // item, which is heavier than the old list refetch but pure.
  const reloadSubs = () =>
    fetchItem(id)
      .then((d) => {
        setItem(d)
        setSubs(d.negotiated?.subtitles ?? [])
      })
      // Not `setSubs([])`: the reload failing says nothing about what the
      // item has. Blanking it printed "No subtitles in the file" — under the
      // success toast for the subtitle that had just been downloaded.
      .catch((e: unknown) => notify(`Could not refresh the subtitle list: ${e}`))
  /// The child list, in its own effect and keyed on `childAttempt` so a
  /// failure has something to retry. Sharing the effect below meant the only
  /// key was the item, which does not change when a retry is what you want —
  /// so a track list that failed once could not be asked for again.
  useEffect(() => {
    if (item?.kind !== 'show' && item?.kind !== 'album') return
    let live = true
    setChildError('')
    fetchChildren(item.id)
      .then((c) => live && setEpisodes(c.children))
      .catch((e: unknown) => live && setChildError(String(e)))
    return () => {
      live = false
    }
  }, [item?.id, item?.kind, childAttempt])

  useEffect(() => {
    // Library context: media type (per-type track settings, HUB-33).
    // Anime presentation (HUB-31) is purely a user preference; default
    // is the projected seasons view.
    // Kept separate so a prefs failure does not also cost the media type,
    // which comes from the other half. `prefsOrNone` reports its own.
    Promise.all([fetchLibraries(), prefsOrNone()])
      .then(([l, p]) => {
        const mt = l.libraries.find((x) => x.id === fromLib)?.media_type ?? ''
        setMediaType(mt)
        setPrefs(p.prefs)
        const mine = p.prefs.find((x) => x.scope === '' && x.key === 'anime_view')?.value
        setAnimeView(mine === 'native' ? 'native' : 'seasons')
        setSubLangs(
          (p.prefs.find((x) => x.scope === '' && x.key === `subs.${mt}`)?.value ?? '')
            .split(',')
            .map((s) => s.trim())
            .filter(Boolean),
        )
        // This title's own subtitle memory, which outranks the per-type
        // list — set by picking a language while watching, so the page has
        // to say when one is standing or the list above it reads as a lie.
        setTitleSub(
          p.prefs.find((x) => x.scope === (item?.parent_id ?? item?.id) && x.key === 'subs')
            ?.value ?? '',
        )
      })
      // The comment above is the reason this cannot be silent: the page has to
      // say when a title's own subtitle memory is standing, and swallowing the
      // load meant the language list rendered as though nothing was.
      //
      // A dead hub fails this AND the prefs half, so two notices are raised and
      // the host keeps the later one — deliberate, and `toast.ts` says why:
      // "two failures in a row are usually the same failure twice". The item's
      // own load has failed too in that case, so the screen the viewer is
      // actually looking at is `Failed`, with a retry.
      .catch((e: unknown) => notify(`Could not load the library details: ${e}`))
  }, [item?.id, item?.kind, item?.parent_id, fromLib])
  // Escape closes the subtitle dialog, and only while it is open, so it
  // cannot swallow an Escape meant for anything else.
  useEffect(() => {
    if (!subCands) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setSubCands(null)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [subCands])

  if (loadError)
    return (
      <Failed
        what="Could not load this item."
        message={loadError}
        onRetry={() => setAttempt((n) => n + 1)}
        away={{ label: 'Back to library', go: () => onOpenLibrary(fromLib) }}
      />
    )
  if (!item) return null

  // Hierarchical back: episode → its series page, series/movie → the
  // library in the URL (the one we navigated from).
  const goUp = () => {
    if (item.kind === 'episode' && item.parent_id) onOpenEpisode(item.parent_id)
    else onOpenLibrary(fromLib)
  }
  const upLabel =
    item.kind === 'episode' && item.parent_id ? `← ${item.show_title ?? 'Series'}` : '← Library'

  if (item.kind === 'album') {
    const tracks = episodes // children, ordered disc/track by the API
    return (
      <main>
        <button className="btn ghost small" onClick={goUp}>
          {upLabel}
        </button>
        <DetailHead
          item={item}
          progress={null}
          subline={[item.artist, tracks === null ? null : `${tracks.length} tracks`]
            .filter(Boolean)
            .join(' · ')}
        >
          <div className="play-row">
            <button
              className="btn"
              disabled={!tracks?.length}
              // A disabled control must say why. Absent data and an empty
              // record are different reasons, and neither is "no".
              title={
                childError
                  ? 'The track list could not be loaded'
                  : tracks === null
                    ? 'Still loading the track list'
                    : tracks.length === 0
                      ? 'This record has no tracks'
                      : undefined
              }
              onClick={() => tracks && onPlayAlbum(tracks, 0)}
            >
              ▶&nbsp; Play album
            </button>
            <button
              className="btn ghost"
              disabled={!tracks?.length}
              title={
                tracks?.length
                  ? 'Add the record to the end of the queue, without interrupting'
                  : childError
                    ? 'The track list could not be loaded'
                    : tracks === null
                      ? 'Still loading the track list'
                      : 'This record has no tracks'
              }
              onClick={() => tracks && onEnqueueAlbum(tracks)}
            >
              + Queue album
            </button>
          </div>
        </DetailHead>
        <div className="season-head">
          <h2>Tracks</h2>
        </div>
        {childError && (
          <p className="error">
            Could not load the track list: {childError}{' '}
            <button className="linklike" onClick={() => setChildAttempt((n) => n + 1)}>
              Try again
            </button>
          </p>
        )}
        {tracks !== null && tracks.length === 0 && !childError && (
          <p className="dim">No tracks in this record.</p>
        )}
        <ul className="rows tracks">
          {(tracks ?? []).map((t, i) => (
            <li key={t.id} className={`ep-row${t.id === playingId ? ' playing' : ''}`}>
              <button
                className="ep-open"
                title="Play from here"
                onClick={() => tracks && onPlayAlbum(tracks, i)}
              >
                {/* Marked, not numbered-and-marked: prepending the glyph
                    to the number crowded it out of a fixed-width column
                    and clipped it. Which track is playing matters more
                    than its position while it plays. */}
                <span className="tno mono dim">
                  {t.id === playingId ? '▶' : (t.episode ?? i + 1)}
                </span>
                <span className="ep-title">{t.title}</span>
                {t.played && (
                  <span className="track-played" title="played">
                    <Icon name="check" size={13} />
                  </span>
                )}
              </button>
              {/* Its own button, outside the row's: pressing a track plays
                  the record from there, which is not the same wish as
                  wanting this one track later. */}
              <button
                className="track-add"
                title="Add this track to the queue"
                onClick={() => onEnqueueTrack(t)}
              >
                <Icon name="plus" size={13} />
              </button>
            </li>
          ))}
        </ul>
      </main>
    )
  }

  const related = item.related && item.related.length > 0 && (
    <section className="meta-block">
      <h2>Related</h2>
      <ul className="related">
        {item.related.map((r) => (
          <li key={`${r.kind}-${r.title}`}>
            <span className="chip dim">{r.kind.replace('_', ' ')}</span>{' '}
            {r.item_id ? (
              <button className="linklike" onClick={() => onOpenEpisode(r.item_id!)}>
                {r.title ?? '?'}
              </button>
            ) : (
              <span className="dim">{r.title ?? '?'} (not in library)</span>
            )}
          </li>
        ))}
      </ul>
    </section>
  )

  if (item.kind === 'show') {
    // HUB-31: absolute-numbered (anime) episodes carry a TVDB-style
    // projection; the library's anime_view picks the presentation.
    // Derived from the list, so it has to wait for the list. Every one of
    // these read as a fact about the show while the request was still out: no
    // episodes, none watched, and no Continue button.
    // Everything below the head is derived from the list, so it waits for the
    // list. The head is NOT: the title, the poster, the overview and the way
    // back are all already in hand, and returning early meant a series over a
    // slow link showed a blank page where the old code at least showed the head
    // with a wrong count.
    const waiting = episodes === null
    const eps = episodes ?? []
    const projected = projecting(animeView, eps)
    const gSeason = (e: Item) => seasonOf(e, projected)
    const gEpisode = (e: Item) => episodeOf(e, projected)
    const ordered = projected
      ? [...eps].sort(
          (a, b) =>
            (gSeason(a) ?? 999) - (gSeason(b) ?? 999) || (gEpisode(a) ?? 0) - (gEpisode(b) ?? 0),
        )
      : eps
    const seasons = [...new Set(ordered.map(gSeason))]
    // First unwatched (or in-progress) episode = the continue point. Not while
    // waiting: "start from the beginning" is the wrong answer to "we have not
    // asked yet", and it flashed in as the list arrived.
    const next = waiting ? undefined : eps.find((e) => !e.played)
    return (
      <main>
        <button className="btn ghost small" onClick={goUp}>
          {upLabel}
        </button>
        <DetailHead
          item={item}
          progress={null}
          subline={
            waiting ? '' : `${eps.length} episodes · ${eps.filter((e) => e.played).length} watched`
          }
        >
          {/* The show's one action: get on with it. Named, so it is
              obvious which episode pressing it starts. */}
          {next && (
            <div className="play-row">
              <button className="btn" onClick={() => onOpenEpisode(next.id)}>
                {/* The same numbering as the list below it. Reading the native
                    fields here put "Continue · E10" above a row reading
                    "S01E10", and on a show whose projection spans seasons the
                    two numbers are not even close. */}
                ▶&nbsp; Continue · {seLabel(gSeason(next), gEpisode(next), next.episode_end)}
              </button>
              <span className="small-note">{next.title}</span>
            </div>
          )}
        </DetailHead>
        {childError && (
          <p className="error">
            Could not load the episodes: {childError}{' '}
            <button className="linklike" onClick={() => setChildAttempt((n) => n + 1)}>
              Try again
            </button>
          </p>
        )}
        {!waiting && !childError && eps.length === 0 && (
          <p className="dim">No episodes in this series yet.</p>
        )}
        {seasons.map((s) => {
          const inSeason = ordered.filter((e) => gSeason(e) === s)
          const watched = inSeason.filter((e) => e.played).length
          const all = watched === inSeason.length
          return (
            <section key={String(s)}>
              <div className="season-head">
                {/* The heading is the way into the season's own page, where the
                    episodes are stills rather than rows. */}
                <button className="season-open" onClick={() => onOpenSeason(s)}>
                  <h2>{seasonLabel(s, projected)}</h2>
                  <span className="shelf-arrow">→</span>
                </button>
                <span className="mono dimmer">
                  {watched}/{inSeason.length} watched
                </span>
                {/* One press for a season you have already seen
                    elsewhere — the alternative is thirteen presses. */}
                <button
                  className="linklike season-mark"
                  onClick={async () => {
                    try {
                      // One call for the whole season. WHICH episodes are
                      // in it is decided here, because the season a viewer
                      // sees can be a projection of absolute numbering —
                      // the hub would have to guess.
                      const r = await setWatched(
                        item.id,
                        !all,
                        inSeason.map((e) => e.id),
                      )
                      // The response carries the new state of every row it
                      // touched, so nothing needs re-asking.
                      const byId = new Map(r.updated.map((u) => [u.item_id, u]))
                      setEpisodes(
                        (prev) =>
                          // Not `?? []`: turning "not answered" into "no
                          // episodes" is the misreport this change is about.
                          prev &&
                          prev.map((e) => {
                            const u = byId.get(e.id)
                            return u
                              ? {
                                  ...e,
                                  played: u.played,
                                  play_count: u.play_count,
                                  resume_position_ms: 0,
                                }
                              : e
                          }),
                      )
                    } catch (err) {
                      notify(`Could not mark the season: ${err}`)
                    }
                  }}
                >
                  {all ? 'Mark none watched' : 'Mark all watched'}
                </button>
              </div>
              <ul className="rows">
                {inSeason.map((e) => (
                  <li key={e.id} className="ep-row">
                    <button className="ep-open" onClick={() => onOpenEpisode(e.id)}>
                      <span className="ep-no mono dim">
                        {seLabel(gSeason(e), gEpisode(e), e.episode_end)}
                        {/* HUB-31: the projection renumbers, so the file's
                            own absolute number is kept beside it. */}
                        {projected && <span className="dimmer"> #{e.episode}</span>}
                      </span>
                      <span className="ep-title">{e.title}</span>
                      {e.id === next?.id && !e.played && <span className="chip sand">next up</span>}
                      <span className="ep-state mono">
                        {e.played
                          ? ''
                          : e.resume_position_ms && e.resume_duration_ms
                            ? `${Math.round(watchedPct(e) ?? 0)}% in`
                            : ''}
                      </span>
                    </button>
                    <EpisodeSeen
                      episode={e}
                      onDone={() =>
                        fetchChildren(item.id)
                          .then((c) => setEpisodes(c.children))
                          .catch((e: unknown) => notify(`Could not refresh the episodes: ${e}`))
                      }
                    />
                  </li>
                ))}
              </ul>
            </section>
          )
        })}
        {related}
        {error && <div className="error">{error}</div>}
      </main>
    )
  }

  const best = item.sources_detail[0]
  const v = best?.streams?.video?.[0] as { fps?: [number, number] } | undefined
  const fileFps = v?.fps ? v.fps[0] / v.fps[1] : null
  // Both off the item's own duration, via `label.ts`. Computing them from
  // `best` — the largest single source FILE — lost the resume on every
  // multi-part film and over-read the bar on the rest.
  const resumeMs = resumeMsFor(item)
  const progress = watchedPct(item) ?? 0
  // For the runtime label only, and in that order: the hub's figure covers the
  // whole item, parts summed, but it is written by playback, so an item nobody
  // has opened has none. The file's own duration is the best guess left, and
  // for a multi-part film it is one part's — the client cannot do better,
  // because the source rows do not say which part they are (UI-27).
  const duration = item.resume_duration_ms ?? best?.streams?.duration_ms

  async function findSubs(langs: string[]) {
    setSubBusy(true)
    setSubNote('')
    try {
      const r = await searchSubtitles(item!.id, langs)
      setSubCands(r.candidates)
      setSubQuota(r.quota)
      if (r.candidates.length === 0) {
        setSubNote(
          langs.length > 0
            ? `Nothing in ${langs.join(', ')} for this file.`
            : 'No subtitles found for this file.',
        )
      }
    } catch (e) {
      setSubNote(String(e))
    } finally {
      setSubBusy(false)
    }
  }

  return (
    <main>
      <button className="btn ghost small" onClick={goUp}>
        {upLabel}
      </button>
      <DetailHead
        item={item}
        progress={progress > 0 ? progress : null}
        subline={[
          item.kind === 'episode' && item.show_title ? item.show_title : null,
          item.kind === 'episode' ? seLabel(item.season, item.episode, item.episode_end) : null,
          fmtDuration(duration),
          item.metadata?.genres?.length ? item.metadata.genres.join(' · ') : null,
        ]
          .filter(Boolean)
          .join(' · ')}
      >
        <div className="play-row">
          <button className="btn" disabled={!best?.available} onClick={() => onPlay(item.id)}>
            ▶&nbsp; {resumeMs ? 'Resume' : 'Play'}
          </button>
          {resumeMs > 0 && (
            <button className="btn ghost" onClick={() => onPlay(item.id, true)}>
              Play from start
            </button>
          )}
          <MarkWatched item={item} onDone={reloadSubs} />
        </div>
      </DetailHead>

      {item.metadata?.cast?.length ? (
        <section className="meta-block">
          <h2>Cast</h2>
          <ul className="cast">
            {item.metadata.cast.map((p) => (
              <li key={p.name}>
                <span>{p.name}</span>
                {p.character && <span className="dim"> as {p.character}</span>}
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {item.negotiated && (
        <>
          <h2>Playback plan</h2>
          <div className="plan">
            <div className="plan-head">
              <span className={`chip strong ${plan(item.negotiated.cost).tone}`}>
                {plan(item.negotiated.cost).chip}
              </span>
              {plan(item.negotiated.cost).note && (
                <span className="small-note dim">{plan(item.negotiated.cost).note}</span>
              )}
            </div>
            {/* One row per elementary stream, because that is the grain
                negotiation decides at: a file can copy its video and
                re-encode only its audio. */}
            <PlanRow label="video" verdict={item.negotiated.streams.video} />
            <PlanRow label="audio" verdict={item.negotiated.streams.audio} />
            {/* The one track you would actually get, not an inventory of
                the twenty-six in the file. Which of them plays is a
                question about your preferences, and it is the only
                subtitle fact that changes what you see. */}
            <PlanRow label="subtitles" {...chosenSubtitle(item, subs, prefs, mediaType)} />
            <div className="plan-foot">
              {/* Here, not in Settings and not only in the player: a mask
                  takes effect on the NEXT session, so it has to be
                  settable before Play is pressed. */}
              <button className="btn ghost small mono" onClick={() => setShowCaps((v) => !v)}>
                client capabilities
              </button>
              {/* Just "masked", as the player's transport says it. Listing
                  which bits were dropped repeated what the panel below shows
                  in full the moment it is open, and the badge only has to
                  answer whether what you are reading is real. */}
              {masked.length > 0 && (
                <span
                  className="caps-badge mono"
                  title="A mask is active — this is not what your real browser would get"
                >
                  masked
                </span>
              )}
              {/* OPS-10: the LAST session for this item, whoever played
                  it — the point is debugging a report from someone else,
                  after they have closed the player. */}
              {isAdmin() && (
                <button
                  className="btn ghost small log-link"
                  onClick={() =>
                    downloadWithAuth(`/admin/v1/items/${encodeURIComponent(id)}/log`).catch(
                      (e: unknown) => notify(`Could not download the session log: ${e}`),
                    )
                  }
                >
                  Last session log
                </button>
              )}
            </div>
            {/* Re-ASK, don't just re-badge. The negotiated half of this
                page is an answer to the capability profile, so editing
                the mask invalidates it — the deliveries, the ASS rung and
                the reasons all move. */}
            {showCaps && (
              <CapabilityDebug
                onChange={() => {
                  setCapsRev((n) => n + 1)
                  void reloadSubs()
                }}
              />
            )}
          </div>
        </>
      )}
      {error && <div className="error">{error}</div>}

      {related}

      <div className="season-head">
        <h2>Subtitles</h2>
        {/* A standing choice for this title beats the language list in
            Settings, so it has to be visible here — and revocable, or the
            only way back is to guess where it was set. */}
        {titleSub && (
          <span className="chip sand title-sub" title="This title overrides your language settings">
            {titleSub === 'off' ? 'no subtitles' : titleSub} for this title
            <button
              className="chip-x"
              title="Follow my language settings again"
              onClick={async () => {
                const scope = item.parent_id ?? item.id
                try {
                  await putPref(scope, 'subs', '')
                  setTitleSub('')
                } catch (e) {
                  notify(`Could not clear the override: ${e}`)
                }
              }}
            >
              ×
            </button>
          </span>
        )}
      </div>
      {/* The player is where tracks get picked; this section is about
          managing downloads, so the file's own tracks are one line. */}
      <p className="dim">
        {(() => {
          const own = subs.filter((s) => s.origin === 'embedded' || s.origin === 'sidecar')
          if (own.length === 0) return 'No subtitles in the file.'
          const langs = [...new Set(own.map((s) => s.language ?? '?'))]
          return `${own.length} in the file: ${langs.slice(0, 12).join(', ')}${
            langs.length > 12 ? `, +${langs.length - 12} more` : ''
          }`
        })()}
      </p>
      <ul className="rows subs-list">
        {subs
          .filter((s) => s.origin === 'downloaded' || s.origin === 'ocr' || s.origin === 'raster')
          .map((s) => (
            <li key={s.id}>
              <span className="chips">
                {/* "ocr" stays visible: machine-read text is imperfect
                    by nature and must say so (HUB-32c). */}
                <span className="chip">{s.origin}</span>
                <span>
                  {s.language ?? '?'} · {s.format}
                </span>
                {/* What this track is DOING for the caps this browser
                    declares. A stored artefact the ladder currently
                    skips otherwise reads as the only subtitle on the
                    item — the file's own tracks are one line of prose
                    above, so an idle row looks like the whole story. */}
                <span
                  className={s.delivery === 'none' ? 'chip dim' : 'chip'}
                  title={s.note || undefined}
                >
                  {s.delivery === 'none' ? 'unused' : s.delivery}
                </span>
              </span>
              {/* Only a DOWNLOADED track, and only for whoever spent
                  the provider quota on it (or an admin). The caches
                  rebuild themselves, so removing one would be a button
                  that undoes nothing. */}
              {s.deletable && (
                <button
                  className="btn ghost small"
                  onClick={() =>
                    deleteSubtitle(s.id)
                      .then(reloadSubs)
                      .catch((e) => setError(String(e)))
                  }
                >
                  Remove
                </button>
              )}
            </li>
          ))}
      </ul>
      <div className="row-form">
        <button
          className="btn ghost small"
          disabled={subBusy}
          onClick={() => void findSubs(subLangs)}
        >
          {subBusy
            ? 'Searching…'
            : subLangs.length > 0
              ? `Find subtitles (${subLangs.join(', ')})`
              : 'Find subtitles online'}
        </button>
        {/* The language filter comes from Settings → this media type;
            offer the unfiltered search when it finds nothing. */}
        {subLangs.length > 0 && subCands?.length === 0 && !subBusy && (
          <button className="btn ghost small" onClick={() => void findSubs([])}>
            Search all languages
          </button>
        )}
        {subNote && <span className="dim">{subNote}</span>}
        {quotaLabel(subQuota) && <span className="dim mono">{quotaLabel(subQuota)}</span>}
      </div>
      {/* In a dialog, not in the page: twenty-five candidates shoved
          Sources and the attribution a screen and a half down, and
          choosing one is a decision that deserves the foreground. */}
      {subCands && (
        <div className="dialog-backdrop" onClick={() => setSubCands(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <div className="dialog-head">
              <h2>Subtitles for “{item.title}”</h2>
              <button className="chip-x" onClick={() => setSubCands(null)} title="Close">
                ✕
              </button>
            </div>
            <p className="dim small-note">
              {quotaLabel(subQuota) ||
                'Downloads are shared with everyone on this server unless you attach your own account in Settings.'}
            </p>
            {subNote && <p className="dim">{subNote}</p>}
            {subCands.length === 0 && (
              <div className="row-form">
                {subLangs.length > 0 && !subBusy && (
                  <button className="btn ghost small" onClick={() => void findSubs([])}>
                    Search every language instead
                  </button>
                )}
              </div>
            )}
            <ul className="rows sub-candidates">
              {subCands.slice(0, 25).map((c) => (
                <li key={c.file_id}>
                  <span className="chips">
                    {c.hash_match && (
                      <span
                        className="chip"
                        title="the provider has this exact file's hash on this subtitle"
                      >
                        hash
                      </span>
                    )}
                    <span className="chip dim">{c.language ?? '?'}</span>
                    <span className="cand-name">{c.release_name ?? '(no name)'}</span>
                    <span className="dim mono">{c.downloads} dl</span>
                    {c.rating ? <span className="dim mono">★ {c.rating.toFixed(1)}</span> : null}
                    {c.uploader && <span className="dim">by {c.uploader}</span>}
                    {/* fps mismatch is the classic cause of progressive drift */}
                    {c.fps && fileFps && Math.abs(c.fps - fileFps) > 0.1 ? (
                      <span
                        className="chip warn"
                        title={`timed for ${c.fps} fps; this file is ${fileFps.toFixed(3)} fps`}
                      >
                        {c.fps} fps
                      </span>
                    ) : null}
                  </span>
                  <button
                    className="btn small"
                    disabled={subBusy}
                    onClick={() => {
                      setSubBusy(true)
                      downloadSubtitle(item.id, c.file_id, c.language)
                        .then((r) => {
                          setSubQuota(r.quota)
                          setSubCands(null)
                          notify('Subtitle downloaded — it is now a track on this item.')
                          return reloadSubs()
                        })
                        .catch((e) => setSubNote(String(e)))
                        .finally(() => setSubBusy(false))
                    }}
                  >
                    Download
                  </button>
                </li>
              ))}
            </ul>
          </div>
        </div>
      )}

      <h2>Sources</h2>
      <ul className="sources">
        {item.sources_detail.map((s) => (
          <li key={s.path_rel}>
            <span className="path mono">{s.path_rel}</span>
            {s.revision > 1 && (
              <span className="chip" title="corrected release (v2 / REPACK / PROPER)">
                v{s.revision}
              </span>
            )}
            <Chips s={s} />
          </li>
        ))}
      </ul>
      {item.metadata?.provider === 'tmdb' && (
        <footer className="tmdb-attrib">
          <img src={tmdbLogo} alt="TMDB" />
          <span>
            This product uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise
            approved by TMDB.
          </span>
        </footer>
      )}
      {item.metadata?.provider === 'musicbrainz' && (
        <footer className="tmdb-attrib">
          <span>Metadata from MusicBrainz; cover art from the Cover Art Archive.</span>
        </footer>
      )}
      {item.metadata?.provider === 'anilist' && (
        <footer className="tmdb-attrib">
          <span>Metadata from AniList and AniDB.</span>
        </footer>
      )}
      {item.metadata?.provider === 'tvdb' && (
        <footer className="tmdb-attrib">
          <span>Metadata provided by TheTVDB. Please consider contributing missing data.</span>
        </footer>
      )}
    </main>
  )
}
