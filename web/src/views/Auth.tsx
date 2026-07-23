import { useState } from 'react'
import { storeTokens, type Tokens } from '../api'

export default function Auth({ mode, onDone }: { mode: 'setup' | 'login'; onDone: () => void }) {
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
    const r = await fetch(url as string, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    })
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
            First run. Enter the setup token printed on the hub console to
            create the admin account.
          </p>
        ) : (
          <p className="auth-hint">Sign in to your library.</p>
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
