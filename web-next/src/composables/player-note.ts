/// A notice painted inside the picture, for failures the toast cannot carry.
///
/// The shell's toast host is a sibling of `.videobox`, and `.videobox` is what
/// goes fullscreen — while it is, the browser paints only that subtree, so a
/// toast raised there is shown to nobody. That is exactly the mode where a
/// freeze is most alarming.
///
/// The same shape as `toast.ts` and for the same reason: any part of the player
/// says something went wrong by calling a function, with nothing threaded down
/// to it. Latest wins, because two failures in a row are usually one failure
/// twice.

/// Milliseconds a note stays up. Longer than a toast: these interrupt a film,
/// so the reader's attention is elsewhere.
export const NOTE_MS = 6000

type Listener = (msg: string) => void

let listener: Listener | null = null

/// The host registers itself; null unregisters. One host, mounted with the
/// player — a note has nowhere to go when nothing is playing.
export function onPlayerNote(fn: Listener | null) {
  listener = fn
}

export function playerNote(msg: string) {
  listener?.(msg)
}
