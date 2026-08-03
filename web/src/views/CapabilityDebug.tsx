// The capability mask editor (see capabilities.ts for what a mask is
// and why it only ever subtracts). Rendered twice, because a mask only
// takes effect on the next session: on the item page, where it is set
// before playback starts, and in the player, next to the verdict it
// changes — toggle a codec off, restart, read what the hub decided
// instead.

import { useState } from 'react'
import {
  buildProfile,
  loadMask,
  maskSummary,
  probedProfile,
  saveMask,
  type CapabilityMask,
} from '../capabilities'

type DropKind = 'video' | 'audio' | 'containers'

export default function CapabilityDebug({
  onApply,
  applying,
  onChange,
}: {
  /** Restart playback with the new mask; absent = show the hint only. */
  onApply?: () => void
  applying?: boolean
  /** Told after every edit, so a mask badge outside this panel can
   *  stop showing what the mask USED to be. */
  onChange?: () => void
}) {
  const probed = probedProfile()
  const [mask, setMask] = useState<CapabilityMask>(loadMask)
  const [copied, setCopied] = useState(false)

  const update = (next: CapabilityMask) => {
    setMask(next)
    saveMask(next)
    onChange?.()
  }

  const dropped = (kind: DropKind, name: string) => !!mask[kind]?.includes(name)

  const toggleDrop = (kind: DropKind, name: string) => {
    const cur = mask[kind] ?? []
    const next = { ...mask }
    const without = cur.filter((n) => n !== name)
    if (cur.includes(name)) {
      if (without.length) next[kind] = without
      else delete next[kind]
    } else {
      next[kind] = [...cur, name]
    }
    update(next)
  }

  const setFlag = (flag: 'hdr' | 'ass_render' | 'graphics_overlay' | 'vtt_render', value: boolean) => {
    const next = { ...mask }
    // Back at the probe's own answer = no override at all, so the
    // summary never reports a mask that changes nothing.
    if (value === probed[flag]) delete next[flag]
    else next[flag] = value
    update(next)
  }

  const setCeiling = (key: 'max_height' | 'max_audio_channels', value: number) => {
    const next = { ...mask }
    if (value > 0) next[key] = value
    else delete next[key]
    update(next)
  }

  const flag = (k: 'hdr' | 'ass_render' | 'graphics_overlay' | 'vtt_render') => mask[k] ?? probed[k]
  const summary = maskSummary(mask)

  const codecRow = (kind: DropKind, label: string, names: string[]) => (
    <div className="caps-row">
      <span className="caps-label dim">{label}</span>
      <span className="caps-opts">
        {names.length === 0 ? (
          <span className="dim">none probed</span>
        ) : (
          names.map((n) => (
            <label key={n} className={dropped(kind, n) ? 'caps-off' : ''}>
              <input
                type="checkbox"
                checked={!dropped(kind, n)}
                onChange={() => toggleDrop(kind, n)}
              />
              {n}
            </label>
          ))
        )}
      </span>
    </div>
  )

  return (
    <div className="caps-panel mono">
      <div className="dim caps-intro">
        Unchecking removes a capability from the profile sent to the hub — and from what this
        player renders — so the negotiation takes the branch a lesser client would. Encodes target
        h264/aac, so dropping those makes sources that need an encode honestly UNPLAYABLE — that
        refusal is the branch under test.
      </div>

      {codecRow(
        'video',
        'video',
        probed.video.map((c) => c.codec),
      )}
      {codecRow('audio', 'audio', probed.audio)}
      {codecRow('containers', 'container', probed.containers)}

      <div className="caps-row">
        <span className="caps-label dim">declares</span>
        <span className="caps-opts">
          {(['hdr', 'ass_render', 'graphics_overlay', 'vtt_render'] as const).map((k) => (
            <label key={k} className={flag(k) ? '' : 'caps-off'}>
              <input type="checkbox" checked={flag(k)} onChange={(e) => setFlag(k, e.target.checked)} />
              {k}
            </label>
          ))}
        </span>
      </div>

      {/* The one declaration that is not a boolean: correctness,
          latency and encode cost, pick two. `short` is the only mode
          that can force a video encode, so it carries the ceiling it
          is buying. */}
      <div className="caps-row">
        <span className="caps-label dim">targetduration</span>
        <span className="caps-opts">
          {(['ignore', 'accurate', 'short'] as const).map((m) => {
            const cur = mask.target_duration ?? probed.target_duration
            const on = cur.mode === m
            return (
              <label key={m} className={on ? '' : 'caps-off'}>
                <input
                  type="radio"
                  name="targetduration"
                  checked={on}
                  onChange={() =>
                    update({
                      ...mask,
                      target_duration: m === 'short' ? { mode: m, max_secs: 6 } : { mode: m },
                    })
                  }
                />
                {m}
              </label>
            )
          })}
          {(mask.target_duration ?? probed.target_duration).mode === 'short' && (
            <label>
              max
              <select
                value={
                  (mask.target_duration as { mode: 'short'; max_secs: number } | undefined)
                    ?.max_secs ?? 6
                }
                onChange={(e) =>
                  update({
                    ...mask,
                    target_duration: { mode: 'short', max_secs: Number(e.target.value) },
                  })
                }
              >
                {[2, 4, 6, 10].map((n) => (
                  <option key={n} value={n}>
                    {n}s
                  </option>
                ))}
              </select>
            </label>
          )}
        </span>
      </div>

      <div className="caps-row">
        <span className="caps-label dim">ceilings</span>
        <span className="caps-opts">
          <label>
            height
            <select
              value={mask.max_height ?? 0}
              onChange={(e) => setCeiling('max_height', Number(e.target.value))}
            >
              <option value={0}>none</option>
              <option value={2160}>2160</option>
              <option value={1080}>1080</option>
              <option value={720}>720</option>
              <option value={480}>480</option>
            </select>
          </label>
          <label>
            channels
            <select
              value={mask.max_audio_channels ?? 0}
              onChange={(e) => setCeiling('max_audio_channels', Number(e.target.value))}
            >
              <option value={0}>unlimited</option>
              <option value={6}>6</option>
              <option value={2}>2</option>
              <option value={1}>1</option>
            </select>
          </label>
        </span>
      </div>

      <div className="caps-actions">
        {onApply && (
          <button className="btn small" onClick={onApply} disabled={applying}>
            {applying ? 'restarting…' : 'apply & restart'}
          </button>
        )}
        <button className="btn ghost small" onClick={() => update({})} disabled={!summary.length}>
          reset
        </button>
        <button
          className="btn ghost small"
          onClick={() => {
            // The same profile as JSON, for `kahawai-play.sh -P` and
            // kahawai-sweep --profile: a mask reproduced outside the
            // browser sweeps the whole library the same way. (Without
            // the source-aware refinements — those depend on the item,
            // not the browser.)
            const json = JSON.stringify(buildProfile(), null, 2)
            void navigator.clipboard?.writeText(json).then(
              () => {
                setCopied(true)
                setTimeout(() => setCopied(false), 1500)
              },
              () => {},
            )
          }}
        >
          {copied ? 'copied' : 'copy profile json'}
        </button>
        <span className="dim">
          {summary.length ? `masked: ${summary.join(' ')}` : 'no mask — the browser as it is'}
        </span>
      </div>
    </div>
  )
}
