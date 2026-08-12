/// Where the film is, as opposed to where the video element is.
///
/// The element's clock starts at whatever the current pipeline run began
/// producing, and a multi-part source restarts it again per part. Everything a
/// viewer sees — the clock, the seekbar, every subtitle path — is in absolute
/// film time, so the conversion happens in one place rather than at each.

/// Absolute position. `known` is false until the element has loaded metadata,
/// where its `currentTime` is 0 and would read as the start of the film rather
/// than as "not yet".
export function absoluteMs(p: {
  known: boolean
  offsetMs: number
  currentTimeS: number
  resumeMs: number
}): number {
  return p.known ? p.offsetMs + p.currentTimeS * 1000 : p.resumeMs
}

/// How much of the film the pipeline has actually written. 0 when the element
/// has no seekable range yet, which is not the same as "nothing produced" but
/// is the only honest answer available.
export function producedEndMs(p: { offsetMs: number; seekableEndS: number | null }): number {
  return p.seekableEndS === null ? 0 : p.offsetMs + p.seekableEndS * 1000
}

/// A seek is either inside what the pipeline has produced — a jump the element
/// can do by itself — or past it, which needs the pipeline restarted at the
/// target.
export type SeekPlan =
  | { kind: 'in-run'; toS: number }
  | { kind: 'restart'; toMs: number }
  /// Direct play has no runs: the whole file is there.
  | { kind: 'whole-file'; toS: number }

export function planSeek(p: {
  targetMs: number
  offsetMs: number
  producedEndS: number
  isHls: boolean
}): SeekPlan {
  if (!p.isHls) return { kind: 'whole-file', toS: p.targetMs / 1000 }
  const inRunS = (p.targetMs - p.offsetMs) / 1000
  if (inRunS >= 0 && inRunS <= p.producedEndS) return { kind: 'in-run', toS: inRunS }
  return { kind: 'restart', toMs: p.targetMs }
}

/// Where a nudge lands. `durationMs` is 0 when the hub has no probed duration,
/// and clamping to it turned every nudge — both buttons and both arrow keys —
/// into a jump to the start of the film.
export function nudgeTarget(p: { posMs: number; bySec: number; durationMs: number }): number {
  const want = p.posMs + p.bySec * 1000
  return Math.max(0, p.durationMs > 0 ? Math.min(p.durationMs, want) : want)
}

/// A part's playlist ending is not the film ending. Within three seconds of the
/// end it is, and a quarter second past the boundary is what moves the pipeline
/// into the next part rather than re-producing the end of this one.
export function nextPartSeekMs(p: {
  absMs: number
  durationMs: number
  parts: number
  isHls: boolean
}): number | null {
  if (!p.isHls || p.parts <= 1 || p.durationMs <= 0) return null
  return p.absMs < p.durationMs - 3000 ? p.absMs + 250 : null
}
