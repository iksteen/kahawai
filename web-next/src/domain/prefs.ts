/// The settings' own rules: what a language token may be, how a wishlist is
/// spelled, and what the subtitle ladder means.
///
/// All of it is about a stored string that the hub parses its own way — so
/// these mirror that parse rather than inventing one, and every list is
/// written back in the same shape it was read in.

/// A language token, or the backstop. Two- and three-letter codes, because
/// both are in use and the hub accepts either.
const TOKEN = /^(original|[a-z]{2,3})$/

export function validToken(token: string): boolean {
  return TOKEN.test(token.trim().toLowerCase())
}

/// The permanent audio backstop: always in the list, never removable —
/// reorderable, because another language may be preferred above it.
export const ORIGINAL = 'original'

/// A stored wishlist, as a list. Audio always ends up with the backstop in it,
/// wherever the viewer has put it.
export function wishlist(stored: string, kind: 'audio' | 'subs'): string[] {
  const items = stored
    ? stored
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean)
    : []
  if (kind === 'audio' && !items.includes(ORIGINAL)) return [...items, ORIGINAL]
  return items
}

/// And back into the one string the hub stores.
export function stored(items: string[]): string {
  return items.join(',')
}

/// HUB-32: what to do with a styled subtitle track this client cannot render
/// faithfully, in the order to try.
export const ASS_RUNGS = {
  flatten: {
    name: 'plain text',
    note: 'fonts, colours and positions dropped. Works anywhere, costs nothing.',
  },
  overlay: {
    name: 'drawn on top',
    note: 'the server draws the styling and sends it as its own layer. Looks right, and the picture is untouched.',
  },
  burn: {
    name: 'burnt into the picture',
    note: 'exactly as the author made it, but the video has to be re-encoded to carry it.',
  },
} as const

export type AssRung = keyof typeof ASS_RUNGS

/// A permutation, always: whatever the stored value leaves out is appended,
/// mirroring the server's own parse. A ladder missing a rung would offer a
/// preference the hub does not have.
export function assLadder(stored: string): AssRung[] {
  const all = Object.keys(ASS_RUNGS) as AssRung[]
  const parsed = stored
    .split(',')
    .map((s) => s.trim())
    .filter((s): s is AssRung => (all as string[]).includes(s))
  return [...new Set([...parsed, ...all])]
}

/// The bandwidth box. Empty means no ceiling, which is a real answer and not a
/// missing one — so it is stored as an empty string rather than as a zero.
export function bandwidthValue(typed: string): string | null {
  const trimmed = typed.trim()
  if (trimmed === '') return ''
  const kbps = Number(trimmed)
  if (!Number.isFinite(kbps) || kbps <= 0 || !Number.isInteger(kbps)) return null
  return String(kbps)
}
