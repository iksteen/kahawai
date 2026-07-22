//! Users, first-run setup, and token auth (OPS-1, HUB-10/11 first cut).
//!
//! Argon2id password hashes; 15-minute HS256 access tokens signed with a
//! per-hub secret in `data_dir/jwt.secret`; rotating refresh tokens stored
//! hashed with server-side revocation.
//!
//! ponytail: login throttling (OPS-2) lands with the hardening pass.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
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
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
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

        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&db).await?;
        let setup_token = if users == 0 {
            let raw = random_token(4).to_uppercase();
            let token = format!("{}-{}", &raw[..4], &raw[4..]);
            // OPS-1: printed to the console; gates the one-time setup flow.
            println!("\n  Setup token: {token}\n  Open the web UI (or POST /api/v1/setup) to create the admin account.\n");
            Some(token)
        } else {
            None
        };

        Ok(Self {
            db,
            enc: EncodingKey::from_secret(&secret),
            dec: DecodingKey::from_secret(&secret),
            setup_token: Mutex::new(setup_token),
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
        let id = ulid::Ulid::new().to_string();
        let hash = hash_password(password)?;
        sqlx::query("INSERT INTO users (id, username, password_hash, is_admin) VALUES (?, ?, ?, 1)")
            .bind(&id)
            .bind(username.trim())
            .bind(&hash)
            .execute(&self.db)
            .await?;
        *self.setup_token.lock().unwrap() = None;
        tracing::info!(username, "initial admin created; setup complete");
        self.issue_tokens(&id, username.trim(), true).await
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<TokenPair> {
        let row = sqlx::query("SELECT id, username, password_hash, is_admin FROM users WHERE username = ?")
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
        self.issue_tokens(&row.get::<String, _>("id"), &row.get::<String, _>("username"), row.get::<i64, _>("is_admin") != 0)
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
        sqlx::query("INSERT INTO refresh_tokens (token_hash, user_id, expires_at) VALUES (?, ?, ?)")
            .bind(hash_token(&refresh_token))
            .bind(user_id)
            .bind(now_unix() + REFRESH_TTL_SECS)
            .execute(&self.db)
            .await?;
        Ok(TokenPair { access_token, refresh_token, expires_in: ACCESS_TTL_SECS })
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
