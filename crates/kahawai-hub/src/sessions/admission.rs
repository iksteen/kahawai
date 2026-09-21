use super::*;

/// Holds a per-user admission slot for as long as a start is in flight.
///
/// Drop is the only exit every path shares: the thirteen early returns inside
/// `start_inner`, the error paths, and the caller abandoning the request, which
/// drops the whole future and runs no statement after the await.
pub(super) struct SlotGuard<'a> {
    pub(super) sessions: &'a Sessions,
    pub(super) id: String,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.sessions.release(&self.id);
    }
}

impl Sessions {
    /// Take a slot for `user_id` under `id`, or refuse. The count is
    /// the union of `reserved` and `active`, taken in ONE critical
    /// section — which is the whole point: the previous check counted
    /// `active` alone and released the lock long before the session
    /// landed there.
    ///
    /// Lock order is `reserved` then `active`, and nothing else takes
    /// both, so this cannot deadlock. Counting the union also makes the
    /// window between the insert into `active` and [`Self::release`]
    /// harmless: a session in both is still one session.
    pub(super) fn admit(&self, id: &str, user_id: &str) -> Result<()> {
        let mut reserved = self.reserved.lock().unwrap();
        let held = reserved.values().filter(|u| *u == user_id).count()
            + self
                .active
                .lock()
                .unwrap()
                .values()
                .filter(|s| s.user_id == user_id && !reserved.contains_key(&s.id))
                .count();
        if held >= self.max_per_user {
            bail!(SessionCap { held });
        }
        reserved.insert(id.to_string(), user_id.to_string());
        Ok(())
    }

    /// Give the slot back. Called once `start` knows the outcome —
    /// either the session is in `active` and counts itself, or it never
    /// started and must not count at all.
    pub(super) fn release(&self, id: &str) {
        self.reserved.lock().unwrap().remove(id);
    }
}
