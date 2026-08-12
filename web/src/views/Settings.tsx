import { useEffect, useRef, useState } from 'react'
import { fetchPrefs, putPref } from '../api'
import { notify } from '../toast'
import { addAbove, moved, useDragOrder } from '../reorder'
import Icon from '../icons'
import Failed from '../Failed'

// HUB-33 per-media-type track defaults, stored as user-global prefs:
// key `audio.{type}` / `subs.{type}` = ordered language list ('original'
// allowed for audio). Every mutation saves immediately; per-series
// manual choices still outrank these.
const MEDIA_TYPES = ['movies', 'series', 'anime'] as const

const SUGGEST = [
  'original',
  'en',
  'nl',
  'ja',
  'de',
  'fr',
  'es',
  'it',
  'pt',
  'sv',
  'da',
  'no',
  'fi',
  'pl',
  'ru',
  'zh',
  'ko',
]
const TOKEN = /^(original|[a-z]{2,3})$/

/// An optimistic write that can be put back.
///
/// Reverting to `value` reverts to the prop from the closure that made the
/// request. Two changes in quick succession — a drag, then another before the
/// first answers — and a failure of the older one drags the list back past a
/// change that was saved. So the revert target is the last value the server
/// actually confirmed, and only the newest write is allowed to use it.
function useOptimistic(value: string, onChange: (v: string) => void, flash: () => void) {
  const seq = useRef(0)
  const inflight = useRef(0)
  const saved = useRef(value)
  /// Which write `saved` came from. `putPref` guarantees replies land in this
  /// order too; the sequence still decides whether a failure is current enough
  /// to put anything back on screen.
  const savedSeq = useRef(0)
  // Nothing outstanding means the prop is what the server has.
  if (inflight.current === 0) saved.current = value
  return (v: string, put: () => Promise<unknown>) => {
    const mine = ++seq.current
    inflight.current++
    onChange(v)
    // `putPref` serialises whole-state commits per preference key. The
    // optimistic control still moves now; only its network writes wait.
    put()
      .then(() => {
        // Any success moves the revert target, not just the newest one. Only
        // advancing it for the newest meant an older write succeeding while a
        // newer one was still out left the target at the value from BEFORE
        // both — so when the newer one then failed, the revert went back past
        // a change the server had accepted and kept.
        if (mine > savedSeq.current) {
          savedSeq.current = mine
          saved.current = v
        }
        flash()
      })
      .catch(() => {
        if (mine !== seq.current) return
        onChange(saved.current)
        notify('Could not save that — put back the way it was.')
      })
      .finally(() => {
        inflight.current--
      })
  }
}

function LangChips({
  mediaType,
  kind,
  value,
  onChange,
  flash,
}: {
  mediaType: string
  kind: 'audio' | 'subs'
  value: string
  onChange: (v: string) => void
  flash: () => void
}) {
  // 'original' is the permanent audio backstop: always in the list,
  // not removable (reorderable — others may be preferred above it).
  const stored = value ? value.split(',') : []
  const items = kind === 'audio' && !stored.includes('original') ? [...stored, 'original'] : stored
  const [entry, setEntry] = useState('')
  const [bad, setBad] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  /// Show the new order first, then save it. Waiting for the round trip
  /// meant the pills still read the old order while the request was out, so
  /// a second drag in that window computed its move from the stale list and
  /// wrote it back — losing whatever the first drag had just done.
  const write = useOptimistic(value, onChange, flash)
  const commit = (list: string[]) => {
    const v = list.join(',')
    write(v, () => putPref('', `${kind}.${mediaType}`, v))
  }

  const add = () => {
    const t = entry.trim().toLowerCase()
    if (!t) return
    if (!TOKEN.test(t) || (kind === 'subs' && t === 'original') || items.includes(t)) {
      setBad(true)
      return
    }
    setBad(false)
    setEntry('')
    // Immediately before the pinned backstop, wherever it sits. `resolveTracks`
    // takes the FIRST language the file has, and `original` matches almost
    // every file — so appending after it persisted "original,nl" and the
    // language just added never applied, under a "saved" flash and a tooltip
    // calling original the final fallback. Inserted rather than appended-after-
    // filtering, because `original` is reorderable on purpose: moving it to the
    // end would rewrite an order the viewer had chosen.
    commit(addAbove(items, t, 'original'))
  }

  const listId = `langs-${kind}-${mediaType}`
  const drag = useDragOrder((from, to) => {
    const next = moved(items, from, to)
    if (next) commit(next)
  })

  return (
    <div className="pref-row">
      <span className="pref-label mono">{kind === 'subs' ? 'subtitles' : kind}</span>
      <span className="chips">
        {items.length === 0 && <span className="dimmer">no subtitles</span>}
        {items.map((l, i) => {
          const pinned = kind === 'audio' && l === 'original'
          return (
            <span
              key={l}
              className={`lang-pill${drag.look(i)}`}
              title="Drag to reorder, or click to make it the first choice"
              {...drag.row(i)}
            >
              <span className="lang-grip" aria-hidden="true">
                <Icon name="grip" size={10} />
              </span>
              {/* Click still promotes: a drag is a pointer gesture, and the
                  keyboard has to be able to reach the same outcome. */}
              <button
                className="lang-name mono"
                title={i === 0 ? 'first choice' : 'make it the first choice'}
                onClick={() => i > 0 && commit([l, ...items.filter((x) => x !== l)])}
              >
                {l}
              </button>
              {pinned ? (
                <span className="lang-end lang-lock" title="always the final fallback">
                  <Icon name="lock" size={10} />
                </span>
              ) : (
                <button
                  className="lang-end chip-x"
                  title={`remove ${l}`}
                  /* Says what it did. It is one click, it sits next to a drag
                     handle, and a silently shorter list is not something you
                     notice until a film starts in the wrong language. */
                  onClick={() => {
                    commit(items.filter((x) => x !== l))
                    notify(
                      `Removed ${l} from ${kind === 'subs' ? 'subtitles' : kind} for ${mediaType}.`,
                    )
                  }}
                >
                  ×
                </button>
              )}
            </span>
          )
        })}
      </span>
      <input
        ref={inputRef}
        className={`lang-add${bad ? ' invalid' : ''}`}
        placeholder="add…"
        list={listId}
        value={entry}
        onChange={(e) => {
          setEntry(e.target.value)
          setBad(false)
        }}
        onKeyDown={(e) => e.key === 'Enter' && add()}
        onBlur={() => entry.trim() && add()}
      />
      <datalist id={listId}>
        {SUGGEST.filter((s) => !items.includes(s) && !(kind === 'subs' && s === 'original')).map(
          (s) => (
            <option key={s} value={s} />
          ),
        )}
      </datalist>
    </div>
  )
}

function OpenSubtitlesAccount({
  username,
  hasPassword,
  onSaved,
  flash,
}: {
  username: string
  hasPassword: boolean
  onSaved: (username: string, password: string) => void
  flash: () => void
}) {
  const [user, setUser] = useState(username)
  const [pass, setPass] = useState('')
  const [busy, setBusy] = useState(false)
  useEffect(() => setUser(username), [username])

  const save = (u: string, p: string) => {
    setBusy(true)
    // `allSettled`, because these are two independent writes and reporting a
    // flat failure for a half-save left the hub holding the new username while
    // the card still showed the old one — and the badge still read "shared
    // budget" for an account that was half attached.
    Promise.allSettled([
      putPref('', 'opensubtitles.username', u),
      putPref('', 'opensubtitles.password', p),
    ])
      .then(([un, pw]) => {
        const failed = [
          un.status === 'rejected' ? 'username' : null,
          pw.status === 'rejected' ? 'password' : null,
        ].filter(Boolean)
        setPass('')
        // Whatever landed is what the hub has, so the card must show that.
        onSaved(un.status === 'fulfilled' ? u : username, pw.status === 'fulfilled' ? p : '')
        if (failed.length === 0) flash()
        else notify(`Could not save the ${failed.join(' or ')}.`)
      })
      .finally(() => setBusy(false))
  }

  return (
    <div className="row-form pref-row">
      <span className="pref-label mono">account</span>
      <input
        placeholder="opensubtitles.com username"
        value={user}
        onChange={(e) => setUser(e.target.value)}
      />
      <input
        type="password"
        placeholder={hasPassword ? 'password saved — enter to replace' : 'password'}
        value={pass}
        onChange={(e) => setPass(e.target.value)}
      />
      <button
        className="btn small"
        disabled={busy || !user.trim() || !pass.trim()}
        onClick={() => save(user.trim(), pass)}
      >
        Save
      </button>
      {(username || hasPassword) && (
        <button
          className="btn ghost small"
          disabled={busy}
          onClick={() => {
            setUser('')
            save('', '')
          }}
        >
          Disconnect
        </button>
      )}
    </div>
  )
}

/// The HUB-32a/d fallback ladder: reorder with the arrows. Every rung
/// is always present — the order expresses priority, never removal, so
/// there is always something a client can be served (owner decision,
/// 2026-08-03).
///
/// Named for what the viewer gets, not for the mechanism: the setting is
/// worth having only if you can tell which one you would rather watch.
const ASS_RUNGS = {
  flatten: {
    name: 'plain text',
    note: 'fonts, colours and positions dropped. Works anywhere, costs nothing.',
  },
  overlay: {
    name: 'drawn on top',
    note: 'the server draws the styling and sends it as its own layer. Looks right, and the picture is untouched.',
  },
  burn: {
    name: 'burnt into the picture',
    note: 'exactly as the author made it, but the video has to be re-encoded to carry it.',
  },
} as const
type AssRung = keyof typeof ASS_RUNGS

function AssLadder({
  value,
  onChange,
  flash,
}: {
  value: string
  onChange: (v: string) => void
  flash: () => void
}) {
  const all = Object.keys(ASS_RUNGS) as AssRung[]
  // A permutation, always: whatever the stored value leaves out is
  // appended, mirroring the server's own parse.
  const parsed = value
    .split(',')
    .map((s) => s.trim())
    .filter((s): s is AssRung => (all as string[]).includes(s))
  const order = [...new Set([...parsed, ...all])]
  const write = useOptimistic(value, onChange, flash)
  const save = (next: AssRung[]) => {
    const v = next.join(',')
    write(v, () => putPref('', 'ass_order', v))
  }
  const move = (from: number, to: number) => {
    const next = moved(order, from, to)
    if (next) save(next)
  }
  const drag = useDragOrder(move)
  return (
    <div className="pref-block">
      <div className="pref-block-head">Subtitles with their own styling</div>
      <p className="dim card-prose">
        Some subtitles carry fonts, colours and on-screen positions — signs, songs, anything
        typeset. A player that can draw them itself always does, and this browser can, so this order
        only decides what happens on players that cannot. The first one that player and this server
        can both manage is what gets used.
      </p>
      <ol className="ladder">
        {order.map((r, i) => (
          <li
            key={r}
            className={drag.look(i).trim()}
            title="Drag to reorder"
            /* No arrow buttons: the grip says what to do and two more
               controls per row said it again, louder. The row itself takes
               the arrow keys, so reordering stays reachable without a
               pointer. */
            tabIndex={0}
            onKeyDown={(e) => {
              if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
              e.preventDefault()
              move(i, e.key === 'ArrowUp' ? i - 1 : i + 1)
            }}
            {...drag.row(i)}
          >
            <span className="ladder-grip" aria-hidden="true">
              <Icon name="grip" size={11} />
            </span>
            <span className="ladder-name">{ASS_RUNGS[r].name}</span>
            <span className="dim ladder-note">{ASS_RUNGS[r].note}</span>
          </li>
        ))}
      </ol>
    </div>
  )
}

export default function Settings() {
  const [values, setValues] = useState<Record<string, string>>({})
  /// The bandwidth box's own copy, so a refusal can put the old value back.
  const [bandwidth, setBandwidth] = useState('')
  const [loaded, setLoaded] = useState(false)
  /// A failed load used to render the page anyway, with every control showing
  /// its default — which reads as "these are your settings" when the truth is
  /// "we have no idea what your settings are". Worse than a blank screen,
  /// because the next thing you do is change one.
  const [loadError, setLoadError] = useState('')
  const [attempt, setAttempt] = useState(0)
  const [saved, setSaved] = useState(false)
  const savedTimer = useRef<ReturnType<typeof setTimeout>>(undefined)

  const flash = () => {
    setSaved(true)
    clearTimeout(savedTimer.current)
    savedTimer.current = setTimeout(() => setSaved(false), 1200)
  }

  useEffect(() => {
    // Fenced for the same reason as Libraries: the error and its button now
    // stay up for the whole request, so a second Try again is possible, and
    // an older load rejecting last would put the Failed screen back over
    // settings that had arrived.
    let live = true
    fetchPrefs()
      .then((r) => {
        if (!live) return
        const v: Record<string, string> = {}
        for (const p of r.prefs) {
          if (p.scope === '') v[p.key] = p.value
        }
        setValues(v)
        setBandwidth(v['bandwidth_kbps'] ?? '')
        setLoadError('')
        setLoaded(true)
      })
      .catch((e) => {
        if (!live) return
        setLoadError(String(e))
        setLoaded(true)
      })
    return () => {
      live = false
    }
  }, [attempt])

  if (!loaded) return null
  if (loadError)
    return (
      <Failed
        what="Could not load your settings."
        message={loadError}
        // Neither the error nor `loaded` is dropped here: `if (!loaded) return
        // null` sits one line above, so clearing them blanked the page for the
        // whole request and then brought the same error back. They go when the
        // load answers.
        onRetry={() => setAttempt((n) => n + 1)}
      />
    )
  return (
    <main>
      <div className="settings-head">
        <h1>Settings</h1>
        {/* Always in the layout, only sometimes visible: a chip that
            appears would shift the heading every time you changed
            something. */}
        <span className={`chip saved-flash${saved ? ' on' : ''}`}>saved</span>
      </div>
      <p className="dim settings-intro">Everything here saves the moment you change it.</p>
      <div className="settings-cards">
        <section className="card-plain">
          <div className="card-head">
            <span className="card-glyph">
              <Icon name="download" size={15} />
            </span>
            <span className="card-name">OpenSubtitles</span>
            <span
              className={`chip mono ${values['opensubtitles.username'] ? '' : 'dim'} card-badge`}
            >
              {values['opensubtitles.username'] ? 'your budget' : 'shared budget'}
            </span>
          </div>
          <p className="dim card-prose">
            Subtitle search works without an account, on a small download budget shared by everyone
            on this server. Attach your own opensubtitles.com account to spend your own budget
            instead. Subtitles you download are shared with everyone here.
          </p>
          <OpenSubtitlesAccount
            username={values['opensubtitles.username'] ?? ''}
            hasPassword={!!values['opensubtitles.password']}
            onSaved={(u, p) =>
              setValues((cur) => ({
                ...cur,
                'opensubtitles.username': u,
                'opensubtitles.password': p,
              }))
            }
            flash={flash}
          />
        </section>
        <section className="card-plain">
          <div className="card-head">
            <span className="card-glyph">
              <Icon name="play" size={15} />
            </span>
            <span className="card-name">Playback</span>
          </div>
          <p className="dim card-prose">Applies wherever you play, on this account.</p>
          <div className="pref-row">
            <span className="pref-label mono">bandwidth</span>
            {/* Controlled, so a refusal can put it back. Uncontrolled, this was
                the one write on a page headed "everything here saves the moment
                you change it" that could keep showing a cap the server does not
                have — and the viewer's playback stays uncapped underneath it.
                Stored as the server stores it: `0` means no cap, and the pref
                is cleared rather than set to "0", so the local copy must be
                empty too or it disagrees with the hub about the same key. */}
            <input
              className="pref-input mono"
              type="number"
              min={0}
              placeholder="kbit/s cap (0 = none)"
              value={bandwidth}
              onChange={(e) => setBandwidth(e.currentTarget.value)}
              onBlur={(e) => {
                const raw = e.currentTarget.value.trim()
                const v = raw === '0' ? '' : raw
                if (v === (values['bandwidth_kbps'] ?? '')) return
                putPref('', 'bandwidth_kbps', v)
                  .then(() => {
                    setValues((cur) => ({ ...cur, bandwidth_kbps: v }))
                    setBandwidth(v)
                    flash()
                  })
                  .catch(() => {
                    setBandwidth(values['bandwidth_kbps'] ?? '')
                    notify('Could not save that — the server did not take it.')
                  })
              }}
            />
          </div>
          <p className="dim pref-note">
            A ceiling for how much data playback may use — worth setting on a metered or slow
            connection. Anything above it is re-encoded smaller; a file that cannot be re-encoded
            will refuse to play rather than stall. Leave it empty for no limit.
          </p>
          {/* HUB-32a/d: how styled (ASS) subtitles reach a client that
            cannot render them itself, as an ORDERED ladder. The server
            tries each rung in turn and takes the first one this client
            and this fleet can actually serve. `native` is not listed:
            it is not a fallback, and it always wins when the client
            declares it. */}
          <AssLadder
            value={values['ass_order'] ?? ''}
            onChange={(v) => setValues((cur) => ({ ...cur, ass_order: v }))}
            flash={flash}
          />
        </section>
        <div className="settings-group">
          <h2>Which tracks to start with</h2>
          <p className="dim card-prose">
            When you open something, the first language in each list that the file actually has is
            the one that plays. Drag a language to move it, or click it to make it your first
            choice. <span className="mono teal">original</span> means whatever language the title
            was made in. Picking a different track while watching only affects that title, and it
            wins over these.
          </p>
        </div>
        {MEDIA_TYPES.map((mt) => (
          <section className="card-plain" key={mt}>
            <div className="card-head">
              <span className="card-glyph">
                <Icon name={mt === 'movies' ? 'movie' : 'show'} size={15} />
              </span>
              <span className="card-name">{mt}</span>
            </div>
            {/* Anime's presentation first: it decides how the episode lists
              on the other screens are numbered, which is a bigger
              difference than a track order. */}
            {mt === 'anime' && (
              <div className="pref-row">
                <span className="pref-label mono">view</span>
                <span className="chips">
                  {(['seasons', 'native'] as const).map((v) => (
                    <button
                      key={v}
                      className={`chip toggle${(values['anime_view'] ?? 'seasons') === v ? ' on' : ''}`}
                      title={
                        v === 'seasons'
                          ? 'TVDB-style seasons (projected)'
                          : 'flat absolute numbering (AniDB-native)'
                      }
                      onClick={() =>
                        putPref('', 'anime_view', v)
                          .then(() => {
                            setValues((cur) => ({ ...cur, anime_view: v }))
                            flash()
                          })
                          .catch(() => notify('Could not save that — the server did not take it.'))
                      }
                    >
                      {v}
                    </button>
                  ))}
                </span>
              </div>
            )}
            {mt === 'anime' && (
              <p className="dim pref-note">
                {(values['anime_view'] ?? 'seasons') === 'seasons'
                  ? 'Numbered in seasons, the way most people know these shows.'
                  : 'Numbered straight through, the way they were broadcast.'}
              </p>
            )}
            {(['audio', 'subs'] as const).map((kind) => (
              <LangChips
                key={kind}
                mediaType={mt}
                kind={kind}
                value={values[`${kind}.${mt}`] ?? ''}
                onChange={(v) => setValues((cur) => ({ ...cur, [`${kind}.${mt}`]: v }))}
                flash={flash}
              />
            ))}
          </section>
        ))}
      </div>
    </main>
  )
}
