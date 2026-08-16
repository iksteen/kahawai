/// What the document is called, per screen.
///
/// UI-17. A single-page app does not reload, so nothing tells a screen reader
/// that the screen changed: the browser announces the document title on a real
/// navigation and has no idea this one happened. The title is also what a tab
/// strip, a bookmark and a shared link show, and "kahawai" for every one of
/// nineteen screens is the same as no title at all.
///
/// The NAME first, because a tab strip truncates from the right and the site is
/// the part you already know.

import type { Phase } from './auth.ts'
import type { RouteName } from './routes.ts'

/// Not every screen is a route. The app root shows a gate — starting, first-run
/// setup, sign in, a hub that did not answer — while the router still says
/// whichever page the session ended on, and "Home · kahawai" over a sign-in
/// form is a claim both the tab strip and the screen reader repeat.
///
/// `failed` is not a phase: a boot error outranks every one of them, and can
/// land over a running app when a retry fails.
export type Screen = RouteName | Exclude<Phase, 'app'> | 'failed'

const SCREENS: Record<Screen, string> = {
  libraries: 'Home',
  library: 'Library',
  detail: '',
  season: '',
  settings: 'Settings',
  admin: 'Admin',
  player: '',
  // Nothing, on purpose. Boot is over in about forty milliseconds and shows a
  // deliberately blank page; retitling the tab for that long is the flicker
  // the blank page exists to avoid, and `index.html` already says this.
  boot: '',
  setup: 'Set up',
  login: 'Sign in',
  failed: 'Unavailable',
}

/// The screens with no word of their own: they are titled by the thing they are
/// showing, and that arrives a round trip after the route does. Worth naming
/// separately from `SCREENS[…] === ''` because the announcement waits on it —
/// see `useDocumentTitle`. `library` is here despite having a placeholder: the
/// placeholder is for the tab strip, and announcing "Library" when the answer
/// to "where am I" is "Films" wastes the one announcement a screen gets.
const AWAITS: ReadonlySet<Screen> = new Set<Screen>(['library', 'detail', 'season', 'player'])

export function awaitsName(screen: Screen): boolean {
  return AWAITS.has(screen)
}

/// `named` is what the screen is showing, when it knows: an item's title, a
/// library's name. It arrives a round trip after the route does, so the title
/// changes twice — once to the screen's own word, once to the thing on it.
export function documentTitle(screen: Screen, named?: string | null): string {
  const word = named?.trim() || SCREENS[screen] || ''
  return word ? `${word} · kahawai` : 'kahawai'
}

/// What to call an item on the screen showing it. An episode's own title is
/// "Episode 1" often enough that the show has to be in it — a tab strip full of
/// those names nothing.
export function itemName(item: { title: string; show_title?: string | null }): string {
  const show = item.show_title?.trim()
  const own = item.title.trim()
  if (!own) return show || ''
  return show ? `${show} · ${own}` : own
}
