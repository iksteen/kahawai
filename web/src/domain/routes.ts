/// Where the app can be, and the decisions the router cannot make.
///
/// Addresses are the router's business (`src/router.ts`); this holds what is
/// about MEANING rather than matching, so it can be checked without mounting
/// anything.
///
/// Items live under a library — `/app/library/{lib}/item/{id}` — so the
/// back-target survives a reload and a shared link. Collections are
/// many-to-many with libraries, so the URL is the navigation context and
/// nothing else can supply it. Nothing mints a library-less item link, so
/// there is no bare item form.

export type RouteName =
  | 'libraries'
  | 'library'
  | 'admin'
  | 'settings'
  | 'detail'
  | 'season'
  | 'player'

/// Which SCREEN you are on, which is not the same as which address.
///
/// They agree everywhere but the player, whose address carries the episode.
/// Keying an error boundary on the address meant the autoplay handover — which
/// changes the URL to the next episode and nothing else — remounted the whole
/// player route: the session it had already started was dropped on the floor
/// unreported, so nobody ended it and nobody pinged it, and a third was
/// started for the same episode. Every episode boundary cost a leaked
/// transcoder slot and a rebuilt frame with the starting veil back.
///
/// The cost is that a throw inside the player stays latched across a handover
/// — which cannot happen, because a player that threw is not playing anything
/// to the end. Leaving the player clears it.
export function boundaryKey(name: RouteName, path: string, library?: string): string {
  return name === 'player' ? `player:${library ?? ''}` : path
}

/// The same key, off a route object. Two callers need to agree on it exactly —
/// the boundary that remounts on it and the announcement that fires on it —
/// and they were computing it from the same three pieces in two places.
export function addressOf(route: {
  name?: unknown
  path: string
  params: Record<string, unknown>
}): string {
  return boundaryKey(
    (route.name ?? 'libraries') as RouteName,
    route.path,
    typeof route.params.library === 'string' ? route.params.library : undefined,
  )
}

/// Whether this screen has a results panel under the search box.
///
/// Only the home screen. On a library page the same box filters that library
/// in place, and a dropdown of cross-library hits over a page that is already
/// filtering would be two answers to one question.
///
/// Its own function because the flag OUTLIVED the route once: typing in a
/// library's filter set "the panel is open" with nothing there to show it, and
/// going home then mounted the panel already open over a page nobody had
/// searched.
export function hasSearchPanel(name: RouteName): boolean {
  return name === 'libraries'
}

/// Whether the search box means anything here at all.
///
/// On the player, admin and settings there is nothing for it to search, and a
/// box that silently does nothing is worse than no box.
export function hasSearchBox(name: RouteName): boolean {
  return name === 'libraries' || name === 'library'
}

/// A season segment. `all` rather than an empty one: a null season is ABSOLUTE
/// numbering, a real answer about an anime, and it needs a spelling of its own.
export function seasonSegment(season: number | null): string {
  return season === null ? 'all' : String(season)
}

export function parseSeason(segment: string): number | null {
  return segment === 'all' ? null : Number(segment)
}

/// Somewhere else to go when a screen breaks, or nothing.
///
/// "Home" from the home screen is a button that does nothing — the boundary
/// would clear only if the address changed, and it does not. Every other
/// screen has somewhere to fall back to.
export function awayFrom(name: RouteName): string | undefined {
  return name === 'libraries' ? undefined : 'Home'
}
