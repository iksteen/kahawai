import { useState } from 'react'
import { storeTokens, type Tokens } from '../api'

export default function Auth({
  mode,
  onDone,
  note,
}: {
  mode: 'setup' | 'login'
  onDone: () => void
  /// Why you are here, when you did not ask to be. A session that expired
  /// mid-use lands on this screen with no explanation otherwise, which reads
  /// as the app having forgotten you for no reason.
  note?: string
}) {
  const [token, setToken] = useState('')
  const [user, setUser] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError('')
    const [url, body] =
      mode === 'setup'
        ? ['/api/v1/setup', { token, username: user, password }]
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
    storeTokens((await r.json()) as Tokens)
    onDone()
  }

  return (
    <div className="auth-wrap">
      <form className="auth-card" onSubmit={submit}>
        <div className="wordmark big">
          kahawai<span className="tilde">~</span>
        </div>
        {mode === 'setup' ? (
          <p className="auth-hint">
            First run. Enter the setup token printed on the hub console to create the admin account.
          </p>
        ) : (
          <p className="auth-hint">{note ?? 'Sign in to your library.'}</p>
        )}
        {mode === 'setup' && (
          <input
            placeholder="Setup token (XXXX-XXXX)"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoFocus
          />
        )}
        <input
          placeholder="Username"
          value={user}
          onChange={(e) => setUser(e.target.value)}
          autoFocus={mode === 'login'}
          autoComplete="username"
        />
        <input
          placeholder="Password"
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete={mode === 'setup' ? 'new-password' : 'current-password'}
        />
        {error && <div className="error">{error}</div>}
        <button className="btn" disabled={busy}>
          {mode === 'setup' ? 'Create admin account' : 'Sign in'}
        </button>
      </form>
    </div>
  )
}
