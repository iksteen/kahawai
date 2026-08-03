import { useEffect, useMemo, useState } from 'react'
import {
  artworkUrl,
  fetchChildren,
  fetchItem,
  fetchLibraries,
  fetchPrefs,
  resolveTracks,
  searchSubtitles,
  downloadSubtitle,
  deleteSubtitle,
  quotaLabel,
  type SubtitleQuota,
  type Subtitle,
  type SubtitleCandidate,
  startPlaybackSession,
  type Item,
  type ItemDetail,
  type Session,
  type Source,
  isAdmin,
  itemLogUrl,
} from '../api'
import AlbumPlayer from './AlbumPlayer'
import CapabilityDebug from './CapabilityDebug'
import { loadMask, maskSummary } from '../capabilities'
import tmdbLogo from '../assets/tmdb.svg'
import placeholder from '../assets/placeholder.svg'

const GB = 1024 * 1024 * 1024

// S01E02 for seasoned episodes; E11 for absolute numbering (anime);
// E01-02 for a batch file spanning a range.
function seLabel(season: number | null, episode: number | null, end?: number | null) {
  let e = `E${String(episode ?? 0).padStart(2, '0')}`
  if (end != null) e += `-${String(end).padStart(2, '0')}`
  return season === null ? e : `S${String(season).padStart(2, '0')}${e}`
}

function fmtDuration(ms?: number) {
  if (!ms) return null
  const m = Math.round(ms / 60000)
  return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`
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
      {st.subtitles?.map((t, i) => (
        <span className="chip dim" key={`s${i}`}>
          {t.format}
        </span>
      ))}
      <span className="chip dim">{(s.size / GB).toFixed(1)} GB</span>
      {!s.available && <span className="chip warn">offline</span>}
    </span>
  )
}

export default function Detail({
  id,
  autoPlay,
  fromLib,
  onPlay,
  onOpenEpisode,
  onOpenLibrary,
}: {
  id: string
  autoPlay?: boolean
  fromLib: string
  onPlay: (item: ItemDetail, session: Session, resumeMs: number) => void
  onOpenEpisode: (id: string) => void
  onOpenLibrary: (id: string) => void
}) {
  const [item, setItem] = useState<ItemDetail | null>(null)
  const [episodes, setEpisodes] = useState<Item[]>([])
  const [animeView, setAnimeView] = useState<'seasons' | 'native'>('seasons')
  const [mediaType, setMediaType] = useState('')
  // HUB-21/24: subtitle tracks + external search results.
  const [subs, setSubs] = useState<Subtitle[]>([])
  const [subCands, setSubCands] = useState<SubtitleCandidate[] | null>(null)
  // HUB-33 subtitle-language preference for this library's media type;
  // empty = no preference, search every language.
  const [subLangs, setSubLangs] = useState<string[]>([])
  const [subBusy, setSubBusy] = useState(false)
  const [subNote, setSubNote] = useState('')
  const [subQuota, setSubQuota] = useState<SubtitleQuota | null>(null)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const [showCaps, setShowCaps] = useState(false)
  // The badge reads the stored mask, which the panel edits underneath
  // it; the counter repaints it on an edit instead of leaving the
  // previous mask on screen.
  const [capsRev, setCapsRev] = useState(0)
  const masked = useMemo(() => maskSummary(loadMask()), [capsRev])

  useEffect(() => {
    setItem(null)
    setEpisodes([])
    setSubs([])
    setSubCands(null)
    setSubNote('')
    fetchItem(id)
      .then((d) => {
        setItem(d)
        setSubs(d.negotiated?.subtitles ?? [])
      })
      .catch((e) => setError(String(e)))
  }, [id])
  // The subtitle list rides on the item now — one question, one
  // answer. After a download or a delete that means re-asking the
  // item, which is heavier than the old list refetch but pure.
  const reloadSubs = () =>
    fetchItem(id)
      .then((d) => {
        setItem(d)
        setSubs(d.negotiated?.subtitles ?? [])
      })
      .catch(() => setSubs([]))
  useEffect(() => {
    if (item?.kind === 'show' || item?.kind === 'album') {
      fetchChildren(item.id)
        .then((c) => setEpisodes(c.children))
        .catch((e) => setError(String(e)))
    }
    // Library context: media type (per-type track settings, HUB-33).
    // Anime presentation (HUB-31) is purely a user preference; default
    // is the projected seasons view.
    Promise.all([fetchLibraries(), fetchPrefs().catch(() => ({ prefs: [] }))])
      .then(([l, p]) => {
        const mt = l.libraries.find((x) => x.id === fromLib)?.media_type ?? ''
        setMediaType(mt)
        const mine = p.prefs.find((x) => x.scope === '' && x.key === 'anime_view')?.value
        setAnimeView(mine === 'native' ? 'native' : 'seasons')
        setSubLangs(
          (p.prefs.find((x) => x.scope === '' && x.key === `subs.${mt}`)?.value ?? '')
            .split(',')
            .map((s) => s.trim())
            .filter(Boolean),
        )
      })
      .catch(() => {})
  }, [item?.id, item?.kind, fromLib])
  const [queueAt, setQueueAt] = useState<number | null>(null)
  // Deep-linked or history-forwarded /play URLs: start playback once
  // the item is loaded (shows have nothing to autoplay).
  const [autoPlayed, setAutoPlayed] = useState(false)
  useEffect(() => {
    if (autoPlay && !autoPlayed && item && item.kind !== 'show') {
      setAutoPlayed(true)
      const best = item.sources_detail[0]
      if (best?.available) void play()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoPlay, autoPlayed, item?.id])

  if (error) return <div className="error page-pad">{error}</div>
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
        <div className="detail-head album-head">
          <img
            className="card-art album-art"
            src={artworkUrl(item.id, item.art_version)}
            alt=""
            onError={(e) => {
              e.currentTarget.onerror = null
              e.currentTarget.src = placeholder
            }}
          />
          <div>
            <h1>
              {item.title} {item.year && <span className="year">({item.year})</span>}
            </h1>
            <div className="detail-sub mono">
              {item.artist ?? ''} · {tracks.length} tracks
            </div>
            <div className="play-row">
              <button className="btn" disabled={!tracks.length} onClick={() => setQueueAt(0)}>
                Play album
              </button>
            </div>
          </div>
        </div>
        <ul className="rows">
          {tracks.map((t, i) => (
            <li key={t.id}>
              <button
                className={`card episode track-row${queueAt === i ? ' playing' : ''}`}
                onClick={() => setQueueAt(i)}
              >
                <span className="tno mono">{t.episode ?? i + 1}</span>
                <span>{t.title}</span>
                {t.played && <span className="seen"> ✓</span>}
              </button>
            </li>
          ))}
        </ul>
        {queueAt !== null && (
          <AlbumPlayer
            tracks={tracks}
            startAt={queueAt}
            onTrackChange={(i) => setQueueAt(i)}
            onStop={() => setQueueAt(null)}
          />
        )}
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
    const projected =
      animeView === 'seasons' && episodes.some((e) => e.proj_season != null)
    const gSeason = (e: Item) => (projected ? e.proj_season ?? null : e.season)
    const gEpisode = (e: Item) => (projected ? e.proj_episode ?? e.episode : e.episode)
    const ordered = projected
      ? [...episodes].sort(
          (a, b) =>
            (gSeason(a) ?? 999) - (gSeason(b) ?? 999) ||
            (gEpisode(a) ?? 0) - (gEpisode(b) ?? 0),
        )
      : episodes
    // null season = absolute numbering (anime); distinct from Specials.
    const seasonLabel = (s: number | null) =>
      s === null ? (projected ? 'Other' : 'Episodes') : s === 0 ? 'Specials' : `Season ${s}`
    const seasons = [...new Set(ordered.map(gSeason))]
    // First unwatched (or in-progress) episode = the continue point.
    const next = episodes.find((e) => !e.played)
    return (
      <main>
        <button className="btn ghost small" onClick={goUp}>
          {upLabel}
        </button>
        <div className="detail-head">
          <h1>
            {item.title} {item.year && <span className="year">({item.year})</span>}
          </h1>
          <div className="detail-sub mono">
            {episodes.length} episodes
            {next && (
              <>
                {' · next: '}
                <button className="btn ghost small" onClick={() => onOpenEpisode(next.id)}>
                  {seLabel(next.season, next.episode, next.episode_end)} {next.title}
                </button>
              </>
            )}
          </div>
        </div>
        {seasons.map((s) => (
          <section key={String(s)}>
            <h2>{seasonLabel(s)}</h2>
            <ul className="rows">
              {ordered
                .filter((e) => gSeason(e) === s)
                .map((e) => (
                  <li key={e.id}>
                    <button className="card episode" onClick={() => onOpenEpisode(e.id)}>
                      <span className="mono dim">
                        E{String(gEpisode(e) ?? 0).padStart(2, '0')}
                        {e.episode_end != null &&
                          `-${String(e.episode_end).padStart(2, '0')}`}
                        {projected && <span className="dim"> #{e.episode}</span>}
                      </span>{' '}
                      {e.title}
                      {e.played && <span className="seen"> ✓</span>}
                      {!e.played && e.resume_position_ms ? (
                        <span className="dim"> · resume</span>
                      ) : null}
                    </button>
                  </li>
                ))}
            </ul>
          </section>
        ))}
        {related}
        {error && <div className="error">{error}</div>}
      </main>
    )
  }

  const best = item.sources_detail[0]
  const v = best?.streams?.video?.[0] as { fps?: [number, number] } | undefined
  const fileFps = v?.fps ? v.fps[0] / v.fps[1] : null
  const duration = best?.streams?.duration_ms
  const resumeMs =
    item.resume_position_ms && duration && item.resume_position_ms < duration * 0.9
      ? item.resume_position_ms
      : 0
  const progress = duration && resumeMs ? (resumeMs / duration) * 100 : 0

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

  async function play(fromStart = false) {
    setBusy(true)
    setError('')
    try {
      // Remux/transcode sessions start their pipeline at the resume
      // point (§6) — no waiting for a transcode to catch up. Direct
      // sessions resume client-side.
      const start = fromStart ? 0 : resumeMs
      // HUB-33: pick the default audio track from the user's prefs
      // (series memory > per-media-type settings), entirely client-side.
      const audio = item!.sources_detail[0]?.streams?.audio ?? []
      let audioTrack = 0
      try {
        const p = await fetchPrefs()
        audioTrack = resolveTracks(
          p.prefs,
          item!.parent_id ?? item!.id,
          item!.id,
          mediaType,
          item!.metadata?.original_language,
          audio,
        ).audioTrack
      } catch {
        // prefs unavailable → source order
      }
      // HUB-14: the hub decides the mode from the capability profile
      // (built in one shared place, mask included). start_ms is ignored
      // by the direct path server-side, so resuming needs no mode
      // prediction here.
      const session = await startPlaybackSession(item!, start, audioTrack)
      onPlay(item!, session, start)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <main>
      <button className="btn ghost small" onClick={goUp}>
        {upLabel}
      </button>
      <div className="detail-head album-head">
        {item.kind === 'episode' && item.metadata && (
          <img
            className="card-art episode-still"
            src={artworkUrl(item.id, item.art_version)}
            alt=""
            onError={(e) => {
              e.currentTarget.onerror = null
              e.currentTarget.src = placeholder
            }}
          />
        )}
        <div>
        <h1>
          {item.kind === 'episode' && item.show_title ? `${item.show_title} · ` : ''}
          {item.kind === 'episode'
            ? `${seLabel(item.season, item.episode, item.episode_end)} · `
            : ''}
          {item.title} {item.year && <span className="year">({item.year})</span>}
        </h1>
        <div className="detail-sub mono">
          {fmtDuration(duration)}
          {item.played && <span className="seen"> · seen ×{item.play_count}</span>}
        </div>
        {progress > 0 && (
          <div className="waterline">
            <div className="waterline-fill" style={{ width: `${progress}%` }} />
          </div>
        )}
        </div>
      </div>

      {(item.metadata?.overview || item.metadata?.genres?.length) && (
        <section className="meta-block">
          {item.metadata.overview && <p className="overview">{item.metadata.overview}</p>}
          <div className="detail-sub mono">
            {item.metadata.premiered && <span>{item.metadata.premiered}</span>}
            {item.metadata.rating ? <span> · ★ {item.metadata.rating.toFixed(1)}</span> : null}
            {item.metadata.genres?.length ? (
              <span> · {item.metadata.genres.join(' · ')}</span>
            ) : null}
            {item.metadata.confidence === 'weak' && (
              <span className="dim"> · uncertain match</span>
            )}
          </div>
        </section>
      )}
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

      <div className="play-row">
        <button className="btn" disabled={busy || !best?.available} onClick={() => play()}>
          {resumeMs ? `Resume` : 'Play'}
        </button>
        {resumeMs > 0 && (
          <button className="btn ghost" disabled={busy} onClick={() => play(true)}>
            Play from start
          </button>
        )}
        <span className="dim small-note">
          negotiated per stream — the player overlay shows the taken path
        </span>
        {/* Here, not in Settings and not only in the player: a mask
            takes effect on the NEXT session, so it has to be settable
            before this button is pressed. */}
        <button className="btn ghost small" onClick={() => setShowCaps((v) => !v)}>
          {showCaps ? 'hide caps' : 'caps'}
        </button>
        {masked.length > 0 && <span className="caps-badge mono">masked: {masked.join(' ')}</span>}
        {/* OPS-10: the LAST session for this item, whoever played it —
            the point is debugging a report from someone else, after
            they have closed the player. */}
        {isAdmin() && (
          <a className="btn ghost small" href={itemLogUrl(id)} download>
            Last session log
          </a>
        )}
      </div>
      {showCaps && <CapabilityDebug onChange={() => setCapsRev((n) => n + 1)} />}
      {error && <div className="error">{error}</div>}

      {related}

      <h2>Subtitles</h2>
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
          .filter(
            (s) => s.origin === 'downloaded' || s.origin === 'ocr' || s.origin === 'raster',
          )
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
      {subCands && subCands.length > 0 && (
        <ul className="rows sub-candidates">
          {subCands.slice(0, 25).map((c) => (
            <li key={c.file_id}>
              <span className="chips">
                {c.hash_match && (
                  <span className="chip" title="the provider has this exact file's hash on this subtitle">
                    hash
                  </span>
                )}
                <span className="chip dim">{c.language ?? '?'}</span>
                <span>{c.release_name ?? '(no name)'}</span>
                <span className="dim mono">{c.downloads} dl</span>
                {c.rating ? <span className="dim mono">★ {c.rating.toFixed(1)}</span> : null}
                {c.uploader && <span className="dim">by {c.uploader}</span>}
                {/* fps mismatch is the classic cause of progressive drift */}
                {c.fps && fileFps && Math.abs(c.fps - fileFps) > 0.1 ? (
                  <span className="chip warn" title={`timed for ${c.fps} fps; this file is ${fileFps.toFixed(3)} fps`}>
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
                      setSubNote('Downloaded — available as a subtitle track.')
                      setSubCands(null)
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
            This product uses TMDB and the TMDB APIs but is not endorsed, certified, or
            otherwise approved by TMDB.
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
