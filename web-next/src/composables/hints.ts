/// HUB-11: the hub's invalidation hints, in one place.
///
/// The hub pushes `{kind, ...}` over server-sent events whenever something a
/// client might be showing has changed. They are HINTS, not state: nothing here
/// reads a payload, it decides which reads are now stale and asks for them
/// again. Polling stays as a safety net, at a interval nobody watching a scan
/// should have to wait out.
///
/// `EventSource` cannot set headers, so it authenticates with the
/// `kahawai_media` cookie like the other browser media resources.

import { onScopeDispose } from 'vue'
import { useQueryClient } from '@tanstack/vue-query'

import { getEventsUrl } from '../api/generated/kahawai.ts'

/// Hints arrive in BURSTS — the hub emits one every five hundred files during
/// a scan — so a reload per hint is a request per five hundred files, times
/// however many reads the screen has.
export const DEBOUNCE_MS = 250

/// What a scan can change, and what it cannot.
///
/// Filtered on the KIND because it was not: every scan hint re-read the users
/// and the provider credentials as well, eight requests a burst for the whole
/// of a scan, none of which a scan touches.
const SCAN_TOUCHES = ['libraries', 'collections']

export function useHints(sections: {
  /// Re-read for any hint, including a scan's.
  always: string[]
  /// Re-read only for a hint that is not a scan.
  quiet?: string[]
}) {
  const client = useQueryClient()
  // `EventSource` is absent in a test environment with no network, and this is
  // an optimisation over polling rather than the only way anything updates.
  if (typeof EventSource === 'undefined') return

  let debounce: ReturnType<typeof setTimeout> | undefined
  const source = new EventSource(getEventsUrl())
  source.onmessage = (message) => {
    let kind = ''
    try {
      kind = (JSON.parse(message.data as string) as { kind?: string }).kind ?? ''
    } catch {
      return // malformed hint: ignore
    }
    const stale =
      kind === 'scan'
        ? sections.always.filter((s) => SCAN_TOUCHES.includes(s))
        : [...sections.always, ...(sections.quiet ?? [])]
    if (!stale.length) return
    clearTimeout(debounce)
    debounce = setTimeout(() => {
      for (const section of stale) void client.invalidateQueries({ queryKey: ['admin', section] })
    }, DEBOUNCE_MS)
  }

  onScopeDispose(() => {
    clearTimeout(debounce)
    source.close()
  })
}
