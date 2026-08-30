//! Per-library access grants (HUB-10): which libraries an account may
//! see, and so which items it may browse, search, open, fetch artwork
//! for, download subtitles for, and play.
//!
//! ## What is stored
//!
//! `users.all_libraries`, a flag defaulting to 1; `user_libraries`, a plain
//! (user, library) list; and `users.grants_version`, a counter bumped by every
//! write to either of them:
//!
//! | `all_libraries` | rows in `user_libraries` | the account sees |
//! |-----------------|--------------------------|------------------|
//! | 1               | ignored                  | every library, including ones created later |
//! | 0               | some                     | exactly those |
//! | 0               | none                     | nothing |
//!
//! `grants_version` is what makes a wholesale write safe to make concurrently
//! (UI-25). A reader is told the version it read at; a writer sends it back
//! and the `UPDATE` matches only while it still holds, so of two admins who
//! read the same state and both submit, the second is refused rather than
//! silently replacing the first. See [`set_access`].
//!
//! The flag is there so that "nothing" is expressible. A list on its own
//! cannot say it: with no rows meaning "everything" you can never revoke
//! the last library, and with no rows meaning "nothing" every library
//! created later is invisible until someone remembers to hand it out.
//! The flag also makes the upgrade honest — an existing hub migrates with
//! every account unrestricted, which is what it had a moment earlier.
//!
//! Grants attach to libraries, never directly to collections (0008): a
//! collection owns catalogue identities, while a library is the composition a
//! person is given. `library_collections` therefore decides visibility. Child
//! items carry the same collection as their parent, so episodes and tracks
//! inherit visibility naturally without an item-level membership projection.
//!
//! An item in no library at all is invisible to a restricted account.
//! There is nothing to grant that would reach it; attach its collection
//! to a library first.
//!
//! ## Who bypasses
//!
//! Admins, always — which is why every entry point here takes [`Claims`]
//! rather than a user id, so a caller cannot forget. The admin role is
//! the configuration role: it can grant itself any library through the
//! same endpoint that would restrict it, so enforcing one against it is
//! theatre with a per-request cost. An admin's grant rows are stored and
//! simply not consulted until `is_admin` comes off.
//!
//! ## Why 404 and never 403
//!
//! Denials answer 404, for library ids as much as item ids. 403 says
//! "this exists and is not yours", which turns every endpoint taking an
//! id into an oracle for the rest of the catalogue. 404 tells a
//! restricted account the one thing it is entitled to know: its own
//! grants did not cover that. Callers own the status; what this module
//! returns is a bool.
//!
//! ## Cost
//!
//! An unrestricted account — every account on a single-user hub, and
//! every admin — never reaches the predicates below. Callers resolve
//! [`restricted`] once and otherwise run the SQL they always ran, so the
//! measured NFR-1 browse plans are untouched. A restricted account pays
//! one indexed read per item-scoped request, and one probe per candidate
//! row on the two scan-shaped browses: the cost class of the in-library
//! search predicate that has always been there.

use anyhow::Result;
use kahawai_sqlite::Database as SqlitePool;
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;

use crate::auth::Claims;

/// Whether the grant predicates apply to this request at all.
///
/// An unknown user id reads as restricted-with-no-grants. It cannot
/// happen behind `require_auth` — the token was signed for an account —
/// but the direction a missing row falls is not something to leave to
/// `unwrap_or_default`.
pub async fn restricted(db: &SqlitePool, claims: &Claims) -> Result<bool> {
    if claims.admin {
        return Ok(false);
    }
    let all: Option<i64> = sqlx::query_scalar("SELECT all_libraries FROM users WHERE id = ?")
        .bind(&claims.sub)
        .fetch_optional(db)
        .await?;
    Ok(all.unwrap_or(0) == 0)
}

/// May this account see this item — or the show/album it belongs to?
///
/// One statement: the flag and the membership probe share a round trip,
/// because the caller that asks this is usually about to do one thing
/// with the answer and two queries would be two waits.
pub async fn can_see(db: &SqlitePool, claims: &Claims, item_id: &str) -> Result<bool> {
    if claims.admin {
        return Ok(true);
    }
    let ok: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND all_libraries=1)
             OR EXISTS(SELECT 1 FROM items i JOIN library_collections lc
                  ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)
                  JOIN user_libraries ul ON ul.library_id=lc.library_id AND ul.user_id=?1
                 WHERE i.id=?2)",
    )
    .bind(&claims.sub)
    .bind(item_id)
    .fetch_one(db)
    .await?;
    Ok(ok != 0)
}

/// May this account see this library? True for a library that does not
/// exist when the account is unrestricted — "no such library" is the
/// caller's own 404 to give, not a grant decision.
pub async fn can_see_library(db: &SqlitePool, claims: &Claims, library_id: &str) -> Result<bool> {
    if claims.admin {
        return Ok(true);
    }
    let ok: i64 = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = ?1 AND all_libraries = 1)
             OR EXISTS (SELECT 1 FROM user_libraries WHERE user_id = ?1 AND library_id = ?2)",
    )
    .bind(&claims.sub)
    .bind(library_id)
    .fetch_one(db)
    .await?;
    Ok(ok != 0)
}

/// The membership predicate for a browse scan, correlated on the
/// candidate alias `c` — the same shape and the same alias as the
/// in-library search fragment it sits beside.
///
/// `?1` is the user id in every browse query already (it is what
/// `watch_state` joins on), so this binds nothing new. Only ever
/// interpolated when [`restricted`] said so: it carries no
/// `all_libraries` check of its own, which is what keeps it a single
/// indexed probe instead of a per-row lookup in `users`.
pub const VISIBLE_C: &str = "\
AND EXISTS (SELECT 1 FROM library_collections lc
              JOIN user_libraries ul
                ON ul.library_id=lc.library_id AND ul.user_id=?1
             WHERE (lc.module_id,lc.collection_id)=(c.module_id,c.collection_id))";

/// The same restriction, for the navigation library a browse row carries.
///
/// `VISIBLE_C` decides which items a restricted account may SEE; this decides
/// which library such a row is allowed to NAME. Without it the row reports
/// `MIN(library_id)` over every library the item belongs to, which for an item
/// in a withheld and a granted library is the withheld one — a denial that
/// answers, in the module whose whole point is that denials do not. The client
/// then navigates there and gets a 404.
///
/// Correlated on `il`, so it belongs inside that subquery rather than beside
/// it, and interpolated only when [`restricted`] said so: an unrestricted
/// account has no `user_libraries` rows at all, so applying this to everyone
/// would answer NULL for everyone.
pub const VISIBLE_LIB: &str = "\
AND EXISTS (SELECT 1 FROM user_libraries ul
             WHERE ul.library_id=lc.library_id AND ul.user_id=?1)";

#[derive(Debug, Serialize, ToSchema)]
pub struct UserAccess {
    pub id: String,
    pub username: String,
    pub is_admin: bool,
    pub all_libraries: bool,
    pub created_at: i64,
    pub libraries: Vec<String>,
    /// What this account's grants were when they were read, for the write
    /// that follows. See [`set_access`].
    pub grants_version: i64,
}

/// Every account with its access, for the admin panel. Sorted by name,
/// which is how the panel lists them and how a diff between two hubs
/// stays readable.
pub async fn users_with_access(db: &SqlitePool) -> Result<Vec<UserAccess>> {
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.is_admin, u.all_libraries, u.created_at, u.grants_version,
                (SELECT json_group_array(ul.library_id)
                   FROM user_libraries ul WHERE ul.user_id = u.id) AS libraries
           FROM users u ORDER BY u.username",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| UserAccess {
            id: r.get("id"),
            username: r.get("username"),
            is_admin: r.get::<i64, _>("is_admin") != 0,
            all_libraries: r.get::<i64, _>("all_libraries") != 0,
            created_at: r.get("created_at"),
            grants_version: r.get("grants_version"),
            libraries: serde_json::from_str(&r.get::<String, _>("libraries")).unwrap_or_default(),
        })
        .collect())
}

/// What a write to an account's grants did. Each is something a caller can
/// act on, so they are return values rather than errors to read prose out of;
/// an `Err` from [`set_access`] is the database being unavailable.
#[derive(Debug, PartialEq, Eq)]
pub enum SetAccess {
    Applied {
        grants_version: i64,
        /// What was stored, read inside the same transaction that wrote it.
        ///
        /// The caller used to read it back afterwards, which left a window: a
        /// second admin's write landing in between paired THIS write's version
        /// with THEIR library set, and the panel painted their chips as ours.
        /// It self-healed on the next click — a spent version is refused — but
        /// a feature whose whole purpose is that the two agree should not have
        /// a moment where they do not.
        libraries: Vec<String>,
    },
    /// Somebody else wrote since this admin read. UI-25: the panel sends the
    /// COMPLETE set rather than a delta, so a second write does not merge with
    /// the first — it replaces it, and the first admin's change is gone with
    /// nothing said. A version turns that into a refusal they can see.
    Stale,
    NoSuchUser,
}

/// Replace an account's access wholesale, in one transaction, if nobody else
/// has written since it was read.
///
/// Wholesale rather than add/remove because that is what a panel of
/// checkboxes has in hand, and because two clients toggling different boxes
/// should not be able to interleave into a set neither asked for. That is also
/// why it needs a version: a wholesale write does not merge, so without one
/// the second writer silently replaces the first (UI-25).
///
/// `expected` is the `grants_version` the caller was shown. The check and the
/// write are one statement, so two admins racing cannot both pass it: the
/// loser's `UPDATE` matches no row and is told so.
///
/// Library ids that do not exist are dropped rather than refused — the insert
/// selects from `libraries`, so a stale id from a client holding an old list
/// cannot fail the whole call. The caller reads the stored set back and can
/// see what landed.
pub async fn set_access(
    db: &SqlitePool,
    user_id: &str,
    expected: i64,
    all_libraries: bool,
    libraries: &[String],
) -> Result<SetAccess> {
    let mut tx = db.begin().await?;
    let res = sqlx::query(
        "UPDATE users SET all_libraries = ?, grants_version = grants_version + 1
          WHERE id = ? AND grants_version = ?",
    )
    .bind(all_libraries)
    .bind(user_id)
    .bind(expected)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        // Absent, or written since. Tell them apart so an admin looking at a
        // user who was deleted under them does not read "try again".
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
        return Ok(if exists.is_some() {
            SetAccess::Stale
        } else {
            SetAccess::NoSuchUser
        });
    }
    sqlx::query("DELETE FROM user_libraries WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for library_id in libraries {
        sqlx::query(
            "INSERT OR IGNORE INTO user_libraries (user_id, library_id)
             SELECT ?1, id FROM libraries WHERE id = ?2",
        )
        .bind(user_id)
        .bind(library_id)
        .execute(&mut *tx)
        .await?;
    }
    let grants_version: i64 = sqlx::query_scalar("SELECT grants_version FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT library_id FROM user_libraries WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    tracing::info!(
        user_id,
        all_libraries,
        granted = libraries.len(),
        grants_version,
        "library access set"
    );
    Ok(SetAccess::Applied {
        grants_version,
        libraries: stored,
    })
}
