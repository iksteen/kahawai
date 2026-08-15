/// Starting up. The distinction this file exists for: a hub that did not
/// answer is not a viewer who is not signed in.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { effectScope } from 'vue'

import { ApiError, Offline } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({ bootstrap: vi.fn() }))
vi.mock('../src/api/session.ts', () => ({
  restoreSession: vi.fn(),
  // Returns its disposer, as the real one does — the composable hands that
  // straight to `onScopeDispose`.
  onTokensCleared: vi.fn(() => vi.fn()),
}))

const { bootstrap } = await import('../src/api/generated/kahawai.ts')
const { onTokensCleared, restoreSession } = await import('../src/api/session.ts')
const { useBoot } = await import('../src/composables/boot.ts')

/// In a scope, because the composable registers a session callback and drops
/// it on dispose — and a test that leaked one would have the next test's
/// session events delivered to a dead ref.
function booted() {
  const scope = effectScope()
  const boot = scope.run(() => useBoot())!
  return { ...boot, stop: () => scope.stop() }
}

const ANSWERS = { setup_required: false, setup_available: false, setup_url: null }

beforeEach(() => {
  vi.resetAllMocks()
  vi.mocked(onTokensCleared).mockReturnValue(vi.fn())
})
afterEach(() => vi.restoreAllMocks())

describe('what the hub says to open', () => {
  test('an unreachable hub is not a sign-in screen', async () => {
    // Conflating the two sent a signed-in viewer to a password box over one
    // blip, with their tokens still perfectly good — and there is nothing to
    // sign in TO while the hub is unreachable, so the one thing that screen
    // offers cannot work either.
    vi.mocked(bootstrap).mockRejectedValue(new Offline())
    const boot = booted()
    await boot.start()

    expect(boot.phase.value).not.toBe('login')
    expect(boot.bootError.value).toBe('Could not reach the hub.')
    boot.stop()
  })

  test('and the failure stays on screen while the retry is out', async () => {
    // Clearing it on retry put the phase back to `boot`, which renders
    // nothing: pressing Try again against a wedged hub gave a blank page for
    // the full ten seconds before the message came back.
    vi.mocked(bootstrap).mockRejectedValue(new Offline())
    const boot = booted()
    await boot.start()

    let release = () => {}
    vi.mocked(bootstrap).mockReturnValue(
      new Promise((resolve) => (release = () => resolve(ANSWERS))),
    )
    const again = boot.start()
    expect(boot.bootError.value).not.toBe('')
    expect(boot.phase.value).toBe('boot')

    vi.mocked(restoreSession).mockResolvedValue('anonymous')
    release()
    await again
    expect(boot.bootError.value).toBe('')
    boot.stop()
  })

  test('the bootstrap request has a deadline of its own', async () => {
    // The one request that must not hang: the page is blank until it lands,
    // so without this it is a permanently blank page with no header, no
    // message and nothing to press.
    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('anonymous')
    const boot = booted()
    await boot.start()
    expect(bootstrap).toHaveBeenCalledWith({ signal: expect.any(AbortSignal) })
    expect(vi.mocked(bootstrap).mock.calls[0]![0]!.signal!.aborted).toBe(false)
    boot.stop()
  })

  test('a boot that was given up on cannot come back and undo a later one', async () => {
    // Try again against a wedged hub starts another boot without stopping the
    // first. The first one failing later put "Could not start." over an app
    // somebody had already signed into — `bootError` outranks every phase.
    let failFirst = () => {}
    vi.mocked(bootstrap).mockReturnValueOnce(
      new Promise((_resolve, reject) => (failFirst = () => reject(new Offline()))),
    )
    const boot = booted()
    const abandoned = boot.start()

    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('authenticated')
    await boot.start()
    expect(boot.phase.value).toBe('app')

    failFirst()
    await abandoned
    expect(boot.bootError.value).toBe('')
    expect(boot.phase.value).toBe('app')
    boot.stop()
  })

  test('and an abandoned boot that SUCCEEDS does not move the screen either', async () => {
    // The same race the other way round: a slow first answer landing after a
    // retry has already decided would send a signed-in viewer to the sign-in
    // screen.
    let answerFirst = () => {}
    vi.mocked(bootstrap).mockReturnValueOnce(
      new Promise((resolve) => (answerFirst = () => resolve(ANSWERS))),
    )
    vi.mocked(restoreSession).mockResolvedValue('anonymous')
    const boot = booted()
    const abandoned = boot.start()

    vi.mocked(bootstrap).mockResolvedValue({
      setup_required: true,
      setup_available: true,
      setup_url: 'http://127.0.0.1:8498',
    })
    await boot.start()
    expect(boot.phase.value).toBe('setup')

    answerFirst()
    await abandoned
    expect(boot.phase.value).toBe('setup')
    // Nor may it overwrite what the newer one learned. The stale answer says
    // setup is neither required nor available, and applying half of it leaves
    // a setup screen that says to go somewhere else.
    expect(boot.setupAvailable.value).toBe(true)
    expect(boot.setupUrl.value).toBe('http://127.0.0.1:8498')
    boot.stop()
  })

  test('and an abandoned boot that succeeds cannot rub out a newer failure', async () => {
    // Try again, then a THIRD press: the first answer landing last would
    // clear an error the newest attempt had just put up, leaving a blank
    // screen with nothing to press.
    let answerFirst = () => {}
    vi.mocked(bootstrap).mockReturnValueOnce(
      new Promise((resolve) => (answerFirst = () => resolve(ANSWERS))),
    )
    vi.mocked(restoreSession).mockResolvedValue('anonymous')
    const boot = booted()
    const abandoned = boot.start()

    vi.mocked(bootstrap).mockRejectedValue(new Offline())
    await boot.start()
    expect(boot.bootError.value).not.toBe('')

    answerFirst()
    await abandoned
    expect(boot.bootError.value).not.toBe('')
    boot.stop()
  })

  test('a hub with no administrator opens on setup', async () => {
    vi.mocked(bootstrap).mockResolvedValue({
      setup_required: true,
      setup_available: true,
      setup_url: 'http://127.0.0.1:8498',
    })
    const boot = booted()
    await boot.start()

    expect(boot.phase.value).toBe('setup')
    expect(boot.setupAvailable.value).toBe(true)
    expect(boot.setupUrl.value).toBe('http://127.0.0.1:8498')
    // Never asked: there is no session to restore before there is an account.
    expect(restoreSession).not.toHaveBeenCalled()
    boot.stop()
  })

  test('a restored session goes straight to the app', async () => {
    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('authenticated')
    const boot = booted()
    await boot.start()
    expect(boot.phase.value).toBe('app')
    boot.stop()
  })

  test('and no session is the sign-in screen, with nothing to explain', async () => {
    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('anonymous')
    const boot = booted()
    await boot.start()
    expect(boot.phase.value).toBe('login')
    // Arriving signed out is not the same as being thrown out.
    expect(boot.note.value).toBe('')
    boot.stop()
  })

  test("a refusal is reported in the hub's own words", async () => {
    // A 500 from the hub, or an HTML body from a proxy in front of it, both
    // mean the hub answered — hardcoding "could not reach" would have lied.
    vi.mocked(bootstrap).mockRejectedValue(new ApiError(502, '502 Bad Gateway'))
    const boot = booted()
    await boot.start()
    expect(boot.bootError.value).toBe('502 Bad Gateway')
    boot.stop()
  })
})

describe('a session that ends while the app is open', () => {
  test('drops to the sign-in screen and says why', async () => {
    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('authenticated')
    const boot = booted()
    await boot.start()

    // The callback the composable registered, invoked as the session would.
    const ended = vi.mocked(onTokensCleared).mock.calls[0]![0]!
    ended(false)
    expect(boot.phase.value).toBe('login')
    expect(boot.note.value).not.toBe('')
    boot.stop()
  })

  test('signing out deliberately needs no explanation', async () => {
    vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
    vi.mocked(restoreSession).mockResolvedValue('authenticated')
    const boot = booted()
    await boot.start()

    const ended = vi.mocked(onTokensCleared).mock.calls[0]![0]!
    ended(true)
    expect(boot.phase.value).toBe('login')
    expect(boot.note.value).toBe('')
    boot.stop()
  })

  // Two tabs, one sign-out. A tab already at the sign-in screen is not being
  // thrown out of anything: the guard has to come before the note, or an
  // explanation appears for something that did not happen here. Both kinds,
  // because only the involuntary one has a sentence to leak.
  test.each([[true], [false]])(
    'a peer tab clearing the session while THIS one is signed out says nothing (deliberate=%s)',
    async (deliberate) => {
      vi.mocked(bootstrap).mockResolvedValue(ANSWERS)
      vi.mocked(restoreSession).mockResolvedValue('anonymous')
      const boot = booted()
      await boot.start()
      expect(boot.phase.value).toBe('login')

      const ended = vi.mocked(onTokensCleared).mock.calls[0]![0]!
      ended(deliberate)
      expect(boot.phase.value).toBe('login')
      expect(boot.note.value).toBe('')
      boot.stop()
    },
  )

  test('the callback is dropped when the app goes away', () => {
    // Left registered, it would write to refs nothing renders. Dropped via the
    // disposer rather than by clearing the slot, so a successor that
    // registered first is not wiped — see `onTokensCleared`, and the test of
    // that rule in session.test.ts.
    const drop = vi.fn()
    vi.mocked(onTokensCleared).mockReturnValue(drop)
    const boot = booted()
    expect(drop).not.toHaveBeenCalled()
    boot.stop()
    expect(drop).toHaveBeenCalledTimes(1)
  })
})
