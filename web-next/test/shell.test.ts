/// The shell, mounted. These are the behaviours the old app learned by being
/// wrong about them: a combobox that lied to a screen reader, a menu whose
/// dismissing click went through to the page behind it, and a filter that
/// followed you home.

import { flushPromises, mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'
import { nextTick } from 'vue'
import { createMemoryHistory, createRouter } from 'vue-router'

import AppShell from '../src/components/AppShell.vue'
import MenuPopover from '../src/components/MenuPopover.vue'
import SearchBox from '../src/components/SearchBox.vue'

const Blank = { template: '<div />' }

function testRouter(start = '/') {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: Blank },
      { path: '/admin', name: 'admin', component: Blank },
      { path: '/settings', name: 'settings', component: Blank },
      { path: '/library/:library', name: 'library', component: Blank },
      { path: '/library/:library/item/:id', name: 'detail', component: Blank },
      {
        path: '/library/:library/item/:id/season/:season',
        name: 'season',
        component: Blank,
      },
      { path: '/library/:library/item/:id/play', name: 'player', component: Blank },
    ],
  })
  void router.push(start)
  return router
}

async function shell(start = '/', props: Record<string, unknown> = {}) {
  const router = testRouter(start)
  await router.isReady()
  const wrapper = mount(AppShell, {
    props: {
      libraries: [{ id: 'L1', name: 'Films', media_type: 'movies' }],
      username: 'claude',
      admin: false,
      ...props,
    },
    global: { plugins: [router] },
  })
  return { wrapper, router }
}

describe('the search box', () => {
  // Every screen, not the two that came to mind: `detail` and `season` were in
  // no test at all, and a mutation that put a filter box on both passed.
  test.each([
    ['/', true, 'combobox'],
    ['/library/L1', true, undefined],
    ['/library/L1/item/i7', false, undefined],
    ['/library/L1/item/i7/season/all', false, undefined],
    ['/library/L1/item/i7/play', false, undefined],
    ['/admin', false, undefined],
    ['/settings', false, undefined],
  ])('on %s: box=%s role=%s', async (path, present, role) => {
    const { wrapper } = await shell(path)
    expect(wrapper.findComponent(SearchBox).exists()).toBe(present)
    if (present) {
      // A library filter has nothing to pop up, and telling a screen reader it
      // is a combobox promises a list that never arrives.
      expect(wrapper.find('input').attributes('role')).toBe(role)
    }
  })

  test('the panel reports its own state, and names a list only while it has one', () => {
    const collapsed = mount(SearchBox, {
      props: {
        modelValue: 'heat',
        panel: true,
        shown: false,
        highlight: -1,
        listId: 'results',
        optionId: (i: number) => `option-${i}`,
      },
    })
    expect(collapsed.find('input').attributes('aria-expanded')).toBe('false')
    // Pointing at an id that is not in the document is worse than saying
    // nothing.
    expect(collapsed.find('input').attributes('aria-controls')).toBeUndefined()
    // Same rule for the highlight: with nothing lit there is no option-(-1) in
    // the document to point at.
    expect(collapsed.find('input').attributes('aria-activedescendant')).toBeUndefined()

    const open = mount(SearchBox, {
      props: {
        modelValue: 'heat',
        panel: true,
        shown: true,
        highlight: 2,
        listId: 'results',
        optionId: (i: number) => `option-${i}`,
      },
    })
    expect(open.find('input').attributes('aria-expanded')).toBe('true')
    expect(open.find('input').attributes('aria-controls')).toBe('results')
    // The highlight is announced ON the input, so the caret never leaves the
    // field somebody is still typing in.
    expect(open.find('input').attributes('aria-activedescendant')).toBe('option-2')
  })

  test('the clear button appears only with something to clear, and clears', async () => {
    // Both halves. Asserting only the absence let `v-if="false"` pass.
    const props = {
      panel: true,
      shown: false,
      highlight: -1,
      listId: 'r',
      optionId: (i: number) => `o-${i}`,
    }
    const empty = mount(SearchBox, { props: { ...props, modelValue: '' } })
    expect(empty.find('button[title="Clear"]').exists()).toBe(false)

    const full = mount(SearchBox, { props: { ...props, modelValue: 'heat' } })
    await full.find('button[title="Clear"]').trigger('click')
    expect(full.emitted('clear')).toHaveLength(1)
  })

  test('both a click and a focus bring a dismissed panel back', async () => {
    // Not the same event. Opening a library from the panel leaves focus in the
    // box, so coming back fires no focus event and only the click can reach it.
    const props = {
      modelValue: 'heat',
      panel: true,
      shown: false,
      highlight: -1,
      listId: 'r',
      optionId: (i: number) => `o-${i}`,
    }
    const clicked = mount(SearchBox, { props })
    await clicked.find('input').trigger('click')
    expect(clicked.emitted('reopen')).toHaveLength(1)

    const focused = mount(SearchBox, { props })
    await focused.find('input').trigger('focus')
    expect(focused.emitted('reopen')).toHaveLength(1)
  })

  test('it is not a native search field', () => {
    // `type="search"` brings the UA's own ✕, which would sit on top of this
    // one, and in some browsers Escape reverts the value — losing the query is
    // not what Escape is for here.
    const box = mount(SearchBox, {
      props: {
        modelValue: 'heat',
        panel: true,
        shown: false,
        highlight: -1,
        listId: 'r',
        optionId: (i: number) => `o-${i}`,
      },
    })
    expect(box.find('input').attributes('type')).toBe('text')
  })
})

describe('the menus', () => {
  test('a dismissing click lands on the sheet and nowhere else', async () => {
    // A document listener let the click that closed the menu also act on
    // whatever was behind it.
    const { wrapper } = await shell('/')
    await wrapper.findAll('button')[0]!.trigger('click')
    expect(wrapper.find('[role="menu"]').exists()).toBe(true)

    const sheet = wrapper.find('[data-testid="menu-sheet"]')
    expect(sheet.exists()).toBe(true)
    await sheet.trigger('click')
    expect(wrapper.find('[role="menu"]').exists()).toBe(false)
  })

  test('escape closes it, and only while it is open', async () => {
    // Both edges. The one that matters is AFTER close — a listener that
    // outlives its menu swallows an Escape the player wants — and testing only
    // before-open let the removal be deleted without failing anything.
    const popover = mount(MenuPopover, { props: { open: false, align: 'left' } })
    const escape = () => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))

    escape()
    expect(popover.emitted('close')).toBeUndefined()

    await popover.setProps({ open: true })
    escape()
    expect(popover.emitted('close')).toHaveLength(1)

    await popover.setProps({ open: false })
    escape()
    expect(popover.emitted('close')).toHaveLength(1)
  })

  test('the sheet covers the page it is dismissing over', async () => {
    // happy-dom does no layout, so a click on the sheet would still be caught
    // if the sheet had no size at all: `class=""` passed. Its positioning is
    // the behaviour, so its positioning is what gets asserted.
    const { wrapper } = await shell('/')
    await wrapper.findAll('button')[0]!.trigger('click')
    const sheet = wrapper.find('[data-testid="menu-sheet"]')
    expect(sheet.classes()).toEqual(expect.arrayContaining(['fixed', 'inset-0']))
    // Above the music dock at 13, below the menu it belongs to.
    expect(sheet.classes()).toContain('z-14')
  })

  test('the keyboard can reach and walk the menu', async () => {
    // `role="menuitem"` puts a screen reader into focus mode, where its own
    // browse keys stop working — so a menu with the role and none of the keys
    // leaves that user with nothing that moves.
    const { wrapper } = await shell('/', {
      libraries: [
        { id: 'L1', name: 'Films', media_type: 'movies' },
        { id: 'L2', name: 'Music', media_type: 'music' },
      ],
    })
    // Attached, because focus does nothing to a detached tree.
    const attached = mount(AppShell, {
      props: {
        libraries: [
          { id: 'L1', name: 'Films', media_type: 'movies' },
          { id: 'L2', name: 'Music', media_type: 'music' },
        ],
        username: 'claude',
        admin: false,
      },
      global: { plugins: [wrapper.vm.$router] },
      attachTo: document.body,
    })
    await attached.findAll('button')[0]!.trigger('click')
    await nextTick()

    const items = () => attached.findAll('[role="menuitem"]').map((i) => i.element)
    // Opening puts focus on the first row rather than leaving it on the
    // trigger, which is what the menu pattern promises.
    expect(document.activeElement).toBe(items()[0])

    const key = (k: string) => window.dispatchEvent(new KeyboardEvent('keydown', { key: k }))
    key('ArrowDown')
    expect(document.activeElement).toBe(items()[1])
    key('End')
    expect(document.activeElement).toBe(items().at(-1))
    // Wrapping, per the pattern: down from the last is the first.
    key('ArrowDown')
    expect(document.activeElement).toBe(items()[0])
    key('ArrowUp')
    expect(document.activeElement).toBe(items().at(-1))
    key('Home')
    expect(document.activeElement).toBe(items()[0])
    attached.unmount()
  })

  test('closing hands focus back rather than dropping it', async () => {
    // The focused row is removed from the DOM on close. Without this, focus
    // falls to <body> and the next Tab restarts at the top of the document.
    const router = testRouter('/')
    await router.isReady()
    const wrapper = mount(AppShell, {
      props: { libraries: [], username: 'claude', admin: false },
      global: { plugins: [router] },
      attachTo: document.body,
    })
    const trigger = wrapper.findAll('button')[0]!.element as HTMLElement
    trigger.focus()
    await wrapper.findAll('button')[0]!.trigger('click')
    await nextTick()
    expect(document.activeElement).not.toBe(trigger)

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    await flushPromises()
    expect(document.activeElement).toBe(trigger)
    wrapper.unmount()
  })

  // Both orders. Testing one direction let a mutation that stopped the nav
  // button closing the profile menu pass, because the test never opened them
  // that way round.
  test.each([
    ['nav then profile', 0, -1],
    ['profile then nav', -1, 0],
  ])('opening one menu closes the other (%s)', async (_name, first, second) => {
    const { wrapper } = await shell('/')
    await wrapper.findAll('button').at(first)!.trigger('click')
    expect(wrapper.findAll('[role="menu"]')).toHaveLength(1)
    await wrapper.findAll('button').at(second)!.trigger('click')
    expect(wrapper.findAll('[role="menu"]')).toHaveLength(1)
  })

  test('admin is offered only to an admin', async () => {
    const plain = await shell('/', { admin: false })
    await plain.wrapper.findAll('button').at(-1)!.trigger('click')
    expect(plain.wrapper.text()).not.toContain('Admin')

    const boss = await shell('/', { admin: true })
    await boss.wrapper.findAll('button').at(-1)!.trigger('click')
    expect(boss.wrapper.text()).toContain('Admin')
  })

  test('the menu says where you already are, and where you are not', async () => {
    // Only the positive half was asserted, so `:here="true"` on every row
    // passed — a menu on which everything is the current page says nothing.
    // Two libraries, because with one the mutation is indistinguishable.
    const { wrapper } = await shell('/library/L1', {
      libraries: [
        { id: 'L1', name: 'Films', media_type: 'movies' },
        { id: 'L2', name: 'Music', media_type: 'music' },
      ],
    })
    await wrapper.findAll('button')[0]!.trigger('click')
    const current = wrapper
      .findAll('[role="menuitem"]')
      .filter((i) => i.attributes('aria-current') === 'page')
      .map((i) => i.text())
    expect(current).toEqual(['Films'])
  })

  test('the trigger says whether its menu is open', async () => {
    const { wrapper } = await shell('/')
    const trigger = () => wrapper.findAll('button')[0]!
    expect(trigger().attributes('aria-expanded')).toBe('false')
    await trigger().trigger('click')
    expect(trigger().attributes('aria-expanded')).toBe('true')
    // And its own trigger closes it again.
    await trigger().trigger('click')
    expect(wrapper.find('[role="menu"]').exists()).toBe(false)
  })

  test('signing out is offered to whoever mounted the shell', async () => {
    const { wrapper } = await shell('/')
    await wrapper.findAll('button').at(-1)!.trigger('click')
    const out = wrapper.findAll('[role="menuitem"]').find((i) => i.text() === 'Sign out')!
    await out.trigger('click')
    expect(wrapper.emitted('signOut')).toHaveLength(1)
  })
})

describe('leaving a screen', () => {
  test('going home clears a standing filter', async () => {
    // It would otherwise follow you there and read as missing items.
    const { wrapper } = await shell('/library/L1')
    await wrapper.find('input').setValue('heat')
    expect((wrapper.find('input').element as HTMLInputElement).value).toBe('heat')

    await wrapper.findAll('button')[0]!.trigger('click')
    const home = wrapper.findAll('[role="menuitem"]').find((i) => i.text() === 'Home')!
    await home.trigger('click')
    await wrapper.vm.$nextTick()
    expect(wrapper.findComponent(SearchBox).props('modelValue')).toBe('')
  })
})
