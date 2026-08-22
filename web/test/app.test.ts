/// The app root: which of the four things is on screen, and the one ordering
/// rule that cost a transcoder slot every time it was wrong.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, onUnmounted, ref } from 'vue'
import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { createMemoryHistory, createRouter } from 'vue-router'

import AppShell from '../src/components/AppShell.vue'
import type { Phase } from '../src/domain/auth.ts'

const phase = ref<Phase>('boot')
const bootError = ref('')
const note = ref('')
const start = vi.fn()

/// Records the client App hands it: clearing a client nobody is using would
/// pass every other test in here.
const bootGot: { queryClient?: unknown } = {}
vi.mock('../src/composables/boot.ts', () => ({
  useBoot: (queryClient: unknown) => {
    bootGot.queryClient = queryClient
    return {
      phase,
      bootError,
      note,
      setupAvailable: ref(false),
      setupUrl: ref(undefined),
      start,
    }
  },
}))
vi.mock('../src/api/generated/kahawai.ts', () => ({
  listLibraries: vi.fn(async () => ({ libraries: [] })),
  listItems: vi.fn(async () => ({ items: [], total: 0, limit: 20, offset: 0 })),
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))
vi.mock('../src/api/session.ts', () => ({
  signOut: vi.fn(),
  // The header's name and whether the Admin row is offered.
  whoAmI: () => ({ username: 'claude', admin: false }),
}))

const { signOut } = await import('../src/api/session.ts')
const App = (await import('../src/App.vue')).default

const Blank = { template: '<div />' }

/// Records its own teardown, so the ordering can be asserted rather than
/// inferred from the address.
const order: string[] = []
const Watching = defineComponent({
  setup() {
    onUnmounted(() => order.push('player gone'))
    return () => h('div', 'playing')
  },
})

function app(at = '/') {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: Blank },
      { path: '/settings', name: 'settings', component: Blank },
      { path: '/library/:library/item/:id/play', name: 'player', component: Watching },
    ],
  })
  void router.push(at)
  return router
}

beforeEach(() => {
  phase.value = 'boot'
  bootError.value = ''
  note.value = ''
  vi.mocked(signOut).mockResolvedValue(null)
})
afterEach(() => vi.clearAllMocks())

describe('what is on screen', () => {
  test('booting shows nothing at all, and announces nothing either', async () => {
    // Not a spinner. One that flashes for 40 ms on every load is worse than a
    // blank moment, and a boot slow enough to notice ends in an error instead.
    // Nothing for a screen reader either: there is no screen to have moved
    // between yet, and "Home" over a blank page is a lie about both.
    const router = app()
    await router.isReady()
    const queryClient = new QueryClient()
    const wrapper = mount(App, {
      global: {
        plugins: [
          router,
          [VueQueryPlugin, { queryClient }] as [
            typeof VueQueryPlugin,
            { queryClient: QueryClient },
          ],
        ],
      },
    })
    // boot clears the cache when a session ends; it has to be the app's own.
    expect(bootGot.queryClient).toBe(queryClient)
    expect(wrapper.text()).toBe('')
    expect(wrapper.find('form').exists()).toBe(false)
    expect(wrapper.find('[role="status"]').exists()).toBe(false)
  })

  test('a hub that did not start is not a sign-in screen', async () => {
    // There is nothing to sign in TO while it is unreachable, and a signed-in
    // viewer's tokens are still perfectly good.
    const router = app()
    await router.isReady()
    bootError.value = 'Could not reach the hub.'
    phase.value = 'login'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    expect(wrapper.text()).toContain('Could not start.')
    expect(wrapper.text()).toContain('Could not reach the hub.')
    expect(wrapper.find('input').exists()).toBe(false)
  })

  test('and the gate is titled by the gate, not by the route behind it', async () => {
    // A session that ENDS on its own leaves the route alone, so signing back in
    // returns you to the page you were reading — which means the router still
    // says `libraries` while a sign-in form is on screen. The tab strip read
    // "Home · kahawai" over it, and so did the screen reader.
    const router = app()
    await router.isReady()
    phase.value = 'login'
    mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    await flushPromises()
    expect(document.title).toBe('Sign in · kahawai')
  })

  test('and a boot error outranks the gate, the way the template does', async () => {
    // `bootError` outranks every phase, so the phase underneath it says
    // nothing about what is on screen: this exact state — a hub that did not
    // answer, with the phase left at `login` — was titled "Sign in" over
    // "Could not start."
    const router = app()
    await router.isReady()
    bootError.value = 'Could not reach the hub.'
    phase.value = 'login'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    await flushPromises()
    expect(wrapper.text()).toContain('Could not start.')
    expect(document.title).toBe('Unavailable · kahawai')
  })

  test('and its retry asks again rather than reloading', async () => {
    const router = app()
    await router.isReady()
    bootError.value = 'Could not reach the hub.'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    start.mockClear()
    await wrapper.find('button').trigger('click')
    expect(start).toHaveBeenCalledTimes(1)
  })
})

describe('what is asked for before there is a session', () => {
  test('nothing', async () => {
    // The shell exists from the first frame and the header lists your
    // libraries, so an ungated query fires on the boot and sign-in screens —
    // a guaranteed 401, and two of them on first-run setup where no refresh
    // cookie can exist to recover with.
    const { listLibraries } = await import('../src/api/generated/kahawai.ts')
    const router = app()
    await router.isReady()
    for (const at of ['boot', 'login', 'setup'] as const) {
      phase.value = at
      mount(App, { global: { plugins: [router, VueQueryPlugin] } })
      await flushPromises()
      expect(listLibraries, `asked while ${at}`).not.toHaveBeenCalled()
    }

    phase.value = 'app'
    mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    await flushPromises()
    expect(listLibraries).toHaveBeenCalled()
  })
})

describe('signing out', () => {
  test('unmounts what is playing before it destroys the credentials', async () => {
    // The order is the whole subject. Clearing the tokens first meant the
    // player unmounted afterwards, so its final progress report went out
    // unauthenticated, 401'd, and never landed — one leaked transcoder slot
    // per sign-out.
    //
    // Asserted on the UNMOUNT, not on the address: the navigation alone
    // satisfies a route check, and it is the flush after it that guarantees
    // the teardown ran while the bearer was still good.
    order.length = 0
    const router = app('/library/L1/item/e1/play')
    // Mounted in the SHELL rather than the route, like the music queue: it
    // survives navigation, so only leaving the app takes it down.
    await router.isReady()
    phase.value = 'app'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })
    expect(wrapper.text()).toContain('playing')

    let shellUp = true
    vi.mocked(signOut).mockImplementation(async () => {
      // Whatever the shell owns has to be gone too, not just the route's
      // component: the music queue lives outside the router because it
      // survives navigation, so a route change alone would leave it mounted
      // and its teardown would go out with no bearer.
      shellUp = wrapper.findComponent(AppShell).exists()
      order.push('credentials gone')
      return null
    })

    await wrapper.findAll('button').at(-1)!.trigger('click')
    const out = wrapper.findAll('[role="menuitem"]').find((i) => i.text() === 'Sign out')!
    await out.trigger('click')
    await flushPromises()

    expect(order).toEqual(['player gone', 'credentials gone'])
    expect(shellUp).toBe(false)
  })

  test('even when there is nowhere to navigate to', async () => {
    // Signing out from the home screen: `replace` to the route you are
    // already on resolves immediately, so if the flush came from the
    // navigation rather than from leaving the app, there would be none.
    order.length = 0
    const router = app('/')
    await router.isReady()
    phase.value = 'app'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })

    let shellUp = true
    vi.mocked(signOut).mockImplementation(async () => {
      shellUp = wrapper.findComponent(AppShell).exists()
      return null
    })

    await wrapper.findAll('button').at(-1)!.trigger('click')
    await wrapper
      .findAll('[role="menuitem"]')
      .find((i) => i.text() === 'Sign out')!
      .trigger('click')
    await flushPromises()
    expect(shellUp).toBe(false)
  })

  test('and reaches the address bar, so a reload does not restore the page', async () => {
    // `replace` rather than push: the address bar is what a reload reads, and
    // leaving it on the previous account's item is what this prevents.
    const router = app('/settings')
    await router.isReady()
    phase.value = 'app'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })

    await wrapper.findAll('button').at(-1)!.trigger('click')
    await wrapper
      .findAll('[role="menuitem"]')
      .find((i) => i.text() === 'Sign out')!
      .trigger('click')
    await flushPromises()

    expect(router.currentRoute.value.path).toBe('/')
    // Replaced, so Back does not return to the signed-in page.
    router.back()
    await flushPromises()
    expect(router.currentRoute.value.path).not.toBe('/settings')
  })

  test('what could not be told to the hub is reported, not thrown', async () => {
    const { notice } = await import('../src/composables/notices.ts')
    vi.mocked(signOut).mockResolvedValue('Signed out here, but the hub was not told.')
    const router = app()
    await router.isReady()
    phase.value = 'app'
    const wrapper = mount(App, { global: { plugins: [router, VueQueryPlugin] } })

    await wrapper.findAll('button').at(-1)!.trigger('click')
    await wrapper
      .findAll('[role="menuitem"]')
      .find((i) => i.text() === 'Sign out')!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toContain('the hub was not told')
  })
})
