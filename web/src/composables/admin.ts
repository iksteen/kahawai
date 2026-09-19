/// What the admin panel reads, and how it reports the two kinds of failure.
///
/// They are two cells on purpose. Sharing one meant a read success cleared an
/// ACTION failure: with a scan running the hint fires every 250 ms, so a
/// refused delete was wiped before it could be read. A lost error is worse
/// than the stale one it replaced.
///
/// - **A read failing** — the poll, a refresh — is cleared by the next
///   successful read, because nothing else ever will, and one blink during a
///   hub restart otherwise pinned "cannot reach the hub" over a panel that had
///   been working again for an hour.
/// - **An action failing** stays until the operator does something else. It is
///   the answer to something they did, and they have to be able to read it.
///
/// The six reads are SEPARATE, which is the other thing this owes the panel.
/// One `Promise.all` meant any failure repainted nothing, so the sections that
/// were fine went stale in silence; six cells means one dead read leaves the
/// others live — and it means each section has to be able to ask whether ITS
/// read failed, or an empty list from a 503 renders as "Nobody is playing
/// anything", which is a statement, and a false one.

import { computed, ref, type Ref } from 'vue'
import { useQueries, useQueryClient } from '@tanstack/vue-query'

import type { CatalogueCollection } from '../api/generated/model/catalogueCollection.ts'
import type { CatalogueLibrary } from '../api/generated/model/catalogueLibrary.ts'
import type { AdminSessionsResponse } from '../api/generated/model/adminSessionsResponse.ts'
import type { EnrollmentsResponse } from '../api/generated/model/enrollmentsResponse.ts'
import type { SatellitesResponse } from '../api/generated/model/satellitesResponse.ts'
import type { UsersResponse } from '../api/generated/model/usersResponse.ts'
import {
  collections,
  adminEnrollments,
  adminSatellites,
  adminSessions,
  adminUsers,
} from '../api/generated/kahawai.ts'
import { librariesQuery } from './catalogue.ts'
import { andList, enrolled } from '../domain/admin.ts'
import { sentence } from '../domain/refusal.ts'

/// How often the panel looks again on its own. Sessions end and satellites
/// come and go without anything telling this page, and an operator watching a
/// scan should not have to press anything.
export const POLL_MS = 15_000

/// In the order the queries are declared, and named as the operator would name
/// them: these strings reach the screen.
const READS = ['enrolments', 'satellites', 'sessions', 'libraries', 'collections', 'users'] as const
export type Read = (typeof READS)[number]

export function useAdmin(showSessions: Ref<boolean>) {
  const client = useQueryClient()

  const queries = useQueries({
    queries: [
      { queryKey: ['admin', 'enrollments'], queryFn: () => adminEnrollments() },
      { queryKey: ['admin', 'satellites'], queryFn: () => adminSatellites() },
      { queryKey: ['admin', 'sessions'], queryFn: () => adminSessions(), enabled: showSessions },
      librariesQuery,
      { queryKey: ['admin', 'collections'], queryFn: () => collections() },
      { queryKey: ['admin', 'users'], queryFn: () => adminUsers() },
    ].map((q) => ({ ...q, refetchInterval: POLL_MS })),
  })

  const at = <T>(index: number): T | undefined => queries.value[index]?.data as T | undefined

  /// Which reads are currently failing. A section asks about its own, so that
  /// "nothing here" is only ever said about a list that was actually read.
  const broken = computed(() => {
    const names: Read[] = []
    for (const [index, name] of READS.entries())
      if (queries.value[index]?.isError && (name !== 'sessions' || showSessions.value))
        names.push(name)
    return names
  })

  /// The panel's own reading, in one line — and it NAMES what could not be
  /// read. Six sentences about one dead hub is six times the same news, but
  /// "could not reach the hub" over four sections of which three are fine and
  /// one is quietly empty is worse: the operator cannot tell which.
  const readError = computed(() => {
    const failed = queries.value.find(
      (q, i) => q.isError && (READS[i] !== 'sessions' || showSessions.value),
    )
    if (!failed) return ''
    const why = sentence(failed.error)
    return broken.value.length === READS.length - (showSessions.value ? 0 : 1)
      ? why
      : `Could not read ${andList(broken.value)}: ${why}`
  })

  /// Something the operator did, that did not work.
  const actionError = ref('')

  /// Read again. Everything, or one section — a grant write has no opinion
  /// about the session list, and making it wait for five other requests is
  /// what put a six-request round trip in front of clearing a form.
  async function reload(section?: 'users' | 'satellites' | 'libraries' | 'sessions') {
    if (section === 'libraries') {
      await client.invalidateQueries({ queryKey: librariesQuery.queryKey })
    } else {
      await Promise.all([
        client.invalidateQueries({ queryKey: section ? ['admin', section] : ['admin'] }),
        ...(!section || section === 'users'
          ? [client.invalidateQueries({ queryKey: librariesQuery.queryKey })]
          : []),
      ])
    }
  }

  /// Every mutation goes through here, so that a success clears the last
  /// failure. Only four call sites used to, so a failure that had since been
  /// resolved stayed above the panel and read as the outcome of the NEXT
  /// action — and the operator repeated something that had already worked.
  ///
  /// It does NOT wait for the re-read. Everything a caller does after a
  /// mutation — clear the form, disarm the confirmation, say what happened —
  /// was waiting on six requests, so on a slow hub the create form kept its
  /// values with Create still live and a second press sent it again.
  async function act(what: () => Promise<unknown>): Promise<boolean> {
    try {
      await what()
      actionError.value = ''
      void reload()
      return true
    } catch (cause) {
      actionError.value = sentence(cause)
      return false
    }
  }

  return {
    enrollments: computed(() => at<EnrollmentsResponse>(0)?.pending ?? []),
    satellites: computed(() => enrolled(at<SatellitesResponse>(1)?.satellites ?? [])),
    sessions: computed(() => at<AdminSessionsResponse>(2)?.sessions ?? []),
    libraries: computed(() => at<CatalogueLibrary[]>(3) ?? []),
    collections: computed(() => at<CatalogueCollection[]>(4) ?? []),
    users: computed(() => at<UsersResponse>(5)?.users ?? []),
    librariesReady: computed(() => queries.value[3]?.data !== undefined),
    collectionsReady: computed(() => queries.value[4]?.data !== undefined),
    /// Whether a given list is a fact or an absence of one.
    broken,
    /// Nothing has ever been read. Not "no request is in flight": a query that
    /// has failed is not pending either, so asking that made the panel's Try
    /// again unreachable — the only condition under which it rendered was one
    /// that could never hold at the same time as an error.
    loaded: computed(() => queries.value.some((q) => q.data !== undefined)),
    readError,
    actionError,
    act,
    reload,
  }
}
