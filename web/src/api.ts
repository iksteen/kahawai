// Same-origin client for the Kahawai API. Browser refresh/media credentials
// are HttpOnly cookies; the short-lived access token exists only in memory.

import { buildProfile } from './capabilities.ts'
import {
  adminApplyMatch as generatedAdminApplyMatch,
  adminApprove as generatedAdminApprove,
  adminAttachCollection as generatedAdminAttachCollection,
  adminCollections as generatedAdminCollections,
  adminCreateLibrary as generatedAdminCreateLibrary,
  adminCreateUser as generatedAdminCreateUser,
  adminDeleteLibrary as generatedAdminDeleteLibrary,
  adminDeleteSatellite as generatedAdminDeleteSatellite,
  adminDeleteUser as generatedAdminDeleteUser,
  adminDetachCollection as generatedAdminDetachCollection,
  adminEndSession as generatedAdminEndSession,
  adminEnrollments as generatedAdminEnrollments,
  adminEnrichRun as generatedAdminEnrichRun,
  adminEnrichStatus as generatedAdminEnrichStatus,
  adminLibraries as generatedAdminLibraries,
  adminProviders as generatedAdminProviders,
  adminRefreshLibrary as generatedAdminRefreshLibrary,
  adminReviewSearch as generatedAdminReviewSearch,
  adminSatellites as generatedAdminSatellites,
  adminSessions as generatedAdminSessions,
  adminSetAnidb as generatedAdminSetAnidb,
  adminSetChain as generatedAdminSetChain,
  adminSetDisabled as generatedAdminSetDisabled,
  adminSetTmdb as generatedAdminSetTmdb,
  adminSetTvdb as generatedAdminSetTvdb,
  adminSetUserAdmin as generatedAdminSetUserAdmin,
  adminSetUserLibraries as generatedAdminSetUserLibraries,
  adminUsers as generatedAdminUsers,
  bootstrap as generatedBootstrap,
  endSession as generatedEndSession,
  getAdminItemLogUrl,
  getAdminSessionLogUrl,
  getEventsUrl,
  getItemArtworkUrl,
  getItemFontUrl,
  getItemSubtitleFileUrl,
  getSessionFileUrl,
  getPrefs as generatedGetPrefs,
  itemChildren as generatedItemChildren,
  itemFonts as generatedItemFonts,
  itemSetWatched as generatedItemSetWatched,
  listItems as generatedListItems,
  listLibraries as generatedListLibraries,
  login as generatedLogin,
  logout as generatedLogout,
  postProgress as generatedPostProgress,
  putPref as generatedPutPref,
  refresh as generatedRefresh,
  seekSession as generatedSeekSession,
  startSession as generatedStartSession,
  subtitleDelete as generatedSubtitleDelete,
  subtitleDownload as generatedSubtitleDownload,
  subtitleSearch as generatedSubtitleSearch,
} from './generated/kahawai.ts'
import type { ProviderCandidate } from './generated/model/index.ts'
import { ApiError, Offline, api, configureApiClient } from './api-client.ts'
import { notify } from './toast.ts'
import { SerialQueue } from './serial.ts'
import { REFRESH_RETRY_MS, refreshDelayMs } from './token.ts'

export { ApiError, Offline, api }

// Browser auth state is application behavior; its wire DTO comes from the
// generated login/refresh bindings.
export type RestoreResult = 'authenticated' | 'anonymous'

let access: string | null = null
let generation = 0
let refreshTimer: number | undefined
let refreshInFlight: Promise<boolean> | null = null
let onCleared: ((deliberate: boolean) => void) | null = null

const REFRESH_TIMEOUT_MS = 15_000
const LOCK_WAIT_MS = 20_000

const authChannel =
  typeof window === 'undefined' || typeof BroadcastChannel === 'undefined'
    ? null
    : new BroadcastChannel('kahawai.auth')
authChannel?.addEventListener('message', (event) => {
  if (event.data === 'sign-out') clearAccess(true)
})

export function scrubLegacyCredentials() {
  localStorage.removeItem('kahawai.access')
  localStorage.removeItem('kahawai.refresh')
  document.cookie = 'kahawai_token=; Path=/; Max-Age=0; SameSite=Lax'
}

export function onTokensCleared(cb: ((deliberate: boolean) => void) | null) {
  onCleared = cb
}

function installAccess(token: string, expected = generation): boolean {
  if (generation !== expected) return false
  access = token
  keepTokenFresh()
  return true
}

function clearAccess(deliberate = false, expected?: number): boolean {
  if (expected !== undefined && generation !== expected) return false
  const had = access !== null
  generation++
  access = null
  refreshInFlight = null
  clearTimeout(refreshTimer)
  if (had) onCleared?.(deliberate)
  return true
}

export function accessToken(): string | null {
  return access
}

function claims(): { username?: string; admin?: boolean; exp?: number } {
  if (!access) return {}
  try {
    return JSON.parse(atob(access.split('.')[1]))
  } catch {
    return {}
  }
}

export function keepTokenFresh() {
  clearTimeout(refreshTimer)
  const exp = claims().exp
  if (!exp) return
  refreshTimer = setTimeout(
    () => {
      void refreshTokens().then((ok) => {
        if (!ok && accessToken()) refreshTimer = setTimeout(keepTokenFresh, REFRESH_RETRY_MS)
      })
    },
    refreshDelayMs(exp * 1000, Date.now()),
  )
}

export function username(): string {
  return claims().username ?? ''
}

export function isAdmin(): boolean {
  return claims().admin === true
}

async function alone<T>(run: () => Promise<T>): Promise<T> {
  const locks = typeof navigator === 'undefined' ? undefined : navigator.locks
  if (!locks) return run()
  return locks.request('kahawai.auth', { signal: AbortSignal.timeout(LOCK_WAIT_MS) }, run)
}

async function rotate(started: number, throwTransient: boolean): Promise<boolean> {
  if (generation !== started) return false
  try {
    const fresh = await generatedRefresh(
      { client: 'browser' },
      {
        signal: AbortSignal.timeout(REFRESH_TIMEOUT_MS),
        skipAuthRefresh: true,
        skipAuthorization: true,
      },
    )
    return installAccess(fresh.access_token, started)
  } catch (error) {
    if (error instanceof ApiError && (error.status === 401 || error.status === 403)) {
      clearAccess(false, started)
      return false
    }
    if (throwTransient) throw error
    return false
  }
}

export function refreshTokens(): Promise<boolean> {
  if (!access) return Promise.resolve(false)
  const started = generation
  refreshInFlight ??= alone(() => rotate(started, false))
    .catch(() => false)
    .finally(() => {
      refreshInFlight = null
    })
  return refreshInFlight
}

configureApiClient(accessToken, refreshTokens)

export async function restoreSession(): Promise<RestoreResult> {
  const started = generation
  return (await alone(() => rotate(started, true))) ? 'authenticated' : 'anonymous'
}

export async function browserLogin(username: string, password: string): Promise<void> {
  const started = ++generation
  await alone(async () => {
    const session = await generatedLogin(
      { client: 'browser', username, password },
      { skipAuthRefresh: true, skipAuthorization: true },
    )
    installAccess(session.access_token, started)
  })
}

async function revoke(capturedAccess: string): Promise<void> {
  await alone(async () => {
    const post = (bearer: string) =>
      generatedLogout(
        { client: 'browser' },
        {
          headers: { Authorization: `Bearer ${bearer}` },
          skipAuthRefresh: true,
        },
      )
    try {
      await post(capturedAccess)
    } catch (error) {
      if (!(error instanceof ApiError) || error.status !== 401) throw error
      const session = await generatedRefresh(
        { client: 'browser' },
        {
          signal: AbortSignal.timeout(REFRESH_TIMEOUT_MS),
          skipAuthRefresh: true,
          skipAuthorization: true,
        },
      )
      await post(session.access_token)
    }
  })
}

export async function signOut(): Promise<void> {
  const capturedAccess = access
  clearAccess(true)
  authChannel?.postMessage('sign-out')
  if (capturedAccess) {
    await revoke(capturedAccess).catch((error: unknown) => {
      notify(`Signed out here, but ${error}. The session may still work on other devices.`)
    })
  }
}

// The transport and its errors live beside Orval's mutator so generated
// bindings never import this application facade back and form a cycle.

export type Item = {
  id: string
  kind: 'movie' | 'show' | 'episode' | 'album' | 'track'
  title: string
  artist?: string | null
  year: number | null
  season: number | null
  episode: number | null
  episode_end?: number | null
  /// HUB-31: TVDB-style projection of absolute numbering (anime).
  proj_season?: number | null
  proj_episode?: number | null
  /// The show an episode belongs to — search can return episodes, and a
  /// hit called "Pilot" needs to say which of its namesakes it is. For a
  /// track hit, `parent_id` is the album to open: tracks have no detail
  /// view of their own.
  parent_id?: string | null
  parent_title?: string | null
  /// A library this item is in, as navigation context: item URLs live
  /// under a library, and a row from a cross-library browse (search,
  /// continue watching) has no other way to know one. Membership is
  /// many-to-many, so this is "a library it is in", not "its library".
  /// Null for an item in no library, which only an unrestricted account
  /// can be looking at. Browse rows only — children carry their parent's
  /// context instead.
  library_id?: string | null
  sources: number
  /// HUB-19 ReplayGain, as the file states it. Gains are dB to apply;
  /// peaks are linear sample values where 1.0 is full scale. Absent for
  /// anything untagged.
  replay_gain?: {
    track_gain_db?: number | null
    track_peak?: number | null
    album_gain_db?: number | null
    album_peak?: number | null
    reference_level_db?: number | null
  } | null
  /// Enrichment state (movie/show): null = never enriched,
  /// miss/rejected = unmatched, weak = uncertain, auto/manual = good.
  match_confidence?: string | null
  /// Metadata timestamp, used to bust the artwork cache on a re-match.
  art_version?: number | null
  /// The filename-derived identity (title/year as parsed from disk) and
  /// the provider's matched title — for the review dialog.
  file_title?: string | null
  file_year?: number | null
  matched_title?: string | null
  premiered?: string | null
  resume_position_ms: number | null
  resume_duration_ms?: number | null
  played: boolean
  play_count: number
}

export type StreamInfo = {
  container?: string
  duration_ms?: number
  video?: {
    codec: string
    width: number
    height: number
    display_width?: number | null
    display_height?: number | null
    orientation?: string | null
    pixel_aspect_ratio?: [number, number] | null
    profile?: string | null
    level?: string | null
  }[]
  audio?: { codec: string; channels: number; language?: string | null }[]
  subtitles?: { format: string; language?: string | null }[]
}

export type Source = {
  path_rel: string
  size: number
  available: boolean
  /// Release revision: 1 plain, 2+ for v2 / REPACK / PROPER. Higher
  /// outranks lower within the same resolution when playback picks.
  revision: number
  streams: StreamInfo | null
}

export type ItemMetadata = {
  overview: string | null
  rating: number | null
  premiered: string | null
  confidence: 'auto' | 'weak'
  provider: string
  /// ISO 639-1 original language of the matched title (feeds the
  /// future default-track mechanism).
  original_language?: string | null
  /// HUB-6. Both describe the work, so an episode shows its show's.
  genres?: string[] | null
  cast?: { name: string; character: string | null }[] | null
}

export type ItemDetail = Item & {
  negotiated?: Negotiated
  sources_detail: Source[]
  show_title?: string | null
  parent_id?: string | null
  metadata?: ItemMetadata
  related?: { kind: string; title: string | null; item_id: string | null }[]
}

/// Local artwork (cover.jpg etc). <img> requests authenticate with the
/// media cookie; 404 = no artwork (hide the img).
///
/// The response is cached hard (a day), so pass the item's `art_version`
/// — its metadata timestamp — or a re-match leaves the old poster on
/// screen until the cache expires.
/// `size` names one of the hub's sizes (`thumb`, `card`); omitting it
/// serves the original, which is what the detail view wants. Ask for the
/// size the element actually displays — a grid of 34px rows pulling
/// 600px covers is the reason this parameter exists.
export const artworkUrl = (
  id: string,
  version?: number | null,
  size?: 'thumb' | 'card1x' | 'card',
) => getItemArtworkUrl(id, { size, v: version ? String(version) : undefined })

/// One poster at both densities, for the `srcset` of anything that shows a
/// card. What varies between clients here is the display, not the layout —
/// the CSS widths are fixed — so these are `x` descriptors and there is no
/// `sizes` to get wrong. A 1× display stops being sent 6× the pixels it can
/// show; a 2× one is unaffected.
export const artworkSrcSet = (id: string, version?: number | null) =>
  `${artworkUrl(id, version, 'card1x')} 1x, ${artworkUrl(id, version, 'card')} 2x`

export const fetchChildren = (id: string) =>
  generatedItemChildren(id) as Promise<{ children: Item[] }>

/// One unified track row (subtitle unification): the id is THE key for
/// serving, selection, OCR, deletion and preference memory. `delivery`
/// is computed per request from the capability bits — capability never
/// filters the list, it changes what a track means for this client.
export type SubtitleDelivery = 'text' | 'ass' | 'overlay' | 'burn' | 'none'
export type Subtitle = {
  id: number
  origin: 'embedded' | 'sidecar' | 'downloaded' | 'ocr' | 'raster'
  stream_index: number | null
  format: string
  language: string | null
  label: string | null
  machine: boolean
  derived_from: number | null
  delivery: SubtitleDelivery
  note: string
  /// HUB-32c/d: may THIS user remove it? Only downloaded tracks, and
  /// only their creator or an admin — the other hub-stored origins are
  /// caches that rebuild themselves.
  deletable: boolean
}

export const isImageSub = (s: Subtitle) => ['pgs', 'vobsub', 'dvdsub'].includes(s.format)

/// HUB-32d: a styled script rendered server-side to display sets. It
/// is delivered as an overlay like PGS, but sourced item-level rather
/// than from the live session tap — see `overlayUrl`.
export const isRasterSub = (s: Subtitle) => s.origin === 'raster'

/// Where a track's display sets come from. An embedded image track is
/// decoded by the running pipeline and tail-followed off the session;
/// a rasterised one is a finished artefact on the item.
export const overlayUrl = (s: Subtitle, itemId: string, streamUrl: string) =>
  isRasterSub(s)
    ? getItemSubtitleFileUrl(itemId, `${s.id}.jsonl`)
    : getSessionFileUrl(streamUrl.split('/').at(-2) ?? '', `subs-${s.id}.jsonl`)

/// HUB-21/22/24: external subtitle search + download.
export type SubtitleCandidate = {
  provider: string
  file_id: string
  language: string | null
  release_name: string | null
  hash_match: boolean
  downloads: number
  uploader: string | null
  rating: number | null
  fps: number | null
}

/// HUB-21/24: download entitlement. per_account = false means the
/// budget is shared by everyone on this server, which the UI must say.
export type SubtitleQuota = {
  remaining: number | null
  total: number | null
  resets_in_secs: number | null
  per_account: boolean
}

export const searchSubtitles = (itemId: string, languages: string[]) =>
  generatedSubtitleSearch(itemId, { languages })

export const downloadSubtitle = (itemId: string, fileId: string, language: string | null) =>
  generatedSubtitleDownload(itemId, { file_id: fileId, language })

/// "3 of 5 downloads left today (shared by everyone on this server)"
export function quotaLabel(q: SubtitleQuota | null): string {
  if (!q || q.remaining === null) return ''
  const scope = q.per_account ? '' : ' — shared by everyone on this server'
  const resets =
    q.resets_in_secs && q.resets_in_secs > 0
      ? `, resets in ${Math.max(1, Math.round(q.resets_in_secs / 3600))} h`
      : ''
  return `${q.remaining}${q.total ? ` of ${q.total}` : ''} downloads left today${resets}${scope}`
}

/// Hub-stored tracks (downloaded/OCR) only; scan-owned rows refuse.
export const deleteSubtitle = (id: number) => generatedSubtitleDelete(id)

/// HUB-32c: OCR an image track (embedded or VobSub sidecar) into a new
/// text track. Synchronous — a feature film takes ~30 s; cached, and

export const fetchFonts = (itemId: string) => generatedItemFonts(itemId)
export const fontUrl = (itemId: string, index: number) => getItemFontUrl(itemId, index)
export const subtitleFileUrl = (itemId: string, file: string, shiftMs?: number) =>
  getItemSubtitleFileUrl(itemId, file, { shift_ms: shiftMs })

// One uniform label across origins; delivery adds the honest suffix
// (a burn restarts the session; 'none' renders disabled).
export const subtitleLabel = (s: Subtitle) =>
  `${s.language ?? 'unknown'} · ${s.format}` +
  (s.origin === 'sidecar'
    ? ' · file'
    : s.origin === 'downloaded'
      ? ' · downloaded'
      : s.origin === 'ocr'
        ? ' · ocr'
        : s.origin === 'raster'
          ? ' · typeset'
          : '') +
  (s.delivery === 'burn' ? ' · burn-in' : s.delivery === 'none' ? ' · unavailable' : '')

export type LibrarySummary = { id: string; name: string; media_type: string }

export const fetchLibraries = () => generatedListLibraries()

/// How a library is browsed. `sort` is one of the names the hub knows
/// (`title`, `-title`, `year`, `-year`, `added`, `-added`); anything else
/// falls back to title there rather than erroring.
export type ItemsPage = {
  library?: string
  q?: string
  sort?: string
  limit?: number
  offset?: number
  /// Started and not finished, most recently watched first — the home
  /// screen's continue-watching row. Its own order, so `sort` and `q` do
  /// not apply; `library` still scopes it.
  in_progress?: boolean
}

/// The hub pages this endpoint — 200 items unless asked otherwise, capped
/// at 1000. Sending no window is not "give me everything", it is "give me
/// the first 200 and do not mention the rest", which is how the browser
/// spent three commits showing 200 of 881 films. Always read `total`.
export const fetchItems = (page: ItemsPage) =>
  generatedListItems(page) as Promise<{
    items: Item[]
    total: number
    limit: number
    offset: number
  }>

/// Which screen to open on. Public: needs no token, and answers before
/// setup has happened.
export type Bootstrap = {
  setup_required: boolean
  setup_available: boolean
  setup_url?: string
}

/// Which screen to open on. The app renders NOTHING until this answers, so it
/// is the one request that must not be able to hang: no timeout meant a hub
/// that accepted the connection and then wedged left a permanently blank page
/// with no header, no message and nothing to press.
export const fetchBootstrap = async (): Promise<Bootstrap> => {
  const state = await generatedBootstrap({
    signal: AbortSignal.timeout(BOOTSTRAP_TIMEOUT_MS),
  })
  return { ...state, setup_url: state.setup_url ?? undefined }
}

/// Shorter than a session start's ceiling: this is a database read and a token
/// check, and the page is blank until it lands.
const BOOTSTRAP_TIMEOUT_MS = 10_000

/// What this client would actually be served, for the profile it asked
/// with — the converged half of the item resource.
export type Negotiated = {
  source: {
    module_id: string
    collection_id: string
    path_rel: string
    display_width?: number | null
    display_height?: number | null
    orientation?: string | null
  } | null
  /// What negotiation decided. A `remux` may still be dispatched to a
  /// transcoder when the session starts; that is placement, which a
  /// safe method does not do.
  mode: string
  cost: string
  streams: { video: string; audio: string; subtitles: SubtitleVerdict[] }
  /// The unified track list for the source negotiation chose, each with
  /// the delivery it would get.
  subtitles: Subtitle[]
}

export type SubtitleVerdict = {
  index: number
  track_id?: number | null
  format: string
  language?: string | null
  tier: 'text' | 'convert' | 'graphics' | 'ocr' | 'burn' | 'unavailable'
  note: string
}
export type StreamVerdict = {
  /// Aggregate semantic work. Unlike session `mode`, this says whether an
  /// elementary stream is encoded and follows track-switch re-plans.
  cost: 'direct' | 'copy' | 'audio_encode' | 'video_encode'
  video: string
  audio: string
  subtitles?: SubtitleVerdict[]
}

export type Session = {
  session_id: string
  mode: 'direct' | 'remux' | 'transcode'
  stream_url: string
  content_type: string
  size: number
  duration_ms: number | null
  /// Timeline base of the part the pipeline started in (multi-part
  /// CD1/CD2 sources; 0 for single files).
  part_base_ms?: number
  parts?: number
  streams: StreamVerdict | null
}

/// HUB-14: no `mode` — the hub negotiates from the capability profile.
/// (An explicit mode remains available on the wire for scripts/debug.)
/// A session start that never answers must not be waited on for ever.
///
/// `fetch` has no timeout, and the player latches `recovering` across this
/// call: a hub that accepts the connection and then wedges left the latch set,
/// so the give-up dialog's "press play to try again" was a permanent no-op —
/// the handback it exists for, dead on arrival.
///
/// A minute, because the only job of this number is to be FINITE. Starting a
/// session is the slowest thing the hub does — it can be waiting on a subtitle
/// index walk, a playlist runway or a pipeline coming up — and a ceiling tight
/// enough to cut across that turns "slow" into "impossible", with the hub
/// building a session nobody collects. It is a patience bound, not a mirror of
/// anything the hub knows: if the hub's own waits ever exceed this, the symptom
/// is a retry, not a wrong answer.
const START_TIMEOUT_MS = 60000

export function startSession(
  itemId: string,
  profile: import('./capabilities').CapabilityProfile,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
  subtitleTrack?: number,
): Promise<Session> {
  return generatedStartSession(
    {
      item_id: itemId,
      profile,
      start_ms: Math.round(startMs),
      audio_track: audioTrack,
      video_track: videoTrack,
      // An IMAGE track id forces its burn-in from the first segment;
      // text tracks need no session involvement.
      subtitle_track: subtitleTrack ?? null,
    },
    { signal: AbortSignal.timeout(START_TIMEOUT_MS) },
  ) as Promise<Session>
}

/// One place builds a play request: bandwidth pref → probed profile →
/// source-aware refinements → debug mask → session. Shared by the player
/// route's start and the player's own capability restart, so
/// both negotiate from an identical profile.
export async function startPlaybackSession(
  item: ItemDetail,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
  known?: Pref[],
  quiet = false,
  /// Where a prefs failure should be said. The default is a toast, which the
  /// player cannot use: its Try again is reachable with the picture fullscreen,
  /// and the toast host is a sibling of the element that goes fullscreen. The
  /// player's other two prefs reads already pass `playerNote` for that reason;
  /// this one read through here and was invisible.
  report?: (message: string) => void,
): Promise<Session> {
  // Prefs from the caller when it has them. the player route and the auto-advance both
  // resolved the audio track from this same read a moment earlier, so fetching
  // again here was a second round trip and — once the read started reporting —
  // a second copy of the same sentence, in a second host the first could not
  // replace.
  //
  // `quiet` for the callers that must not speak: the stand-by retry runs every
  // five seconds for as long as a host is away, and a report there is a toast
  // on a timer about the weather its own dialog is already describing. It is
  // opt-IN, and the default reports, because the first cut of this had it the
  // other way round and quietly took the report away from three deliberate
  // presses — a Play from any page, an Apply in the capability dialog and
  // a Try again — that read prefs through here and nowhere else. This is the
  // only reader of `bandwidth_kbps` in the app, so silence here is the cap
  // dropped with nothing said anywhere.
  const prefs =
    known ??
    (quiet ? await fetchPrefs().catch(() => ({ prefs: [] })) : await prefsOrNone(report)).prefs
  const v = prefs.find((x) => x.scope === '' && x.key === 'bandwidth_kbps')?.value
  const cap = v ? Number(v) : undefined
  // Source-aware precision: probe the exact strings the announced
  // streams call for (profile/level from the hub's own probing).
  const announced = item.sources_detail.flatMap((s) => s.streams?.video ?? [])
  return startSession(item.id, buildProfile(cap, announced), startMs, audioTrack, videoTrack)
}

/// Music plays direct by operator contract (browsers decode flac/mp3
/// natively; HUB-19 owns future music delivery shapes).
export function startSessionDirect(itemId: string, signal?: AbortSignal): Promise<Session> {
  return generatedStartSession({ item_id: itemId, mode: 'direct' }, { signal }) as Promise<Session>
}

export type Pref = { scope: string; key: string; value: string }

/// Preferences, or none — with the failure SAID rather than swallowed.
///
/// Five call sites resolved tracks from prefs and fell back to an empty list,
/// each with its own silent catch: the audio track the viewer last chose, the
/// remembered subtitle track, the per-media-type language wishlist, the anime
/// view, and the bandwidth cap they typed into Settings. Every one of them is
/// something a person picked and would simply not be getting, with nothing on
/// screen to say why — and a cap silently dropped is the expensive one, since
/// it is the setting that exists to keep a session off a metered link.
///
/// The fallback stays: a page that renders on source order beats one that does
/// not render. What changes is that it stops pretending nothing happened.
///
/// `report` because the player cannot use `notify`: the toast host is a sibling
/// of `.videobox`, which is what goes fullscreen, so a toast raised while the
/// picture fills the screen is painted nowhere. Player call sites pass
/// `showNote`.
export async function prefsOrNone(
  report: (msg: string) => void = notify,
): Promise<{ prefs: Pref[] }> {
  try {
    return await fetchPrefs()
  } catch (e) {
    report(`Could not load your playback preferences: ${e}`)
    return { prefs: [] }
  }
}

/// HUB-33, resolved entirely client-side from /api/v1/prefs.
/// Precedence: per-series/movie memory (what the user last set) >
/// per-media-type settings (ordered language lists, 'original' resolves
/// via the item's original_language) > track 0 / no subs.
/// Settings live user-global: key `audio.{media_type}` = "nl,original,en",
/// key `subs.{media_type}` = "nl,en" ('' or absent = no subs).
export function resolveTracks(
  prefs: Pref[],
  seriesId: string,
  itemId: string,
  mediaType: string,
  originalLanguage: string | null | undefined,
  audio: { language?: string | null }[],
): { audioTrack: number; subs: string[]; subTrack: number | null } {
  const get = (scope: string, key: string) =>
    prefs.find((p) => p.scope === scope && p.key === key)?.value
  const list = (v?: string) =>
    (v ?? '')
      .split(',')
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean)
  const langEq = (l: string | null | undefined, want: string) => {
    if (!l) return false
    const a = l.toLowerCase()
    const b = want.toLowerCase()
    return a === b || a.slice(0, 2) === b.slice(0, 2)
  }

  // Audio, most specific first: THIS item's exact track (two English
  // tracks — feature + commentary — are common, so language cannot
  // express the choice), then the series language memory, then the
  // ordered per-type list.
  let audioTrack: number | undefined
  const exact = get(itemId, 'audio.track')
  if (exact?.startsWith('#')) {
    const i = Number(exact.slice(1))
    if (i >= 0 && i < audio.length) audioTrack = i
  }
  const remembered = get(seriesId, 'audio')
  if (audioTrack !== undefined) {
    // exact item pref already decided
  } else if (remembered?.startsWith('#')) {
    const i = Number(remembered.slice(1))
    if (i >= 0 && i < audio.length) audioTrack = i
  } else if (remembered) {
    const i = audio.findIndex((a) => langEq(a.language, remembered))
    if (i >= 0) audioTrack = i
  }
  if (audioTrack === undefined) {
    // 'original' is the standing backstop: implicit final entry of
    // every audio wishlist (and the whole list when none is set).
    const wish = list(get('', `audio.${mediaType}`))
    if (!wish.includes('original')) wish.push('original')
    for (const want of wish) {
      const lang = want === 'original' ? originalLanguage : want
      if (!lang) continue
      const i = audio.findIndex((a) => langEq(a.language, lang))
      if (i >= 0) {
        audioTrack = i
        break
      }
    }
  }

  // Subs: memory ('off' | 'any' | lang), else the per-type list.
  // Returned as the ordered language wishlist ([] = subtitles off).
  const subsMem = get(seriesId, 'subs')
  const subs = subsMem === 'off' ? [] : subsMem ? [subsMem] : list(get('', `subs.${mediaType}`))
  // Top precedence (subtitle unification): THIS item's exact track id
  // — the only spelling that can name a specific downloaded/OCR row.
  // Callers honor it iff the id is still in the fetched list.
  const exactSub = get(itemId, 'subs.track')
  const subTrack = exactSub && /^\d+$/.test(exactSub) ? Number(exactSub) : null
  return { audioTrack: audioTrack ?? 0, subs, subTrack }
}

/// First subtitle matching the wishlist, in wishlist order ('any'
/// matches the first text sub). null = leave subtitles off.
export function pickSubtitle(wishlist: string[], subs: Subtitle[]): Subtitle | null {
  // Language wishes auto-pick only client-rendered tracks: silently
  // forcing a burn (a video encode restart) is never what a language
  // preference means — burns are explicit picks.
  const auto = (s: Subtitle) =>
    s.delivery === 'text' || s.delivery === 'ass' || s.delivery === 'overlay'
  // The server's fidelity order (HUB-32a/d): the client's own ASS
  // renderer first, then a server-rasterised overlay, then flattened
  // text. Within one language the BEST reading wins, not whichever row
  // the listing happened to put first — otherwise a client with ASS
  // masked off would take the flattened VTT and never notice the
  // rasterised track sitting right behind it.
  const rank = (s: Subtitle) => (s.delivery === 'ass' ? 0 : s.delivery === 'overlay' ? 1 : 2)
  const best = (cs: Subtitle[]) =>
    cs.length === 0 ? null : cs.reduce((a, b) => (rank(b) < rank(a) ? b : a))
  for (const want of wishlist) {
    const eligible = subs.filter((s) => auto(s) && !isImageSub(s))
    const hit =
      want === 'any'
        ? best(eligible)
        : best(
            eligible.filter(
              (s) => (s.language ?? '').toLowerCase().slice(0, 2) === want.slice(0, 2),
            ),
          )
    if (hit) return hit
  }
  return null
}

export const fetchPrefs = () => generatedGetPrefs()

/// HUB-11 event channel: invalidation hints ({kind, ...}). Authenticates
/// via the HttpOnly `kahawai_media` cookie (EventSource cannot set headers).
/// The browser auto-reconnects; callers just react to hints.
export function openEvents(onEvent: (e: { kind: string } & Record<string, unknown>) => void) {
  const es = new EventSource(getEventsUrl())
  es.onmessage = (m) => {
    try {
      onEvent(JSON.parse(m.data))
    } catch {
      // malformed hint: ignore
    }
  }
  return es
}

// Preferences are whole-value writes. One queue per key preserves the order
// the viewer changed that value while unrelated controls still save in
// parallel. Filtering stale responses in a component cannot provide this: an
// older request can commit last and only reveal the rollback after a reload.
const prefWrites = new Map<string, SerialQueue>()
export const putPref = (scope: string, key: string, value: string) => {
  const target = `${scope}\0${key}`
  const queue = prefWrites.get(target) ?? new SerialQueue()
  prefWrites.set(target, queue)
  return queue.run(() => generatedPutPref({ scope, key, value }))
}

/// Seek-restart: the pipeline restarts at the offset; re-attach the
/// player. An audio_track switches tracks during the restart (HUB-27).
export function seekSession(
  sessionId: string,
  positionMs: number,
  audioTrack?: number,
  videoTrack?: number,
  subtitleTrack?: number,
): Promise<{ part_base_ms: number; streams?: StreamVerdict | null }> {
  return generatedSeekSession(sessionId, {
    position_ms: Math.round(positionMs),
    audio_track: audioTrack ?? null,
    video_track: videoTrack ?? null,
    // An image track id switches the burn mid-session; 0 withdraws
    // an explicit burn; absent = keep as is.
    subtitle_track: subtitleTrack ?? null,
  }) as Promise<{ part_base_ms: number; streams?: StreamVerdict | null }>
}

/// Mark an item watched, or not, without playing it — something seen on
/// another television, or a tick undone. Either direction clears the
/// resume position; the play count only ever climbs.
///
/// `items` marks a batch in one call: a season, or a whole show. Every id
/// must be `itemId` itself or one of its children, which is what lets the
/// hub cover the batch with one access check. Ids outside that are not
/// marked and are not reported — the returned rows say what was, so a
/// caller that cares can compare.
///
/// One statement server-side, so a season either applies or does not.
/// Looping here instead meant 26 round trips for an anime season and a
/// half-applied mark whenever one of them failed.
export const setWatched = (itemId: string, played: boolean, items?: string[]) =>
  generatedItemSetWatched(itemId, items ? { played, items } : { played })

export function postProgress(sessionId: string, positionMs: number, keepalive = false) {
  return (
    generatedPostProgress(
      sessionId,
      { position_ms: Math.round(positionMs) },
      { keepalive, rawResponse: true },
    ) as unknown as Promise<Response>
  ).catch(() => undefined)
}

export function endSession(sessionId: string, keepalive = false) {
  return generatedEndSession(sessionId, { keepalive }).catch(() => undefined)
}

// ---- admin ----

export async function downloadWithAuth(path: string): Promise<void> {
  const response = await api(path)
  if (!response.ok)
    throw new ApiError(response.status, (await response.text()) || `${response.status}`)
  const url = URL.createObjectURL(await response.blob())
  const link = document.createElement('a')
  link.href = url
  link.download =
    response.headers.get('content-disposition')?.match(/filename="([^"]+)"/i)?.[1] ?? ''
  try {
    link.click()
  } finally {
    URL.revokeObjectURL(url)
  }
}

export const adminSessionLogUrl = getAdminSessionLogUrl
export const adminItemLogUrl = getAdminItemLogUrl

export type PendingEnrollment = {
  csr_fingerprint: string
  module_type: string
  module_id: string
  name: string
}

/// One verified encoder and what it was measured doing (HUB-36).
/// Speeds are realtime multiples; null = never measured, which is not
/// the same as slow.
export type EncoderCap = {
  codec: string
  element: string
  hardware: boolean
  speed_1080?: number | null
  speed_2160?: number | null
}

export type SatelliteCaps = {
  encoders?: EncoderCap[]
  max_sessions?: number
  tonemap?: boolean
  tonemap_speed_1080?: number | null
  tonemap_speed_2160?: number | null
}

/// What a box has ACHIEVED on a kind of work, as opposed to what its
/// benchmark claims. `class` is `{res}|{src}|{dst}[|tm]`.
export type PaceRow = { class: string; multiple: number }

/// The in-process mediahost's stand-in for a certificate fingerprint —
/// AR-5 replaces the link's transport with channels, so there is no TLS
/// identity to pin or revoke. Mirrors Registry::IN_PROCESS; it is part
/// of the admin API's shape, not a private detail.
export const IN_PROCESS = 'in-process'

export type Satellite = {
  module_id: string
  module_type: string
  name: string
  cert_fingerprint: string
  connected: boolean
  disabled: boolean
  capabilities?: SatelliteCaps | null
  pace?: PaceRow[]
  link_bytes_per_sec?: number | null
}

export type AdminSession = {
  session_id: string
  username: string | null
  title: string | null
  mode: string
  module_id: string
  idle_secs: number
  streams: StreamVerdict | null
}

export const adminEnrollments = () => generatedAdminEnrollments()
export const adminApprove = (code: string) => generatedAdminApprove({ code })
export const adminSatellites = () => generatedAdminSatellites()
export const adminDeleteSatellite = (id: string) => generatedAdminDeleteSatellite(id)

export type LibraryCollection = {
  module_id: string
  collection_id: string
  host_name: string | null
}

export type Library = {
  id: string
  name: string
  media_type: string
  collections: LibraryCollection[]
}

export type ScanState = {
  scanned: number
  failed: number
  skipped: number
  complete: boolean
}

export type CollectionInfo = LibraryCollection & {
  media_type: string
  connected: boolean
  scan: ScanState | null
}

export const adminLibraries = () => generatedAdminLibraries()

export const adminCollections = () => generatedAdminCollections()

export type ProviderChain = { order: string[]; default: string[] }

export const adminProviders = () => generatedAdminProviders()

/// HUB-5: precedence per media type. Earlier wins a field; later ones
/// fill what it left empty. Applying re-merges from stored answers, so
/// it is instant and sends no provider a request.
export const adminSetChain = (mediaType: string, order: string[]) =>
  generatedAdminSetChain(mediaType, { order })
export const adminSetAnidb = (username: string, password: string, udpApiKey?: string) =>
  generatedAdminSetAnidb({ username, password, udp_api_key: udpApiKey || null })
export const adminSetTvdbKey = (apiKey: string, pin?: string) =>
  generatedAdminSetTvdb({ api_key: apiKey, pin: pin || null })
export const adminSetTmdbKey = (apiKey: string) => generatedAdminSetTmdb({ api_key: apiKey })
export const adminEnrichStatus = () => generatedAdminEnrichStatus()
export type MatchCandidate = ProviderCandidate

export const adminReviewSearch = (
  kind: string,
  query: string,
  year?: number | null,
  item?: string,
) => generatedAdminReviewSearch({ kind, query, year: year ?? null, item: item ?? null })
export const adminApplyMatch = (
  itemId: string,
  action: 'pick' | 'confirm' | 'reject',
  candidate?: MatchCandidate,
) =>
  generatedAdminApplyMatch(itemId, {
    action,
    provider: candidate?.provider ?? null,
    candidate: candidate ?? null,
  })

export const adminRefreshLibrary = (id: string) => generatedAdminRefreshLibrary(id)

export const adminEnrichRun = () => generatedAdminEnrichRun()

export const adminCreateLibrary = (name: string, mediaType: string) =>
  generatedAdminCreateLibrary({ name, media_type: mediaType })

export const adminDeleteLibrary = (id: string) => generatedAdminDeleteLibrary(id)

export const adminAttachCollection = (id: string, moduleId: string, collectionId: string) =>
  generatedAdminAttachCollection(id, { module_id: moduleId, collection_id: collectionId })

export const adminDetachCollection = (id: string, moduleId: string, collectionId: string) =>
  generatedAdminDetachCollection(id, moduleId, collectionId)

export const adminSetSatelliteDisabled = (id: string, disabled: boolean) =>
  generatedAdminSetDisabled(id, { disabled })
export const adminSessions = () => generatedAdminSessions() as Promise<{ sessions: AdminSession[] }>
export const adminEndSession = (id: string) => generatedAdminEndSession(id)

/// HUB-10. `all_libraries` wins over `libraries`: with it set the list is
/// stored but not consulted, and libraries created later are included.
export type AdminUser = {
  id: string
  username: string
  is_admin: boolean
  all_libraries: boolean
  libraries: string[]
  created_at: number
}

export const adminUsers = () => generatedAdminUsers()

/// HUB-10: promote or demote. The hub refuses to strip your own rights and
/// refuses to demote the last admin, so this cannot lock an operator out — the
/// client does not have to mirror either rule, it just reports what came back.
/// Both refusals are 409, deliberately not the 403 that `require_admin` uses
/// for "this token is not an admin": otherwise a client could not tell
/// re-authenticate from pick-a-different-account.
export const adminSetUserAdmin = (id: string, admin: boolean) =>
  generatedAdminSetUserAdmin(id, { admin })

export const adminCreateUser = (username: string, password: string, admin: boolean) =>
  generatedAdminCreateUser({ username, password, admin })

export const adminDeleteUser = (id: string) => generatedAdminDeleteUser(id)

/// Whole state, not a toggle: the panel holds every box, and sending all
/// of them is what keeps two admins from interleaving into a set neither
/// picked.
export const adminSetUserLibraries = (id: string, allLibraries: boolean, libraries: string[]) =>
  generatedAdminSetUserLibraries(id, { all_libraries: allLibraries, libraries })
