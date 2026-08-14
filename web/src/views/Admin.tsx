import { useEffect, useRef, useState } from 'react'
import { notify } from '../toast'
import { moved, useDragOrder } from '../reorder'
import { SerialQueue } from '../serial'
import Icon from '../icons'
import {
  adminApprove,
  adminEnrichRun,
  adminEnrichStatus,
  adminProviders,
  adminSetChain,
  type ProviderChain,
  adminRefreshLibrary,
  openEvents,
  adminSetAnidb,
  adminSetTmdbKey,
  adminSetTvdbKey,
  adminAttachCollection,
  adminCollections,
  adminCreateLibrary,
  adminDeleteLibrary,
  adminDetachCollection,
  adminSetSatelliteDisabled,
  adminDeleteSatellite,
  IN_PROCESS,
  adminEndSession,
  adminEnrollments,
  adminLibraries,
  adminSatellites,
  adminUsers,
  adminCreateUser,
  adminDeleteUser,
  adminSetUserLibraries,
  adminSetUserAdmin,
  refreshTokens,
  username,
  downloadWithAuth,
  adminSessions,
  type AdminSession,
  type AdminUser,
  type CollectionInfo,
  type Library,
  type PendingEnrollment,
  type Satellite,
} from '../api'
import { deliveryPlan } from '../delivery'

// HUB-11: the events channel pushes invalidation hints; polling remains
// only as a slow fallback for anything a hint doesn't cover.
const POLL_MS = 15000

/// One flat scroll put five unrelated jobs in one column, and the two that
/// change on their own — a satellite waiting to be admitted, someone
/// streaming — were as likely to be off-screen as not. A nav can carry a
/// count, so those two announce themselves from wherever you are.
///
/// The intros are the prototype's, and say things that are not guessable
/// from the controls: that deleting a satellite revokes it at the TLS
/// layer, and that a grant checkbox writes the whole set rather than a
/// change to it.
const SECTIONS = [
  {
    id: 'satellites',
    label: 'Satellites',
    intro:
      'Enrolled mediahosts and transcoders. This list is the mTLS allowlist — deleting a satellite revokes its certificate, so it is refused at the TLS layer and cannot come back on its own.',
  },
  {
    id: 'libraries',
    label: 'Libraries',
    intro:
      'Compose libraries from the collections mediahosts announce. Same-type collections from different hosts merge into one browsable library; duplicate items become extra sources.',
  },
  {
    id: 'providers',
    label: 'Providers',
    intro: 'Credentials for the services that identify and describe your media.',
  },
  {
    id: 'users',
    label: 'Users & grants',
    intro:
      'Each account gets every library until you narrow it. A checkbox writes the account\u2019s whole access, not a change to it, so two admins editing at once cannot merge into a set neither picked.',
  },
  {
    id: 'sessions',
    label: 'Sessions',
    intro: 'Who is playing what, how it is being delivered, and where.',
  },
] as const
type SectionId = (typeof SECTIONS)[number]['id']

/// HUB-10/26: accounts and what each may see.
///
/// A checkbox writes the account's WHOLE access, not a delta — that is
/// what the endpoint takes, and it is why two admins with the panel open
/// cannot interleave into a set neither picked.
function UsersSection({
  libraries,
  onNotice,
  onError,
  onReadError,
  tick,
}: {
  libraries: Library[]
  onNotice: (s: string) => void
  onError: (s: string) => void
  /// The panel's own reading failed, as opposed to something the operator did.
  onReadError: (s: string) => void
  tick: number
}) {
  const [users, setUsers] = useState<AdminUser[]>([])
  const [name, setName] = useState('')
  const [pass, setPass] = useState('')
  const [admin, setAdmin] = useState(false)
  const [confirming, setConfirming] = useState<string | null>(null)
  /// One entry per user whose grants have been written: which write is newest
  /// (`seq`), how many are out (`inflight`), which one the revert target came
  /// from (`savedSeq`) and what it is (`saved`). Kept in a ref because none of
  /// it is rendered — it exists so that answers landing out of order cannot
  /// leave the chips disagreeing with the hub.
  const writes = useRef(
    new Map<
      string,
      {
        seq: number
        inflight: number
        savedSeq: number
        saved: { all_libraries: boolean; libraries: string[] } | null
        queue: SerialQueue
      }
    >(),
  )

  /// Monotonic for the same reason the panel's own `reload` is: this fires
  /// from mount, from every hint burst and from four mutation callbacks with
  /// nothing serialising them, and an older snapshot landing last repaints the
  /// chips a click computes its whole-set payload from.
  const readSeq = useRef(0)
  const refresh = () => {
    const mine = ++readSeq.current
    return adminUsers()
      .then((r) => {
        if (mine !== readSeq.current) return
        // A read that started before a grant write committed answers after it,
        // and repainting the hub's pre-write set over the optimistic chips is
        // not merely stale: the next click computes its whole-set payload from
        // what is on screen and revokes for real what the operator had just
        // granted. `setRole`, create, delete and every non-scan hint call this,
        // and a hint is emitted whenever anyone starts or stops playing — so it
        // lands under the operator's hands routinely. The write's own answer is
        // authoritative for the fields it owns until it settles.
        setUsers((prev) =>
          r.users.map((u) => {
            const w = writes.current.get(u.id)
            if (!w || w.inflight === 0) return u
            const shown = prev.find((x) => x.id === u.id)
            return shown
              ? { ...u, all_libraries: shown.all_libraries, libraries: shown.libraries }
              : u
          }),
        )
        onReadError('')
      })
      .catch((e) => mine === readSeq.current && onReadError(String(e)))
  }
  useEffect(() => {
    void refresh()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick])

  /// Shows the new set first, then saves it, then takes the server's answer.
  ///
  /// This is a whole-SET write, so the chips a click computes from have to be
  /// up to date or the click undoes the last one. They were not: the state only
  /// moved when the round trip finished, so granting two libraries in quick
  /// succession sent the same pre-click set twice and the second write lost the
  /// first — both reporting success, and the refresh afterwards showing the
  /// loss as though the click had missed. Measured before this: two grants in,
  /// one granted.
  ///
  /// Optimistic, reconciled against the response, reverted on failure — the
  /// same shape as `useOptimistic` in Settings, which exists for this exact
  /// bug in the drag-ordered lists.
  const setAccess = (u: AdminUser, all: boolean, libs: string[]) => {
    // Per user, because two users' grants are two independent writes and a
    // shared counter would have them cancel each other.
    const w = writes.current.get(u.id) ?? {
      seq: 0,
      inflight: 0,
      savedSeq: 0,
      saved: null,
      queue: new SerialQueue(),
    }
    writes.current.set(u.id, w)
    // Nothing outstanding means the chips on screen are what the hub has.
    if (w.inflight === 0) w.saved = { all_libraries: u.all_libraries, libraries: u.libraries }
    const mine = ++w.seq
    w.inflight++
    const apply = (v: { all_libraries: boolean; libraries: string[] }) =>
      setUsers((us) => us.map((x) => (x.id === u.id ? { ...x, ...v } : x)))
    apply({ all_libraries: all, libraries: libs })
    // The optimistic chips move at once; only the commits wait. Filtering stale
    // replies did not order SQLite writes, so request A could commit after B
    // and leave the hub holding A while the panel continued to show B.
    return w.queue
      .run(() => adminSetUserLibraries(u.id, all, libs))
      .then((r) => {
        // The revert target moves on ANY success, newest-first: an older write
        // succeeding while a newer one is out still tells us something the hub
        // has accepted, and reverting past it would undo it.
        if (mine > w.savedSeq) {
          w.savedSeq = mine
          w.saved = { all_libraries: r.all_libraries, libraries: r.libraries }
        }
        // The CHIPS, though, only ever come from the newest write. This is a
        // whole-set write and the next click computes its payload from what is
        // on screen, so painting an older answer does not merely look stale:
        // grant A, grant B, A answers first, and the chips drop back to just A
        // while B is still out — a click there sends [A, C] and revokes B on
        // the hub for real. `savedSeq` counts answers that have LANDED, so it
        // is satisfied by a stale write whenever the newest has not answered;
        // the sequence is the thing to compare against.
        if (mine !== w.seq) return
        onError('')
        if (w.saved) apply(w.saved)
      })
      .catch((e) => {
        // An older write failing says nothing about where the newest one is
        // going, and reverting to what was on screen two clicks ago would undo
        // a grant the operator has since made. The newest write reports its own
        // outcome. Same rule as `useOptimistic` in Settings.
        if (mine !== w.seq) return
        if (w.saved) apply(w.saved)
        onError(String(e))
      })
      .finally(() => {
        w.inflight--
      })
  }

  /// The hub owns both refusals — your own rights, and the last admin. The
  /// client disables the one it can see coming and reports the other.
  const setRole = (u: AdminUser, admin: boolean) =>
    adminSetUserAdmin(u.id, admin)
      // No confirmation: the row itself answers this. The admin chip lights,
      // and `all libraries` lights and locks with the tooltip explaining why —
      // which is the design this panel already chose over a sentence.
      //
      // The error is cleared by hand, because a success used to do that
      // through the notice it no longer sends.
      .then(async () => {
        onError('')
        if (u.username === username() && !admin) {
          // The role write invalidated the token that authorized it. Rotate to
          // a current non-admin access token before leaving this screen; a bare
          // reload would make bootstrap see only the invalid old one and show
          // sign-in despite the refresh family still being live.
          if (!(await refreshTokens()))
            throw new Error(
              'Your role changed, but the session could not be refreshed. Sign in again.',
            )
          window.location.assign('/app/')
          return
        }
        return refresh()
      })
      .catch((e) => onError(String(e)))

  const me = username()
  return (
    <>
      {/* First of the panel's headings, so `.admin-panel h2:first-of-type`
          pulls its margin off and it sits under the intro rather than adrift
          from it — which is also what gives the one below the form its gap. */}
      <h2>New account</h2>
      <form
        className="row-form user-new"
        onSubmit={(e) => {
          e.preventDefault()
          adminCreateUser(name.trim(), pass, admin)
            .then(() => {
              onNotice(`Created ${name.trim()} — it can see every library until you say otherwise.`)
              setName('')
              setPass('')
              setAdmin(false)
              // Like `clearThenReload` one section over: a refusal left on
              // screen reads as the outcome of the create that just worked.
              onError('')
              return refresh()
            })
            .catch((e) => onError(String(e)))
        }}
      >
        <input placeholder="new username" value={name} onChange={(e) => setName(e.target.value)} />
        <input
          type="password"
          // Says so before you press Create, not after.
          className={pass && Array.from(pass).length < 12 ? 'invalid' : ''}
          placeholder="At least 12 characters"
          value={pass}
          onChange={(e) => setPass(e.target.value)}
          minLength={12}
        />
        <button
          type="button"
          className={`chip toggle${admin ? ' on' : ''}`}
          title="Create as an administrator"
          onClick={() => setAdmin(!admin)}
        >
          admin
        </button>
        <button className="btn small" disabled={!name.trim() || Array.from(pass).length < 12}>
          Create
        </button>
      </form>
      {/* The same shape as Satellites: a heading over the thing you fill in,
          another over the list of what exists. Without the second one the new
          account's fields ran straight into the first existing account, and the
          panel read as one long list with an editable row at the top. */}
      <h2>Accounts</h2>
      <ul className="rows">
        {users.map((u) => (
          // Who it is and what kind of account, on one line. The libraries
          // it may see go underneath, like a satellite's measurements —
          // seven of them inline pushed the date and Delete out of line with
          // every other row, which is the same problem in the same shape.
          <li key={u.id} className="user-row">
            <div className="user-head">
              <span className="user-name">{u.username}</span>
              <button
                className={`chip toggle${u.is_admin ? ' on' : ''}`}
                title={
                  u.is_admin
                    ? u.username === me
                      ? 'Demote this account and return to the home screen'
                      : 'Demote to an ordinary account, bound by its grants'
                    : 'Make an administrator: every library, and this panel'
                }
                onClick={() => void setRole(u, !u.is_admin)}
              >
                admin
              </button>
              {/* For an admin the same control, held on: an admin does have
                  every library, and saying so with everyone else's toggle
                  beats a sentence explaining why there is no toggle here. */}
              <button
                className={`chip toggle${u.is_admin || u.all_libraries ? ' on' : ''}`}
                disabled={u.is_admin}
                title={
                  u.is_admin
                    ? 'An admin configures the grants, so it is not bound by them'
                    : 'Every library, including ones added later'
                }
                onClick={() => void setAccess(u, !u.all_libraries, u.libraries)}
              >
                all libraries
              </button>
              {!u.is_admin && !u.all_libraries && u.libraries.length === 0 && (
                <span
                  className="chip warn"
                  title="This account can sign in, but its home screen is empty"
                >
                  no access
                </span>
              )}
              <span className="user-tail">
                <span className="dim mono user-note">
                  since{' '}
                  {new Date(u.created_at * 1000).toLocaleDateString(undefined, {
                    year: 'numeric',
                    month: 'short',
                    day: 'numeric',
                  })}
                </span>
                <button
                  className="btn ghost small"
                  // The API refuses this too; saying so before the click is
                  // kinder than an error afterwards.
                  disabled={u.username === me}
                  title={
                    u.username === me
                      ? 'You are signed in as this account'
                      : 'Removes the account, its watch state and its sessions'
                  }
                  onClick={() => {
                    if (confirming !== u.id) {
                      setConfirming(u.id)
                      return
                    }
                    setConfirming(null)
                    adminDeleteUser(u.id)
                      .then(() => {
                        onError('')
                        return refresh()
                      })
                      .catch((e) => onError(String(e)))
                  }}
                  // Disarmed on blur, as the satellite delete already was.
                  // Without it "Really delete?" stayed armed indefinitely,
                  // through every fifteen-second refresh.
                  onBlur={() => setConfirming(null)}
                >
                  {confirming === u.id ? 'Really delete?' : 'Delete'}
                </button>
              </span>
            </div>
            {/* Only when the account is actually being narrowed. Toggles
                rather than checkboxes: at this size a native box is the
                smallest target on the screen and its label was not part
                of it. */}
            {!u.is_admin && !u.all_libraries && (
              <div className="user-libs">
                <span className="pref-label mono">granted</span>
                <span className="chips">
                  {libraries.map((l) => {
                    const on = u.libraries.includes(l.id)
                    return (
                      <button
                        className={`chip toggle${on ? ' on' : ''}`}
                        key={l.id}
                        onClick={() =>
                          void setAccess(
                            u,
                            false,
                            on ? u.libraries.filter((x) => x !== l.id) : [...u.libraries, l.id],
                          )
                        }
                      >
                        {l.name}
                      </button>
                    )
                  })}
                </span>
              </div>
            )}
          </li>
        ))}
      </ul>
    </>
  )
}

function TmdbSection({
  onNotice,
  onError,
  onReadError,
  tick,
}: {
  onNotice: (s: string) => void
  onError: (s: string) => void
  /// The panel's own reading failed, as opposed to something the operator did.
  onReadError: (s: string) => void
  tick: number
}) {
  const [configured, setConfigured] = useState(false)
  const [tvdbConfigured, setTvdbConfigured] = useState(false)
  const [anidbConfigured, setAnidbConfigured] = useState(false)
  /// Whether ANY provider can answer. The enrich button used to read TMDB's
  /// flag alone.
  const anyProvider = configured || tvdbConfigured || anidbConfigured
  const [anidbUser, setAnidbUser] = useState('')
  const [anidbPass, setAnidbPass] = useState('')
  const [anidbKey, setAnidbKey] = useState('')
  const [key, setKey] = useState('')
  const [tvdbKey, setTvdbKey] = useState('')
  const [tvdbPin, setTvdbPin] = useState('')
  const [status, setStatus] = useState<{
    running: boolean
    matched: number
    weak: number
    missed: number
  } | null>(null)
  const [chains, setChains] = useState<Record<string, ProviderChain>>({})

  /// This panel polls every 15 s and both calls swallowed their failures, so
  /// a hub that stopped answering left the credentials and the match order
  /// on screen looking current. Said once, on the way into failure, and once
  /// again on the way out — a notice every 15 s would be worse than silence.
  const failing = useRef(false)
  const refresh = () => {
    const ok = () => {
      onReadError('')
      if (failing.current) {
        failing.current = false
        notify('Provider settings are up to date again.')
      }
    }
    const bad = () => {
      if (failing.current) return
      failing.current = true
      notify('Cannot reach the hub — what is shown here may be out of date.')
    }
    Promise.all([
      adminProviders().then((p) => {
        setConfigured(p.tmdb.configured)
        setTvdbConfigured(p.tvdb.configured)
        setAnidbConfigured(p.anidb?.configured ?? false)
        setChains(p.chains ?? {})
      }),
      adminEnrichStatus().then(setStatus),
    ])
      .then(ok)
      .catch(bad)
  }
  useEffect(() => {
    refresh()
    const t = setInterval(refresh, POLL_MS)
    return () => clearInterval(t)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick])

  return (
    // Grouped by what they are for, not by who runs them: an admin comes
    // here because anime is matching badly, not because they were thinking
    // about AniDB. The provider name is the row label.
    <div className="settings-cards admin-cards">
      <section className="card-plain">
        <div className="card-head">
          <span className="card-glyph">
            <Icon name="movie" size={15} />
          </span>
          <span className="card-name">Movies &amp; series</span>
        </div>
        <div className="pref-row">
          <span className="pref-label mono">TMDB</span>
          <input
            type="password"
            placeholder={configured ? 'key configured — paste to replace' : 'API key'}
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <button
            className="btn small"
            disabled={!key.trim()}
            onClick={() => {
              void adminSetTmdbKey(key.trim())
                .then(() => {
                  setKey('')
                  onNotice('TMDB key saved — enrichment started')
                  refresh()
                })
                .catch((e: unknown) => onError(String(e)))
            }}
          >
            Save
          </button>
        </div>
        <div className="pref-row">
          <span className="pref-label mono">TheTVDB</span>
          <input
            type="password"
            placeholder={tvdbConfigured ? 'key configured — paste to replace' : 'API key'}
            value={tvdbKey}
            onChange={(e) => setTvdbKey(e.target.value)}
          />
          <input
            className="pref-narrow"
            type="password"
            placeholder="PIN, if your key needs one"
            value={tvdbPin}
            onChange={(e) => setTvdbPin(e.target.value)}
          />
          <button
            className="btn small"
            disabled={!tvdbKey.trim()}
            onClick={() => {
              void adminSetTvdbKey(tvdbKey.trim(), tvdbPin.trim() || undefined)
                .then(() => {
                  setTvdbKey('')
                  setTvdbPin('')
                  onNotice('TVDB key saved — enrichment started')
                  refresh()
                })
                .catch((e: unknown) => onError(String(e)))
            }}
          >
            Save
          </button>
        </div>
      </section>

      <section className="card-plain">
        <div className="card-head">
          <span className="card-glyph">
            <Icon name="show" size={15} />
          </span>
          <span className="card-name">Anime</span>
          <span className={`chip mono card-badge${anidbConfigured ? '' : ' dim'}`}>
            {anidbConfigured ? 'account attached' : 'title search only'}
          </span>
        </div>
        <p className="dim card-prose">
          An AniDB account enables exact file matching — the precise episode, release group and
          version. Without one, matching falls back to searching by title.
        </p>
        <div className="pref-row">
          <span className="pref-label mono">AniDB</span>
          <input
            placeholder={anidbConfigured ? 'account configured — enter to replace' : 'username'}
            value={anidbUser}
            onChange={(e) => setAnidbUser(e.target.value)}
          />
          <input
            type="password"
            placeholder="password"
            value={anidbPass}
            onChange={(e) => setAnidbPass(e.target.value)}
          />
          <button
            className="btn small"
            id="save-anidb"
            disabled={!anidbUser.trim() || !anidbPass.trim()}
            onClick={() => {
              void adminSetAnidb(anidbUser.trim(), anidbPass.trim(), anidbKey.trim() || undefined)
                .then((r) => {
                  setAnidbUser('')
                  setAnidbPass('')
                  setAnidbKey('')
                  onNotice(
                    r.verified
                      ? 'AniDB account verified — enrichment started'
                      : `AniDB saved but login failed: ${r.error ?? 'unknown'}`,
                  )
                  refresh()
                })
                .catch((e: unknown) => onError(String(e)))
            }}
          >
            Save
          </button>
        </div>
        <div className="pref-row">
          <span className="pref-label mono" />
          <input
            type="password"
            placeholder="UDP API key — optional, encrypts the session"
            value={anidbKey}
            onChange={(e) => setAnidbKey(e.target.value)}
          />
        </div>
        <p className="dim pref-note">AniList and the AniDB↔TVDB mapping need no key.</p>
      </section>

      <section className="card-plain">
        <div className="card-head">
          <span className="card-glyph">
            <Icon name="grip" size={15} />
          </span>
          <span className="card-name">Matching order</span>
        </div>
        <ProviderOrder chains={chains} onNotice={onNotice} onError={onError} onDone={refresh} />
      </section>

      {/* Library-wide, so it sits under the cards rather than in one. */}
      <div className="enrich-row">
        <button
          className="btn ghost small"
          // Any provider, not TMDB alone: the hub enriches from TheTVDB or AniDB
          // perfectly well — saving a TVDB key even spawns a run — so a
          // series-only deployment had a permanently greyed button and no
          // explanation of why.
          disabled={!anyProvider || status?.running}
          title={anyProvider ? undefined : 'Configure a metadata provider first'}
          onClick={() =>
            void adminEnrichRun()
              .then(refresh)
              .catch((e: unknown) => onError(String(e)))
          }
        >
          {status?.running ? 'Enriching…' : 'Enrich now'}
        </button>
        {status && (
          <span className={`dim mono${status.running ? ' enriching' : ''}`}>
            {status.matched} matched · {status.weak} weak · {status.missed} missed
          </span>
        )}
      </div>
    </div>
  )
}

/// A realtime multiple, or nothing at all. Never "0×": zero on the wire
/// means unmeasured, and printing it as a speed would read as a box that
/// cannot encode (HUB-36).
function mult(v?: number | null): string | null {
  return typeof v === 'number' && v > 0 ? `${v.toFixed(1)}×` : null
}

/// `6.2× / 2.1×` — 1080p and 2160p, dropping whichever was not measured.
function pair(a?: number | null, b?: number | null): string | null {
  const parts = [mult(a), mult(b)].filter((x): x is string => x !== null)
  return parts.length ? parts.join(' / ') : null
}

/// What a transcoder was MEASURED doing, under what it claims it can do.
/// Benchmarks are per element; `pace` is per class of real work and
/// overrides them in placement, so it is shown apart and last.
function MeasuredFacts({ s }: { s: Satellite }) {
  const caps = s.capabilities
  const encoders = caps?.encoders ?? []
  const tm = pair(caps?.tonemap_speed_1080, caps?.tonemap_speed_2160)
  const link =
    typeof s.link_bytes_per_sec === 'number' && s.link_bytes_per_sec > 0
      ? `${(s.link_bytes_per_sec / 1_000_000).toFixed(1)} MB/s`
      : null
  const pace = s.pace ?? []
  if (!encoders.length && !tm && !link && !pace.length) return null
  return (
    <div className="sat-facts">
      <span className="pref-label mono">measured</span>
      <span className="chips dim">
        {encoders.map((e) => {
          const sp = pair(e.speed_1080, e.speed_2160)
          return (
            <span className="chip" key={e.element} title={e.element}>
              {e.codec}
              {e.hardware ? ' hw' : ''}
              {sp ? ` ${sp}` : ' —'}
            </span>
          )
        })}
        {tm && <span className="chip">tone-map {tm}</span>}
        {link && <span className="chip">link {link}</span>}
        {pace.map((p) => (
          <span
            className={p.multiple < 1 ? 'chip warn' : 'chip'}
            key={p.class}
            title="measured on real sessions; overrides the benchmark"
          >
            {p.class} {p.multiple.toFixed(1)}×
          </span>
        ))}
      </span>
    </div>
  )
}

export default function Admin() {
  const [pending, setPending] = useState<PendingEnrollment[]>([])
  const [satellites, setSatellites] = useState<Satellite[]>([])
  const [sessions, setSessions] = useState<AdminSession[]>([])
  const [code, setCode] = useState('')
  /// Confirmations go to the shell's toast host, not into this panel.
  ///
  /// They used to be an inline <p> above the content, so every "saved" and
  /// "approved" pushed the table you were working in down by a line — the row
  /// you had just clicked moved out from under the pointer. A confirmation is
  /// transient and belongs over the page; the error below stays inline,
  /// because a failure should sit there until it is dealt with.
  const setNotice = notify
  /// A failure the operator caused: a save refused, a delete rejected. It stays
  /// until they do something else, because it is an answer to something they
  /// did and they have to be able to read it.
  const [error, setError] = useState('')
  /// A failure of the panel's own reading — the poll, the refreshes. Cleared by
  /// the next successful read, because nothing else will ever clear it and one
  /// blink during a hub restart otherwise pinned "cannot reach the hub" over a
  /// panel that had been working again for an hour.
  ///
  /// Separate cells on purpose. Sharing one meant a read success cleared an
  /// action failure: with a scan running the SSE hint fires every 250 ms, so a
  /// refused delete was wiped before it could be read — a lost error, which is
  /// worse than the stale one it replaced.
  const [readError, setReadError] = useState('')
  const [confirming, setConfirming] = useState<string | null>(null)
  const [libraries, setLibraries] = useState<Library[]>([])
  const [collections, setCollections] = useState<CollectionInfo[]>([])
  const [newLibName, setNewLibName] = useState('')
  const [newLibType, setNewLibType] = useState('movies')
  const [tab, setTab] = useState<SectionId>('satellites')

  /// What every successful mutation does. Only four sites cleared `error`, so
  /// a failure that had since been resolved stayed above the panel and read as
  /// the outcome of the NEXT action — the operator repeats something that
  /// already worked.
  const clearThenReload = () => {
    setError('')
    return reload()
  }

  /// Monotonic, so a slower earlier read cannot win. `reload` fires from mount,
  /// a fifteen-second timer, every hint burst and eight mutation callbacks with
  /// no in-flight guard: a poll that started 200ms before a Disable resolved
  /// after it and repainted the pre-mutation snapshot, so the button flipped
  /// back and the operator's click looked like it had done nothing.
  const readSeq = useRef(0)

  async function reload() {
    const mine = ++readSeq.current
    try {
      const [e, s, x, l, c] = await Promise.all([
        adminEnrollments(),
        adminSatellites(),
        adminSessions(),
        adminLibraries(),
        adminCollections(),
      ])
      if (mine !== readSeq.current) return
      // The hub's own in-process mediahost (AR-5) is not an enrolled
      // satellite: it has no certificate to show, nothing to enable or
      // disable, and nothing to revoke. Listing it only offered a Delete
      // that would wipe the index of everything it serves — the whole
      // library, on an all-in-one deployment. Its COLLECTIONS still
      // appear in the composer below, which reads the collections table
      // and never this list.
      setPending(e.pending)
      setSatellites(s.satellites.filter((x) => x.cert_fingerprint !== IN_PROCESS))
      setSessions(x.sessions)
      setLibraries(l.libraries)
      setCollections(c.collections)
      setReadError('')
    } catch (err) {
      if (mine === readSeq.current) setReadError(String(err))
    }
  }

  // Events push; the interval is only a safety net. Hints arrive in
  // bursts (scan progress), so reloads are debounced.
  const [tick, setTick] = useState(0)
  useEffect(() => {
    reload()
    const t = setInterval(reload, POLL_MS)
    let debounce: ReturnType<typeof setTimeout> | undefined
    const es = openEvents((e) => {
      // Filtered on the hint's kind. The hub emits a `scan` hint every five
      // hundred files, and every one of them used to re-read users and provider
      // credentials as well — eight requests a burst, for the whole of a scan,
      // none of which a scan can change. `tick` drives those two panels, so it
      // only moves for the kinds that touch them.
      const touchesPanels = e.kind !== 'scan'
      clearTimeout(debounce)
      debounce = setTimeout(() => {
        reload()
        if (touchesPanels) setTick((n) => n + 1)
      }, 250)
    })
    return () => {
      clearInterval(t)
      clearTimeout(debounce)
      es.close()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  async function approve(e: React.FormEvent) {
    e.preventDefault()
    setError('')
    try {
      const r = await adminApprove(code.trim())
      setNotice(`Approved: ${r.approved}`)
      setCode('')
      reload()
    } catch (err) {
      setError(String(err))
    }
  }

  async function deleteSatellite(id: string) {
    if (confirming !== id) {
      setConfirming(id)
      return
    }
    setConfirming(null)
    setError('')
    try {
      await adminDeleteSatellite(id)
      setNotice(
        `Deleted ${id}: certificate revoked, collections removed. Watch state is archived and restored if the media returns.`,
      )
      reload()
    } catch (err) {
      setError(String(err))
    }
  }

  const here = SECTIONS.find((x) => x.id === tab)!
  /// MH-8: files a mediahost could not read. The mediahost counts them in its
  /// own scan reports, per collection; this is that, per host, because the
  /// host is what you would go and look at.
  ///
  /// A count and no more: `FileError` carries the path and the reason, and the
  /// hub only logs them — there is no endpoint that would let this say WHICH
  /// files. Saying "three" and pointing at the log beats saying nothing.
  const unreadableOn = (moduleId: string) =>
    collections
      .filter((c) => c.module_id === moduleId)
      .reduce((n, c) => n + (c.scan?.failed ?? 0), 0)

  /// Counts, and only for the two that change without you: a number that
  /// never moves is furniture.
  const badge = (id: SectionId) =>
    id === 'satellites' ? pending.length : id === 'sessions' ? sessions.length : 0

  return (
    <main className="admin">
      <nav className="admin-nav">
        {SECTIONS.map((x) => (
          <button
            key={x.id}
            className={`admin-tab${tab === x.id ? ' on' : ''}`}
            onClick={() => {
              setTab(x.id)
              setError('')
              setReadError('')
            }}
          >
            {x.label}
            {badge(x.id) > 0 && (
              /* A satellite waiting to be let in is the one thing here that
                 needs you now, so its count pulses until you go and look. */
              <span
                className={`admin-badge${x.id === 'satellites' && tab !== 'satellites' ? ' urgent' : ''}`}
              >
                {badge(x.id)}
              </span>
            )}
          </button>
        ))}
      </nav>

      <div className="admin-panel">
        <h1>{here.label}</h1>
        <p className="dim admin-intro">{here.intro}</p>
        {/* The operator's own failure wins: it is an answer to something they
            did, and a connectivity line underneath it would only compete. */}
        {(error || readError) && <div className="error">{error || readError}</div>}

        {tab === 'providers' && (
          <TmdbSection
            onNotice={setNotice}
            onError={setError}
            onReadError={setReadError}
            tick={tick}
          />
        )}

        {tab === 'satellites' && (
          <>
            <h2>Pending enrollments</h2>
            {pending.length === 0 ? (
              <p className="dim">
                None. A new satellite prints its code on its console when it first starts; enter
                that code here to admit it.
              </p>
            ) : (
              <ul className="rows">
                {pending.map((p) => (
                  <li key={p.csr_fingerprint}>
                    <span className="chips">
                      <span className="chip">{p.module_type}</span>
                      <span title={`${p.module_id}\ncsr ${p.csr_fingerprint}`}>{p.name}</span>
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <form className="approve-row" onSubmit={approve}>
              <input
                placeholder="Enrollment code (XXXX-XXXX)"
                value={code}
                onChange={(e) => setCode(e.target.value)}
              />
              <button className="btn" disabled={!code.trim()}>
                Approve
              </button>
            </form>

            <h2>Enrolled</h2>
            {satellites.length === 0 && <p className="dim">No satellites enrolled.</p>}
            <ul className="rows">
              {satellites.map((s) => (
                // Who it is on one line, what it can do underneath. As one flex
                // row the capability chips sat between the name and the buttons
                // and pushed them around, so no two rows lined up.
                <li key={s.module_id} className="sat-row">
                  <div className="sat-head">
                    <span className="chips">
                      <span className={s.connected ? 'chip' : 'chip warn'}>
                        {s.connected ? 'online' : 'offline'}
                      </span>
                      <span className="chip dim">{s.module_type}</span>
                      {/* The id and the certificate are what you need when
                    something is wrong, not when you are reading the list.
                    On the name, where you would go looking for them. */}
                      <span title={`${s.module_id}\ncert ${s.cert_fingerprint}`}>{s.name}</span>
                    </span>
                    <span className="sat-actions">
                      {/* On the right, where a transcoder's toggle is: this side of
                    the row is what the host's state is, and the left is who it
                    is. */}
                      {unreadableOn(s.module_id) > 0 && (
                        <span
                          className="chip warn"
                          title="Files this host reported it could not read during a scan. They stay known — nothing was dropped from the library — and the hub log names each one."
                        >
                          {unreadableOn(s.module_id)} unreadable
                        </span>
                      )}
                      {s.module_type === 'transcoder' && (
                        /* No `disabled` chip: the button already says which way
                     round it is, and saying it twice left the state to be
                     read off a badge while the action sat elsewhere. The
                     colour is on the button that clears it. */
                        <button
                          className={`btn ghost small${s.disabled ? ' warn' : ''}`}
                          title={
                            s.disabled
                              ? 'Disabled — no work is sent here'
                              : 'Stop sending work here'
                          }
                          onClick={() =>
                            adminSetSatelliteDisabled(s.module_id, !s.disabled)
                              .then(clearThenReload)
                              .catch((e: unknown) => setError(String(e)))
                          }
                        >
                          {s.disabled ? 'Disabled — enable' : 'Disable'}
                        </button>
                      )}
                      <button
                        className={
                          confirming === s.module_id ? 'btn danger small' : 'btn ghost small'
                        }
                        onClick={() => deleteSatellite(s.module_id)}
                        onBlur={() => setConfirming(null)}
                      >
                        {confirming === s.module_id ? 'Really delete + revoke?' : 'Delete'}
                      </button>
                    </span>
                  </div>
                  {s.module_type === 'transcoder' && <MeasuredFacts s={s} />}
                </li>
              ))}
            </ul>
          </>
        )}

        {tab === 'libraries' && (
          <>
            <form
              className="row-form"
              onSubmit={(e) => {
                e.preventDefault()
                adminCreateLibrary(newLibName, newLibType)
                  .then(() => {
                    setNewLibName('')
                    return clearThenReload()
                  })
                  .catch((err) => setError(String(err)))
              }}
            >
              <input
                placeholder="new library name"
                value={newLibName}
                onChange={(e) => setNewLibName(e.target.value)}
              />
              <select value={newLibType} onChange={(e) => setNewLibType(e.target.value)}>
                <option value="movies">movies</option>
                <option value="series">series</option>
                <option value="anime">anime</option>
                <option value="music">music</option>
              </select>
              <button className="btn small" disabled={!newLibName.trim()}>
                Create
              </button>
            </form>
            <ul className="rows">
              {libraries.map((l) => {
                const attachable = collections.filter(
                  (c) =>
                    c.media_type === l.media_type &&
                    !l.collections.some(
                      (m) => m.module_id === c.module_id && m.collection_id === c.collection_id,
                    ),
                )
                return (
                  <li key={l.id}>
                    <span className="chips">
                      <span className="chip">{l.media_type}</span>
                      <span>{l.name}</span>
                      {l.collections.map((m) => {
                        const info = collections.find(
                          (c) => c.module_id === m.module_id && c.collection_id === m.collection_id,
                        )
                        const scan = info?.scan
                        return (
                          <span
                            className={info && !info.connected ? 'chip warn' : 'chip dim'}
                            key={`${m.module_id}/${m.collection_id}`}
                          >
                            {m.host_name ?? m.module_id}/{m.collection_id}
                            {info && !info.connected && ' (offline)'}
                            {scan && (
                              <span className="mono">
                                {' '}
                                · {scan.complete ? 'scanned' : 'scanning'} {scan.scanned}
                                {scan.skipped > 0 && ` (+${scan.skipped} unchanged)`}
                                {scan.failed > 0 && ` · ${scan.failed} failed`}
                              </span>
                            )}{' '}
                            <button
                              className="chip-x"
                              title="detach"
                              onClick={() =>
                                adminDetachCollection(l.id, m.module_id, m.collection_id)
                                  .then(clearThenReload)
                                  .catch((err) => setError(String(err)))
                              }
                            >
                              ×
                            </button>
                          </span>
                        )
                      })}
                      {attachable.length > 0 && (
                        <select
                          value=""
                          onChange={(e) => {
                            const c = attachable[Number(e.target.value)]
                            if (c)
                              adminAttachCollection(l.id, c.module_id, c.collection_id)
                                .then(clearThenReload)
                                .catch((err) => setError(String(err)))
                          }}
                        >
                          <option value="">attach…</option>
                          {attachable.map((c, i) => (
                            <option key={`${c.module_id}/${c.collection_id}`} value={i}>
                              {c.host_name ?? c.module_id}/{c.collection_id}
                            </option>
                          ))}
                        </select>
                      )}
                    </span>
                    <span>
                      <button
                        className="btn ghost small"
                        disabled={l.collections.length === 0}
                        onClick={() =>
                          adminRefreshLibrary(l.id)
                            .then((r) => {
                              setNotice(
                                `Refresh requested: ${r.asked} collection(s)` +
                                  (r.offline > 0 ? `, ${r.offline} offline` : ''),
                              )
                              return clearThenReload()
                            })
                            .catch((err) => setError(String(err)))
                        }
                      >
                        Refresh
                      </button>
                      <button
                        // Armed, like the satellite and user deletes. This is
                        // the most destructive of the three and was the only
                        // one that went on a single click: it cascades the
                        // collection attachments AND every per-user grant, and
                        // re-creating the library mints a new id, so every
                        // narrowed account has to be granted again by hand —
                        // an account granted only this library silently becomes
                        // "no access".
                        className={confirming === l.id ? 'btn danger small' : 'btn ghost small'}
                        title="Removes the library, its collection attachments and every grant to it"
                        onBlur={() => setConfirming(null)}
                        onClick={() => {
                          if (confirming !== l.id) {
                            setConfirming(l.id)
                            return
                          }
                          setConfirming(null)
                          adminDeleteLibrary(l.id)
                            .then(() => {
                              setError('')
                              return reload()
                            })
                            .catch((err) => setError(String(err)))
                        }}
                      >
                        {confirming === l.id ? 'Really delete + revoke grants?' : 'Delete'}
                      </button>
                    </span>
                  </li>
                )
              })}
            </ul>
          </>
        )}

        {tab === 'users' && (
          <UsersSection
            libraries={libraries}
            onNotice={setNotice}
            onError={setError}
            onReadError={setReadError}
            tick={tick}
          />
        )}

        {tab === 'sessions' && (
          <>
            {sessions.length === 0 && <p className="dim">Nobody is streaming.</p>}
            <ul className="rows">
              {sessions.map((s) => (
                <li key={s.session_id}>
                  <span className="chips">
                    <span className="chip">{deliveryPlan(s.streams?.cost ?? s.mode).chip}</span>
                    <span>{s.title ?? s.session_id}</span>
                    {s.streams && (
                      <span className="mono dim">
                        v: {s.streams.video} · a: {s.streams.audio}
                      </span>
                    )}
                    <span className="dim">{s.username ?? '?'}</span>
                    <span className="mono dim">idle {s.idle_secs}s</span>
                  </span>
                  <span>
                    <button
                      className="btn ghost small"
                      onClick={() =>
                        downloadWithAuth(
                          `/admin/v1/sessions/${encodeURIComponent(s.session_id)}/log`,
                        ).catch((err: unknown) => setError(String(err)))
                      }
                    >
                      Log
                    </button>
                    <button
                      className="btn ghost small"
                      onClick={() =>
                        adminEndSession(s.session_id)
                          .then(clearThenReload)
                          .catch((err: unknown) => setError(String(err)))
                      }
                    >
                      End
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>
    </main>
  )
}

/// One media type's chain. Its own component because each chain is its own
/// list and therefore needs its own drag state.
function ChainPills({
  order,
  onMove,
}: {
  order: string[]
  onMove: (from: number, to: number) => void
}) {
  const drag = useDragOrder(onMove)
  return (
    <span className="chips">
      {order.map((p, i) => (
        <span
          key={p}
          className={`chain-pill${drag.look(i)}`}
          title="Drag, or use the arrow keys, to change precedence"
          /* The same arrangement as the subtitle ladder in Settings, and for
             the same reason. The redesign replaced this chain's per-pill ↑/↓
             buttons with dragging alone, which took the order away from the
             keyboard entirely and from touch as well — HTML5 drag events do
             not fire on a touchscreen, so on a tablet the precedence became
             unchangeable. The grip still says what the mouse can do. */
          tabIndex={0}
          onKeyDown={(e) => {
            if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return
            e.preventDefault()
            const to = e.key === 'ArrowLeft' ? i - 1 : i + 1
            if (to < 0 || to >= order.length) return
            onMove(i, to)
          }}
          {...drag.row(i)}
        >
          <span className="lang-grip" aria-hidden="true">
            <Icon name="grip" size={10} />
          </span>
          <span className="dim mono">{i + 1}.</span>
          {p}
        </span>
      ))}
    </span>
  )
}

/// HUB-5: which provider wins a field, per media type. Earlier providers
/// own a field; later ones only fill what the earlier left empty. Applying
/// re-merges from answers already on disk — no provider is contacted, so
/// this is safe to try and trivially reversible.
function ProviderOrder({
  chains,
  onNotice,
  onError,
  onDone,
}: {
  chains: Record<string, ProviderChain>
  onNotice: (m: string) => void
  onError: (m: string) => void
  onDone: () => void
}) {
  const [draft, setDraft] = useState<Record<string, string[]>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const order = (mt: string) => draft[mt] ?? chains[mt]?.order ?? []
  const dirty = (mt: string) =>
    JSON.stringify(order(mt)) !== JSON.stringify(chains[mt]?.order ?? [])

  const move = (mt: string, from: number, to: number) => {
    const next = moved(order(mt), from, to)
    if (next) setDraft({ ...draft, [mt]: next })
  }

  const apply = (mt: string) => {
    setBusy(mt)
    void adminSetChain(mt, order(mt))
      .then(() => {
        onNotice(`${mt}: provider order applied — metadata re-merged`)
        setDraft((d) => {
          const { [mt]: _dropped, ...rest } = d
          return rest
        })
        onDone()
      })
      .catch((e: unknown) => onError(String(e)))
      .finally(() => setBusy(null))
  }

  const names = Object.keys(chains)
  if (names.length === 0) return null
  return (
    <>
      <p className="dim card-prose">
        The first provider to supply a field owns it; the rest fill what it left empty. Applying
        re-merges answers already on disk — instant, and no provider is contacted.
      </p>
      {names.map((mt) => (
        <div className="pref-row" key={mt}>
          <span className="pref-label mono">{mt}</span>
          <ChainPills order={order(mt)} onMove={(from, to) => move(mt, from, to)} />
          {order(mt).length < 2 && <span className="dim">only one provider</span>}
          <button
            className="btn small"
            disabled={!dirty(mt) || busy === mt}
            onClick={() => apply(mt)}
          >
            {busy === mt ? 'Applying…' : 'Apply'}
          </button>
          {dirty(mt) && (
            <button
              className="btn ghost small"
              onClick={() =>
                setDraft((d) => {
                  const { [mt]: _dropped, ...rest } = d
                  return rest
                })
              }
            >
              Reset
            </button>
          )}
        </div>
      ))}
    </>
  )
}
