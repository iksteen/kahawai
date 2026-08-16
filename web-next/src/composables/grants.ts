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
///
/// The optimistic paint lives HERE, and that is the fourth bug. It was the
/// view's: two callbacks wrote an override into a record the view owned, and
/// nothing ever took it out. From the first click on a row until a reload,
/// that account's grants could never be repainted by the hub again — another
/// admin narrowing it changed nothing on screen — and the version override
/// starved the `stale_write` re-read of the fresher version that is the whole
/// point of it, so every later click on that row was refused for ever. The
/// state machine that knows when a write is outstanding is the only thing that
/// can know when the override should go, so it owns it.

import { ref } from 'vue'

import { adminSetUserLibraries } from '../api/generated/kahawai.ts'
import { sentence } from '../domain/refusal.ts'
import { SerialQueue } from './serial.ts'

export type Access = { all_libraries: boolean; libraries: string[] }

export type GrantRow = {
  id: string
  all_libraries: boolean
  libraries: string[]
  grants_version: number
}

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
  /// Something the operator did, that did not work.
  refused: (why: string) => void
  /// Read the users again, and resolve once the answer has landed. Called
  /// after the last write on a row settles — the row underneath the optimistic
  /// chips is whatever the last poll said, which predates the write, so
  /// dropping the override before this lands flicks the chips back to the old
  /// set for the rest of the polling interval.
  reread: () => Promise<void>
}) {
  const queue = new SerialQueue()
  /// Per user, because two users' grants are two independent writes and a
  /// shared counter would have them cancel each other.
  const writes = new Map<string, Write>()
  /// What to paint instead of the row, while a write is outstanding.
  const overlay = ref<Record<string, Access>>({})

  function paint(id: string, access: Access) {
    overlay.value = { ...overlay.value, [id]: access }
  }

  function unpaint(id: string) {
    const next = { ...overlay.value }
    delete next[id]
    overlay.value = next
  }

  return {
    /// A row as the panel is showing it: the hub's answer, with anything an
    /// outstanding write has put on top.
    asShown<T extends GrantRow>(user: T): T {
      const optimistic = overlay.value[user.id]
      return optimistic ? { ...user, ...optimistic } : user
    },

    async set(user: GrantRow, all: boolean, libraries: string[]): Promise<void> {
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
      paint(user.id, { all_libraries: all, libraries })

      try {
        const answer = await queue.run(user.id, () =>
          adminSetUserLibraries(user.id, {
            all_libraries: all,
            libraries,
            grants_version: write.version,
          }),
        )
        // Whatever else this answer is, it moved the version on — and the
        // queue means the next write is not sent until this lands. Leaving the
        // spent version behind made every second edit come back `stale_write`,
        // telling an operator somebody else had changed it when nobody had.
        write.version = answer.grants_version

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
        if (write.saved) paint(user.id, write.saved)
      } catch (cause) {
        // An older write failing says nothing about where the newest one is
        // going, and reverting to what was on screen two clicks ago would undo
        // a grant the operator has since made.
        if (mine === write.seq) {
          if (write.saved) paint(user.id, write.saved)
          options.refused(sentence(cause))
        }
      } finally {
        write.inflight--
        if (write.inflight === 0) {
          // Always, not only after `stale_write`. A refused write leaves this
          // panel holding a version the hub has moved past, and nothing else
          // here would go and look: granting libraries emits no hint. Every
          // further click would send the same spent version and be refused
          // again — on an idle hub, for ever, with a message blaming an admin
          // who was not there. Reading is also what RELEASES the override
          // below, so the two are the same act.
          // Swallowed, deliberately. This runs in a `finally`, and a
          // rejection here would skip the release below and leave the row
          // frozen for ever — which is the very incident this override was
          // rewritten to prevent, restored through the error path. It would
          // also reject `set`, whose two callers do not await it.
          await options.reread().catch(() => {})
          // Asked again: a click DURING the re-read starts another write, and
          // dropping the override then would flick that one's chips back.
          if (write.inflight === 0) unpaint(user.id)
        }
      }
    },
  }
}
