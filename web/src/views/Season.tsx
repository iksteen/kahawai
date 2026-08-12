import { useEffect, useState } from 'react'
import Failed from '../Failed'
import {
  artworkSrcSet,
  artworkUrl,
  fetchChildren,
  fetchItem,
  prefsOrNone,
  setWatched,
  type Item,
  type ItemDetail,
} from '../api'
import Icon from '../icons'
import Lane from '../Lane'
import {
  episodeOf,
  projecting,
  resumeMsFor,
  seLabel,
  seasonLabel,
  seasonOf,
  watchedPct,
} from '../label'
import { notify } from '../toast'

const GB = 1024 * 1024 * 1024

/// How much of the strip one press of an arrow moves: two and a bit
/// cards, so something new is always in view and something old still is.
const STRIP_STEP = 220 * 2.5

/// One season, browsed by its stills.
///
/// The show page lists episodes as rows, which is the right shape for
/// picking a number. This is the other question — "which one was that
/// again" — so the episodes are pictures, and the one you land on opens
/// underneath rather than on a page of its own.
export default function Season({
  showId,
  season,
  onPlay,
  onOpenShow,
}: {
  showId: string
  /// The season as it appears in the URL. Null is absolute numbering, not
  /// "unknown" — see `seasonLabel`.
  season: number | null
  onPlay: (id: string, fromStart?: boolean) => void
  onOpenShow: (id: string) => void
}) {
  const [show, setShow] = useState<ItemDetail | null>(null)
  /// `null` until the children answer. `[]` meant either "loading" or "this
  /// show has no episodes", so the empty-season explanation was suppressed for
  /// the case it was written for and the page read as broken instead.
  const [eps, setEps] = useState<Item[] | null>(null)
  const [animeView, setAnimeView] = useState<'seasons' | 'native'>('seasons')
  const [pickedId, setPickedId] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [picked, setPicked] = useState<ItemDetail | null>(null)
  /// The season itself failed to load: there is no page to show, so the
  /// whole view becomes `Failed`.
  const [loadError, setLoadError] = useState('')
  /// Bumped by Try again, so asking again re-runs the load that failed.
  const [attempt, setAttempt] = useState(0)

  /// `fatal` only for the load that builds the page. The two mark handlers
  /// call this to refresh AFTER a write that already succeeded, and turning
  /// that into `Failed` threw away the season for a stale tick — the mark
  /// applied, and the screen said the season could not be loaded.
  const reload = (fatal = false, live: () => boolean = () => true) =>
    fetchChildren(showId)
      .then((c) => {
        if (!live()) return
        // The episodes ARE the page, so this is what clears a fatal error —
        // not the item request beside it. Clearing on that one meant a
        // children failure was erased by the title arriving: `eps` stayed
        // null, so the page read as still loading, for ever, with no error
        // and nothing left to ask again. Which of the two you got depended on
        // which request settled last.
        setLoadError('')
        setEps(c.children)
      })
      .catch((e) => {
        if (!live()) return
        if (fatal) setLoadError(String(e))
        else notify(`Could not refresh the episodes: ${e}`)
      })

  useEffect(() => {
    setShow(null)
    setEps(null)
    setPickedId(null)
    // Fenced like the others: Try again leaves the button up for the whole
    // request, so two loads can be in flight and the older one must not speak.
    let live = true
    // NOT fatal. All this request supplies is the title on the back button,
    // which falls back to "Series" — so failing it took a page whose episodes
    // had arrived and replaced the whole thing with `Failed`, and which of the
    // two the viewer got depended on which request settled last.
    fetchItem(showId)
      .then((d) => live && setShow(d))
      .catch((e) => live && notify(`Could not load the show's details: ${e}`))
    void reload(true, () => live)
    prefsOrNone()
      .then(
        (p) =>
          live &&
          setAnimeView(
            p.prefs.find((x) => x.scope === '' && x.key === 'anime_view')?.value === 'native'
              ? 'native'
              : 'seasons',
          ),
      )
      .catch(() => {})
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showId, attempt])

  const waiting = eps === null
  const all = eps ?? []
  const projected = projecting(animeView, all)
  const mine = all
    .filter((e) => seasonOf(e, projected) === season)
    .sort((a, b) => (episodeOf(a, projected) ?? 0) - (episodeOf(b, projected) ?? 0))

  // Land on the first thing you have not finished — the reason you came.
  useEffect(() => {
    if (pickedId || mine.length === 0) return
    setPickedId((mine.find((e) => !e.played) ?? mine[0]).id)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mine.length])

  /// And bring it into the strip, which is the half that was missing. Landing
  /// on the tenth episode of a season scrolled the lane nowhere: the panel
  /// below opened on an episode whose card was 950px off the right-hand edge,
  /// so the strip showed five watched episodes with nothing lit and read as
  /// unrelated to what was underneath it. `inline` moves the lane, `block:
  /// 'nearest'` keeps the page where it is.
  useEffect(() => {
    if (!pickedId) return
    document
      .querySelector('.ep-card.picked')
      ?.scrollIntoView({ inline: 'nearest', block: 'nearest' })
  }, [pickedId])

  // The strip carries what browse gives; the panel underneath needs the
  // item's own answer (overview, sources, what it would be served as), so
  // it is fetched per selection.
  useEffect(() => {
    if (!pickedId) return
    let live = true
    setPicked(null)
    fetchItem(pickedId)
      .then((d) => live && setPicked(d))
      // Not `error`: that renders inside the panel this failure prevents, so
      // the card took the highlight, nothing opened, and clicking it again
      // was a no-op because `pickedId` had not changed. A dead click, for
      // good.
      .catch((e) => {
        if (!live) return
        notify(`Could not open that episode: ${e}`)
        setPickedId(null)
      })
    return () => {
      live = false
    }
  }, [pickedId])

  const watched = mine.filter((e) => e.played).length
  const allSeen = mine.length > 0 && watched === mine.length

  if (loadError)
    return (
      <Failed
        what="Could not load this season."
        message={loadError}
        // The error stays up while the retry is out — clearing it dropped the
        // season back to skeletons for the length of the request.
        onRetry={() => setAttempt((n) => n + 1)}
        away={{ label: 'Back to the show', go: () => onOpenShow(showId) }}
      />
    )

  return (
    <main>
      <button className="btn ghost small back" onClick={() => onOpenShow(showId)}>
        ← {show?.title ?? 'Series'}
      </button>
      <div className="season-top">
        <h1>{seasonLabel(season, projected)}</h1>
        <span className="mono dim">
          {waiting ? '' : `${mine.length} episodes · ${watched} watched`}
        </span>
        <button
          className={`btn ghost small mark${allSeen ? ' on' : ''}`}
          disabled={busy || mine.length === 0}
          title={
            waiting
              ? 'Still loading the episodes'
              : mine.length === 0
                ? 'This season has no episodes'
                : undefined
          }
          onClick={async () => {
            setBusy(true)
            try {
              // One call: which episodes make up this season is decided
              // here, since it can be a projection of absolute numbering.
              await setWatched(
                showId,
                !allSeen,
                mine.map((e) => e.id),
              )
              await reload()
            } catch (err) {
              notify(`Could not mark the season: ${err}`)
            } finally {
              setBusy(false)
            }
          }}
        >
          <Icon name="check" size={13} />
          {allSeen ? 'Mark none watched' : 'Mark all watched'}
        </button>
      </div>

      {/* A hand-typed or stale season number renders a heading, an empty strip
          and two dead arrows, which reads as broken rather than as empty. */}
      {!waiting && mine.length === 0 && (
        <p className="dim">
          No episodes in {seasonLabel(season, projected).toLowerCase()}.{' '}
          <button className="linklike" onClick={() => onOpenShow(showId)}>
            All episodes
          </button>
        </p>
      )}
      <Lane className="ep-strip" step={STRIP_STEP}>
        {mine.map((e) => {
          const done = watchedPct(e)
          return (
            <button
              key={e.id}
              className={`ep-card${e.id === pickedId ? ' picked' : ''}`}
              onClick={() => setPickedId(e.id)}
            >
              <span className="card-artbox">
                <img
                  className="ep-still"
                  src={artworkUrl(e.id, e.art_version, 'card')}
                  srcSet={artworkSrcSet(e.id, e.art_version)}
                  loading="lazy"
                  alt=""
                  onError={(ev) => ev.currentTarget.classList.add('art-failed')}
                />
                <span className="ep-badge mono">
                  {seLabel(seasonOf(e, projected), episodeOf(e, projected), e.episode_end)}
                </span>
                {e.played && (
                  <span className="seen-badge" title="seen">
                    <Icon name="check" />
                  </span>
                )}
                {done !== null && !e.played && (
                  <span className="card-progress">
                    <span className="card-progress-fill" style={{ width: `${done}%` }} />
                  </span>
                )}
              </span>
              <span className="shelf-title">{e.title}</span>
              <span className="shelf-meta mono">
                {e.played ? 'seen' : done !== null ? `${Math.round(done)}% in` : ''}
              </span>
            </button>
          )
        })}
      </Lane>

      {picked && (
        <div className="ep-detail">
          <span className="card-artbox ep-detail-art">
            <img
              className="ep-still"
              src={artworkUrl(picked.id, picked.art_version, 'card')}
              srcSet={artworkSrcSet(picked.id, picked.art_version)}
              alt=""
              onError={(ev) => ev.currentTarget.classList.add('art-failed')}
            />
          </span>
          <div className="detail-meta">
            {/* Numbered from the strip's row, not from the fetched item: only
                browse carries the projection, so asking the item for itself got
                `proj_season: null` and this line printed E10 under a card
                badged S01E10 — the same episode, numbered two ways, a
                centimetre apart. */}
            <div className="mono dim">
              {(() => {
                const row = mine.find((e) => e.id === picked.id) ?? picked
                return seLabel(seasonOf(row, projected), episodeOf(row, projected), row.episode_end)
              })()}
            </div>
            <h2 className="ep-detail-title">{picked.title}</h2>
            {picked.metadata?.premiered && (
              <div className="detail-sub mono">{picked.metadata.premiered}</div>
            )}
            {picked.metadata?.overview && <p className="overview">{picked.metadata.overview}</p>}
            <div className="play-row">
              <button
                className="btn"
                disabled={busy || !picked.sources_detail[0]?.available}
                title={
                  picked.sources_detail[0]?.available
                    ? undefined
                    : 'The machine holding this file is not answering'
                }
                onClick={() => onPlay(picked.id)}
              >
                ▶&nbsp; {resumeMsFor(picked) ? 'Resume' : 'Play'}
              </button>
              {!!resumeMsFor(picked) && (
                <button className="btn ghost" onClick={() => onPlay(picked.id, true)}>
                  Play from start
                </button>
              )}
              <button
                className={`btn ghost small mark${picked.played ? ' on' : ''}`}
                disabled={busy}
                onClick={async () => {
                  setBusy(true)
                  try {
                    await setWatched(picked.id, !picked.played)
                    await reload()
                    setPicked(await fetchItem(picked.id))
                  } catch (err) {
                    notify(`Could not change the watched mark: ${err}`)
                  } finally {
                    setBusy(false)
                  }
                }}
              >
                <Icon name="check" />
                {picked.played ? 'Watched' : 'Mark watched'}
              </button>
            </div>
            {/* Not the path. `path_rel` is release-group filenames and the
                directory layout of somebody's disk, and this drawer is the
                ordinary way to browse episodes — Detail shows it under a
                Sources heading, which is a different audience. The file
                still identifies itself by what it is. */}
            <div className="mono dimmer ep-path">
              {((picked.sources_detail[0]?.size ?? 0) / GB).toFixed(1)} GB
            </div>
          </div>
        </div>
      )}
    </main>
  )
}
