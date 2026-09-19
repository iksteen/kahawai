import { useQuery } from '@tanstack/vue-query'
import type { Ref } from 'vue'
import { libraries } from '../api/generated/kahawai.ts'

export function useCatalogueLibraries(enabled: Ref<boolean>) {
  return useQuery({
    queryKey: ['catalogue', 'libraries'],
    queryFn: ({ signal }) => libraries({ signal }),
    enabled,
  })
}
