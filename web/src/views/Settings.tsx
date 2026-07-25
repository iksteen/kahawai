import { useEffect, useRef, useState } from 'react'
import { fetchPrefs, putPref } from '../api'

// HUB-33 per-media-type track defaults, stored as user-global prefs:
// key `audio.{type}` / `subs.{type}` = ordered language list ('original'
// allowed for audio). Every mutation saves immediately; per-series
// manual choices still outrank these.
const MEDIA_TYPES = ['movies', 'series', 'anime'] as const

const SUGGEST = [
  'original', 'en', 'nl', 'ja', 'de', 'fr', 'es', 'it', 'pt', 'sv',
  'da', 'no', 'fi', 'pl', 'ru', 'zh', 'ko',
]
const TOKEN = /^(original|[a-z]{2,3})$/

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
  const items =
    kind === 'audio' && !stored.includes('original') ? [...stored, 'original'] : stored
  const [entry, setEntry] = useState('')
  const [bad, setBad] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  const commit = (list: string[]) => {
    const v = list.join(',')
    putPref('', `${kind}.${mediaType}`, v)
      .then(() => {
        onChange(v)
        flash()
      })
      .catch(() => setBad(true))
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
    commit([...items, t])
  }

  const listId = `langs-${kind}-${mediaType}`
  return (
    <div className="row-form pref-row">
      <span className="pref-label mono">{kind}</span>
      <span className="chips">
        {items.length === 0 && <span className="dim">no subtitles</span>}
        {items.map((l, i) => (
          <button
            key={l}
            className={l === 'original' ? 'chip' : 'chip dim'}
            title={i === 0 ? 'first choice' : 'make first choice'}
            onClick={() => i > 0 && commit([l, ...items.filter((x) => x !== l)])}
          >
            {l}
            {!(kind === 'audio' && l === 'original') && (
              <>
                {' '}
                <span
                  className="chip-x"
                  title="remove"
                  onClick={(e) => {
                    e.stopPropagation()
                    commit(items.filter((x) => x !== l))
                  }}
                >
                  ×
                </span>
              </>
            )}
          </button>
        ))}
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
        {SUGGEST.filter(
          (s) => !items.includes(s) && !(kind === 'subs' && s === 'original'),
        ).map((s) => (
          <option key={s} value={s} />
        ))}
      </datalist>
    </div>
  )
}

export default function Settings() {
  const [values, setValues] = useState<Record<string, string>>({})
  const [loaded, setLoaded] = useState(false)
  const [saved, setSaved] = useState(false)
  const savedTimer = useRef<ReturnType<typeof setTimeout>>(undefined)

  const flash = () => {
    setSaved(true)
    clearTimeout(savedTimer.current)
    savedTimer.current = setTimeout(() => setSaved(false), 1200)
  }

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
      <h1>
        Settings{' '}
        {saved && <span className="chip saved-flash">saved</span>}
      </h1>
      <p className="dim">
        Default tracks per media type, first match wins left to right.{' '}
        <code>original</code> follows the title's original language; click a language to
        make it the first choice. Changes save immediately. Anything you set manually
        while watching overrides these for that series or movie.
      </p>
      {MEDIA_TYPES.map((mt) => (
        <section key={mt}>
          <h2>{mt}</h2>
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
    </main>
  )
}
