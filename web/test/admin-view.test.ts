/// The admin panel, mounted.
///
/// Its recorded incidents are all about state that is not the happy path: a
/// read succeeding must not wipe a refusal nobody has read, a success must
/// clear the failure before it, an optimistic chip must be released when the
/// write settles, and an empty list must never be reported as an empty world
/// when the read that would have filled it failed.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'
import { IN_PROCESS } from '../src/domain/admin.ts'
import { POLL_MS } from '../src/composables/admin.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  adminEnrollments: vi.fn(),
  adminSatellites: vi.fn(),
  adminSessions: vi.fn(),
  libraries: vi.fn(),
  collections: vi.fn(),
  adminUsers: vi.fn(),
  adminApprove: vi.fn(),
  setCollections: vi.fn(),
  createLibrary: vi.fn(),
  adminCreateUser: vi.fn(),
  deleteLibrary: vi.fn(),
  adminDeleteSatellite: vi.fn(),
  adminDeleteUser: vi.fn(),
  adminDetachCollection: vi.fn(),
  adminDisconnectProvider: vi.fn(),
  adminEndSession: vi.fn(),
  adminEnrichRun: vi.fn(),
  adminEnrichStatus: vi.fn(),
  adminProviders: vi.fn(),
  adminRefreshLibrary: vi.fn(),
  adminSessionLog: vi.fn(),
  adminSetAnidb: vi.fn(),
  adminSetChain: vi.fn(),
  adminSetDisabled: vi.fn(),
  adminSetFanart: vi.fn(),
  adminSetTheaudiodb: vi.fn(),
  adminSegmentsStatus: vi.fn(),
  adminSetTmdb: vi.fn(),
  adminSetTvdb: vi.fn(),
  adminSetUserAdmin: vi.fn(),
  adminSetUserLibraries: vi.fn(),
}))
vi.mock('../src/api/session.ts', () => ({
  whoAmI: () => ({ username: 'boss', admin: true }),
  refreshTokens: vi.fn(async () => true),
}))

const api = await import('./api-fixture.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')
const Admin = (await import('../src/views/Admin.vue')).default

const satellite = (over: Record<string, unknown> = {}) => ({
  build: null,
  capabilities: null,
  cert_fingerprint: 'ab12',
  connected: true,
  disabled: false,
  enrolled_at: 0,
  link_bytes_per_sec: null,
  module_id: 'mh1',
  module_type: 'mediahost',
  name: 'attic',
  pace: [],
  ...over,
})

const box = (over: Record<string, unknown> = {}) =>
  satellite({ module_type: 'transcoder', module_id: 'tc1', name: 'nvenc', ...over })

const user = (over: Record<string, unknown> = {}) => ({
  id: 'u1',
  username: 'claude',
  is_admin: false,
  all_libraries: false,
  libraries: ['films'],
  grants_version: 3,
  created_at: 1_700_000_000,
  ...over,
})

async function open() {
  const wrapper = mount(Admin, {
    attachTo: document.body,
    global: {
      plugins: [
        [
          VueQueryPlugin,
          { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
      ],
    },
  })
  await flushPromises()
  return wrapper
}

type Panel = Awaited<ReturnType<typeof open>>

const press = async (wrapper: Panel, label: string) => {
  await wrapper
    .findAll('button')
    .find((b) => b.text() === label)!
    .trigger('click')
  await flushPromises()
}

const tab = async (wrapper: Panel, label: string) => {
  await wrapper
    .findAll('[role="tab"]')
    .find((b) => b.text().startsWith(label))!
    .trigger('click')
  await flushPromises()
}

/// An answer somebody else decides when to give.
function held<T>(value: T) {
  let settle!: () => void
  const promise = new Promise<T>((resolve) => {
    settle = () => resolve(value)
  })
  return { promise, settle }
}

beforeEach(() => {
  vi.mocked(api.adminEnrollments).mockResolvedValue({ pending: [] } as never)
  vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [satellite()] } as never)
  vi.mocked(api.adminSessions).mockResolvedValue({ sessions: [] } as never)
  vi.mocked(api.libraries).mockResolvedValue([
    { id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] },
  ] as never)
  vi.mocked(api.collections).mockResolvedValue([])
  vi.mocked(api.adminUsers).mockResolvedValue({ users: [user()] } as never)
  vi.mocked(api.adminApprove).mockResolvedValue({ approved: 'attic' } as never)
  vi.mocked(api.adminProviders).mockResolvedValue({
    tmdb: { configured: false },
    tvdb: { configured: false },
    anidb: { configured: false },
    fanart: { configured: false },
    theaudiodb: { premium_key_configured: false },
    chains: {},
  } as never)
  vi.mocked(api.adminEnrichStatus).mockResolvedValue({
    running: false,
    matched: 0,
    weak: 0,
    missed: 0,
  } as never)
  vi.mocked(api.adminSegmentsStatus).mockResolvedValue({ collections: [] })
  clearNotices()
})
afterEach(() => {
  vi.resetAllMocks()
  vi.useRealTimers()
})

describe('the fleet', () => {
  test('lists what is enrolled, and never the hub’s own mediahost', async () => {
    // It has no certificate to revoke, and the Delete it was offered would wipe
    // the index of everything it serves.
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [satellite(), satellite({ cert_fingerprint: IN_PROCESS, name: 'in-process' })],
    } as never)
    const wrapper = await open()
    expect(wrapper.text()).toContain('attic')
    expect(wrapper.text()).not.toContain('in-process')
  })

  test('deleting one is asked twice, because it revokes a certificate', async () => {
    const wrapper = await open()
    const button = wrapper.findAll('button').find((candidate) => candidate.text() === 'Delete')!
    expect(button.attributes('aria-label')).toBe('Delete attic')
    await button.trigger('click')
    expect(button.attributes('aria-label')).toBe('Really delete attic and revoke its certificate?')
    expect(wrapper.text()).toContain('Really delete + revoke?')
    expect(api.adminDeleteSatellite).not.toHaveBeenCalled()

    await button.trigger('click')
    expect(api.adminDeleteSatellite).toHaveBeenCalledWith('mh1')
    expect(notice.value).toContain('certificate revoked')
  })

  test('and the arming is on the SAME button, so the keyboard keeps it', async () => {
    // Swapping the button for a question destroys the focused element: focus
    // falls to the body, the keyboard user is returned to the top of the
    // document, and a screen reader is told nothing at all.
    const wrapper = await open()
    const button = wrapper.findAll('button').find((b) => b.text() === 'Delete')!
    const element = button.element
    await button.trigger('click')
    expect(element.textContent).toContain('Really delete + revoke?')
    expect(element.isConnected).toBe(true)
  })

  test('and looking away disarms it', async () => {
    // Without this it stayed armed indefinitely, through every fifteen-second
    // poll, and the next click on that row was a delete nobody meant.
    const wrapper = await open()
    const button = wrapper.findAll('button').find((b) => b.text() === 'Delete')!
    await button.trigger('click')
    await button.trigger('blur')
    expect(wrapper.text()).not.toContain('Really delete + revoke?')
    expect(api.adminDeleteSatellite).not.toHaveBeenCalled()
  })

  test('one that is away says so', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [satellite({ connected: false })],
    } as never)
    expect((await open()).text()).toContain('offline')
  })

  test('and approving is by the code the satellite printed', async () => {
    // Not a button on a row: the code is what proves whoever is at this panel
    // can also see that machine.
    const wrapper = await open()
    await wrapper.find('#enrol-code').setValue('QUIET-OTTER')
    await wrapper.find('form').trigger('submit')
    await flushPromises()
    expect(api.adminApprove).toHaveBeenCalledWith({ code: 'QUIET-OTTER' })
    // What the HUB says it admitted. Echoing the typed code reports the
    // operator's own keystrokes back as though they were an answer.
    expect(notice.value).toContain('attic')
    expect((wrapper.find('#enrol-code').element as HTMLInputElement).value).toBe('')
  })

  test('Disable is offered only where it does something', async () => {
    // The hub takes the flag for any satellite and only transcoder placement
    // reads it, so an operator draining a mediahost got a 204, a persisted flag
    // and a row that said "Enable" for ever — while the host kept serving.
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [satellite(), box()],
    } as never)
    const wrapper = await open()
    expect(wrapper.findAll('button').filter((b) => b.text().includes('Disable'))).toHaveLength(1)
  })

  test('pressing Disable twice sends two DIFFERENT writes', async () => {
    // The row does not move until the re-read lands, so computing the value
    // from it sent `disabled: true` twice — "disable, no wait, enable" was
    // silently one write, and the row still read `Disabled — enable`.
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockResolvedValue(undefined as never)
    const wrapper = await open()
    await press(wrapper, 'Disable')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Disabled — enable')).toBe(true)
    await press(wrapper, 'Disabled — enable')
    expect(vi.mocked(api.adminSetDisabled).mock.calls.map((c) => c[1])).toEqual([
      { disabled: true },
      { disabled: false },
    ])
  })

  test('and a refused Disable puts the label back', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockRejectedValue(new ApiError(403, 'not allowed'))
    const wrapper = await open()
    await press(wrapper, 'Disable')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Disable')).toBe(true)
  })

  test('a transcoder shows what it was measured doing', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [
        box({
          link_bytes_per_sec: 118_000_000,
          capabilities: {
            ass_burn: true,
            decode_caps: [],
            max_sessions: 2,
            tonemap: true,
            tonemap_speed_1080: 4.5,
            tonemap_speed_2160: 1.2,
            encoders: [
              {
                codec: 'h264',
                element: 'nvh264enc',
                hardware: true,
                speed_1080: 6.2,
                speed_2160: 2.1,
              },
            ],
          },
          pace: [{ class: '1080p', multiple: 0.8 }],
        }),
      ],
    } as never)
    const text = (await open()).text()
    expect(text).toContain('h264 hw 6.2× / 2.1×')
    expect(text).toContain('tone-map 4.5× / 1.2×')
    expect(text).toContain('link 118.0 MB/s')
    expect(text).toContain('1080p 0.8×')
  })

  test('and a transcoder that was never measured shows no measurements', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    expect((await open()).text()).not.toContain('measured')
  })

  test('a pace of zero is not a slow box, it is an unmeasured one', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [box({ pace: [{ class: '1080p', multiple: 0 }] })],
    } as never)
    const wrapper = await open()
    const chip = wrapper.findAll('span').find((x) => x.text().startsWith('1080p'))!
    expect(chip.text()).toContain('not measured')
    expect(chip.classes().join(' ')).not.toContain('warn')
  })

  test('and a pace under realtime is marked, because that is the one worth seeing', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({
      satellites: [box({ pace: [{ class: '2160p', multiple: 0.4 }] })],
    } as never)
    const wrapper = await open()
    const chip = wrapper.findAll('span').find((s) => s.text() === '2160p 0.4×')!
    expect(chip.classes().join(' ')).toContain('warn')
  })

  test('a row carries the ids you need when something is wrong', async () => {
    // Not as visible text: they are what you go looking for when a host is
    // misbehaving, not what you read down a list.
    vi.mocked(api.adminEnrollments).mockResolvedValue({
      pending: [
        { csr_fingerprint: 'aa11', module_id: 'mh-7f3c', module_type: 'mediahost', name: 'new' },
      ],
    } as never)
    const wrapper = await open()
    expect(wrapper.html()).toContain('mh-7f3c')
    expect(wrapper.findAll('span').some((x) => x.attributes('title')?.includes('cert ab12'))).toBe(
      true,
    )
  })

  test('imported files are counted on their mediahost', async () => {
    vi.mocked(api.collections).mockResolvedValue([
      { mediahost_id: 'mh1', file_count: 10 },
      { mediahost_id: 'mh1', file_count: 5 },
    ] as never)
    expect((await open()).text()).toContain('15 files')
  })
})

describe('what an empty list means', () => {
  test('nothing pending is only said when the read worked', async () => {
    const wrapper = await open()
    expect(wrapper.text()).toContain('prints its code on its console')
  })

  test('and a failed read says so instead', async () => {
    // An empty list from a 503 rendered as a statement, and a false one, next
    // to a warn line about the hub.
    vi.mocked(api.adminEnrollments).mockRejectedValue(new ApiError(503, 'hub restarting'))
    const wrapper = await open()
    expect(wrapper.text()).not.toContain('prints its code on its console')
    expect(wrapper.text()).toContain('not saying there is nothing')
  })

  test('nobody playing anything is only said when the read worked', async () => {
    vi.mocked(api.adminSessions).mockRejectedValue(new ApiError(503, 'hub restarting'))
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    expect(wrapper.text()).not.toContain('Nobody is streaming')
  })
})

describe('the two kinds of failure', () => {
  test('a read failing is reported without taking the panel, and NAMES what failed', async () => {
    vi.mocked(api.collections).mockRejectedValue(new ApiError(503, 'hub restarting'))
    const wrapper = await open()
    expect(wrapper.find('[role="status"]').text()).toContain('hub restarting')
    expect(wrapper.find('[role="status"]').text()).toContain('collections')
    // The satellites still arrived, so they are still on screen.
    expect(wrapper.text()).toContain('attic')
  })

  test('and one dead hub is reported once, not six times', async () => {
    // Nothing readable at all: the Failed block below says it, with a Try
    // again on it, and the line above goes quiet rather than saying the same
    // thing a second time over a panel that is not there.
    for (const read of [
      api.adminEnrollments,
      api.adminSatellites,
      api.adminSessions,
      api.libraries,
      api.collections,
      api.adminUsers,
    ]) {
      vi.mocked(read).mockRejectedValue(new ApiError(503, 'hub restarting'))
    }
    const wrapper = await open()
    expect(wrapper.find('[role="status"]').text()).toBe('')
    expect(wrapper.text().match(/hub restarting/g)).toHaveLength(1)
    // ...and the panel underneath is gone, rather than empty: an Approve form
    // over a hub that cannot be read is a control that cannot work.
    expect(wrapper.find('[role="tabpanel"]').exists()).toBe(false)
  })

  test('but one dead read among five live ones names itself, over a live panel', async () => {
    vi.mocked(api.collections).mockRejectedValue(new ApiError(503, 'hub restarting'))
    const wrapper = await open()
    expect(wrapper.find('[role="status"]').text()).toContain('collections')
    expect(wrapper.find('[role="tabpanel"]').exists()).toBe(true)
    expect(wrapper.text()).not.toContain('Could not read the hub.')
  })

  test('nothing readable at all offers Try again', async () => {
    // `loaded` used to ask whether any query was still pending, which a failed
    // one is not — so the only condition under which this rendered was one that
    // could never hold at the same time as an error.
    for (const read of [
      api.adminEnrollments,
      api.adminSatellites,
      api.adminSessions,
      api.libraries,
      api.collections,
      api.adminUsers,
    ]) {
      vi.mocked(read).mockRejectedValue(new ApiError(503, 'hub restarting'))
    }
    const wrapper = await open()
    expect(wrapper.text()).toContain('Could not read the hub.')
    expect(wrapper.findAll('button').some((b) => b.text().includes('Try again'))).toBe(true)
  })

  test('an action failing stays until the operator does something else', async () => {
    vi.useFakeTimers()
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockRejectedValue(new ApiError(403, 'not allowed'))
    const wrapper = await open()
    await press(wrapper, 'Disable')
    expect(wrapper.find('[role="alert"]').text()).toContain('not allowed')

    // The POLL landing does not wipe it. Sharing one cell meant a read
    // succeeding erased a refused delete before it could be read — every
    // fifteen seconds here, and every 250 ms while a scan is running. A lost
    // error is worse than the stale one it replaced.
    const reads = vi.mocked(api.adminSatellites).mock.calls.length
    vi.advanceTimersByTime(POLL_MS + 100)
    await flushPromises()
    expect(vi.mocked(api.adminSatellites).mock.calls.length).toBeGreaterThan(reads)
    expect(wrapper.find('[role="alert"]').text()).toContain('not allowed')
  })

  test('and a success clears it', async () => {
    // Only four sites used to, so a failure that had since been resolved read
    // as the outcome of the NEXT action.
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockRejectedValueOnce(new ApiError(403, 'not allowed'))
    const wrapper = await open()
    await press(wrapper, 'Disable')
    expect(wrapper.find('[role="alert"]').text()).toContain('not allowed')

    vi.mocked(api.adminSetDisabled).mockResolvedValue(undefined as never)
    await press(wrapper, 'Disable')
    expect(wrapper.find('[role="alert"]').text()).toBe('')
  })

  test('a grant re-reads the accounts, and nothing else', async () => {
    // A grant write has no opinion about the session list, and making it wait
    // for five other requests is what put a six-request round trip in front of
    // clearing a form.
    vi.mocked(api.adminSetUserLibraries).mockResolvedValue({
      all_libraries: false,
      libraries: [],
      grants_version: 4,
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    const users = vi.mocked(api.adminUsers).mock.calls.length
    const sessions = vi.mocked(api.adminSessions).mock.calls.length

    await press(wrapper, 'Films')
    expect(vi.mocked(api.adminUsers).mock.calls.length).toBeGreaterThan(users)
    expect(vi.mocked(api.adminSessions).mock.calls.length).toBe(sessions)
  })

  test('a mutation that worked reads again, so the panel shows what it did', async () => {
    // The whole reason `act` exists. Without it the row keeps saying "Disable"
    // after the host has been disabled, until the next poll.
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockResolvedValue(undefined as never)
    const wrapper = await open()
    const reads = vi.mocked(api.adminSatellites).mock.calls.length
    await press(wrapper, 'Disable')
    await flushPromises()
    expect(vi.mocked(api.adminSatellites).mock.calls.length).toBeGreaterThan(reads)
  })

  test('and changing tab clears it, because it was an answer to another screen', async () => {
    vi.mocked(api.adminSatellites).mockResolvedValue({ satellites: [box()] } as never)
    vi.mocked(api.adminSetDisabled).mockRejectedValue(new ApiError(403, 'not allowed'))
    const wrapper = await open()
    await press(wrapper, 'Disable')
    await tab(wrapper, 'Users')
    expect(wrapper.find('[role="alert"]').text()).toBe('')
  })

  test('both live regions exist before they have anything to say', async () => {
    // A live region has to be in the accessibility tree before its content
    // changes; a node inserted with its text already in it is not reliably
    // announced. This is the only channel that reports a refused delete.
    const wrapper = await open()
    expect(wrapper.find('[role="status"]').exists()).toBe(true)
    expect(wrapper.find('[role="alert"]').exists()).toBe(true)
  })
})

describe('the tabs', () => {
  test('are tabs, and the arrow keys move between them', async () => {
    // A COLUMN of tabs, so Down and Up. `aria-orientation` is a promise about
    // which keys work, and Left/Right on a column is the wrong one.
    const wrapper = await open()
    const tabs = wrapper.findAll('[role="tab"]')
    expect(tabs.map((tab) => tab.text())).toEqual([
      'Satellites',
      'Libraries',
      'Providers',
      'Users & grants',
      'Sessions',
    ])
    expect(wrapper.get('[role="tablist"]').attributes('aria-orientation')).toBe('vertical')
    expect(tabs[0]!.attributes('aria-selected')).toBe('true')
    expect(tabs[1]!.attributes('tabindex')).toBe('-1')

    await tabs[0]!.trigger('keydown', { key: 'ArrowDown' })
    await flushPromises()
    expect(wrapper.findAll('[role="tab"]')[1]!.attributes('aria-selected')).toBe('true')
    expect(wrapper.find('[role="tabpanel"]').attributes('aria-labelledby')).toBe('tab-libraries')
  })

  test('and the focus follows, or it is not a tablist', async () => {
    // A roving-tabindex tablist where the arrow keys change the selection but
    // leave the focus behind puts the keyboard user on a control that is no
    // longer in the tab order.
    const wrapper = await open()
    await wrapper.findAll('[role="tab"]')[0]!.trigger('keydown', { key: 'ArrowDown' })
    await flushPromises()
    expect(document.activeElement).toBe(wrapper.findAll('[role="tab"]')[1]!.element)
  })

  test('and ArrowUp goes back', async () => {
    const wrapper = await open()
    await wrapper.findAll('[role="tab"]')[0]!.trigger('keydown', { key: 'ArrowDown' })
    await flushPromises()
    await wrapper.findAll('[role="tab"]')[1]!.trigger('keydown', { key: 'ArrowUp' })
    await flushPromises()
    expect(wrapper.findAll('[role="tab"]')[0]!.attributes('aria-selected')).toBe('true')
  })

  test('and the horizontal keys are left alone', async () => {
    // Not merely unbound: Left/Right belong to whatever the panel holds, and a
    // tablist that swallowed them would take them off a text field in a form.
    const wrapper = await open()
    await wrapper.findAll('[role="tab"]')[0]!.trigger('keydown', { key: 'ArrowRight' })
    await flushPromises()
    expect(wrapper.findAll('[role="tab"]')[0]!.attributes('aria-selected')).toBe('true')
  })

  test('and the nav sticks, on its own ground where it overlaps', async () => {
    // On a busy fleet a strip along the top means scrolling back up past the
    // whole of Libraries to change section, so it sticks. Below `sm` it wraps
    // to full width ABOVE the content, and a sticky nav with no ground of its
    // own let the section scroll straight through it; it also has to paint
    // over a sibling that comes after it in the document.
    const wrapper = await open()
    const nav = wrapper.get('[role="tablist"]').classes()
    expect(nav).toContain('sticky')
    expect(nav).toContain('max-sm:bg-surface')
    expect(nav).toContain('max-sm:z-10')
  })

  test('and Home goes to the first', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    await wrapper.findAll('[role="tab"]').at(-1)!.trigger('keydown', { key: 'Home' })
    await flushPromises()
    expect(wrapper.findAll('[role="tab"]')[0]!.attributes('aria-selected')).toBe('true')
  })

  test('and the panel is reachable by the next Tab press', async () => {
    // Focusable, not focused: the arrow keys move the focus with the
    // selection, so it belongs on the tablist. `tabindex="-1"` is what stops
    // the next Tab press skipping past everything the tab just revealed — and
    // it is in the template, because a panel that only becomes focusable after
    // the first tab change is not focusable when it matters.
    const wrapper = await open()
    expect(wrapper.find('[role="tabpanel"]').attributes('tabindex')).toBe('-1')
  })

  test('and End goes to the last one', async () => {
    const wrapper = await open()
    await wrapper.findAll('[role="tab"]')[0]!.trigger('keydown', { key: 'End' })
    await flushPromises()
    expect(wrapper.findAll('[role="tab"]').at(-1)!.attributes('aria-selected')).toBe('true')
  })

  test('carry a count for the two things that change on their own', async () => {
    // A satellite waiting to be admitted is invisible unless you happen to be
    // standing on that tab, which is the reason the nav exists.
    vi.mocked(api.adminEnrollments).mockResolvedValue({
      pending: [{ csr_fingerprint: 'aa', module_id: 'm1', module_type: 'mediahost', name: 'new' }],
    } as never)
    const wrapper = await open()
    const satellites = wrapper.findAll('[role="tab"]')[0]!
    expect(satellites.text()).toContain('1')
    // And not on the ones that do not: a number that never moves is furniture.
    expect(wrapper.findAll('[role="tab"]')[1]!.text()).toBe('Libraries')
  })

  test('two requests from one module do not collide', async () => {
    // A satellite restarted before it was admitted asks twice, and a key that
    // is only the module's name drops one of them.
    vi.mocked(api.adminEnrollments).mockResolvedValue({
      pending: [
        { csr_fingerprint: 'aa', module_id: 'm1', module_type: 'mediahost', name: 'first try' },
        { csr_fingerprint: 'bb', module_id: 'm1', module_type: 'mediahost', name: 'second try' },
      ],
    } as never)
    const wrapper = await open()
    expect(wrapper.text()).toContain('first try')
    expect(wrapper.text()).toContain('second try')
  })
})

describe('accounts', () => {
  test('a new one needs a name and a password the hub will take', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Users')
    const create = wrapper.findAll('button').find((b) => b.text() === 'Create')!
    expect(create.attributes('disabled')).toBeDefined()

    await wrapper.find('#new-user').setValue('newbie')
    await wrapper.find('#new-pass').setValue('short')
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.text() === 'Create')!
        .attributes('disabled'),
    ).toBeDefined()
    // And it SAYS why, before Create is pressed.
    expect(wrapper.find('#new-pass').attributes('aria-invalid')).toBe('true')

    await wrapper.find('#new-pass').setValue('a'.repeat(12))
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.text() === 'Create')!
        .attributes('disabled'),
    ).toBeUndefined()
    expect(wrapper.find('#new-pass').attributes('aria-invalid')).toBe('false')
  })

  test('and creating one says what it can see', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Users')
    await wrapper.find('#new-user').setValue('newbie')
    await wrapper.find('#new-pass').setValue('a'.repeat(12))
    await wrapper.findAll('form').at(-1)!.trigger('submit')
    await flushPromises()
    expect(api.adminCreateUser).toHaveBeenCalledWith({
      username: 'newbie',
      password: 'a'.repeat(12),
      admin: false,
    })
    expect(notice.value).toContain('every library until you say otherwise')
  })

  test('and the form is cleared without waiting for the re-read', async () => {
    // It used to wait for six requests, so on a slow hub the fields kept their
    // values with Create still live — one Enter from creating it twice.
    const wrapper = await open()
    await tab(wrapper, 'Users')
    const slow = held({ users: [user()] })
    vi.mocked(api.adminUsers).mockReturnValue(slow.promise as never)
    await wrapper.find('#new-user').setValue('newbie')
    await wrapper.find('#new-pass').setValue('a'.repeat(12))
    await wrapper.findAll('form').at(-1)!.trigger('submit')
    await flushPromises()

    expect((wrapper.find('#new-user').element as HTMLInputElement).value).toBe('')
    slow.settle()
    await flushPromises()
  })

  test('an admin’s library toggle is held on and locked', async () => {
    // An admin does have every library, and saying so with everyone else's
    // toggle beats a sentence explaining why there is no toggle here.
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ is_admin: true, all_libraries: false })],
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    const toggle = wrapper.findAll('button').find((b) => b.text() === 'all libraries')!
    expect(toggle.attributes('aria-pressed')).toBe('true')
    // Reachable, and inert: `disabled` would take the explanation out of the
    // tab order along with the control.
    expect(toggle.attributes('aria-disabled')).toBe('true')
    expect(toggle.attributes('disabled')).toBeUndefined()
    await toggle.trigger('click')
    expect(api.adminSetUserLibraries).not.toHaveBeenCalled()
  })

  test('an account says when it was created', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Users')
    expect(wrapper.text()).toContain('since')
    expect(wrapper.text()).toMatch(/since \w/)
  })

  test('an account with nothing granted is marked', async () => {
    vi.mocked(api.adminUsers).mockResolvedValue({ users: [user({ libraries: [] })] } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    expect(wrapper.text()).toContain('no access')
  })

  test('and you cannot delete the account you are signed in as', async () => {
    // The API refuses it too; saying so before the click is kinder than an
    // error afterwards — and it is SAID, rather than left as a tooltip on a
    // button no keyboard can reach.
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ username: 'boss', is_admin: true })],
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Delete')).toBe(false)
    expect(wrapper.text()).toContain('signed in as this account')
  })

  test('granting one library writes the whole set, with the version', async () => {
    vi.mocked(api.adminSetUserLibraries).mockResolvedValue({
      all_libraries: false,
      libraries: ['films', 'music'],
      grants_version: 4,
    } as never)
    vi.mocked(api.libraries).mockResolvedValue([
      { id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] },
      { id: 'music', name: 'Music', media_type: 'music', collection_ids: [] },
    ] as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')

    await press(wrapper, 'Music')
    expect(api.adminSetUserLibraries).toHaveBeenCalledWith('u1', {
      all_libraries: false,
      libraries: ['films', 'music'],
      grants_version: 3,
    })
  })

  test('the chip moves before the hub answers, and stays live for the next click', async () => {
    // The chips must not be disabled for the round trip: the queue orders the
    // clicks, so disabling swallows the second one instead of ordering it — and
    // it takes the just-pressed button out of the tab order with nothing
    // announcing why.
    vi.mocked(api.libraries).mockResolvedValue([
      { id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] },
      { id: 'music', name: 'Music', media_type: 'music', collection_ids: [] },
    ] as never)
    const slow = held({ all_libraries: false, libraries: ['films', 'music'], grants_version: 4 })
    vi.mocked(api.adminSetUserLibraries).mockReturnValue(slow.promise as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Music')!
      .trigger('click')
    const music = wrapper.findAll('button').find((b) => b.text() === 'Music')!
    expect(music.attributes('aria-pressed')).toBe('true')
    expect(music.attributes('disabled')).toBeUndefined()
    slow.settle()
    await flushPromises()
  })

  test('and once it has settled the hub can repaint the row again', async () => {
    // The override was never released, so from the first click that row was
    // frozen until a reload: another admin narrowing the account changed
    // nothing on screen, and the panel showed a grant the hub did not have.
    vi.useFakeTimers()
    vi.mocked(api.libraries).mockResolvedValue([
      { id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] },
      { id: 'music', name: 'Music', media_type: 'music', collection_ids: [] },
    ] as never)
    vi.mocked(api.adminSetUserLibraries).mockResolvedValue({
      all_libraries: false,
      libraries: ['films', 'music'],
      grants_version: 4,
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')

    // What the hub will say once the write has landed.
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ libraries: ['films', 'music'], grants_version: 4 })],
    } as never)
    await press(wrapper, 'Music')
    const pressed = () =>
      wrapper
        .findAll('button')
        .find((b) => b.text() === 'Music')!
        .attributes('aria-pressed')
    expect(pressed()).toBe('true')

    // Somebody else narrows the account, and the next poll says so.
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ libraries: ['films'], grants_version: 5 })],
    } as never)
    vi.advanceTimersByTime(POLL_MS + 100)
    await flushPromises()
    expect(pressed()).toBe('false')
  })

  test('narrowing an account shows its chips before the hub answers', async () => {
    // The chips row is what the next click computes from, so it has to appear
    // with the optimistic write rather than a round trip later.
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ all_libraries: true })],
    } as never)
    const slow = held({ all_libraries: false, libraries: ['films'], grants_version: 4 })
    vi.mocked(api.adminSetUserLibraries).mockReturnValue(slow.promise as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Films')).toBe(false)

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'all libraries')!
      .trigger('click')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Films')).toBe(true)
    slow.settle()
    await flushPromises()
  })

  test('the "no access" chip follows the optimistic chips, not the row', async () => {
    // Revoking an account's last library is exactly when it becomes marooned,
    // and reading the row says it has not until the round trip lands.
    vi.mocked(api.adminSetUserLibraries).mockResolvedValue({
      all_libraries: false,
      libraries: [],
      grants_version: 4,
    } as never)
    const slow = held({ all_libraries: false, libraries: [], grants_version: 4 })
    vi.mocked(api.adminSetUserLibraries).mockReturnValue(slow.promise as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    expect(wrapper.text()).not.toContain('no access')

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Films')!
      .trigger('click')
    expect(wrapper.text()).toContain('no access')
    slow.settle()
    await flushPromises()
  })

  test('and a grant that worked clears the refusal before it', async () => {
    vi.mocked(api.adminSetUserLibraries)
      .mockRejectedValueOnce(new ApiError(403, 'not allowed'))
      .mockResolvedValue({ all_libraries: false, libraries: [], grants_version: 4 } as never)
    const wrapper = await open()
    await tab(wrapper, 'Users')
    await press(wrapper, 'Films')
    expect(wrapper.find('[role="alert"]').text()).toContain('not allowed')
    await press(wrapper, 'Films')
    expect(wrapper.find('[role="alert"]').text()).toBe('')
  })

  test('and deleting one is asked twice', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Users')
    await press(wrapper, 'Delete')
    expect(wrapper.text()).toContain('Really delete?')
    expect(api.adminDeleteUser).not.toHaveBeenCalled()
    await press(wrapper, 'Really delete?')
    expect(api.adminDeleteUser).toHaveBeenCalledWith('u1')
  })

  test('and an arming on one tab is not still armed on the way back', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Users')
    await press(wrapper, 'Delete')
    await tab(wrapper, 'Sessions')
    await tab(wrapper, 'Users')
    expect(wrapper.text()).not.toContain('Really delete?')
  })
})

describe('demoting yourself', () => {
  test('rotates the session before leaving, rather than reloading into a sign-in', async () => {
    // The write invalidated the token that authorised it: a bare reload has
    // bootstrap see only the invalid old one.
    const { refreshTokens } = await import('../src/api/session.ts')
    const assign = vi.fn()
    vi.stubGlobal('location', { assign })
    vi.mocked(api.adminSetUserAdmin).mockResolvedValue(undefined as never)
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ username: 'boss', is_admin: true })],
    } as never)

    const wrapper = await open()
    await tab(wrapper, 'Users')
    await wrapper.find('[title^="Demote"]').trigger('click')
    await flushPromises()

    expect(api.adminSetUserAdmin).toHaveBeenCalledWith('u1', { admin: false })
    expect(refreshTokens).toHaveBeenCalled()
    expect(assign).toHaveBeenCalledWith('/app/')
    vi.unstubAllGlobals()
  })

  test('and says so rather than leaving when the session cannot be rotated', async () => {
    const { refreshTokens } = await import('../src/api/session.ts')
    const assign = vi.fn()
    vi.stubGlobal('location', { assign })
    vi.mocked(refreshTokens).mockResolvedValue(false)
    vi.mocked(api.adminSetUserAdmin).mockResolvedValue(undefined as never)
    vi.mocked(api.adminUsers).mockResolvedValue({
      users: [user({ username: 'boss', is_admin: true })],
    } as never)

    const wrapper = await open()
    await tab(wrapper, 'Users')
    await wrapper.find('[title^="Demote"]').trigger('click')
    await flushPromises()

    expect(assign).not.toHaveBeenCalled()
    expect(notice.value).toContain('Sign in again')
    vi.unstubAllGlobals()
  })
})

describe('sessions', () => {
  const playing = {
    sessions: [
      {
        session_id: 's1',
        username: 'claude',
        title: 'Heat',
        mode: 'remux',
        module_id: 'mh1',
        idle_secs: 42,
        streams: { video: 'h264', audio: 'aac', cost: 'copy' },
      },
    ],
  }

  test('say what is playing, how it is being delivered, and for how long it has been idle', async () => {
    // The operator picks WHICH session to end by these.
    vi.mocked(api.adminSessions).mockResolvedValue(playing as never)
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    const text = wrapper.text().replace(/\s+/g, ' ')
    expect(text).toContain('claude')
    expect(text).toContain('Heat')
    expect(text).toContain('REMUX')
    expect(text).toContain('v: h264 · a: aac')
    expect(text).toContain('idle 42s')

    await press(wrapper, 'End')
    expect(api.adminEndSession).toHaveBeenCalledWith('s1')
  })

  test('and the chip comes from the NEGOTIATED plan, not from the mode', async () => {
    // `mode` is what an old client asked for by name; `cost` is what the hub
    // decided to do. A fixture where both spell the same chip cannot tell them
    // apart — this one has a mode the hub does not use as a cost.
    vi.mocked(api.adminSessions).mockResolvedValue({
      sessions: [
        {
          ...playing.sessions[0],
          mode: 'transcode',
          streams: { video: 'h264', audio: 'aac', cost: 'direct' },
        },
      ],
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    const chip = wrapper.findAll('span').find((x) => x.text() === 'DIRECT')!
    expect(chip.classes()).toContain('chip-teal')
  })

  test('an untitled one is named by its session, not by its host', async () => {
    // Two untitled sessions on one mediahost rendered as the same line, and
    // neither named the session the End button was about to kill.
    vi.mocked(api.adminSessions).mockResolvedValue({
      sessions: [
        { ...playing.sessions[0], session_id: 's1', title: null, username: null },
        { ...playing.sessions[0], session_id: 's2', title: null, username: null },
      ],
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    expect(wrapper.text()).toContain('s1')
    expect(wrapper.text()).toContain('s2')
    expect(wrapper.text()).toContain('?')
  })

  test('the log is fetched and saved, without touching the panel', async () => {
    // OPS-10. NOT through `act`: that re-reads all six lists and clears the
    // last refusal on success, so downloading a log would wipe an error the
    // operator had not read.
    vi.mocked(api.adminSessions).mockResolvedValue(playing as never)
    vi.mocked(api.adminSessionLog).mockResolvedValue('the log' as never)
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    const reads = vi.mocked(api.adminSessions).mock.calls.length
    await press(wrapper, 'Log')
    expect(api.adminSessionLog).toHaveBeenCalledWith('s1')
    expect(click).toHaveBeenCalled()
    expect(vi.mocked(api.adminSessions).mock.calls.length).toBe(reads)
    click.mockRestore()
  })

  test('and a log that could not be fetched says so, on the notice host', async () => {
    vi.mocked(api.adminSessions).mockResolvedValue(playing as never)
    vi.mocked(api.adminSessionLog).mockRejectedValue(new ApiError(404, 'no log for that session'))
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    await press(wrapper, 'Log')
    expect(notice.value).toContain('no log for that session')
  })

  test('and nobody streaming says so', async () => {
    const wrapper = await open()
    await tab(wrapper, 'Sessions')
    expect(wrapper.text()).toContain('Nobody is streaming')
  })
})

describe('providers', () => {
  test('saving a key says what happened and clears the field', async () => {
    vi.mocked(api.adminSetTmdb).mockResolvedValue({ saved: true } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    // Padded on purpose: a key goes out as typed, here and in every other
    // provider form.
    await wrapper.find('#tmdb-key').setValue(' secret ')
    await press(wrapper, 'Save')
    expect(api.adminSetTmdb).toHaveBeenCalledWith({ api_key: ' secret ' })
    expect((wrapper.find('#tmdb-key').element as HTMLInputElement).value).toBe('')
    expect(notice.value).toContain('TMDB key saved')
  })

  test('saving a Fanart.tv key starts the artist artwork prefetch', async () => {
    vi.mocked(api.adminSetFanart).mockResolvedValue({ saved: true } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.find('#fanart-key').setValue(' artist-key ')
    await wrapper
      .findAll('button')
      .find((button) => button.element.previousElementSibling?.id === 'fanart-key')!
      .trigger('click')
    await flushPromises()
    expect(api.adminSetFanart).toHaveBeenCalledWith({ client_key: ' artist-key ' })
    expect((wrapper.find('#fanart-key').element as HTMLInputElement).value).toBe('')
    expect(notice.value).toContain('artist artwork prefetch started')
  })

  test('saving a TheAudioDB premium key leaves the free key as the default', async () => {
    vi.mocked(api.adminSetTheaudiodb).mockResolvedValue({ saved: true } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    expect(wrapper.find('#theaudiodb-key').attributes('placeholder')).toContain(
      'free key is active',
    )
    await wrapper.find('#theaudiodb-key').setValue(' premium-key ')
    await wrapper
      .findAll('button')
      .find((button) => button.element.previousElementSibling?.id === 'theaudiodb-key')!
      .trigger('click')
    await flushPromises()
    expect(api.adminSetTheaudiodb).toHaveBeenCalledWith({ api_key: ' premium-key ' })
    expect((wrapper.find('#theaudiodb-key').element as HTMLInputElement).value).toBe('')
    expect(notice.value).toContain('premium key saved')
  })

  test('an AniDB account that saved but could not log in says both', async () => {
    vi.mocked(api.adminSetAnidb).mockResolvedValue({
      saved: true,
      verified: false,
      error: 'bad password',
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.find('#anidb-user').setValue('someone')
    await wrapper.find('#anidb-pass').setValue('hunter2')
    await wrapper
      .findAll('button')
      .filter((b) => b.text() === 'Save')[2]!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toContain('bad password')
  })

  test('an AniDB account goes out exactly as typed', async () => {
    // The form used to trim the username and the UDP key. The key is a cipher
    // input: trimmed, it encrypts packets AniDB cannot read, and the failure
    // arrives as a decrypt error about a key the admin pasted correctly.
    vi.mocked(api.adminSetAnidb).mockResolvedValue({
      saved: true,
      verified: true,
      error: null,
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.find('#anidb-user').setValue(' someone ')
    await wrapper.find('#anidb-pass').setValue('  hunter2  ')
    await wrapper.find('#anidb-udp').setValue('  a-udp-key  ')
    await wrapper
      .findAll('button')
      .filter((b) => b.text() === 'Save')[2]!
      .trigger('click')
    await flushPromises()
    expect(api.adminSetAnidb).toHaveBeenCalledWith({
      username: ' someone ',
      password: '  hunter2  ',
      udp_api_key: '  a-udp-key  ',
    })
  })

  test('polls segment work only while the provider controls are open', async () => {
    const wrapper = await open()
    expect(api.adminSegmentsStatus).not.toHaveBeenCalled()
    await tab(wrapper, 'Providers')
    expect(wrapper.text()).toContain('Media analysis')
    expect(api.adminSegmentsStatus).toHaveBeenCalled()
  })

  test('Enrich now is offered for ANY provider, not TMDB alone', async () => {
    // A series-only deployment had a permanently greyed button and no
    // explanation of why.
    vi.mocked(api.adminProviders).mockResolvedValue({
      tmdb: { configured: false },
      tvdb: { configured: true },
      anidb: { configured: false },
      available: ['tvdb'],
      chains: {},
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    const enrich = wrapper.findAll('button').find((b) => b.text() === 'Enrich now')!
    expect(enrich.attributes('disabled')).toBeUndefined()
  })

  test('and with none configured it is disabled, and says why IN TEXT', async () => {
    // Not in a `title` on the disabled button: a disabled button is out of the
    // tab order, so the explanation would be in the one place a keyboard user
    // cannot reach.
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    const enrich = wrapper.findAll('button').find((b) => b.text() === 'Enrich now')!
    expect(enrich.attributes('disabled')).toBeDefined()
    expect(wrapper.text()).toContain('Configure a metadata provider first')
  })

  test('a TVDB key with no PIN does not send an empty one', async () => {
    vi.mocked(api.adminSetTvdb).mockResolvedValue({ saved: true } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.find('#tvdb-key').setValue('secret')
    await wrapper
      .findAll('button')
      .filter((b) => b.text() === 'Save')[1]!
      .trigger('click')
    await flushPromises()
    expect(api.adminSetTvdb).toHaveBeenCalledWith({ api_key: 'secret' })

    // And a pin that is sent goes out as typed — a PIN is a credential, not
    // a number the form may tidy.
    await wrapper.find('#tvdb-key').setValue('secret')
    await wrapper.find('#tvdb-pin').setValue(' 1234 ')
    await wrapper
      .findAll('button')
      .filter((b) => b.text() === 'Save')[1]!
      .trigger('click')
    await flushPromises()
    expect(api.adminSetTvdb).toHaveBeenLastCalledWith({ api_key: 'secret', pin: ' 1234 ' })
  })

  test('each Disconnect says which provider it disconnects', async () => {
    // Three buttons reading "Disconnect" in a row: the visible text cannot
    // tell them apart, so the accessible name has to. It still contains the
    // word on screen, so a spoken "disconnect TMDB" matches what is there.
    vi.mocked(api.adminProviders).mockResolvedValue({
      tmdb: { configured: true },
      tvdb: { configured: true },
      anidb: { configured: true },
      chains: {},
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    const named = wrapper
      .findAll('button')
      .filter((b) => b.text() === 'Disconnect')
      .map((b) => b.attributes('aria-label'))
    expect(named).toEqual(['Disconnect TMDB', 'Disconnect TheTVDB', 'Disconnect AniDB'])

    // Armed, the name follows the text — a button that now says "Really
    // disconnect?" must not still be announced as the offer it replaced.
    const tmdb = wrapper
      .findAll('button')
      .find((b) => b.attributes('aria-label') === 'Disconnect TMDB')!
    await tmdb.trigger('click')
    await flushPromises()
    expect(
      wrapper
        .findAll('button')
        .some((b) => b.attributes('aria-label') === 'Really disconnect TMDB?'),
    ).toBe(true)
  })

  test('a chain is a draft until it is applied', async () => {
    vi.mocked(api.adminProviders).mockResolvedValue({
      tmdb: { configured: true },
      tvdb: { configured: false },
      anidb: { configured: false },
      chains: { movies: { default: ['tmdb', 'tvdb'], order: ['tmdb', 'tvdb'] } },
    } as never)
    vi.mocked(api.adminSetChain).mockResolvedValue({ ok: true } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.text() === 'Apply')!
        .attributes('disabled'),
    ).toBeDefined()

    // UI-12: the order is reachable from the keyboard, not by dragging alone.
    await wrapper.findAll('[role="listitem"]')[1]!.trigger('keydown', { key: 'ArrowUp' })
    await flushPromises()
    expect(api.adminSetChain).not.toHaveBeenCalled()

    await press(wrapper, 'Apply')
    expect(api.adminSetChain).toHaveBeenCalledWith('movies', { order: ['tvdb', 'tmdb'] })
    expect(notice.value).toContain('Existing matches are unchanged')
    // The draft is dropped: it is what the hub holds now, so there is nothing
    // left to apply and nothing left to reset.
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.text() === 'Apply')!
        .attributes('disabled'),
    ).toBeDefined()
    expect(wrapper.findAll('button').some((b) => b.text() === 'Reset')).toBe(false)
  })

  test('and Reset puts a draft back without asking the hub', async () => {
    vi.mocked(api.adminProviders).mockResolvedValue({
      tmdb: { configured: true },
      tvdb: { configured: false },
      anidb: { configured: false },
      chains: { movies: { default: ['tmdb', 'tvdb'], order: ['tmdb', 'tvdb'] } },
    } as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.findAll('[role="listitem"]')[1]!.trigger('keydown', { key: 'ArrowUp' })
    await flushPromises()
    await press(wrapper, 'Reset')
    expect(api.adminSetChain).not.toHaveBeenCalled()
    expect(wrapper.findAll('[role="listitem"]')[0]!.text()).toContain('tmdb')
  })

  test('and Applying says so, and cannot be pressed twice', async () => {
    vi.mocked(api.adminProviders).mockResolvedValue({
      tmdb: { configured: true },
      tvdb: { configured: false },
      anidb: { configured: false },
      chains: { movies: { default: ['tmdb', 'tvdb'], order: ['tmdb', 'tvdb'] } },
    } as never)
    const slow = held({ ok: true })
    vi.mocked(api.adminSetChain).mockReturnValue(slow.promise as never)
    const wrapper = await open()
    await tab(wrapper, 'Providers')
    await wrapper.findAll('[role="listitem"]')[1]!.trigger('keydown', { key: 'ArrowUp' })
    await flushPromises()

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Apply')!
      .trigger('click')
    const applying = wrapper.findAll('button').find((b) => b.text() === 'Applying…')!
    expect(applying.attributes('disabled')).toBeDefined()
    slow.settle()
    await flushPromises()
  })
})

test('analysis status distinguishes unknown, disabled and offline reports', async () => {
  vi.mocked(api.adminSegmentsStatus).mockResolvedValue({
    collections: [
      {
        collection_id: 'a',
        mediahost_id: 'host',
        mediahost_name: 'NAS',
        name: 'shows',
        connected: true,
        media_type: 'series',
        pending_loudness: 12,
        pending_sources: 7,
        enabled: true,
      },
      {
        collection_id: 'b',
        mediahost_id: 'old',
        mediahost_name: 'Older',
        name: 'anime',
        connected: true,
        media_type: 'anime',
        pending_loudness: null,
        pending_sources: null,
        enabled: null,
      },
      {
        collection_id: 'c',
        mediahost_id: 'disabled',
        mediahost_name: 'Disabled',
        name: 'shows',
        connected: true,
        media_type: 'series',
        pending_loudness: 12,
        pending_sources: 0,
        enabled: false,
      },
      {
        collection_id: 'd',
        mediahost_id: 'offline',
        mediahost_name: 'Offline',
        name: 'shows',
        connected: false,
        media_type: 'series',
        pending_loudness: null,
        pending_sources: null,
        enabled: null,
      },
      {
        collection_id: 'e',
        mediahost_id: 'host',
        mediahost_name: 'NAS',
        name: 'movies',
        connected: true,
        media_type: 'movies',
        pending_sources: 0,
        pending_loudness: 0,
        enabled: true,
      },
    ],
  })
  const wrapper = await open()
  await tab(wrapper, 'Providers')
  expect(wrapper.text()).toContain('7 sources awaiting analysis')
  expect(wrapper.text()).toContain('12 sources awaiting measurement')
  const movies = wrapper.findAll('li').find((row) => row.text().includes('NAS/movies'))!
  expect(movies.text()).toContain('0 sources awaiting measurement')
  expect(movies.text()).not.toContain('Skip points:')
  const disabled = wrapper.findAll('li').find((row) => row.text().includes('Disabled/shows'))!
  expect(disabled.text()).toContain('12 sources awaiting measurement')
  expect(wrapper.text()).toContain('waiting for mediahost status')
  expect(wrapper.text()).toContain('detection disabled')
  expect(wrapper.text()).not.toContain('Find skip points now')
  expect(wrapper.text()).not.toContain('episodes done since')
})

test('movie-only analysis reports loudness', async () => {
  vi.mocked(api.adminSegmentsStatus).mockResolvedValue({
    collections: [
      {
        collection_id: 'movies',
        mediahost_id: 'host',
        mediahost_name: 'NAS',
        name: 'movies',
        media_type: 'movies',
        connected: true,
        pending_sources: 0,
        pending_loudness: 3,
        enabled: true,
      },
    ],
  })
  const wrapper = await open()
  await tab(wrapper, 'Providers')
  expect(wrapper.text()).toContain('3 sources awaiting measurement')
  expect(wrapper.text()).not.toContain('Find skip points now')
})
