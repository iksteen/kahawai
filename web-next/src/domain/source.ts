/// How a file describes itself on an item page: its running time, its
/// streams, and what the hub says it would do with each of them.
///
/// Nothing here re-derives a verdict. The whole point of asking the item what
/// it would serve is that the answer comes from the code that will serve it —
/// these only decide how to WORD it.

const GB = 1024 * 1024 * 1024

/// Whole minutes, and hours once there are enough of them. Null rather than
/// "0 min" for a duration nobody knows: a browse row has no `duration_ms` at
/// all, and printing zero there states a fact about the file.
export function duration(ms?: number | null): string | null {
  if (!ms) return null
  const minutes = Math.round(ms / 60000)
  return minutes >= 60 ? `${Math.floor(minutes / 60)} h ${minutes % 60} min` : `${minutes} min`
}

export function size(bytes: number): string {
  return `${(bytes / GB).toFixed(1)} GB`
}

export type SubStream = { format: string; language?: string | null }

/// One chip for the subtitles, not one per track.
///
/// A file with 26 embedded tracks produced 26 chips all reading "text", which
/// said nothing 26 times and pushed the size and the offline mark off the row.
///
/// "3 subs · en nl" for a handful, "26 subs · 5 formats" past that. The full
/// list goes in the tooltip, where length costs nothing.
export function subtitleChip(subs: SubStream[]): string {
  const languages = [...new Set(subs.map((t) => t.language).filter(Boolean))]
  const count = `${subs.length} sub${subs.length === 1 ? '' : 's'}`
  if (languages.length > 0 && languages.length <= 6) return `${count} · ${languages.join(' ')}`
  const formats = [...new Set(subs.map((t) => t.format))]
  return `${count} · ${formats.join(' ')}`
}

export function subtitleChipTitle(subs: SubStream[]): string {
  return subs.map((t) => [t.language, t.format].filter(Boolean).join(' ')).join(', ')
}

/// Deliveries that mean something is being done TO the subtitles rather than
/// them being handed over as they are. Worth a colour, because each one costs
/// something: a burn restarts the video encode.
export const LOUD_DELIVERY: Record<string, 'warn' | 'sand'> = {
  none: 'warn',
  burn: 'sand',
  ocr: 'sand',
}

/// One stream's verdict, split into what happens and why.
///
/// "copy" / "dts → aac (transcoded) · 7.1 → 5.1" — the hub puts the action
/// first and the reasoning after a dash, so splitting on the first one colours
/// the action without parsing the sentence.
export function planRow(
  verdict: string,
  tone?: 'warn' | 'sand',
): { action: string; why: string; tone: 'warn' | 'sand' | 'teal' } {
  const [action = '', ...rest] = verdict.split(' — ')
  return {
    action,
    why: rest.join(' — '),
    // Copying and handing over text are the cheap outcomes, and the only ones
    // worth reading as "nothing is being done here".
    tone: tone ?? (/(copy|direct|text)/i.test(action) ? 'teal' : 'sand'),
  }
}

/// How a chosen subtitle track reads on the plan.
///
/// An ASS track delivered as ASS does not need saying twice; a text track
/// delivered as a burn very much does.
export function subtitleVerdict(
  track: { language?: string | null; format: string; delivery: string } | undefined,
  /// How many languages the viewer's order names. None means they asked for no
  /// subtitles at all, which is a different answer from "nothing here matches".
  wanted: number,
): { verdict: string; tone?: 'warn' | 'sand' } {
  if (!track) return wanted === 0 ? { verdict: 'off' } : { verdict: 'none', tone: 'sand' }
  const name = [track.language ?? '?', track.format].filter(Boolean).join(' ')
  const how = track.delivery === track.format ? '' : ` · ${track.delivery}`
  const tone = LOUD_DELIVERY[track.delivery]
  return tone ? { verdict: `${name}${how}`, tone } : { verdict: `${name}${how}` }
}
