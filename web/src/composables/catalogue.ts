import { queryOptions, useQuery } from '@tanstack/vue-query'
import type { Ref } from 'vue'
import { libraries } from '../api/generated/kahawai.ts'

export const librariesQuery = queryOptions({
  queryKey: ['libraries'],
  queryFn: ({ signal }) => libraries({ signal }),
  // Navigation reuses the list; mutations invalidate it and admin polling
  // refreshes it. A network reconnect also picks up changes made elsewhere.
  staleTime: Infinity,
  refetchOnReconnect: 'always',
})

export function useLibraries(enabled?: Ref<boolean>) {
  return useQuery({ ...librariesQuery, ...(enabled ? { enabled } : {}) })
}
