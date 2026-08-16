/// A FIFO for state-changing requests, one per key.
///
/// Optimistic controls render every click immediately, but the server has to
/// receive their whole-state writes in that same order. Ignoring stale replies
/// is not enough: an older request can commit AFTER a newer one and leave the
/// persisted value opposite to what is on screen. This orders the commits and
/// keeps going after a refusal.
///
/// Per key, not global: two settings that have nothing to do with each other
/// must not queue behind one another, and a slow write to one would otherwise
/// hold up every other control on the page.

export class SerialQueue {
  private tails = new Map<string, Promise<unknown>>()

  run<T>(key: string, operation: () => Promise<T>): Promise<T> {
    const after = this.tails.get(key) ?? Promise.resolve()
    const result = after.then(operation)
    // The queue itself never stays rejected: one failed save must not stop the
    // next click from reaching the server.
    this.tails.set(
      key,
      result.then(
        () => undefined,
        () => undefined,
      ),
    )
    return result
  }
}
