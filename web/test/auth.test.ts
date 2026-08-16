/// Getting in: which screen opens, what the form accepts, and the one that
/// used to hang for ever.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { MIN_PASSWORD, endedNote, passwordLongEnough, phaseFor } from '../src/domain/auth.ts'
import { Offline } from '../src/api/errors.ts'
import Auth from '../src/views/Auth.vue'
import { sentence } from '../src/domain/refusal.ts'

vi.mock('../src/api/session.ts', () => ({ browserLogin: vi.fn() }))
vi.mock('../src/api/generated/kahawai.ts', () => ({ setup: vi.fn() }))

const { browserLogin } = await import('../src/api/session.ts')
const { setup } = await import('../src/api/generated/kahawai.ts')

beforeEach(() => vi.resetAllMocks())
afterEach(() => vi.restoreAllMocks())

describe('which screen opens', () => {
  test('a hub with no administrator opens on setup, signed in or not', () => {
    // Setup wins over everything: there is no account to be signed in as.
    expect(phaseFor({ setup_required: true }, 'anonymous')).toBe('setup')
    expect(phaseFor({ setup_required: true }, 'authenticated')).toBe('setup')
  })

  test('otherwise it is decided by whether the session came back', () => {
    expect(phaseFor({ setup_required: false }, 'authenticated')).toBe('app')
    expect(phaseFor({ setup_required: false }, 'anonymous')).toBe('login')
  })
})

describe('why the sign-in screen is showing', () => {
  test('a session that ended on its own says so', () => {
    // Landing on a password box with no explanation reads as the app having
    // forgotten you for no reason.
    expect(endedNote(false)).not.toBe('')
  })

  test('and signing out deliberately does not', () => {
    // You know why you are here.
    expect(endedNote(true)).toBe('')
  })
})

describe('what the form will accept', () => {
  test("the hub's length rule, counted in code points", () => {
    // '🔑'.length is 2, so six emoji counted as twelve and were let through.
    expect(passwordLongEnough('🔑'.repeat(6))).toBe(false)
    expect(passwordLongEnough('🔑'.repeat(MIN_PASSWORD))).toBe(true)
    expect(passwordLongEnough('a'.repeat(MIN_PASSWORD - 1))).toBe(false)
    expect(passwordLongEnough('a'.repeat(MIN_PASSWORD))).toBe(true)
  })

  test('and it is only enforced where the hub enforces it', () => {
    // A short password on the SIGN-IN screen is somebody with an old account,
    // or a typo. Refusing to send it means they cannot find out which.
    const login = mount(Auth, { props: { mode: 'login' } })
    expect(login.find('button').attributes('disabled')).toBeUndefined()
  })

  test('setup keeps the button off until the password could succeed', async () => {
    const create = mount(Auth, { props: { mode: 'setup', setupAvailable: true } })
    expect(create.find('button').attributes('disabled')).toBeDefined()
    await create.findAll('input')[1]!.setValue('a'.repeat(MIN_PASSWORD))
    expect(create.find('button').attributes('disabled')).toBeUndefined()
  })
})

describe('the fields', () => {
  test('are named by labels rather than by placeholders alone', async () => {
    // A placeholder is not an accessible name and it disappears as soon as
    // anything is typed, which is when it would matter most.
    const form = mount(Auth, { props: { mode: 'login' } })
    const inputs = form.findAll('input')
    for (const input of inputs) {
      const id = input.attributes('id')
      expect(id).toBeTruthy()
      expect(form.find(`label[for="${id}"]`).exists()).toBe(true)
    }
  })

  test('tell a password manager which form this is', async () => {
    // `new-password` on setup, `current-password` on sign-in: the difference
    // is whether a manager offers to generate one or to fill one in.
    const login = mount(Auth, { props: { mode: 'login' } })
    expect(login.findAll('input')[1]!.attributes('autocomplete')).toBe('current-password')

    const create = mount(Auth, { props: { mode: 'setup', setupAvailable: true } })
    expect(create.findAll('input')[1]!.attributes('autocomplete')).toBe('new-password')
  })
})

describe('signing in', () => {
  test('hands the app over when it works', async () => {
    vi.mocked(browserLogin).mockResolvedValue()
    const form = mount(Auth, { props: { mode: 'login' } })
    await form.find('form').trigger('submit')
    await flushPromises()
    expect(form.emitted('done')).toHaveLength(1)
  })

  test('the button is what submits the form', async () => {
    // Every other test here triggers the form directly, so the button-to-form
    // wiring was unexercised: `type="submit"` becoming `type="button"` left a
    // Sign in that does nothing at all, and passed the suite.
    vi.mocked(browserLogin).mockResolvedValue()
    const form = mount(Auth, { props: { mode: 'login' }, attachTo: document.body })
    expect(form.find('button').attributes('type')).toBe('submit')
    await form.findAll('input')[0]!.setValue('claude')
    await form.findAll('input')[1]!.setValue('a-password')
    await form.find('button').trigger('click')
    await flushPromises()
    expect(browserLogin).toHaveBeenCalledTimes(1)
    form.unmount()
  })

  test('an empty form is answered here rather than by the hub', async () => {
    // `required`, so the browser says so immediately. A round trip to be told
    // your blank password is wrong is a round trip and a wrong answer.
    const form = mount(Auth, { props: { mode: 'login' }, attachTo: document.body })
    await form.find('button').trigger('click')
    await flushPromises()
    expect(browserLogin).not.toHaveBeenCalled()
    form.unmount()
  })

  test('and submitting never lets the browser send the form itself', async () => {
    // Without `prevent`, a form with no method does a native GET to the
    // current URL — putting the password in the address bar, in history and
    // in every access log between here and the hub.
    vi.mocked(browserLogin).mockResolvedValue()
    const form = mount(Auth, { props: { mode: 'login' } })
    const event = new Event('submit', { cancelable: true, bubbles: true })
    form.find('form').element.dispatchEvent(event)
    await flushPromises()
    expect(event.defaultPrevented).toBe(true)
  })

  test('the credentials go to the hub the right way round', async () => {
    // Swapping them passed: nothing asserted what `browserLogin` was called
    // with, only that it was.
    vi.mocked(browserLogin).mockResolvedValue()
    const form = mount(Auth, { props: { mode: 'login' } })
    await form.findAll('input')[0]!.setValue('claude')
    await form.findAll('input')[1]!.setValue('a-password')
    await form.find('form').trigger('submit')
    await flushPromises()
    expect(browserLogin).toHaveBeenCalledWith('claude', 'a-password')
  })

  test('and a second press while the first is out sends nothing', async () => {
    // Two logins in flight bump the generation twice, so the FIRST can no
    // longer install — the app then renders signed in with no bearer.
    let release = () => {}
    vi.mocked(browserLogin).mockReturnValue(new Promise((resolve) => (release = () => resolve())))
    const form = mount(Auth, { props: { mode: 'login' } })
    await form.find('form').trigger('submit')
    await form.find('form').trigger('submit')
    expect(browserLogin).toHaveBeenCalledTimes(1)
    release()
    await flushPromises()
  })

  test('gives the form back when the hub never answers', async () => {
    // UI-24. The form was disabled for the duration of the request and the
    // request had no deadline, so a hub that accepted the connection and went
    // quiet left a dead form on the one screen with nothing else to navigate
    // to. The deadline lives in `browserLogin`; this is the half that has to
    // re-enable the button when it expires.
    vi.mocked(browserLogin).mockRejectedValue(new Offline('The hub did not answer in time.'))
    const form = mount(Auth, { props: { mode: 'login' } })
    await form.find('form').trigger('submit')
    await flushPromises()

    expect(form.find('button').attributes('disabled')).toBeUndefined()
    expect(form.find('[role="alert"]').text()).toContain('did not answer in time')
    expect(form.emitted('done')).toBeUndefined()
  })

  test("a second attempt does not show the first one's failure", async () => {
    vi.mocked(browserLogin).mockRejectedValueOnce(new Offline()).mockResolvedValueOnce()
    const form = mount(Auth, { props: { mode: 'login' } })
    await form.find('form').trigger('submit')
    await flushPromises()
    expect(form.find('[role="alert"]').exists()).toBe(true)

    await form.find('form').trigger('submit')
    await flushPromises()
    expect(form.find('[role="alert"]').exists()).toBe(false)
  })
})

describe('creating the first administrator', () => {
  test('is offered only where the hub offers it', () => {
    // Everywhere else the form would be refused, so the screen says where to
    // go instead.
    const remote = mount(Auth, {
      props: { mode: 'setup', setupAvailable: false, setupUrl: 'http://127.0.0.1:8498' },
    })
    expect(remote.find('form').exists()).toBe(false)
    expect(remote.text()).toContain('http://127.0.0.1:8498')
  })

  test('says what to do next rather than signing you in', async () => {
    // Setup mints no session: the admin goes back to the ordinary address.
    vi.mocked(setup).mockResolvedValue(undefined as never)
    const create = mount(Auth, { props: { mode: 'setup', setupAvailable: true } })
    await create.findAll('input')[0]!.setValue('claude')
    await create.findAll('input')[1]!.setValue('a'.repeat(MIN_PASSWORD))
    await create.find('form').trigger('submit')
    await flushPromises()
    expect(create.emitted('done')).toBeUndefined()
    expect(create.text()).toContain('Administrator created')
    // The opt-outs matter: there is no session yet, so sending a dead bearer
    // invites the hub to answer about that instead, and a 401 from here must
    // not re-enter the refresh path.
    expect(setup).toHaveBeenCalledWith(
      { username: 'claude', password: 'a'.repeat(MIN_PASSWORD) },
      { skipAuthRefresh: true, skipAuthorization: true },
    )
  })
})

describe('the sentence shown for a failure', () => {
  test("is the error's own words", () => {
    expect(sentence(new Offline())).toBe('Could not reach the hub.')
  })

  test('and something legible for a throw that is not an error', () => {
    // `String({})` is "[object Object]", which tells whoever is looking at it
    // nothing at all.
    expect(sentence({ nope: true })).toBe('Something went wrong.')
    expect(sentence(new Error(''))).not.toBe('')
  })
})
