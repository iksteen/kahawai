/// Your preferences: reading them once, and writing each one back in order.
///
/// Every write is whole-state for its key, so the ORDER they commit in is the
/// value that sticks. `SerialQueue` per `scope\0key` is what guarantees it —
/// see there for what an out-of-order commit costs.

import { computed, ref } from 'vue'
import { useQuery } from '@tanstack/vue-query'

import { getPrefs, putPref as write } from '../api/generated/kahawai.ts'
import { SerialQueue } from './serial.ts'

const queue = new SerialQueue()

/// One preference, written whole. Scope is '' for the account's own settings
/// and an item id for a title's own memory.
export function putPref(scope: string, key: string, value: string): Promise<unknown> {
  return queue.run(`${scope}\0${key}`, () => write({ scope, key, value }))
}

export function usePrefs() {
  const query = useQuery({ queryKey: ['prefs'], queryFn: () => getPrefs() })

  /// The account's own settings, by key. A scoped pref belongs to one title
  /// and is not one of these.
  const values = ref<Record<string, string>>({})
  /// Rebuilt whenever the server answers, and edited in place by the controls
  /// — which is what makes an optimistic write possible at all: the screen has
  /// somewhere to hold a value the server has not confirmed yet.
  const known = computed(() => {
    const out: Record<string, string> = {}
    for (const pref of query.data.value?.prefs ?? []) {
      if (pref.scope === '') out[pref.key] = pref.value
    }
    return out
  })

  return { query, values, known }
}
