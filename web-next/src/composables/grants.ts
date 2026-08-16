/// Granting libraries to a user: a whole-SET write, optimistic, versioned.
///
/// Every one of those words is a bug that was fixed here.
///
/// **Whole-set**, so the chips a click computes from have to be up to date or
/// the click undoes the last one. They were not — the state only moved when
/// the round trip finished, so granting two libraries in quick succession sent
/// the same pre-click set twice and the second write lost the first. Both
/// reported success. Measured: two grants in, one granted.
///
/// **Optimistic**, so the chips move now and the commits wait. Filtering stale
/// replies does not order the writes: request A can commit after B and leave
/// the hub holding A while the panel shows B.
///
/// **Versioned** (UI-25), so two admins editing the same user cannot silently
/// overwrite each other. The version the next write must carry comes from the
/// last ANSWER, never from the row — the queue means a second click goes out
/// before any repaint, and the row still holds the version the first one
/// consumed.

import { ref } from 'vue'

import { adminSetUserLibraries } from '../api/generated/kahawai.ts'
import { SerialQueue } from './serial.ts'

export type Access = { all_libraries: boolean; libraries: string[] }

type Write = {
  /// Which write is newest.
  seq: number
  /// How many are out.
  inflight: number
  /// Which answer the revert target came from, and what it is.
  savedSeq: number
  saved: Access | null
  /// The `grants_version` the next write must carry.
  version: number
}

export function useGrants(options: {
  /// Paint an access set onto the row.
  show: (userId: string, access: Access) => void
  /// The version the hub has now, onto the row — a different question from
  /// the chips, and it must not paint them.
  version: (userId: string, version: number) => void
  /// Something the operator did, that did not work.
  refused: (why: string) => void
  /// Read the users again. Only for the one refusal that leaves this panel
  /// holding a version the hub has moved past.
  reread: () => void
}) {
  const queue = new SerialQueue()
  /// Per user, because two users' grants are two independent writes and a
  /// shared counter would have them cancel each other.
  const writes = new Map<string, Write>()
  const busy = ref(new Set<string>())

  function setBusy(id: string, out: boolean) {
    const next = new Set(busy.value)
    if (out) next.add(id)
    else next.delete(id)
    busy.value = next
  }

  return {
    busy,
    async set(
      user: { id: string; all_libraries: boolean; libraries: string[]; grants_version: number },
      all: boolean,
      libraries: string[],
    ): Promise<void> {
      const write = writes.get(user.id) ?? {
        seq: 0,
        inflight: 0,
        savedSeq: 0,
        saved: null,
        version: user.grants_version,
      }
      writes.set(user.id, write)

      if (write.inflight === 0) {
        // Nothing outstanding means the chips on screen are what the hub has.
        write.saved = { all_libraries: user.all_libraries, libraries: user.libraries }
        // And the row is the hub's answer — but only if that answer is not
        // OLDER than what is already known. A read that started before a write
        // and lands after it carries the pre-write snapshot, and taking its
        // version back would send a spent one and be told "somebody else
        // changed this" when nobody had. Versions only climb.
        write.version = Math.max(write.version, user.grants_version)
      }

      const mine = ++write.seq
      write.inflight++
      setBusy(user.id, true)
      options.show(user.id, { all_libraries: all, libraries })

      try {
        const answer = await queue.run(user.id, () =>
          adminSetUserLibraries(user.id, {
            all_libraries: all,
            libraries,
            grants_version: write.version,
          }),
        )
        // Whatever else this answer is, it moved the version on — and the
        // queue means the next write is not sent until this lands. Onto the
        // ROW as well: leaving the row holding the version this write just
        // consumed made every second edit send a spent one and come back
        // `stale_write`, telling an operator somebody else had changed it when
        // nobody had.
        write.version = answer.grants_version
        options.version(user.id, answer.grants_version)

        // The revert target moves on ANY success, newest-first: an older write
        // succeeding while a newer one is out still tells us something the hub
        // has accepted, and reverting past it would undo it.
        if (mine > write.savedSeq) {
          write.savedSeq = mine
          write.saved = { all_libraries: answer.all_libraries, libraries: answer.libraries }
        }

        // The CHIPS only ever come from the newest write. Grant A, grant B, A
        // answers first: painting A drops the chips back while B is still out,
        // and the next click there sends [A, C] and revokes B for real.
        if (mine !== write.seq) return
        options.refused('')
        if (write.saved) options.show(user.id, write.saved)
      } catch (cause) {
        // An older write failing says nothing about where the newest one is
        // going, and reverting to what was on screen two clicks ago would undo
        // a grant the operator has since made.
        if (mine === write.seq) {
          if (write.saved) options.show(user.id, write.saved)
          options.refused(String(cause))
        }
        // A refused write leaves this panel holding a version the hub has
        // moved past, and nothing else here would go and look: granting
        // libraries emits no hint. Every further click would send the same
        // spent version and be refused again — on an idle hub, for ever, with
        // a message blaming an admin who was not there. Reading once is what
        // the message asks the operator to do; doing it for them is the whole
        // difference between a refusal and a dead panel.
        if ((cause as { code?: string }).code === 'stale_write') options.reread()
      } finally {
        write.inflight--
        if (write.inflight === 0) setBusy(user.id, false)
      }
    },
  }
}
