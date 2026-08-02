//! Users, first-run setup, and token auth (OPS-1, HUB-10/11 first cut).
//!
//! Argon2id password hashes; 15-minute HS256 access tokens signed with a
//! per-hub secret in `data_dir/jwt.secret`; rotating refresh tokens stored
//! hashed with server-side revocation.
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
    pub exp: i64,
}

#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

pub struct Auth {
    db: SqlitePool,
    enc: EncodingKey,
    dec: DecodingKey,
    /// `Some(token)` while in setup mode (no users yet, OPS-1).
    setup_token: Mutex<Option<String>>,
    /// OPS-2 login throttle (in-memory; resets on restart).
    pub throttle: Throttle,
    /// Users deleted while their access token was still valid, and when
    /// that token can last have been issued.
    ///
    /// Access tokens are stateless JWTs and `require_auth` never reads
    /// the database, so without this a deleted account keeps working
    /// until its token expires. Entries are swept once past the TTL, so
    /// the map is bounded by deletions-per-15-minutes. Per process by
    /// design: a restart clears it, and any token it would have refused
    /// has expired by then.
    deleted: Mutex<std::collections::HashMap<String, std::time::Instant>>,
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

        // Dead tokens have no audit value beyond their revoked flag's
        // lifetime: an expired row can never authenticate again, so it
        // is pure growth (1,569 rows for 2 users when this landed,
        // mostly scripted test-account logins). Pruned at open.
        let pruned = sqlx::query("DELETE FROM refresh_tokens WHERE expires_at < unixepoch()")
            .execute(&db)
            .await?
            .rows_affected();
        if pruned > 0 {
            tracing::info!(pruned, "expired refresh tokens removed");
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
            deleted: Mutex::new(Default::default()),
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
        self.issue_tokens(&id, username.trim(), true).await
    }

    /// True while `user_id` is a deleted account whose access token
    /// could still verify. Sweeps expired entries as it goes.
    pub fn is_deleted(&self, user_id: &str) -> bool {
        let mut map = self.deleted.lock().unwrap();
        let ttl = std::time::Duration::from_secs(ACCESS_TTL_SECS as u64);
        map.retain(|_, at| at.elapsed() < ttl);
        map.contains_key(user_id)
    }

    /// Remove a user and everything that is theirs alone.
    ///
    /// Refuses the last remaining admin: `setup_required` is decided
    /// once, at `Auth::new`, so an emptied hub does not fall back into
    /// setup mode until it restarts — deleting the last admin would
    /// lock the operator out of their own hub with no way back in.
    ///
    /// Not removed: subtitles this user downloaded. Those belong to the
    /// item and stay available to everyone, which is the same call
    /// `created_by` records rather than owns.
    pub async fn delete_user(&self, id: &str) -> Result<String> {
        let (username, is_admin): (String, bool) =
            sqlx::query_as("SELECT username, is_admin FROM users WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.db)
                .await?
                .with_context(|| format!("no such user: {id}"))?;
        if is_admin {
            let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
                .fetch_one(&self.db)
                .await?;
            anyhow::ensure!(admins > 1, "refusing to delete the last admin");
        }

        let mut tx = self.db.begin().await?;
        // Order matters. refresh_tokens references users with no ON
        // DELETE CASCADE and foreign keys are on, so it goes first or
        // the user row cannot be removed at all.
        sqlx::query("DELETE FROM refresh_tokens WHERE user_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // watch_state_archive has no foreign key, so these rows would
        // outlive the user — and the HUB-20 restore path copies them
        // back into watch_state, whose key WOULD then fail and roll
        // back a whole scan. Purged rather than kept: the archive is
        // keyed by a ULID that no recreated account will ever have.
        sqlx::query("DELETE FROM watch_state_archive WHERE user_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // watch_state and user_prefs cascade from here.
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.deleted
            .lock()
            .unwrap()
            .insert(id.to_string(), std::time::Instant::now());
        tracing::info!(username, %id, "user deleted");
        Ok(username)
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
            "SELECT id, username, password_hash, is_admin FROM users WHERE username = ?",
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
        )
        .await
    }

    /// Rotate a refresh token: single use, server-side revocation.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenPair> {
        let hash = hash_token(refresh_token);
        let row = sqlx::query(
            "SELECT rt.user_id, rt.expires_at, rt.revoked, u.username, u.is_admin
             FROM refresh_tokens rt JOIN users u ON u.id = rt.user_id
             WHERE rt.token_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&self.db)
        .await?
        .context("unknown refresh token")?;
        if row.get::<i64, _>("revoked") != 0 || row.get::<i64, _>("expires_at") < now_unix() {
            bail!("refresh token expired or revoked");
        }
        sqlx::query("UPDATE refresh_tokens SET revoked = 1 WHERE token_hash = ?")
            .bind(&hash)
            .execute(&self.db)
            .await?;
        self.issue_tokens(
            &row.get::<String, _>("user_id"),
            &row.get::<String, _>("username"),
            row.get::<i64, _>("is_admin") != 0,
        )
        .await
    }

    async fn issue_tokens(&self, user_id: &str, username: &str, admin: bool) -> Result<TokenPair> {
        let claims = Claims {
            sub: user_id.to_string(),
            username: username.to_string(),
            admin,
            exp: now_unix() + ACCESS_TTL_SECS,
        };
        let access_token = jsonwebtoken::encode(&Header::default(), &claims, &self.enc)?;
        let refresh_token = random_token(32);
        sqlx::query(
            "INSERT INTO refresh_tokens (token_hash, user_id, expires_at) VALUES (?, ?, ?)",
        )
        .bind(hash_token(&refresh_token))
        .bind(user_id)
        .bind(now_unix() + REFRESH_TTL_SECS)
        .execute(&self.db)
        .await?;
        Ok(TokenPair {
            access_token,
            refresh_token,
            expires_in: ACCESS_TTL_SECS,
        })
    }

    pub fn verify(&self, bearer: &str) -> Result<Claims> {
        let data = jsonwebtoken::decode::<Claims>(bearer, &self.dec, &Validation::default())?;
        Ok(data.claims)
    }
}

/// CLI escape hatch (OPS-1): overwrite a user's password hash and revoke
/// their refresh tokens.
pub async fn reset_password(db: &SqlitePool, username: &str, new_password: &str) -> Result<()> {
    let hash = hash_password(new_password)?;
    let res = sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
        .bind(&hash)
        .bind(username)
        .execute(db)
        .await?;
    if res.rows_affected() == 0 {
        bail!("no such user: {username}");
    }
    sqlx::query(
        "UPDATE refresh_tokens SET revoked = 1
         WHERE user_id = (SELECT id FROM users WHERE username = ?)",
    )
    .bind(username)
    .execute(db)
    .await?;
    Ok(())
}
