/// The panel itself. Nothing mounted this component, so everything in it
/// except the option rows could be deleted silently — the "No matches" line,
/// the failure sentence, the click-catcher, the thumbnails' eager loading, the
/// listbox's label and the retry's handler all passed the suite when removed.

import { mount } from '@vue/test-utils'
import { describe, expect, test, vi } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import type { SearchRow } from '../src/domain/search-nav.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))

const SearchPanel = (await import('../src/components/SearchPanel.vue')).default

const films = { id: 'films', name: 'Films', media_type: 'movies' }
const music = { id: 'music', name: 'Music', media_type: 'music' }
const item = (id: string) => ({ id, title: id, kind: 'movie' }) as ItemRowI64

const rows: SearchRow[] = [
  { kind: 'library', library: films, total: 9, shown: 2 },
  { kind: 'item', item: item('heat'), library: films },
  { kind: 'item', item: item('the insider'), library: films },
]

function panel(props: Partial<InstanceType<typeof SearchPanel>['$props']> = {}) {
  return mount(SearchPanel, {
    props: {
      query: 'heat',
      rows,
      failed: [],
      allFailed: false,
      searching: false,
      highlight: -1,
      ...props,
    },
  })
}

describe('the rows', () => {
  test('a heading says how many of how many, and where it leads', async () => {
    const wrapper = panel()
    const heading = wrapper.findAll('[role="option"]')[0]!
    expect(heading.text()).toContain('Films')
    expect(heading.text()).toContain('2 of 9')
    await heading.trigger('click')
    expect(wrapper.emitted('openLibrary')).toEqual([['films']])
  })

  test('and says it OUT LOUD as what pressing it does', async () => {
    // An accessible name comes from the content when there is any, and this
    // row's content is a library's name beside a bare number: arrowing onto
    // the first result announced "Films 2 of 9". The `title` explaining it is
    // a tooltip and loses to content; a label wins.
    const heading = panel().findAll('[role="option"]')[0]!
    expect(heading.attributes('aria-label')).toBe('Show everything in Films, 2 of 9')
  })

  test('a hit opens its item, under the library it was found in', async () => {
    const wrapper = panel()
    await wrapper.findAll('[role="option"]')[1]!.trigger('click')
    expect(wrapper.emitted('openItem')).toEqual([['heat', 'films']])
  })

  test('the lit row is marked, and told apart from a hover', async () => {
    // The mouse rests where it last was while the arrows move somewhere else,
    // so the row Enter opens has to be the obvious one.
    const wrapper = panel({ highlight: 1 })
    const lit = wrapper.findAll('[role="option"]')[1]!
    expect(lit.classes()).toContain('row-on')
    expect(wrapper.findAll('.row-on')).toHaveLength(1)
  })

  test('and the thumbnails are not deferred', async () => {
    // `loading="lazy"` was inherited from the page this replaced, and in here
    // those images never loaded at all: every row sat with an empty 34px box.
    // A dropdown is at most fifteen thumbnails, all on screen the instant it
    // opens.
    for (const img of panel().findAll('img')) {
      expect(img.attributes('loading')).toBeUndefined()
    }
  })

  test('the same item under two libraries is two rows, keyed apart', () => {
    // Membership is many-to-many, so an item id alone is not unique here.
    const twice: SearchRow[] = [
      { kind: 'library', library: films, total: 1, shown: 1 },
      { kind: 'item', item: item('heat'), library: films },
      { kind: 'library', library: music, total: 1, shown: 1 },
      { kind: 'item', item: item('heat'), library: music },
    ]
    const wrapper = panel({ rows: twice })
    expect(wrapper.findAll('[role="option"]')).toHaveLength(4)
  })
})

describe('what it says when there are no rows', () => {
  test('nothing matched, quoting what was searched for', () => {
    const wrapper = panel({ rows: [], query: 'zzz' })
    expect(wrapper.find('[role="status"]').text()).toContain('zzz')
  })

  test('and the label names the query the rows belong to', () => {
    // Which is not always what is in the box: the rows stay while their
    // replacement loads.
    expect(panel({ query: 'heat' }).find('[role="listbox"]').attributes('aria-label')).toBe(
      'Results for heat',
    )
  })
})

describe('when a library could not be asked', () => {
  test('it is named, and the results are called incomplete', () => {
    const wrapper = panel({ failed: ['Music'] })
    expect(wrapper.text()).toContain('Could not search Music')
    expect(wrapper.text()).toContain('incomplete')
    // Not "nothing matches": the rows that did arrive are still there.
    expect(wrapper.text()).not.toContain('No matches')
  })

  test('all of them failing says the hub did not answer', () => {
    const wrapper = panel({ rows: [], failed: ['Films', 'Music'], allFailed: true })
    expect(wrapper.text()).toContain('the hub did not answer')
    expect(wrapper.text()).not.toContain('incomplete')
  })

  test('and either way it is announced, not just drawn', () => {
    // Outside the listbox, because a listbox may contain nothing but options:
    // a paragraph inside one can be dropped from the accessibility tree.
    const wrapper = panel({ failed: ['Music'] })
    const statuses = wrapper.findAll('[role="status"]')
    expect(statuses.some((s) => s.text().includes('Could not search'))).toBe(true)
    expect(wrapper.find('[role="listbox"]').text()).not.toContain('Could not search')
  })

  test('Try again asks the caller to ask again', async () => {
    const wrapper = panel({ failed: ['Music'] })
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Try again')!
      .trigger('click')
    expect(wrapper.emitted('retry')).toHaveLength(1)
  })
})

describe('while the next answer is on its way', () => {
  test('it says so, without moving the rows', () => {
    // In the flow this pushed the whole list down 17px on every debounced
    // keystroke and back up when it settled.
    const wrapper = panel({ searching: true })
    const badge = wrapper.findAll('[role="status"]').find((s) => s.text() === 'updating')!
    expect(badge.classes()).toContain('float-right')
    expect(badge.classes()).toContain('sticky')
  })

  test('and says nothing when it is not searching', () => {
    expect(panel().text()).not.toContain('updating')
  })
})

describe('dismissing', () => {
  test('a click anywhere else lands on the sheet', async () => {
    // Rather than on whatever it was over: closing the panel by pressing a
    // card behind it would both close it and open the card.
    const wrapper = panel()
    const sheet = wrapper.find('[data-testid="search-sheet"]')
    expect(sheet.classes()).toEqual(expect.arrayContaining(['fixed', 'inset-0']))
    await sheet.trigger('click')
    expect(wrapper.emitted('close')).toHaveLength(1)
  })
})
