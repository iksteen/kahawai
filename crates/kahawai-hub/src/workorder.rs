//! The order hub-driven extraction sweeps work through a backlog.
//!
//! Every sweep here faces the same problem: thousands of files, seconds to
//! tens of seconds each, and one of them is the thing somebody is about to
//! press Play on. Left to the catalogue's own order that is alphabetical by
//! path, which is the one order nobody is ever waiting on.
//!
//! The rule, in order:
//!
//!   1. In flight — the movie, season or series someone is part-way through
//!      or has queued up next, across every user on this hub.
//!   2. Newest first, by mtime.
//!   3. Movies ahead of episodes within a tier.
//!   4. Path, so rounds are deterministic.
//!
//! Tiers 2-4 match what the mediahost already applies to its own loudness
//! work (`loudness::priority`), so a file's position does not depend on
//! which side scheduled it. Tier 1 cannot: watch state is the hub's, and the
//! mediahost has never been told it.

use std::collections::HashSet;

/// The most recently touched watch rows to treat as in flight. Bounded
/// because this is a hint, not a queue: a household's live viewing is a
/// handful of titles, and reading every row ever written would rank a show
/// abandoned last year alongside tonight's.
const IN_FLIGHT_ROWS: i64 = 500;

/// What a sweep knows about one candidate file.
pub(crate) struct Ordered {
    pub in_flight: bool,
    pub movie: bool,
    pub mtime_unix: i64,
    pub path_rel: String,
}

impl Ordered {
    /// Ascending, so a plain `sort_by` puts the most wanted work first — the
    /// opposite convention to `loudness::priority`, which sorts ascending
    /// and pops the greatest. Same resulting order; the queues are drained
    /// from opposite ends.
    pub(crate) fn first(&self, other: &Self) -> std::cmp::Ordering {
        other
            .in_flight
            .cmp(&self.in_flight)
            .then(other.movie.cmp(&self.movie))
            .then(other.mtime_unix.cmp(&self.mtime_unix))
            .then_with(|| self.path_rel.cmp(&other.path_rel))
    }
}

/// Library items any user is part-way through, plus the series they and
/// recently finished episodes belong to — so the rest of a season someone is
/// watching ranks with it rather than behind the alphabet.
///
/// Not every recent row: a watch row is written when an episode is finished
/// and when one is marked watched by hand, and marking a long series watched
/// writes hundreds at once. Those say nothing about what anyone is about to
/// play, and left unfiltered they would push the handful of genuinely
/// part-watched titles out of a bounded window — prioritising exactly the
/// series nobody needs next.
///
/// So the two signals are read differently. A part-watched item (started,
/// not finished) brings itself and its parent: somebody is in the middle of
/// it. A finished one brings only its parent, for the episode after it, and
/// sorts behind every part-watched row so a bulk mark-watched can never
/// displace live viewing.
///
/// Every user, because a sweep serves the hub rather than a request. On a
/// household hub that union is a few titles; the bound keeps it that way on
/// a larger one.
pub(crate) async fn in_flight(db: &sqlx::SqlitePool) -> HashSet<String> {
    let rows = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT item_id, parent_id, played FROM catalogue_watch_state
         WHERE played = 1 OR position_ms > 0
         ORDER BY played ASC, updated_at DESC LIMIT ?",
    )
    .bind(IN_FLIGHT_ROWS)
    .fetch_all(db)
    .await;
    match rows {
        Ok(rows) => rows
            .into_iter()
            .flat_map(|(item, parent, played)| {
                // A finished episode is not itself worth warming again.
                [if played { String::new() } else { item }, parent]
            })
            .filter(|id| !id.is_empty())
            .collect(),
        Err(error) => {
            // An ordering hint is not worth failing a sweep over; without it
            // the backlog is still drained, just newest-first only.
            tracing::warn!(%error, "could not read in-flight items for sweep ordering");
            HashSet::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn watch_row(
        db: &kahawai_sqlite::Database,
        item: &str,
        parent: &str,
        pos: i64,
        played: i64,
    ) {
        let (item, parent) = (item.to_string(), parent.to_string());
        db.write("test watch row", move |connection| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO catalogue_watch_state
                     (user_id,item_id,parent_id,position_ms,played,updated_at)
                     VALUES ('u',?,?,?,?,?)",
                )
                .bind(item)
                .bind(parent)
                .bind(pos)
                .bind(played)
                // Finished rows are the NEWER ones here: a bulk mark-watched
                // is exactly the case where recency must not win.
                .bind(1_000 + played)
                .execute(&mut *connection)
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn marking_a_series_watched_does_not_displace_what_is_being_watched() {
        let db = crate::db::open_in_memory().await.unwrap();
        db.write("test user", |connection| {
            Box::pin(async move {
                sqlx::query("INSERT INTO users (id,username,password_hash) VALUES ('u','u','x')")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        watch_row(&db, "ep-live", "series-live", 500, 0).await;
        // Somebody marked an entire other series watched, just now.
        for n in 0..50 {
            watch_row(&db, &format!("ep-done-{n}"), "series-done", 0, 1).await;
        }
        // And an item that was opened but never actually started.
        watch_row(&db, "ep-untouched", "series-untouched", 0, 0).await;

        let flight = in_flight(&db).await;
        assert!(
            flight.contains("ep-live") && flight.contains("series-live"),
            "the part-watched episode and its series stay in flight"
        );
        assert!(
            flight.contains("series-done"),
            "a finished episode still pulls its series forward, for the next one"
        );
        assert!(
            !flight.contains("ep-done-0"),
            "but not the finished episode itself"
        );
        assert!(
            !flight.contains("ep-untouched") && !flight.contains("series-untouched"),
            "an item nobody has started is not in flight"
        );
    }

    fn c(in_flight: bool, movie: bool, mtime_unix: i64, path_rel: &str) -> Ordered {
        Ordered {
            in_flight,
            movie,
            mtime_unix,
            path_rel: path_rel.into(),
        }
    }

    #[test]
    fn in_flight_outranks_everything_else() {
        let mut v = [
            c(false, true, 9_000, "new-movie.mkv"),
            c(true, false, 1, "the-show-s01e04.mkv"),
        ];
        v.sort_by(Ordered::first);
        assert_eq!(v[0].path_rel, "the-show-s01e04.mkv");
    }

    #[test]
    fn then_movies_then_newest_then_path() {
        let mut v = [
            c(false, false, 5, "episode-old.mkv"),
            c(false, false, 9, "episode-new.mkv"),
            c(false, true, 5, "movie.mkv"),
            c(false, false, 9, "episode-new-b.mkv"),
        ];
        v.sort_by(Ordered::first);
        assert_eq!(
            v.iter().map(|o| o.path_rel.as_str()).collect::<Vec<_>>(),
            vec![
                "movie.mkv",
                "episode-new-b.mkv",
                "episode-new.mkv",
                "episode-old.mkv"
            ],
            "movies first, then newest, ties broken by path"
        );
    }
}
