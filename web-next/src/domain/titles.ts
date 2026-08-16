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

import type { RouteName } from './routes.ts'

const SCREENS: Record<RouteName, string> = {
  libraries: 'Home',
  library: 'Library',
  detail: '',
  season: '',
  settings: 'Settings',
  admin: 'Admin',
  player: '',
}

/// `named` is what the screen is showing, when it knows: an item's title, a
/// library's name. It arrives a round trip after the route does, so the title
/// changes twice — once to the screen's own word, once to the thing on it.
export function documentTitle(route: RouteName, named?: string | null): string {
  const screen = named?.trim() || SCREENS[route] || ''
  return screen ? `${screen} · kahawai` : 'kahawai'
}
