import { useEffect, useState } from 'react'
import {
  adminApprove,
  adminSetSatelliteDisabled,
  adminDeleteSatellite,
  adminEndSession,
  adminEnrollments,
  adminSatellites,
  adminSessions,
  type AdminSession,
  type PendingEnrollment,
  type Satellite,
} from '../api'

// ponytail: 3 s polling; the /api/v1/events channel replaces this later.
const POLL_MS = 3000

export default function Admin() {
  const [pending, setPending] = useState<PendingEnrollment[]>([])
  const [satellites, setSatellites] = useState<Satellite[]>([])
  const [sessions, setSessions] = useState<AdminSession[]>([])
  const [code, setCode] = useState('')
  const [notice, setNotice] = useState('')
  const [error, setError] = useState('')
  const [confirming, setConfirming] = useState<string | null>(null)

  async function reload() {
    try {
      const [e, s, x] = await Promise.all([
        adminEnrollments(),
        adminSatellites(),
        adminSessions(),
      ])
      setPending(e.pending)
      setSatellites(s.satellites)
      setSessions(x.sessions)
    } catch (err) {
      setError(String(err))
    }
  }

  useEffect(() => {
    reload()
    const t = setInterval(reload, POLL_MS)
    return () => clearInterval(t)
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
