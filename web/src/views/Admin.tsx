import { useEffect, useState } from 'react'
import {
  adminApprove,
  adminEnrichRun,
  adminEnrichStatus,
  adminProviders,
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
  adminEndSession,
  adminEnrollments,
  adminLibraries,
  adminSatellites,
  adminSessions,
  type AdminSession,
  type CollectionInfo,
  type Library,
  type PendingEnrollment,
  type Satellite,
} from '../api'

// HUB-11: the events channel pushes invalidation hints; polling remains
// only as a slow fallback for anything a hint doesn't cover.
const POLL_MS = 15000

function TmdbSection({ onNotice, tick }: { onNotice: (s: string) => void; tick: number }) {
  const [configured, setConfigured] = useState(false)
  const [tvdbConfigured, setTvdbConfigured] = useState(false)
  const [anidbConfigured, setAnidbConfigured] = useState(false)
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

  const refresh = () => {
    adminProviders()
      .then((p) => {
        setConfigured(p.tmdb.configured)
        setTvdbConfigured(p.tvdb.configured)
        setAnidbConfigured(p.anidb?.configured ?? false)
      })
      .catch(() => {})
    adminEnrichStatus().then(setStatus).catch(() => {})
  }
  useEffect(() => {
    refresh()
    const t = setInterval(refresh, POLL_MS)
    return () => clearInterval(t)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick])

  return (
    <>
      <h2>Metadata providers</h2>
      <div className="row-form">
        <input
          type="password"
          placeholder={configured ? 'key configured — paste to replace' : 'TMDB API key'}
          value={key}
          onChange={(e) => setKey(e.target.value)}
        />
        <button
          className="btn small"
          disabled={!key.trim()}
          onClick={() => {
            void adminSetTmdbKey(key.trim()).then(() => {
              setKey('')
              onNotice('TMDB key saved — enrichment started')
              refresh()
            })
          }}
        >
          Save
        </button>
        <button
          className="btn ghost small"
          disabled={!configured || status?.running}
          onClick={() => void adminEnrichRun().then(refresh)}
        >
          {status?.running ? 'Enriching…' : 'Enrich now'}
        </button>
        {status && (
          <span className="dim mono">
            {status.matched} matched · {status.weak} weak · {status.missed} missed
          </span>
        )}
      </div>
      <div className="row-form">
        <input
          type="password"
          placeholder={tvdbConfigured ? 'TVDB key configured — paste to replace' : 'TheTVDB API key (backup resolver)'}
          value={tvdbKey}
          onChange={(e) => setTvdbKey(e.target.value)}
        />
        <input
          type="password"
          placeholder="TVDB PIN (if your key needs one)"
          value={tvdbPin}
          onChange={(e) => setTvdbPin(e.target.value)}
          style={{ width: '14rem' }}
        />
        <button
          className="btn small"
          disabled={!tvdbKey.trim()}
          onClick={() => {
            void adminSetTvdbKey(tvdbKey.trim(), tvdbPin.trim() || undefined).then(() => {
              setTvdbKey('')
              setTvdbPin('')
              onNotice('TVDB key saved — enrichment started')
              refresh()
            })
          }}
        >
          Save
        </button>
      </div>
      <div className="row-form">
        <input
          placeholder={
            anidbConfigured
              ? 'AniDB account configured — enter to replace'
              : 'AniDB username (exact file matching)'
          }
          value={anidbUser}
          onChange={(e) => setAnidbUser(e.target.value)}
        />
        <input
          type="password"
          placeholder="AniDB password"
          value={anidbPass}
          onChange={(e) => setAnidbPass(e.target.value)}
        />
        <input
          type="password"
          placeholder="UDP API key (optional, encrypts the session)"
          value={anidbKey}
          onChange={(e) => setAnidbKey(e.target.value)}
          style={{ width: '16rem' }}
        />
        <button
          className="btn small"
          disabled={!anidbUser.trim() || !anidbPass.trim()}
          onClick={() => {
            void adminSetAnidb(anidbUser.trim(), anidbPass.trim(), anidbKey.trim() || undefined).then(
              (r) => {
                setAnidbUser('')
                setAnidbPass('')
                setAnidbKey('')
                onNotice(
                  r.verified
                    ? 'AniDB account verified — enrichment started'
                    : `AniDB saved but login failed: ${r.error ?? 'unknown'}`,
                )
                refresh()
              },
            )
          }}
        >
          Save
        </button>
      </div>
    </>
  )
}

export default function Admin() {
  const [pending, setPending] = useState<PendingEnrollment[]>([])
  const [satellites, setSatellites] = useState<Satellite[]>([])
  const [sessions, setSessions] = useState<AdminSession[]>([])
  const [code, setCode] = useState('')
  const [notice, setNotice] = useState('')
  const [error, setError] = useState('')
  const [confirming, setConfirming] = useState<string | null>(null)
  const [libraries, setLibraries] = useState<Library[]>([])
  const [collections, setCollections] = useState<CollectionInfo[]>([])
  const [newLibName, setNewLibName] = useState('')
  const [newLibType, setNewLibType] = useState('movies')

  async function reload() {
    try {
      const [e, s, x, l, c] = await Promise.all([
        adminEnrollments(),
        adminSatellites(),
        adminSessions(),
        adminLibraries(),
        adminCollections(),
      ])
      setPending(e.pending)
      setSatellites(s.satellites)
      setSessions(x.sessions)
      setLibraries(l.libraries)
      setCollections(c.collections)
    } catch (err) {
      setError(String(err))
    }
  }

  // Events push; the interval is only a safety net. Hints arrive in
  // bursts (scan progress), so reloads are debounced.
  const [tick, setTick] = useState(0)
  useEffect(() => {
    reload()
    const t = setInterval(reload, POLL_MS)
    let debounce: ReturnType<typeof setTimeout> | undefined
    const es = openEvents(() => {
      clearTimeout(debounce)
      debounce = setTimeout(() => {
        reload()
        setTick((n) => n + 1)
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
    setNotice('')
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
      setNotice(`Deleted ${id}: certificate revoked, collections removed. Watch state is archived and restored if the media returns.`)
      reload()
    } catch (err) {
      setError(String(err))
    }
  }

  return (
    <main>
      <h1>Admin</h1>
      {notice && <p className="notice">{notice}</p>}
      {error && <div className="error">{error}</div>}

      <TmdbSection onNotice={setNotice} tick={tick} />

      <h2>Pending enrollments</h2>
      {pending.length === 0 ? (
        <p className="dim">
          None. A new satellite prints its code on its console when it first
          starts; enter that code here to admit it.
        </p>
      ) : (
        <ul className="rows">
          {pending.map((p) => (
            <li key={p.csr_fingerprint}>
              <span className="chips">
                <span className="chip">{p.module_type}</span>
                <span>{p.name}</span>
                <span className="mono dim">{p.module_id}</span>
                <span className="mono dim">{p.csr_fingerprint.slice(0, 16)}…</span>
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

      <h2>Satellites</h2>
      {satellites.length === 0 && <p className="dim">No satellites enrolled.</p>}
      <ul className="rows">
        {satellites.map((s) => (
          <li key={s.module_id}>
            <span className="chips">
              <span className={s.connected ? 'chip' : 'chip warn'}>
                {s.connected ? 'online' : 'offline'}
              </span>
              <span className="chip dim">{s.module_type}</span>
              <span>{s.name}</span>
              <span className="mono dim">{s.module_id}</span>
              <span className="mono dim">{s.cert_fingerprint.slice(0, 16)}…</span>
              {s.disabled && <span className="chip warn">disabled</span>}
            </span>
            <span>
              {s.module_type === 'transcoder' && (
                <button
                  className="btn ghost small"
                  onClick={() =>
                    adminSetSatelliteDisabled(s.module_id, !s.disabled).then(reload)
                  }
                >
                  {s.disabled ? 'Enable' : 'Disable'}
                </button>
              )}
              <button
                className={confirming === s.module_id ? 'btn danger small' : 'btn ghost small'}
                onClick={() => deleteSatellite(s.module_id)}
                onBlur={() => setConfirming(null)}
              >
                {confirming === s.module_id ? 'Really delete + revoke?' : 'Delete'}
              </button>
            </span>
          </li>
        ))}
      </ul>

      <h2>Libraries</h2>
      <form
        className="row-form"
        onSubmit={(e) => {
          e.preventDefault()
          adminCreateLibrary(newLibName, newLibType)
            .then(() => {
              setNewLibName('')
              return reload()
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
                          .then(reload)
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
                          .then(reload)
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
                        return reload()
                      })
                      .catch((err) => setError(String(err)))
                  }
                >
                  Refresh
                </button>
                <button
                  className="btn ghost small"
                  onClick={() =>
                    adminDeleteLibrary(l.id).then(reload).catch((err) => setError(String(err)))
                  }
                >
                  Delete
                </button>
              </span>
            </li>
          )
        })}
      </ul>

      <h2>Active sessions</h2>
      {sessions.length === 0 && <p className="dim">Nobody is streaming.</p>}
      <ul className="rows">
        {sessions.map((s) => (
          <li key={s.session_id}>
            <span className="chips">
              <span className="chip">{s.mode}</span>
              <span>{s.title ?? s.session_id}</span>
              {s.streams && (
                <span className="mono dim">
                  v: {s.streams.video} · a: {s.streams.audio}
                </span>
              )}
              <span className="dim">{s.username ?? '?'}</span>
              <span className="mono dim">idle {s.idle_secs}s</span>
            </span>
            <button
              className="btn ghost small"
              onClick={() => adminEndSession(s.session_id).then(reload)}
            >
              End
            </button>
          </li>
        ))}
      </ul>
    </main>
  )
}
