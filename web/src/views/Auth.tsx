import { useState } from 'react'
import { storeTokens, type Tokens } from '../api'

export default function Auth({
  mode,
  onDone,
  note,
  setupAvailable = false,
  setupUrl,
}: {
  mode: 'setup' | 'login'
  onDone: () => void
  /// Why you are here, when you did not ask to be. A session that expired
  /// mid-use lands on this screen with no explanation otherwise, which reads
  /// as the app having forgotten you for no reason.
  note?: string
  setupAvailable?: boolean
  setupUrl?: string
}) {
  const [user, setUser] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const [setupDone, setSetupDone] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError('')
    const [url, body] =
      mode === 'setup'
        ? ['/api/v1/setup', { username: user, password }]
        : ['/api/v1/auth/token', { username: user, password }]
    // Raw `fetch`, because there is no token yet and `api()` exists to
    // attach one — but that also means nothing here turns an unreachable hub
    // into a sentence. Unhandled, the rejection skipped `setBusy(false)` and
    // left the only button on the screen greyed out with nothing said: one
    // wifi blip and signing in needed a reload.
    let r: Response
    try {
      r = await fetch(url as string, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      })
    } catch {
      setBusy(false)
      setError('Could not reach the hub.')
      return
    }
    setBusy(false)
    if (!r.ok) {
      setError((await r.text()) || 'Something went wrong')
      return
    }
    if (mode === 'setup') {
      // The trusted-local listener closes after this response. Its origin is
      // deliberately not the normal hub origin, so its tokens would not be
      // useful there; tell the operator to return to the address they use.
      setSetupDone(true)
      return
    }
    storeTokens((await r.json()) as Tokens)
    onDone()
  }

  if (mode === 'setup' && !setupAvailable) {
    return (
      <div className="auth-wrap">
        <div className="auth-card">
          <div className="wordmark big">
            kahawai<span className="tilde">~</span>
          </div>
          <p className="auth-hint">
            Initial setup is available only through the hub’s local control plane.
          </p>
          <p className="auth-hint">
            Open <code>{setupUrl ?? 'the local setup URL printed by the hub'}</code> on the hub,
            connect to it with an SSH tunnel, or run <code>kahawai hub init-admin</code>.
          </p>
        </div>
      </div>
    )
  }

  if (setupDone) {
    return (
      <div className="auth-wrap">
        <div className="auth-card">
          <div className="wordmark big">
            kahawai<span className="tilde">~</span>
          </div>
          <p className="auth-hint">
            Administrator created. Return to your normal Kahawai address and sign in.
          </p>
        </div>
      </div>
    )
  }

  return (
    <div className="auth-wrap">
      <form className="auth-card" onSubmit={submit}>
        <div className="wordmark big">
          kahawai<span className="tilde">~</span>
        </div>
        {mode === 'setup' ? (
          <p className="auth-hint">
            First run. Create the initial administrator from this local-only page.
          </p>
        ) : (
          <p className="auth-hint">{note ?? 'Sign in to your library.'}</p>
        )}
        <input
          placeholder="Username"
          value={user}
          onChange={(e) => setUser(e.target.value)}
          autoFocus
          autoComplete="username"
        />
        <input
          placeholder="Password"
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete={mode === 'setup' ? 'new-password' : 'current-password'}
          minLength={mode === 'setup' ? 12 : undefined}
        />
        {mode === 'setup' && <p className="auth-hint">At least 12 characters.</p>}
        {error && <div className="error">{error}</div>}
        <button
          className="btn"
          disabled={busy || (mode === 'setup' && Array.from(password).length < 12)}
        >
          {mode === 'setup' ? 'Create admin account' : 'Sign in'}
        </button>
      </form>
    </div>
  )
}
