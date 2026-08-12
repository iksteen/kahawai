import { useRef, useState } from 'react'

/// Move one item to another position. Dragging says exactly where something
/// goes, which a swap with its neighbour cannot express in one gesture.
/// Returns null when the move would not change anything, so a caller can
/// skip the write.
export function moved<T>(list: T[], from: number, to: number): T[] | null {
  // `from` is bounded like `to`. It was not, and an out-of-range source spliced
  // out nothing and spliced `undefined` back in — a list one longer with a hole
  // in it, saved as the new order. `useDragOrder` holds the source index in a
  // ref for the whole gesture, so a list that changes under it produces exactly
  // that.
  if (from === to || from < 0 || to < 0 || from >= list.length || to >= list.length) return null
  const next = [...list]
  const [taken] = next.splice(from, 1)
  next.splice(to, 0, taken)
  return next
}

/// Add `item` to an ordered preference list, above a pinned last-resort entry.
///
/// Appending is wrong when the list carries a backstop that matches almost
/// anything: `original` in an audio wishlist resolves to the file's own
/// language, so a language added AFTER it is never reached and the setting
/// silently does nothing. Inserted immediately before the pin, wherever the pin
/// sits — moving the pin to the end instead would rewrite an order the viewer
/// chose, since it is reorderable on purpose.
export function addAbove<T>(list: T[], item: T, pin: T): T[] {
  const at = list.indexOf(pin)
  return at === -1 ? [...list, item] : [...list.slice(0, at), item, ...list.slice(at)]
}

/// Drag-to-reorder, as row props. Three lists want this gesture — languages
/// and subtitle fallbacks in Settings, provider precedence in Admin — and
/// the fiddly part is worth getting wrong in only one place: `drop` has to
/// read the source index in the same gesture that set it, before any state
/// has committed, so the source lives in a ref. The state alongside it is
/// for looks only and may lag a frame.
export function useDragOrder(move: (from: number, to: number) => void) {
  const from = useRef<number | null>(null)
  const [lifting, setLifting] = useState<number | null>(null)
  const [over, setOver] = useState<number | null>(null)
  const clear = () => {
    from.current = null
    setLifting(null)
    setOver(null)
  }
  /// Spread onto the row. `className` stays the caller's.
  const row = (i: number) => ({
    draggable: true,
    onDragStart: (e: React.DragEvent) => {
      from.current = i
      setLifting(i)
      // Firefox starts no drag at all unless the event carries data, however
      // little — the index is what this needs anyway, and every drop reads it
      // from the ref rather than from here.
      e.dataTransfer?.setData('text/plain', String(i))
    },
    onDragEnter: () => setOver(i),
    onDragOver: (e: React.DragEvent) => e.preventDefault(),
    onDrop: () => {
      if (from.current !== null) move(from.current, i)
      clear()
    },
    onDragEnd: clear,
  })
  /// What the row should look like mid-drag.
  const look = (i: number) => `${lifting === i ? ' lifting' : ''}${over === i ? ' over' : ''}`
  return { row, look }
}
