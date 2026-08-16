/// A row that scrolls sideways: which arrows are live, and when to ask for
/// more.
///
/// Here rather than in the component because a test environment has no layout
/// — every `scrollWidth` is zero in happy-dom — so this is the half that can
/// be checked at all. The component reads the numbers off the element and
/// hands them here.

export type Extent = {
  scrollLeft: number
  clientWidth: number
  scrollWidth: number
}

/// Which way there is more to see.
///
/// Read off the element rather than computed from the number of children: the
/// element knows its own overflow, and a card's width depends on the font.
export function edges(at: Extent): { left: boolean; right: boolean } {
  return {
    left: at.scrollLeft > 1,
    // A pixel of slack: fractional layout widths mean `scrollLeft` never quite
    // reaches the arithmetic end, and an arrow that cannot move anything is
    // worse than no arrow.
    right: at.scrollLeft + at.clientWidth < at.scrollWidth - 1,
  }
}

/// Whether the end is within one press. Fetch before the viewer arrives rather
/// than when they hit the wall, and one press is the natural threshold — the
/// same distance the arrow moves.
export function nearEnd(at: Extent, step: number): boolean {
  return at.scrollWidth - (at.scrollLeft + at.clientWidth) < step
}

/// Once per WIDTH, not once per look.
///
/// The edges are re-read on every render — they have to be, for the arrows —
/// and a lane sitting at its end is near it on all of them. Without this, a
/// shelf scrolled to the end fetched another page on every keystroke in the
/// search box, appending items nobody had scrolled to. A page that FAILED was
/// worse: its notice re-rendered the shell, which asked again, which failed
/// again. The width changes exactly when new cards arrive, which is exactly
/// when asking again is meaningful.
///
/// `-1` means "not near the end": scrolling away and back asks again, sitting
/// still does not.
export function askAgain(
  firedAt: number,
  at: Extent,
  step: number,
): { ask: boolean; firedAt: number } {
  if (!nearEnd(at, step)) return { ask: false, firedAt: -1 }
  if (firedAt === at.scrollWidth) return { ask: false, firedAt }
  return { ask: true, firedAt: at.scrollWidth }
}
