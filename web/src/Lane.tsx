import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import Icon from './icons'

/// A row that scrolls sideways, with arrows that appear on hover and only
/// on the side there is more to see — so a lane whose contents fit shows
/// neither.
///
/// The edges are read off the element rather than computed from the number
/// of children: the element knows its own overflow, and a card's width
/// depends on the font. A ResizeObserver catches the cases a scroll event
/// never reports — the window narrowing, or images arriving and widening
/// the row after it was first measured.
export default function Lane({
  className,
  /// How far one press moves. Callers pass a whole number of cards, so a
  /// press always lands on a card boundary.
  step,
  /// Called when the end is less than one press away, so the caller can
  /// fetch more before the viewer reaches it. Fired on scroll and on
  /// layout, and safe to call repeatedly — the caller is expected to
  /// ignore it while a fetch is already out.
  onNearEnd,
  children,
}: {
  className?: string
  step: number
  onNearEnd?: () => void
  children: React.ReactNode
}) {
  const lane = useRef<HTMLDivElement | null>(null)
  const [more, setMore] = useState({ left: false, right: false })
  /// The lane width the last `onNearEnd` was fired at, or -1 when the lane is
  /// not near its end. Scrolling away and back asks again; sitting still does
  /// not.
  const firedAt = useRef(-1)

  const readEdges = useCallback(() => {
    const el = lane.current
    if (!el) return
    const left = el.scrollLeft > 1
    // A pixel of slack: fractional layout widths mean scrollLeft never
    // quite reaches the arithmetic end, and an arrow that cannot move
    // anything is worse than no arrow.
    const right = el.scrollLeft + el.clientWidth < el.scrollWidth - 1
    // Same values, same object — so this can run on every render without
    // becoming a render loop.
    setMore((prev) => (prev.left === left && prev.right === right ? prev : { left, right }))
    // Fetch before the viewer arrives, not when they hit the wall. One
    // press from the end is the natural threshold: the same distance the
    // arrow moves.
    const near = el.scrollWidth - (el.scrollLeft + el.clientWidth) < step
    // Once per width, not once per render. This runs on EVERY render — it has
    // to, for the arrows — and a lane sitting at its end is near it on all of
    // them, so a shelf scrolled to the end fetched another page on every
    // keystroke in the search box, appending items nobody had scrolled to. A
    // page that failed was worse: its toast re-rendered the shell, which fired
    // it again, which failed again. The width changes exactly when new cards
    // arrive, which is exactly when asking again is meaningful.
    if (!near) {
      firedAt.current = -1
      return
    }
    if (firedAt.current === el.scrollWidth) return
    firedAt.current = el.scrollWidth
    onNearEnd?.()
  }, [step, onNearEnd])

  // Every render, because children can change without the element
  // scrolling: a lane that overflows before it is ever touched is the
  // common case, and the right arrow has to be there the first time you
  // look at it.
  useLayoutEffect(readEdges)

  useEffect(() => {
    const el = lane.current
    if (!el || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(readEdges)
    ro.observe(el)
    return () => ro.disconnect()
  }, [readEdges])

  const nudge = (by: number) => lane.current?.scrollBy({ left: by, behavior: 'smooth' })

  return (
    // Both arrows are always present while the pointer is over the lane —
    // the one that cannot move anything is disabled, not removed. Removing
    // it took the target out from under the cursor at the moment of the
    // click, which then landed on the card underneath and opened it. A
    // disabled button keeps the hit area and swallows that click.
    <div className="shelf-lane-wrap">
      <button
        className="lane-nudge left"
        title="Scroll left"
        disabled={!more.left}
        onClick={() => nudge(-step)}
      >
        <Icon name="chevronLeft" size={20} />
      </button>
      <div
        className={className ? `shelf-lane ${className}` : 'shelf-lane'}
        ref={lane}
        onScroll={readEdges}
      >
        {children}
      </div>
      <button
        className="lane-nudge right"
        title="Scroll right"
        disabled={!more.right}
        onClick={() => nudge(step)}
      >
        <Icon name="chevronRight" size={20} />
      </button>
    </div>
  )
}
