/// A FIFO for state-changing requests.
///
/// Optimistic controls may render every click immediately, but the server must
/// receive their whole-state writes in that same order. Ignoring stale replies
/// is not enough: an older request can commit after a newer one and leave the
/// persisted value opposite to the screen. This queue orders the commits and
/// keeps going after a refusal.
export class SerialQueue {
  private tail: Promise<void> = Promise.resolve()

  run<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.tail.then(operation)
    // The queue itself never stays rejected: one failed save must not prevent
    // the next click from reaching the server.
    this.tail = result.then(
      () => undefined,
      () => undefined,
    )
    return result
  }
}
