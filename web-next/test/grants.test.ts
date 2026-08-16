/// Granting libraries. Every test here is a race or a version, because those
/// are the two ways this went wrong: two clicks in quick succession losing one
/// of them, and a spent version telling an operator somebody else had changed
/// something when nobody had.

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
function panel() {
  const shown: { all_libraries: boolean; libraries: string[] }[] = []
  const versions: number[] = []
  const refusals: string[] = []
  let rereads = 0
  const grants = useGrants({
    show: (_id, access) => shown.push(access),
    version: (_id, v) => versions.push(v),
    refused: (why) => refusals.push(why),
    reread: () => (rereads += 1),
  })
  return { grants, shown, versions, refusals, rereads: () => rereads }
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
    expect(shown[0]).toEqual({ all_libraries: false, libraries: ['films', 'music'] })
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

  test('the answer’s version goes onto the row, not just into the next write', async () => {
    // Leaving the row holding the version this write consumed made every
    // second edit send a spent one and come back `stale_write`.
    const { grants, versions } = panel()
    await grants.set(user(), false, ['films'])
    expect(versions).toEqual([4])
  })

  test('and a second edit uses it', async () => {
    const { grants } = panel()
    const who = user({ grants_version: 3 })
    await grants.set(who, false, ['films'])
    // The row has not been repainted by the caller here — which is the point:
    // the version comes from the answer, not from the row.
    await grants.set(who, false, ['films', 'music'])
    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({ grants_version: 4 })
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
    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['films', 'music'] })

    const two = grants.set(user({ libraries: ['films', 'music'] }), false, [
      'films',
      'music',
      'shows',
    ])
    first.settle()
    await Promise.all([one, two])

    expect(vi.mocked(adminSetUserLibraries).mock.calls[1]![1]).toMatchObject({
      libraries: ['films', 'music', 'shows'],
    })
  })

  test('and the second is not even sent until the first has landed', async () => {
    // Filtering stale replies does not order the writes: A could commit after
    // B and leave the hub holding A while the panel showed B. The queue is
    // what makes the hub see them in the order they were made — so the second
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
    const { grants, shown } = panel()

    const one = grants.set(user(), false, ['a'])
    const two = grants.set(user(), false, ['a', 'b'])
    first.settle()
    await one
    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['a', 'b'] })

    second.settle()
    await two
    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['a', 'b'] })
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
    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['a', 'b'] })
    expect(refusals).toEqual([])

    second.settle()
    await two
  })
})

describe('an older success under a newer failure', () => {
  test('reverts to what the hub accepted, not to what came before both', async () => {
    // The older write SUCCEEDED, so its set is what the hub holds. Reverting
    // to the value the failing write started from would undo it.
    const first = held({ all_libraries: false, libraries: ['a'], grants_version: 4 })
    const second = held({})
    vi.mocked(adminSetUserLibraries)
      .mockReturnValueOnce(first.promise as never)
      .mockReturnValueOnce(second.promise as never)
    const { grants, shown } = panel()

    const one = grants.set(user({ libraries: [] }), false, ['a'])
    const two = grants.set(user({ libraries: ['a'] }), false, ['a', 'b'])

    first.settle()
    await one
    second.refuse(new ApiError(500, 'no'))
    await two

    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['a'] })
  })
})

describe('when the hub refuses', () => {
  test('the chips go back to what it holds, and it says so', async () => {
    vi.mocked(adminSetUserLibraries).mockRejectedValue(new ApiError(403, 'not allowed'))
    const { grants, shown, refusals } = panel()
    await grants.set(user(), false, ['films', 'music'])
    expect(shown.at(-1)).toEqual({ all_libraries: false, libraries: ['films'] })
    expect(refusals.at(-1)).toContain('not allowed')
  })

  test('and a stale version reads the users again, rather than dying', async () => {
    // Nothing else would go and look: granting emits no hint, so every further
    // click sent the same spent version and was refused again — on an idle
    // hub, for ever, blaming an admin who was not there.
    const stale = new ApiError(409, 'somebody else changed this', 'stale_write')
    vi.mocked(adminSetUserLibraries).mockRejectedValue(stale)
    const { grants, rereads } = panel()
    await grants.set(user(), false, ['films'])
    expect(rereads()).toBe(1)
  })

  test('but an ordinary refusal does not', async () => {
    vi.mocked(adminSetUserLibraries).mockRejectedValue(new ApiError(403, 'not allowed'))
    const { grants, rereads } = panel()
    await grants.set(user(), false, ['films'])
    expect(rereads()).toBe(0)
  })
})

describe('a read that answers late', () => {
  test('cannot take the version backwards', async () => {
    // A refresh that started BEFORE a write and lands after it carries the
    // pre-write snapshot; taking its version back would send a spent one and
    // be told somebody else had changed it when nobody had.
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
})
