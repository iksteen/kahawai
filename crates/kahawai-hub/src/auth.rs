//! Users, first-run setup, and token auth (OPS-1, HUB-10/11 first cut).
//!
//! Argon2id password hashes; 15-minute HS256 access tokens signed with a
//! per-hub secret in `data_dir/jwt.secret`; rotating refresh tokens stored
//! hashed with server-side revocation.
//!
//! ## Refresh families
//!
//! Every setup or login creates one independent refresh family. Its bearer
//! token is `v1.<family selector>.<secret>`: the 128-bit random selector finds
//! the family, while only the SHA-256 hash of the complete token is stored.
//! The 256-bit secret never enters the database or logs. A family has exactly
//! one current hash, so storage is one row per login rather than one row per
//! rotation.
//!
//! Rotation takes a SQLite immediate transaction and conditionally replaces
//! that current hash only while the family is active, unexpired and still
//! names the presented hash. One concurrent request therefore wins. A later
//! presentation of any consumed token still carries the family selector but
//! has the wrong hash; that is replay and revokes the whole family. Separate
//! logins remain separate families. Logout revokes one, password reset revokes
//! all of a user's families, and deleting the user cascades to them.
//!
//! ## Access invalidation generation
//!
//! `users.auth_version` is the durable generation of every access credential
//! for an account. Access tokens carry the generation at issue time; every
//! authenticated HTTP request loads the user by primary key and accepts the
//! token only when its generation still matches. Role changes and password
//! resets increment it in the same committed write as the change, and deletion
//! removes the row, so all three invalidate access immediately across hub
//! restart and across processes. The database row is also authoritative for
//! mutable username and administrator state; those token claims are never
//! trusted after signature and expiry validation.
//!
//! The rolling expiry is extended on successful rotation, preserving the
//! existing 30-day idle lifetime. Revoked families stop rotating, so retaining
//! them until that expiry identifies repeated replay without unbounded growth.
//!
//! ponytail: login throttling (OPS-2) lands with the hardening pass.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

const ACCESS_TTL_SECS: i64 = 15 * 60;
const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// User id.
    pub sub: String,
    pub username: String,
    pub admin: bool,
    /// Durable access generation; must equal `users.auth_version`.
    pub auth_version: i64,
    pub exp: i64,
}

/// What `set_admin` did, or why it declined to.
#[derive(Debug, PartialEq, Eq)]
pub enum SetAdmin {
    Changed,
    /// Already the requested state; nothing to do and nothing wrong.
    Unchanged,
    NoSuchUser,
    /// Demoting this account would leave the hub with no admin at all.
    LastAdmin,
}

/// What `delete_user` did, or why it declined to.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteUser {
    Deleted(String),
    NoSuchUser,
    /// Deleting this account would leave the hub with no admin at all.
    LastAdmin,
}

#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("invalid refresh token")]
    Invalid,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub struct Auth {
    db: SqlitePool,
    enc: EncodingKey,
    dec: DecodingKey,
    /// `Some(token)` while in setup mode (no users yet, OPS-1).
    setup_token: Mutex<Option<String>>,
    /// OPS-2 login throttle (in-memory; resets on restart).
    pub throttle: Throttle,
}

/// OPS-2: consecutive-failure lockout with exponential backoff, keyed
/// per account and per source address ("u:{name}" / "ip:{addr}"). After
/// `threshold` consecutive failures: 30 s, doubling per further failure,
/// capped at 15 min. Success clears the key. In-memory by design — a
/// restart forgiving lockouts is acceptable; sustained attacks re-lock
/// within one threshold's worth of attempts.
#[derive(Default)]
pub struct Throttle {
    entries: Mutex<std::collections::HashMap<String, ThrottleEntry>>,
}

struct ThrottleEntry {
    fails: u32,
    locked_until: Option<std::time::Instant>,
    last: std::time::Instant,
}

const LOCK_BASE_SECS: u64 = 30;
const LOCK_CAP_SECS: u64 = 900;

impl Throttle {
    /// Remaining lockout for this key, if any.
    pub fn locked(&self, key: &str) -> Option<std::time::Duration> {
        let entries = self.entries.lock().unwrap();
        entries
            .get(key)?
            .locked_until?
            .checked_duration_since(std::time::Instant::now())
    }

    /// Record a failure; returns the lockout now in force, if any.
    pub fn fail(&self, key: &str, threshold: u32) -> Option<std::time::Duration> {
        let mut entries = self.entries.lock().unwrap();
        // Bounded memory under username/address scanning: when large,
        // drop entries idle for an hour (their backoff has long lapsed).
        if entries.len() > 4096 {
            let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(3600);
            entries.retain(|_, e| e.last > cutoff);
        }
        let e = entries.entry(key.to_string()).or_insert(ThrottleEntry {
            fails: 0,
            locked_until: None,
            last: std::time::Instant::now(),
        });
        e.fails += 1;
        e.last = std::time::Instant::now();
        if e.fails >= threshold {
            let lock = std::time::Duration::from_secs(
                (LOCK_BASE_SECS << (e.fails - threshold).min(16)).min(LOCK_CAP_SECS),
            );
            e.locked_until = Some(std::time::Instant::now() + lock);
            Some(lock)
        } else {
            None
        }
    }

    pub fn clear(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_token(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn make_refresh_token(family_id: &str) -> (String, String) {
    let token = format!("v1.{family_id}.{}", random_token(32));
    let hash = hash_token(&token);
    (token, hash)
}

fn parse_refresh_token(token: &str) -> Option<(&str, String)> {
    let mut parts = token.split('.');
    let (Some("v1"), Some(family_id), Some(secret), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let is_lower_hex = |s: &str, len: usize| {
        s.len() == len
            && s.as_bytes()
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    };
    if !is_lower_hex(family_id, 32) || !is_lower_hex(secret, 64) {
        return None;
    }
    Some((family_id, hash_token(token)))
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

impl Auth {
    /// Load or create the JWT secret; enter setup mode if no users exist.
    pub async fn new(db: SqlitePool, data_dir: &Path) -> Result<Self> {
        let secret_path = data_dir.join("jwt.secret");
        let secret = if secret_path.exists() {
            std::fs::read(&secret_path)?
        } else {
            let mut s = vec![0u8; 32];
            OsRng.fill_bytes(&mut s);
            std::fs::write(&secret_path, &s)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
            }
            s
        };

        // An expired family cannot authenticate or be made active by replay.
        // Active families have a rolling expiry; revoked ones do not, so this
        // also bounds the tombstones retained to identify replay.
        let pruned = sqlx::query("DELETE FROM refresh_families WHERE expires_at < unixepoch()")
            .execute(&db)
            .await?
            .rows_affected();
        if pruned > 0 {
            tracing::info!(pruned, "expired refresh families removed");
        }

        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&db)
            .await?;
        let setup_token = if users == 0 {
            let raw = random_token(4).to_uppercase();
            let token = format!("{}-{}", &raw[..4], &raw[4..]);
            // OPS-1: printed to the console; gates the one-time setup flow.
            println!(
                "\n  Setup token: {token}\n  Open the web UI (or POST /api/v1/setup) to create the admin account.\n"
            );
            Some(token)
        } else {
            None
        };

        Ok(Self {
            db,
            enc: EncodingKey::from_secret(&secret),
            dec: DecodingKey::from_secret(&secret),
            setup_token: Mutex::new(setup_token),
            throttle: Throttle::default(),
        })
    }

    pub fn setup_required(&self) -> bool {
        self.setup_token.lock().unwrap().is_some()
    }

    /// The one-time setup token, while in setup mode (console/CLI display).
    pub fn setup_token(&self) -> Option<String> {
        self.setup_token.lock().unwrap().clone()
    }

    /// One-time initial admin creation, gated by the printed token (OPS-1).
    pub async fn complete_setup(
        &self,
        token: &str,
        username: &str,
        password: &str,
    ) -> Result<TokenPair> {
        {
            let guard = self.setup_token.lock().unwrap();
            let Some(expected) = guard.as_ref() else {
                bail!("setup already completed");
            };
            if hash_token(token.trim()) != hash_token(expected) {
                bail!("wrong setup token");
            }
        }
        if username.trim().is_empty() || password.len() < 8 {
            bail!("username required and password must be at least 8 characters");
        }
        let id = ulid::Ulid::generate().to_string();
        let hash = hash_password(password)?;
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_admin) VALUES (?, ?, ?, 1)",
        )
        .bind(&id)
        .bind(username.trim())
        .bind(&hash)
        .execute(&self.db)
        .await?;
        *self.setup_token.lock().unwrap() = None;
        tracing::info!(username, "initial admin created; setup complete");
        self.issue_tokens(&id, username.trim(), true, 1).await
    }

    /// Promote or demote an account.
    ///
    /// Refuses to demote the last remaining admin, for the same reason
    /// `delete_user` refuses to delete one — and the consequence is worse
    /// than "inconvenient". `setup_required` is decided once at `Auth::new`
    /// from `users == 0`, NOT from `admins == 0`, so a hub with no admin and
    /// any ordinary account left does not fall back into setup mode on a
    /// restart either. It is unadministrable until somebody edits SQLite by
    /// hand.
    ///
    /// The count and the write are ONE statement on purpose. Read-then-write
    /// let two admins demoting each other concurrently both see two admins
    /// and both proceed, reaching zero — reproduced in
    /// `library_grants::concurrent_mutual_demotion_keeps_an_admin`, which
    /// found it within twenty attempts. Neither call is a self-demotion, so
    /// the route's self-guard never fires; only atomicity helps. SQLite runs
    /// this under its write lock, so the subquery cannot go stale between
    /// being read and being acted on. Same shape as the `BEGIN IMMEDIATE`
    /// AUTH-4 used for refresh rotation, which is the same class of bug.
    ///
    /// This one does NOT go away when AUTH-1/2/3 land: those invalidate stale
    /// tokens, and this is a database race with no token in it.
    ///
    /// The outcome is a value, not a message: the caller has to answer 409
    /// for a refusal and 404 for a missing account, and neither is something
    /// to recover by reading prose off an error.
    pub async fn set_admin(&self, id: &str, admin: bool) -> Result<SetAdmin> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let Some(was): Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(SetAdmin::NoSuchUser);
        };
        // Idempotent on purpose: a panel that shows the state and sets it
        // with the same control will re-send what is already true. Answering
        // before the write also keeps a no-op demotion from being refused as
        // the last admin, which it is not — nothing would have changed.
        if was == admin {
            tx.rollback().await?;
            return Ok(SetAdmin::Unchanged);
        }
        let changed = sqlx::query(
            "UPDATE users
                SET is_admin = ?1, auth_version = auth_version + 1
              WHERE id = ?2
                AND (?1 OR (SELECT COUNT(*) FROM users WHERE is_admin = 1) > 1)",
        )
        .bind(admin)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 1 {
            tx.commit().await?;
            return Ok(SetAdmin::Changed);
        }
        // The immediate transaction keeps the row present and unchanged from
        // the read above through this outcome. A promotion cannot be refused;
        // therefore only a last-admin demotion reaches here.
        Ok(SetAdmin::LastAdmin)
    }

    /// Remove a user and everything that is theirs alone.
    ///
    /// Refuses the last remaining admin. `setup_required` is decided once at
    /// `Auth::new` from whether any users exist, so a hub with ordinary users
    /// and no administrator remains unadministrable across restart.
    ///
    /// The guard and deletion run under one immediate write transaction. A
    /// concurrent demotion or deletion therefore observes this operation
    /// either wholly before or wholly after its own last-admin check; neither
    /// can pass a stale count and take the total to zero.
    ///
    /// Not removed: subtitles this user downloaded. Those belong to the item
    /// and stay available to everyone, which is the same call `created_by`
    /// records rather than owns.
    pub async fn delete_user(&self, id: &str) -> Result<DeleteUser> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let Some((username, _is_admin)): Option<(String, bool)> =
            sqlx::query_as("SELECT username, is_admin FROM users WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            tx.rollback().await?;
            return Ok(DeleteUser::NoSuchUser);
        };

        // watch_state_archive has no foreign key, so these rows would outlive
        // the user. This write is rolled back too if the guarded delete below
        // refuses the last administrator.
        sqlx::query("DELETE FROM watch_state_archive WHERE user_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // The count and delete are one statement under SQLite's write lock.
        // Other user-owned rows and refresh families cascade from users.
        let changed = sqlx::query(
            "DELETE FROM users
              WHERE id = ?1
                AND (is_admin = 0 OR (SELECT COUNT(*) FROM users WHERE is_admin = 1) > 1)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 0 {
            tx.rollback().await?;
            return Ok(DeleteUser::LastAdmin);
        }
        tx.commit().await?;

        tracing::info!(username, %id, "user deleted");
        Ok(DeleteUser::Deleted(username))
    }

    /// Admin user creation (HUB-26 first cut).
    pub async fn create_user(&self, username: &str, password: &str, admin: bool) -> Result<String> {
        if username.trim().is_empty() || password.len() < 8 {
            bail!("username required and password must be at least 8 characters");
        }
        let id = ulid::Ulid::generate().to_string();
        let hash = hash_password(password)?;
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_admin) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(username.trim())
        .bind(&hash)
        .bind(admin)
        .execute(&self.db)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db) if db.is_unique_violation() => {
                anyhow::anyhow!("username already exists")
            }
            e => e.into(),
        })?;
        tracing::info!(username, admin, "user created");
        Ok(id)
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<TokenPair> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, is_admin, auth_version FROM users WHERE username = ?",
        )
        .bind(username.trim())
        .fetch_optional(&self.db)
        .await?;
        // Verify against a dummy hash when the user is unknown so timing
        // doesn't reveal account existence.
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let dummy = DUMMY.get_or_init(|| hash_password("dummy-password").unwrap());
        let Some(row) = row else {
            let _ = verify_password(password, dummy);
            bail!("invalid credentials");
        };
        let hash: String = row.get("password_hash");
        if !verify_password(password, &hash) {
            bail!("invalid credentials");
        }
        self.issue_tokens(
            &row.get::<String, _>("id"),
            &row.get::<String, _>("username"),
            row.get::<i64, _>("is_admin") != 0,
            row.get("auth_version"),
        )
        .await
    }

    /// Rotate exactly once. Replaying a consumed token revokes its family.
    pub async fn refresh(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<TokenPair, RefreshError> {
        let Some((family_id, presented_hash)) = parse_refresh_token(refresh_token) else {
            return Err(RefreshError::Invalid);
        };
        let now = now_unix();
        let (next_token, next_hash) = make_refresh_token(family_id);
        let mut tx = self
            .db
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("beginning refresh rotation")?;
        let changed = sqlx::query(
            "UPDATE refresh_families
                SET current_token_hash = ?, expires_at = ?, rotated_at = ?
              WHERE id = ? AND current_token_hash = ?
                AND revoked_at IS NULL AND expires_at >= ?",
        )
        .bind(&next_hash)
        .bind(now + REFRESH_TTL_SECS)
        .bind(now)
        .bind(family_id)
        .bind(&presented_hash)
        .bind(now)
        .execute(&mut *tx)
        .await
        .context("rotating refresh family")?
        .rows_affected();

        if changed == 0 {
            let family = sqlx::query(
                "SELECT current_token_hash, expires_at, revoked_at
                   FROM refresh_families WHERE id = ?",
            )
            .bind(family_id)
            .fetch_optional(&mut *tx)
            .await
            .context("checking rejected refresh family")?;
            if let Some(family) = family
                && family.get::<Option<i64>, _>("revoked_at").is_none()
                && family.get::<i64, _>("expires_at") >= now
                && family.get::<String, _>("current_token_hash") != presented_hash
            {
                sqlx::query(
                    "UPDATE refresh_families SET revoked_at = ?
                      WHERE id = ? AND revoked_at IS NULL",
                )
                .bind(now)
                .bind(family_id)
                .execute(&mut *tx)
                .await
                .context("revoking replayed refresh family")?;
                tx.commit().await.context("committing replay revocation")?;
            }
            return Err(RefreshError::Invalid);
        }

        let user = sqlx::query(
            "SELECT u.id, u.username, u.is_admin, u.auth_version
               FROM users u JOIN refresh_families rf ON rf.user_id = u.id
              WHERE rf.id = ?",
        )
        .bind(family_id)
        .fetch_one(&mut *tx)
        .await
        .context("loading refresh-family user")?;
        let access_token = self.issue_access_token(
            &user.get::<String, _>("id"),
            &user.get::<String, _>("username"),
            user.get::<i64, _>("is_admin") != 0,
            user.get("auth_version"),
        )?;
        tx.commit().await.context("committing refresh rotation")?;
        Ok(TokenPair {
            access_token,
            refresh_token: next_token,
            expires_in: ACCESS_TTL_SECS,
        })
    }

    async fn issue_tokens(
        &self,
        user_id: &str,
        username: &str,
        admin: bool,
        auth_version: i64,
    ) -> Result<TokenPair> {
        let access_token = self.issue_access_token(user_id, username, admin, auth_version)?;
        let family_id = random_token(16);
        let (refresh_token, token_hash) = make_refresh_token(&family_id);
        sqlx::query(
            "INSERT INTO refresh_families
                (id, user_id, current_token_hash, expires_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&family_id)
        .bind(user_id)
        .bind(token_hash)
        .bind(now_unix() + REFRESH_TTL_SECS)
        .execute(&self.db)
        .await?;
        Ok(TokenPair {
            access_token,
            refresh_token,
            expires_in: ACCESS_TTL_SECS,
        })
    }

    fn issue_access_token(
        &self,
        user_id: &str,
        username: &str,
        admin: bool,
        auth_version: i64,
    ) -> Result<String> {
        let claims = Claims {
            sub: user_id.to_string(),
            username: username.to_string(),
            admin,
            auth_version,
            exp: now_unix() + ACCESS_TTL_SECS,
        };
        Ok(jsonwebtoken::encode(
            &Header::default(),
            &claims,
            &self.enc,
        )?)
    }

    /// Revoke the named current family if it belongs to `user_id`.
    ///
    /// Deliberately idempotent and non-oracular: malformed, unknown, stale,
    /// foreign and already-revoked tokens all do nothing successfully.
    pub async fn logout(&self, user_id: &str, token: &str) -> Result<()> {
        let Some((family_id, token_hash)) = parse_refresh_token(token) else {
            return Ok(());
        };
        sqlx::query(
            "UPDATE refresh_families SET revoked_at = unixepoch()
              WHERE id = ? AND user_id = ? AND current_token_hash = ?
                AND revoked_at IS NULL",
        )
        .bind(family_id)
        .bind(user_id)
        .bind(token_hash)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Verify the token and resolve its mutable account state from storage.
    ///
    /// Signature and expiry establish who minted the token and its bounded
    /// lifetime. The indexed user lookup establishes that the account still
    /// exists, that its generation is current, and what username/admin rights
    /// it has now. No process-local cache participates in invalidation.
    pub async fn authenticate(&self, bearer: &str) -> Result<Claims> {
        let token =
            jsonwebtoken::decode::<Claims>(bearer, &self.dec, &Validation::default())?.claims;
        let row = sqlx::query("SELECT username, is_admin, auth_version FROM users WHERE id = ?")
            .bind(&token.sub)
            .fetch_optional(&self.db)
            .await?
            .context("account no longer exists")?;
        let current_version: i64 = row.get("auth_version");
        anyhow::ensure!(
            token.auth_version == current_version,
            "access token has been invalidated"
        );
        Ok(Claims {
            sub: token.sub,
            username: row.get("username"),
            admin: row.get::<i64, _>("is_admin") != 0,
            auth_version: current_version,
            exp: token.exp,
        })
    }
}

/// CLI escape hatch (OPS-1): overwrite a user's password hash and revoke
/// every refresh family in the same committed transaction.
pub async fn reset_password(db: &SqlitePool, username: &str, new_password: &str) -> Result<()> {
    let hash = hash_password(new_password)?;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let res = sqlx::query(
        "UPDATE users
            SET password_hash = ?, auth_version = auth_version + 1
          WHERE username = ?",
    )
    .bind(&hash)
    .bind(username)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        bail!("no such user: {username}");
    }
    sqlx::query(
        "UPDATE refresh_families SET revoked_at = unixepoch()
         WHERE user_id = (SELECT id FROM users WHERE username = ?)",
    )
    .bind(username)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
