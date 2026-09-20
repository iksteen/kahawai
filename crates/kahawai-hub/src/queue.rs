//! The one loop every durable hub queue runs on.
//!
//! A queue here is a table of rows with a state, a due time and a lease
//! (`enrichment_jobs`, `subtitle_jobs`). The driver does not know the
//! table; it knows the rhythm: claim and work while there is something to
//! do, and when there is not, sleep until the earliest moment the clock
//! can make a row claimable again — or until something wakes it sooner.
//!
//! Wakes are the primary signal: a catalogue commit created rows, a
//! mediahost reconnected, a landing changed what is missing. The fallback
//! tick is lost-event insurance only, and its cost is one indexed
//! `SELECT` per tick, so it can be slow. A step that fails is retried
//! after a short pause rather than immediately: a broken query must not
//! become a hot loop.
//!
//! The `notified()` future is created BEFORE the step runs, so a wake
//! that arrives while the step is busy is not lost — it is exactly the
//! wake that says "there is more now than when you looked".

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

/// What one step found.
pub(crate) enum Step {
    /// Did something; look again at once.
    Worked,
    /// Nothing claimable. `next_due` is the unix time the clock next makes
    /// a row claimable (a due time or a lease expiry), if any.
    Idle { next_due: Option<i64> },
}

/// How long a failing step waits before trying again.
const ERROR_PAUSE: Duration = Duration::from_secs(2);

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// How long to sleep for an idle step: until `next_due`, capped by the
/// fallback, and never negative.
pub(crate) fn idle_for(next_due: Option<i64>, fallback: Duration, now: i64) -> Duration {
    match next_due {
        Some(due) => Duration::from_secs(due.saturating_sub(now).max(0) as u64).min(fallback),
        None => fallback,
    }
}

/// Run `step` for the life of the process.
pub(crate) fn spawn<F, Fut>(
    name: &'static str,
    wake: Arc<Notify>,
    fallback: Duration,
    mut step: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Step>> + Send,
{
    tokio::spawn(async move {
        loop {
            let notified = wake.notified();
            let pause = match step().await {
                Ok(Step::Worked) => continue,
                Ok(Step::Idle { next_due }) => idle_for(next_due, fallback, now()),
                Err(error) => {
                    tracing::error!(
                        queue = name,
                        error = format!("{error:#}"),
                        "queue step failed"
                    );
                    ERROR_PAUSE
                }
            };
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(pause) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_sleeps_until_the_next_due_time_but_never_past_the_fallback() {
        let fallback = Duration::from_secs(600);
        assert_eq!(idle_for(None, fallback, 100), fallback);
        assert_eq!(idle_for(Some(130), fallback, 100), Duration::from_secs(30));
        assert_eq!(idle_for(Some(90), fallback, 100), Duration::ZERO);
        assert_eq!(idle_for(Some(10_000), fallback, 100), fallback);
    }

    #[tokio::test]
    async fn a_wake_during_a_step_is_not_lost() {
        let wake = Arc::new(Notify::new());
        let steps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let counter = steps.clone();
        let inner = wake.clone();
        let handle = spawn("test", wake.clone(), Duration::from_secs(3600), move || {
            let counter = counter.clone();
            let tx = tx.clone();
            let inner = inner.clone();
            async move {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx.send(n).unwrap();
                if n == 0 {
                    // Woken while still inside the step: must run again
                    // without waiting out the hour.
                    inner.notify_waiters();
                }
                Ok(Step::Idle { next_due: None })
            }
        });
        assert_eq!(rx.recv().await, Some(0));
        assert_eq!(rx.recv().await, Some(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "nothing woke it, so it sleeps"
        );
        wake.notify_waiters();
        assert_eq!(rx.recv().await, Some(2), "an explicit wake ends the sleep");
        handle.abort();
    }
}
