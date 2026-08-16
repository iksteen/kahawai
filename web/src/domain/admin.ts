/// What the admin panel shows about the fleet, and what it refuses to.

/// The in-process mediahost's stand-in for a certificate fingerprint.
///
/// AR-5 replaces that link's transport with channels, so there is no TLS
/// identity to pin or revoke. It is part of the admin API's shape rather than
/// a private detail — the hub mirrors this constant.
export const IN_PROCESS = 'in-process'

/// The satellites an operator can act on.
///
/// The hub's own in-process mediahost is not an enrolled satellite: it has no
/// certificate to show, nothing to enable or disable, and nothing to revoke.
/// Listing it offered a Delete that would wipe the index of everything it
/// serves — the whole library, on an all-in-one deployment. Its COLLECTIONS
/// still appear in the composer, which reads the collections table and never
/// this list.
export function enrolled<T extends { cert_fingerprint: string }>(satellites: T[]): T[] {
  return satellites.filter((s) => s.cert_fingerprint !== IN_PROCESS)
}

/// A measured multiple, or nothing when it was never measured. Zero is "not
/// measured" here, not "infinitely slow".
export function multiple(value?: number | null): string | null {
  return typeof value === 'number' && value > 0 ? `${value.toFixed(1)}×` : null
}

/// "6.2× / 2.1×" — 1080p and 2160p, dropping whichever was not measured.
export function measuredPair(hd?: number | null, uhd?: number | null): string | null {
  const parts = [multiple(hd), multiple(uhd)].filter((x): x is string => x !== null)
  return parts.length ? parts.join(' / ') : null
}

/// The hub's own rule, mirrored to keep the button off until it could work.
/// The hub enforces it, and its refusal is what a short password is actually
/// told by.
export const MIN_PASSWORD = 12

/// By code point, not by UTF-16 unit — `'🔑'.length` is 2.
export function longEnough(password: string): boolean {
  return [...password].length >= MIN_PASSWORD
}

/// Whether a new account can be created from what has been typed.
export function canCreate(username: string, password: string): boolean {
  return username.trim() !== '' && longEnough(password)
}

/// What an account's two toggles mean.
///
/// An admin HAS every library — saying so with everyone else's toggle, held on
/// and locked, beats a sentence explaining why there is no toggle here.
export function seesEverything(user: { is_admin: boolean; all_libraries: boolean }): boolean {
  return user.is_admin || user.all_libraries
}

/// Whether changing this account's role signs the operator out of the panel.
///
/// Demoting yourself invalidates the token that authorised the write, so the
/// session has to be rotated before leaving the screen — a bare reload would
/// have bootstrap see only the invalid old token and show sign-in, despite the
/// refresh family still being live.
export function demotesSelf(user: { username: string }, toAdmin: boolean, me: string): boolean {
  return !toAdmin && user.username === me
}

/// "satellites and users", "enrolments, satellites and users".
///
/// A sentence, because it is read as one: this is the line that tells an
/// operator which half of the panel is telling the truth.
export function andList(names: readonly string[]): string {
  if (names.length <= 1) return names[0] ?? ''
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`
}
