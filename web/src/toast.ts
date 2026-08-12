/// One notice at a time, bottom-right, gone in five seconds.
///
/// A module function rather than a context provider: a view reports a
/// failure by importing `notify`, with nothing threaded down to it and no
/// provider above it. That is the point — UX-1 exists because failures
/// were being swallowed where saying so cost a prop drill.
///
/// Latest wins. Two failures in a row are usually the same failure twice,
/// and a queue would show the stale one first.

/// Milliseconds a notice stays up. Long enough to read a sentence.
export const NOTICE_MS = 5000

type Listener = (msg: string) => void

let listener: Listener | null = null

/// The host registers itself; passing null unregisters. Exactly one host
/// (the app shell) — a second would mean two toasts saying the same
/// thing.
export function onNotice(fn: Listener | null) {
  listener = fn
}

/// Say something went wrong. Silently dropped when no host is mounted,
/// which is the login screen and the boot phase — nowhere with anywhere
/// to put it.
export function notify(msg: string) {
  listener?.(msg)
}
