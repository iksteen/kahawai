/// Granting libraries. Every test here is a race, a version, or the release of
/// the optimistic paint — the three ways this went wrong: two clicks in quick
/// succession losing one of them, a spent version telling an operator somebody
/// else had changed something when nobody had, and an override that was never
/// taken off, so the hub could never repaint that row again.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({ adminSetUserLibraries: vi.fn() }))

const { adminSetUserLibraries } = await import('../src/api/generated/kahawai.ts')
const { useGrants } = await import('../src/composables/grants.ts')

const user = (over: Record<string, unknown> = {}) => ({
  id: 'u1',
  all_libraries: false,
  libraries: ['films'],
  grants_version: 3,
  ...over,
})

/// The panel, as the composable sees it.
///
/// `atReread` is the probe: the re-read runs after the last write has settled
/// and BEFORE the override is released, so what it captures is the paint the
/// write finished on. Reading the overlay after `set` resolves would only ever
/// see the row again, which is the whole point of the release.
function panel(rows: () => ReturnType<typeof user> = user) {
  const refusals: string[] = []
  const atReread: { all_libraries: boolean; libraries: string[] }[] = []
  let rereads = 0
  const grants = useGrants({
    refused: (why) => refusals.push(why),
    reread: async () => {
      rereads += 1
      atReread.push(shown())
    },
  })
  const shown = () => {
    const { all_libraries, libraries } = grants.asShown(rows())
    return { all_libraries, libraries }
  }
  return { grants, refusals, atReread, rereads: () => rereads, shown }
}

/// An answer somebody else decides when to give.
function held(answer: Record<string, unknown>) {
  let settle!: () => void
  let refuse!: (why: unknown) => void
  const promise = new Promise((resolve, reject) => {
    settle = () => resolve(answer)
    refuse = reject
  })
  return { promise, settle, refuse }
}

beforeEach(() => {
  vi.mocked(adminSetUserLibraries).mockResolvedValue({
    all_libraries: false,
    libraries: ['films'],
    grants_version: 4,
  } as never)
})
afterEach(() => vi.resetAllMocks())

describe('one grant', () => {
  test('shows the new set before the hub has answered', async () => {
    const { grants, shown } = panel()
    const saving = grants.set(user(), false, ['films', 'music'])
    expect(shown()).toEqual({ all_libraries: false, libraries: ['films', 'music'] })
    await saving
  })

  test('and carries the version the hub gave last', async () => {
    const { grants } = panel()
    await grants.set(user({ grants_version: 3 }), false, ['films'])
    expect(adminSetUserLibraries).toHaveBeenCalledWith('u1', {
      all_libraries: false,
      libraries: ['films'],
      grants_version: 3,
    })
  })

  test('a second edit uses the version from the ANSWER, not from the row', async () => {
    // Leaving the row holding the version this write consumed made every second
    // edit send a spent one and come back `stale_write`, telling an operator
    // somebody else had changed it when nobody had.
    const { grants } = panel()
    const who = user({ grants_version: 3 })
    await grants.set(who, false, ['films'])
    // The row is deliberately NOT repainted between the two.
    await grants.set(who, false, ['films', 'music'])
    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({ grants_version: 4 })
  })
})

describe('once it has settled', () => {
  test('the override is released, so the hub can repaint the row', async () => {
    // It never was, and the row was frozen from the first click until a reload:
    // another admin narrowing the account changed nothing on screen, and the
    // panel showed a grant the hub did not have.
    const { grants } = panel()
    await grants.set(user(), false, ['films', 'music'])
    const narrowed = user({ libraries: ['shows'], grants_version: 4 })
    expect(grants.asShown(narrowed).libraries).toEqual(['shows'])
  })

  test('and the read happens BEFORE the release', async () => {
    // The row underneath is whatever the last poll said, which predates the
    // write. Dropping the override first flicks the chips back to the old set
    // for the rest of the polling interval.
    const { grants, atReread } = panel()
    await grants.set(user(), false, ['films', 'music'])
    expect(atReread).toEqual([{ all_libraries: false, libraries: ['films'] }])
  })

  test('a click during the re-read keeps its own paint', async () => {
    // The re-read is awaited, so a second click lands inside it. Releasing then
    // would drop the override the second click had just painted.
    let second: Promise<void> | null = null
    const grants = useGrants({
      refused: () => {},
      reread: async () => {
        second ??= grants.set(user(), false, ['films', 'music', 'shows'])
      },
    })
    await grants.set(user(), false, ['films', 'music'])
    expect(grants.asShown(user()).libraries).toEqual(['films', 'music', 'shows'])
    await second
  })
})

describe('two grants in quick succession', () => {
  test('the second is computed from the first, not from the pre-click set', async () => {
    // Measured before this existed: two grants in, one granted — and both
    // reported success.
    const first = held({ all_libraries: false, libraries: ['films'], grants_version: 4 })
    vi.mocked(adminSetUserLibraries).mockReturnValueOnce(first.promise as never)
    const { grants, shown } = panel()

    const one = grants.set(user(), false, ['films', 'music'])
    // The chips already show both, so the next click computes from both.
    expect(shown()).toEqual({ all_libraries: false, libraries: ['films', 'music'] })

    const two = grants.set(grants.asShown(user()), false, ['films', 'music', 'shows'])
    first.settle()
    await Promise.all([one, two])

    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({
      libraries: ['films', 'music', 'shows'],
    })
  })

  test('and the second is not even sent until the first has landed', async () => {
    // Filtering stale replies does not order the writes: A could commit after B
    // and leave the hub holding A while the panel showed B. The queue is what
    // makes the hub see them in the order they were made — so the second
    // request must not exist while the first is out.
    const first = held({ all_libraries: false, libraries: ['a'], grants_version: 4 })
    vi.mocked(adminSetUserLibraries).mockReturnValueOnce(first.promise as never)
    const { grants } = panel()

    const one = grants.set(user(), false, ['a'])
    const two = grants.set(user(), false, ['a', 'b'])
    await Promise.resolve()
    expect(adminSetUserLibraries).toHaveBeenCalledTimes(1)

    first.settle()
    await Promise.all([one, two])
    expect(
      vi
        .mocked(adminSetUserLibraries)
        .mock.calls.map((c) => (c[1] as { libraries: string[] }).libraries.join(',')),
    ).toEqual(['a', 'a,b'])
  })

  test('an older answer does not paint the chips back', async () => {
    // Grant A, grant B, A answers first: painting A drops the chips back while
    // B is still out, and the next click there revokes B for real.
    const first = held({ all_libraries: false, libraries: ['a'], grants_version: 4 })
    const second = held({ all_libraries: false, libraries: ['a', 'b'], grants_version: 5 })
    vi.mocked(adminSetUserLibraries)
      .mockReturnValueOnce(first.promise as never)
      .mockReturnValueOnce(second.promise as never)
    const { grants, shown, atReread } = panel()

    const one = grants.set(user(), false, ['a'])
    const two = grants.set(user(), false, ['a', 'b'])
    first.settle()
    await one
    // Still out, so nothing has been released and the chips are readable.
    expect(shown()).toEqual({ all_libraries: false, libraries: ['a', 'b'] })

    second.settle()
    await two
    expect(atReread.at(-1)).toEqual({ all_libraries: false, libraries: ['a', 'b'] })
  })

  test('and an older failure does not revert them', async () => {
    const first = held({})
    const second = held({ all_libraries: false, libraries: ['a', 'b'], grants_version: 5 })
    vi.mocked(adminSetUserLibraries)
      .mockReturnValueOnce(first.promise as never)
      .mockReturnValueOnce(second.promise as never)
    const { grants, shown, refusals } = panel()

    const one = grants.set(user(), false, ['a'])
    const two = grants.set(user(), false, ['a', 'b'])
    first.refuse(new ApiError(500, 'no'))
    await one
    expect(shown()).toEqual({ all_libraries: false, libraries: ['a', 'b'] })
    expect(refusals).toEqual([])

    second.settle()
    await two
  })
})

describe('an older success under a newer failure', () => {
  test('reverts to what the hub accepted, not to what came before both', async () => {
    // The older write SUCCEEDED, so its set is what the hub holds. Reverting to
    // the value the failing write started from would undo it.
    const first = held({ all_libraries: false, libraries: ['a'], grants_version: 4 })
    const second = held({})
    vi.mocked(adminSetUserLibraries)
      .mockReturnValueOnce(first.promise as never)
      .mockReturnValueOnce(second.promise as never)
    const { grants, atReread } = panel()

    const one = grants.set(user({ libraries: [] }), false, ['a'])
    const two = grants.set(user({ libraries: ['a'] }), false, ['a', 'b'])

    first.settle()
    await one
    second.refuse(new ApiError(500, 'no'))
    await two

    expect(atReread.at(-1)).toEqual({ all_libraries: false, libraries: ['a'] })
  })
})

describe('when the hub refuses', () => {
  test('the chips go back to what it holds, and it says so', async () => {
    vi.mocked(adminSetUserLibraries).mockRejectedValue(new ApiError(403, 'not allowed'))
    const { grants, atReread, refusals } = panel()
    await grants.set(user(), false, ['films', 'music'])
    expect(atReread.at(-1)).toEqual({ all_libraries: false, libraries: ['films'] })
    expect(refusals.at(-1)).toContain('not allowed')
  })

  test('a refusal is put in words, not stringified', async () => {
    // `sentence` is what keeps a thrown non-Error from reaching the operator as
    // "[object Object]". This was the one write path that bypassed it.
    vi.mocked(adminSetUserLibraries).mockRejectedValue({ code: 'forbidden' })
    const { grants, refusals } = panel()
    await grants.set(user(), false, ['films'])
    expect(refusals.at(-1)).not.toContain('[object Object]')
    expect(refusals.at(-1)).toBeTruthy()
  })

  test('a spent version does not dead-end the row', async () => {
    // Nothing else would go and look: granting emits no hint, so every further
    // click sent the same spent version and was refused again — on an idle hub,
    // for ever, blaming an admin who was not there. The re-read after a settled
    // write is what breaks that, and the next click takes the fresher version.
    let row = user({ grants_version: 4 })
    const grants = useGrants({
      refused: () => {},
      reread: async () => {
        row = user({ grants_version: 9 })
      },
    })
    vi.mocked(adminSetUserLibraries).mockRejectedValueOnce(
      new ApiError(409, 'somebody else changed this', 'stale_write'),
    )
    await grants.set(row, false, ['films'])

    await grants.set(row, false, ['films', 'music'])
    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({ grants_version: 9 })
  })
})

describe('a read that answers late', () => {
  test('cannot take the version backwards', async () => {
    // A refresh that started BEFORE a write and lands after it carries the
    // pre-write snapshot; taking its version back would send a spent one and be
    // told somebody else had changed it when nobody had.
    const { grants } = panel()
    await grants.set(user({ grants_version: 3 }), false, ['films'])

    // The row, repainted by a read that predates the write.
    await grants.set(user({ grants_version: 2 }), false, ['music'])
    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({ grants_version: 4 })
  })
})

describe('two users', () => {
  test('do not wait for, or revert, each other', async () => {
    const slow = held({ all_libraries: false, libraries: ['a'], grants_version: 4 })
    vi.mocked(adminSetUserLibraries).mockReturnValueOnce(slow.promise as never)
    const order: string[] = []
    vi.mocked(adminSetUserLibraries).mockImplementation(async (id) => {
      order.push(id)
      return { all_libraries: false, libraries: [], grants_version: 4 } as never
    })
    const { grants } = panel()

    const first = grants.set(user({ id: 'u1' }), false, ['a'])
    await grants.set(user({ id: 'u2' }), false, ['b'])
    expect(order).toContain('u2')

    slow.settle()
    await first
  })

  test('and one user’s override does not paint another’s row', async () => {
    const { grants } = panel()
    const saving = grants.set(user({ id: 'u1' }), false, ['films', 'music'])
    expect(grants.asShown(user({ id: 'u2' })).libraries).toEqual(['films'])
    await saving
  })
})
