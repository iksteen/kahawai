import { useEffect, useState } from 'react'
import { fetchPrefs, putPref } from '../api'

// HUB-33 per-media-type track defaults, stored as user-global prefs:
// key `audio.{type}` = ordered language list, `original` allowed
// ("nl, original, en"); key `subs.{type}` = ordered language list,
// empty = no subtitles. Per-series manual choices still outrank these.
const MEDIA_TYPES = ['movies', 'series', 'anime'] as const

const TOKEN = /^(original|[a-z]{2,3})$/

function normalize(input: string, allowOriginal: boolean): string | null {
  const tokens = input
    .split(',')
    .map((t) => t.trim().toLowerCase())
    .filter(Boolean)
  for (const t of tokens) {
    if (!TOKEN.test(t) || (!allowOriginal && t === 'original')) return null
  }
  return tokens.join(',')
}

function LangList({
  mediaType,
  kind,
  value,
  onSaved,
}: {
  mediaType: string
  kind: 'audio' | 'subs'
  value: string
  onSaved: (v: string) => void
}) {
  const [text, setText] = useState(value)
  const [bad, setBad] = useState(false)
  useEffect(() => setText(value), [value])
  const dirty = text !== value

  const save = () => {
    const norm = normalize(text, kind === 'audio')
    if (norm === null) {
      setBad(true)
      return
    }
    setBad(false)
    putPref('', `${kind}.${mediaType}`, norm)
      .then(() => onSaved(norm))
      .catch(() => setBad(true))
  }

  return (
    <label className="row-form">
      <span className="pref-label mono">{kind}</span>
      <input
        className={bad ? 'invalid' : ''}
        placeholder={
          kind === 'audio'
            ? 'e.g. nl, original, en — empty = first track'
            : 'e.g. nl, en — empty = no subtitles'
        }
        value={text}
        onChange={(e) => {
          setText(e.target.value)
          setBad(false)
        }}
        onKeyDown={(e) => e.key === 'Enter' && save()}
      />
      <button className="btn small" disabled={!dirty} onClick={save}>
        Save
      </button>
      {bad && <span className="error-inline">use 2–3 letter codes{kind === 'audio' ? " or 'original'" : ''}, comma-separated</span>}
    </label>
  )
}

export default function Settings() {
  const [values, setValues] = useState<Record<string, string>>({})
  const [loaded, setLoaded] = useState(false)

  useEffect(() => {
    fetchPrefs()
      .then((r) => {
        const v: Record<string, string> = {}
        for (const p of r.prefs) {
          if (p.scope === '') v[p.key] = p.value
        }
        setValues(v)
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }, [])

  if (!loaded) return null
  return (
    <main>
      <h1>Settings</h1>
      <p className="dim">
        Default tracks per media type, in order of preference. <code>original</code> follows
        the title's original language. Anything you set manually while watching a series or
        movie overrides these for that title. Fallbacks: first audio track, no subtitles.
      </p>
      {MEDIA_TYPES.map((mt) => (
        <section key={mt}>
          <h2>{mt}</h2>
          {(['audio', 'subs'] as const).map((kind) => (
            <LangList
              key={kind}
              mediaType={mt}
              kind={kind}
              value={values[`${kind}.${mt}`] ?? ''}
              onSaved={(v) => setValues((cur) => ({ ...cur, [`${kind}.${mt}`]: v }))}
            />
          ))}
        </section>
      ))}
    </main>
  )
}
