/// Moving one row of an ordered preference to another place.
///
/// Three lists want this — languages and subtitle fallbacks in Settings,
/// provider precedence in Admin — and the fiddly parts are worth getting wrong
/// in only one place.

/// Move one item to another position.
///
/// Dragging says exactly where something goes, which a swap with its
/// neighbour cannot express in one gesture. `null` when the move would change
/// nothing, so the caller can skip the write.
export function moved<T>(list: T[], from: number, to: number): T[] | null {
  // `from` is bounded like `to`. It was not, and an out-of-range source
  // spliced out nothing and spliced `undefined` back in — a list one longer
  // with a hole in it, saved as the new order. The gesture holds its source
  // index for its whole duration, so a list that changes underneath it
  // produces exactly that.
  if (from === to || from < 0 || to < 0 || from >= list.length || to >= list.length) return null
  const next = [...list]
  const [taken] = next.splice(from, 1)
  next.splice(to, 0, taken!)
  return next
}

/// Add an entry to an ordered preference list, above a pinned last resort.
///
/// Appending is wrong when the list carries a backstop that matches almost
/// anything: `original` in an audio wishlist resolves to the file's own
/// language, so a language added AFTER it is never reached and the setting
/// silently does nothing. Inserted immediately before the pin, wherever the
/// pin sits — moving the pin to the end instead would rewrite an order the
/// viewer chose, since it is reorderable on purpose.
export function addAbove<T>(list: T[], item: T, pin: T): T[] {
  const at = list.indexOf(pin)
  return at === -1 ? [...list, item] : [...list.slice(0, at), item, ...list.slice(at)]
}

/// UI-12: the keyboard's version of the same gesture.
///
/// A drag is a mouse gesture and nothing else, so an ordered list that can
/// only be dragged cannot be ordered at all without one. Moving a row one
/// place at a time is what a keyboard can express — it takes more presses than
/// a drag takes seconds, and it is the difference between "fiddly" and
/// "impossible".
export function step<T>(list: T[], at: number, by: -1 | 1): T[] | null {
  return moved(list, at, at + by)
}
