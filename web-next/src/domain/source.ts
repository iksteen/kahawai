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

/// One aggregate name for the work a playback plan performs.
///
/// Keyed on COST, never on `mode`. On this endpoint `mode` is only ever
/// `direct` or `remux` — it says whether bytes are served as they are, not
/// whether anything is re-encoded — so a `remux` carrying `video_encode`
/// re-encodes the picture, and a chip reading REMUX over it would say the
/// opposite of what the rows underneath say.
///
/// No gloss goes with most of them: the panel lists every stream's verdict
/// directly underneath, so a sentence beside the chip said the same thing
/// twice and worse, because one chip cannot describe two streams.
/// `unplayable` keeps its note — it is the one verdict that describes no work
/// at all but a refusal, and there is no stream row to read it off.
const DELIVERY = {
  direct: { chip: 'DIRECT', tone: 'teal', note: '' },
  copy: { chip: 'REMUX', tone: 'teal', note: '' },
  audio_encode: {
    chip: 'TRANSCODE',
    tone: 'sand',
    note: 'the audio is re-encoded; the picture is copied',
  },
  video_encode: { chip: 'TRANSCODE', tone: 'sand', note: '' },
  unplayable: {
    chip: 'UNPLAYABLE',
    tone: 'warn',
    note: 'nothing here can be delivered to this browser',
  },
} as const

export function deliveryPlan(cost: string): { chip: string; tone: string; note: string } {
  // A cost this client has never heard of is still shown, in its own words and
  // in the tone that says "look at this" — the hub may know something this
  // build does not.
  return (
    DELIVERY[cost as keyof typeof DELIVERY] ?? { chip: cost.toUpperCase(), tone: 'warn', note: '' }
  )
}

/// UI-27: one row per FILE, grouped into the works they are parts of.
///
/// The flat list made one film split across seven numbered parts
/// indistinguishable from seven alternative encodes — both are "7 sources" in
/// an order that means nothing to a reader. `source_id` is what tells them
/// apart: rows sharing one are parts of a single work, in `part` order; rows
/// with different ones are alternatives to choose between.
///
/// Opaque and stable only within one response — it exists to be grouped on,
/// never stored.
export function groupSources<T extends { source_id: number; part: number; parts: number }>(
  sources: T[],
): { id: number; parts: T[]; whole: boolean }[] {
  const byWork = new Map<number, T[]>()
  for (const source of sources) {
    const held = byWork.get(source.source_id)
    if (held) held.push(source)
    else byWork.set(source.source_id, [source])
  }
  return [...byWork.entries()].map(([id, parts]) => ({
    id,
    parts: [...parts].sort((a, b) => a.part - b.part),
    // A work missing a part cannot play to the end, and saying "7 sources"
    // over it hides that completely.
    whole: parts.length === (parts[0]?.parts ?? 0),
  }))
}
