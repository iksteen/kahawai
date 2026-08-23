//! The key credentials are sealed under, and the cipher that uses it.
//!
//! `<data_dir>/credentials.secret` is 32 bytes, generated on first start and
//! kept beside `jwt.secret`. It is created with `create_new` and mode 0600 in
//! one call: `open(2)` applies `mode & ~umask`, so umask can only make it
//! stricter, and the file never exists at another mode. `create_new` also
//! makes it exclusive, so two hubs starting on one data directory cannot both
//! write one: replacing a key orphans everything sealed under the first.
//!
//! A key that cannot be used stops the hub, and so does one that has gone
//! missing. The two are told apart by a `settings` row written once the key is
//! proven usable: absent key with no row is a first start, absent key with the
//! row is one somebody deleted. Inferring that from runtime state — an empty
//! table, a fresh-looking data directory — is wrong in exactly the case that
//! matters, and losing the key loses every credential under it.
//!
//! AES-256-GCM from `ring`, already linked by rustls. Each write draws a fresh
//! 96-bit nonce, because GCM forfeits its authentication key if one repeats.
//! The additional data is the row's own identity, so a ciphertext moved to
//! another user or field fails to open rather than decrypting into the wrong
//! account.

use anyhow::{Context, Result, anyhow, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::path::Path;

pub const KEY_FILE: &str = "credentials.secret";
/// Written once the key is usable. In the database, because a marker beside
/// the key would go with the same `rm`.
///
/// Named for the key and not for the file that holds it, so renaming the file
/// leaves it alone: this string is a row in every database that has ever
/// started a hub, and changing it strands the marker it was written under.
pub const SEEDED_SETTING: &str = "credentials.key_seeded";
const KEY_LEN: usize = 32;

pub struct Secrets {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Secrets {
    /// Load the key, generating it the first time. Unreadable, wrong length
    /// or missing-after-seeding are all fatal.
    pub async fn load_or_create(data_dir: &Path, db: &SqlitePool) -> Result<Self> {
        let path = data_dir.join(KEY_FILE);
        let seeded: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(SEEDED_SETTING)
            .fetch_optional(db)
            .await
            .context("reading the credential key marker")?;
        let seeded = seeded.as_deref();
        let rng = SystemRandom::new();
        let mut bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && seeded.is_none() => {
                generate(data_dir, &path, &rng)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "{} is gone, and this hub seeded one — every stored credential is sealed \
                 under it. Restore the file; generating another would silently make them \
                 all unreadable.",
                path.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        narrow(&path)?;
        if bytes.len() != KEY_LEN {
            if seeded.is_some() {
                bail!(
                    "{} is {} bytes, expected {KEY_LEN}. Restore it from a backup.",
                    path.display(),
                    bytes.len()
                );
            }
            // Nothing has been sealed under it, so it is a first start
            // interrupted mid-write and the bytes mean nothing to anyone.
            tracing::warn!(
                path = %path.display(),
                bytes = bytes.len(),
                "replacing a short credential key from an interrupted first start"
            );
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            bytes = generate(data_dir, &path, &rng)?;
        }
        let key = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| anyhow!("rejected the credential key"))?;
        // Which key, not just that there was one. A database paired with a
        // foreign key — a snapshot restored without its own, so the standing
        // one stays — has a key and a marker and would otherwise start, then
        // fail to open every credential, one call at a time, for ever.
        let fingerprint = fingerprint(&bytes);
        if let Some(recorded) = seeded
            && recorded != fingerprint
        {
            bail!(
                "{} is not the key this database was sealed with. Restore the \
                 matching key; the credentials cannot be read without it.",
                path.display()
            );
        }
        // Only once it is proven usable: a marker without a key is the state
        // that refuses to start.
        if seeded.is_none() {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind(SEEDED_SETTING)
            .bind(&fingerprint)
            .execute(db)
            .await
            .context("recording the credential key marker")?;
        }
        Ok(Self {
            key: LessSafeKey::new(key),
            rng,
        })
    }

    /// `nonce || ciphertext || tag`.
    pub fn seal(
        &self,
        owner_id: &str,
        provider: &str,
        field: &str,
        plaintext: &str,
    ) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| anyhow!("no system randomness for a credential nonce"))?;
        let mut sealed = plaintext.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad(owner_id, provider, field)),
                &mut sealed,
            )
            .map_err(|_| anyhow!("sealing a credential failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.append(&mut sealed);
        Ok(out)
    }

    /// Fails if the blob was truncated, tampered with, moved to another row,
    /// or sealed under a different key.
    pub fn open(&self, owner_id: &str, provider: &str, field: &str, blob: &[u8]) -> Result<String> {
        if blob.len() < NONCE_LEN {
            bail!("sealed credential is {} bytes, too short", blob.len());
        }
        let (nonce, rest) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce)
            .map_err(|_| anyhow!("sealed credential has no nonce"))?;
        let mut buf = rest.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(aad(owner_id, provider, field)), &mut buf)
            .map_err(|_| anyhow!("sealed credential did not authenticate"))?;
        String::from_utf8(plain.to_vec()).context("sealed credential is not text")
    }
}

/// The credential store: the key, and the rows it seals.
///
/// `owner_id` is a `users.id`, or [`HUB`] for one the hub holds itself. A row
/// that will not open is an error, not an absence — with a missing key already
/// fatal, the only ways left are tampering or corruption, and answering
/// "nothing is configured" for either would hide it.
pub struct Credentials {
    db: SqlitePool,
    secrets: Secrets,
}

/// The owner of a credential the hub holds for everyone, not for one viewer.
pub const HUB: &str = "";

impl Credentials {
    pub async fn open(data_dir: &Path, db: SqlitePool) -> Result<Self> {
        let secrets = Secrets::load_or_create(data_dir, &db).await?;
        Ok(Self { db, secrets })
    }

    /// Every field this owner holds for this provider. A provider nobody has
    /// configured is an empty map, not an error.
    pub async fn get_provider(
        &self,
        owner_id: &str,
        provider: &str,
    ) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT field, secret FROM credentials WHERE owner_id = ? AND provider = ?",
        )
        .bind(owner_id)
        .bind(provider)
        .fetch_all(&self.db)
        .await?;
        rows.into_iter()
            .map(|(field, blob)| {
                let value = self
                    .secrets
                    .open(owner_id, provider, &field, &blob)
                    .with_context(|| format!("stored {provider} {field}"))?;
                Ok((field, value))
            })
            .collect()
    }

    /// Replace everything this owner holds for this provider, in one
    /// transaction — so no caller can leave a username beside the wrong
    /// password, which is a pair that reports itself configured and cannot
    /// sign in. An empty map is therefore a detach.
    ///
    /// There is deliberately no merge. A provider's credentials are atomic:
    /// callers send the whole set or detach it. A form that cannot show what
    /// it is about to replace — because it will not echo a stored secret — is
    /// a UI problem, and solving it here by keeping fields the caller did not
    /// send is how a pair stops agreeing with itself.
    ///
    /// The `DELETE` is the first statement on purpose: it takes the write
    /// lock immediately. A `SELECT` added ahead of it would make this a
    /// read-then-upgrade, which fails `SQLITE_BUSY` in WAL whatever the busy
    /// timeout says.
    ///
    pub async fn set_provider(
        &self,
        owner_id: &str,
        provider: &str,
        fields: &BTreeMap<&str, &str>,
    ) -> Result<()> {
        // Sealed before the transaction opens: this is the only fallible part
        // and it holds no lock.
        let sealed: Vec<(&str, Vec<u8>)> = fields
            .iter()
            .map(|(field, value)| {
                Ok((*field, self.secrets.seal(owner_id, provider, field, value)?))
            })
            .collect::<Result<_>>()?;

        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM credentials WHERE owner_id = ? AND provider = ?")
            .bind(owner_id)
            .bind(provider)
            .execute(&mut *tx)
            .await?;
        for (field, blob) in sealed {
            sqlx::query(
                "INSERT INTO credentials (owner_id, provider, field, secret) VALUES (?, ?, ?, ?)",
            )
            .bind(owner_id)
            .bind(provider)
            .bind(field)
            .bind(blob)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Move a provider's plaintext `settings` rows into the store, once.
///
/// Returns whether anything moved. The rows are sealed as a set and then
/// deleted, so a failure leaves them where they were and the next start tries
/// again. A provider the store already holds is replaced: reaching that needs
/// a binary older than the adoption to have written the plaintext back, and
/// then its value is the one to keep.
pub async fn adopt_settings(
    credentials: &Credentials,
    provider: &str,
    from: &[(&str, &str)],
) -> Result<bool> {
    let db = &credentials.db;
    let mut found = BTreeMap::new();
    for (setting, field) in from {
        if let Some(value) =
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
                .bind(setting)
                .fetch_optional(db)
                .await?
        {
            found.insert(*field, value);
        }
    }
    if found.is_empty() {
        return Ok(false);
    }

    let borrowed: BTreeMap<&str, &str> = found.iter().map(|(f, v)| (*f, v.as_str())).collect();
    credentials.set_provider(HUB, provider, &borrowed).await?;
    verify(credentials, HUB, provider, &found).await?;
    // One statement, so the plaintext goes all at once. Deleted key by key, a
    // crash between two of them leaves a SUBSET behind, and the next start
    // adopts that subset over the sealed set this one just wrote -- a TVDB
    // whose pin outlived its api_key ends up with the key in neither table.
    let sql = in_clause("DELETE FROM settings WHERE key", from.len());
    let mut delete = sqlx::query(sqlx::AssertSqlSafe(sql));
    for (setting, _) in from {
        delete = delete.bind(*setting);
    }
    delete.execute(db).await?;
    Ok(true)
}

/// Every declared credential, moved out of the clear, and the write-ahead log
/// reset behind it.
///
/// One provider failing is fatal — a credential the hub could not seal is one
/// it will not read and will not report, with its plaintext still on disk. But
/// whatever moved BEFORE it is already deleted, and deleted is not gone until
/// the log has been truncated: the checkpoint therefore runs on the way out of
/// the failure too, and the error is raised after it.
///
/// Each entry is `(provider, &[(plaintext key, field)])`.
pub async fn adopt_all(
    credentials: &Credentials,
    settings: &[(&str, &[(&str, &str)])],
) -> Result<()> {
    let mut retired = false;
    let adopted: Result<()> = async {
        for (provider, from) in settings {
            if adopt_settings(credentials, provider, from).await? {
                tracing::info!(provider, "plaintext credential sealed");
                retired = true;
            }
        }
        Ok(())
    }
    .await;
    if retired {
        crate::db::checkpoint_truncate(&credentials.db)
            .await
            .context("checkpointing after sealing the plaintext credentials")?;
    }
    adopted
}

/// Read the sealed set back and compare, before the plaintext it came from is
/// deleted.
///
/// Sealing reports its own failures, but this is a one-way door: after the
/// delete there is nothing to compare against and nothing to retry from. The
/// error names the provider and never a value.
async fn verify(
    credentials: &Credentials,
    owner_id: &str,
    provider: &str,
    expected: &BTreeMap<&str, String>,
) -> Result<()> {
    let stored = credentials.get_provider(owner_id, provider).await?;
    let same = stored.len() == expected.len()
        && expected
            .iter()
            .all(|(field, value)| stored.get(*field) == Some(value));
    if !same {
        bail!("sealed {provider} did not read back as it was written; plaintext left in place");
    }
    Ok(())
}

/// `<prefix> IN (?, ?, ...)`, because a placeholder list is the only part of a
/// statement that cannot itself be a bind parameter. Nothing but the COUNT
/// reaches the string — every value is still bound — which is what makes the
/// `AssertSqlSafe` at the call sites true rather than a hope.
fn in_clause(prefix: &str, n: usize) -> String {
    let mut sql = String::from(prefix);
    sql.push_str(" IN (");
    for i in 0..n {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    sql.push(')');
    sql
}

/// Everything one owner holds for one provider, in one statement.
///
/// Detaching an account is the only deletion there is, and a provider's fields
/// are not independently meaningful — a username without its password is not
/// half an account, it is a broken one. Returns rows removed, which is fields
/// and not accounts.
///
/// Not on [`Credentials`], and taking an executor rather than a pool: deleting
/// needs no key, and deleting a user has to take their credentials inside the
/// transaction that deletes the user. `Credentials` owns its own connection,
/// which would block on that transaction's write lock rather than join it.
///
/// No empty-owner guard, unlike [`delete_owner`], and not by oversight: an
/// empty owner is [`HUB`], so `delete_provider(db, HUB, TMDB)` is the admin
/// disconnect. One named provider is a normal thing to delete for the hub
/// itself; everything the hub holds is not.
pub async fn delete_provider<'e, E>(db: E, owner_id: &str, provider: &str) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    Ok(
        sqlx::query("DELETE FROM credentials WHERE owner_id = ? AND provider = ?")
            .bind(owner_id)
            .bind(provider)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// Everything one user had. `credentials` has no foreign key to cascade from,
/// so deleting a user calls this, in the same transaction.
pub async fn delete_owner<'e, E>(db: E, owner_id: &str) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    // `HUB` is the empty string. A user id that arrived empty is a caller bug,
    // and reading it as "the hub" would delete every provider key the operator
    // has — with no read path to notice and no plaintext left to recover from.
    if owner_id.is_empty() {
        bail!("refusing to delete credentials for an empty owner");
    }
    Ok(sqlx::query("DELETE FROM credentials WHERE owner_id = ?")
        .bind(owner_id)
        .execute(db)
        .await?
        .rows_affected())
}

/// Names the key without being it: SHA-256 of the key bytes, which reveals
/// nothing usable and is stable across restarts.
pub(crate) fn fingerprint(key: &[u8]) -> String {
    use sha2::Digest;
    data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(key))
}

/// Length-prefixed, not separated: with a separator, `("a\0b", "c")` and
/// `("a", "b\0c")` are two different rows with the same additional data, and a
/// ciphertext moved between them would open. Unreachable while owners are
/// ULIDs and providers are constants — but the API takes `&str`, so the first
/// caller passing something client-supplied would make it reachable.
fn aad(owner_id: &str, provider: &str, field: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in [owner_id, provider, field] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

/// Write a fresh key under its final name.
fn generate(data_dir: &Path, path: &Path, rng: &SystemRandom) -> Result<Vec<u8>> {
    let mut fresh = [0u8; KEY_LEN];
    rng.fill(&mut fresh)
        .map_err(|_| anyhow!("no system randomness for the credential key"))?;

    // `create_new`, so two hubs starting on one data directory cannot both
    // write: the loser reads the winner's key rather than replacing it.
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = kahawai_core::private::create(path)?;
        file.write_all(&fresh)?;
        file.sync_all()
    })();
    match written {
        Ok(()) => {
            if let Ok(dir) = std::fs::File::open(data_dir) {
                let _ = dir.sync_all();
            }
            Ok(fresh.to_vec())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))
        }
        Err(error) => {
            let _ = std::fs::remove_file(path);
            Err(error).with_context(|| format!("creating {}", path.display()))
        }
    }
}

/// The copies this hub did not make: a snapshot out of an object store (which
/// carries no mode at all), a hand copy, a `tar -x` under a lax umask. Warned
/// rather than put right in silence — the chmod is the small half, and the one
/// worth saying is that the key WAS readable to somebody else.
fn narrow(path: &Path) -> Result<()> {
    let found = kahawai_core::private::narrow(path)
        .with_context(|| format!("restricting {}", path.display()))?;
    if let Some(found) = found {
        tracing::warn!(
            path = %path.display(),
            found = format!("{found:04o}"),
            "the credential key was readable beyond its owner; narrowed to 0600"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn keyed() -> (tempfile::TempDir, SqlitePool, Secrets) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        let s = Secrets::load_or_create(dir.path(), &db).await.unwrap();
        (dir, db, s)
    }

    async fn marker(db: &SqlitePool) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(SEEDED_SETTING)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_sealed_credential_opens_again() {
        let (_d, _db, s) = keyed().await;
        let blob = s
            .seal("u1", "opensubtitles", "password", "hunter2")
            .unwrap();
        assert_eq!(
            s.open("u1", "opensubtitles", "password", &blob).unwrap(),
            "hunter2"
        );
    }

    #[tokio::test]
    async fn the_ciphertext_carries_no_plaintext() {
        let (_d, _db, s) = keyed().await;
        let blob = s.seal("", "tmdb", "api_key", "hunter2").unwrap();
        assert!(!blob.windows(7).any(|w| w == b"hunter2"));
        // base64("hunter2") is "aHVudGVyMg==", in case a stub encodes.
        assert!(!String::from_utf8_lossy(&blob).contains("aHVudGVy"));
    }

    #[tokio::test]
    async fn each_seal_draws_a_fresh_nonce() {
        let (_d, _db, s) = keyed().await;
        let a = s.seal("", "tmdb", "api_key", "same").unwrap();
        let b = s.seal("", "tmdb", "api_key", "same").unwrap();
        assert_ne!(a, b, "a repeated GCM nonce forfeits the authentication key");
    }

    #[tokio::test]
    async fn a_row_does_not_open_as_another_row() {
        let (_d, _db, s) = keyed().await;
        let blob = s
            .seal("u1", "opensubtitles", "password", "hunter2")
            .unwrap();
        assert!(s.open("u2", "opensubtitles", "password", &blob).is_err());
        assert!(s.open("u1", "tmdb", "password", &blob).is_err());
        assert!(s.open("u1", "opensubtitles", "username", &blob).is_err());
    }

    #[tokio::test]
    async fn tampering_and_truncation_are_refused() {
        let (_d, _db, s) = keyed().await;
        let blob = s.seal("", "tmdb", "api_key", "hunter2").unwrap();
        let mut flipped = blob.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(s.open("", "tmdb", "api_key", &flipped).is_err());
        for cut in [0, NONCE_LEN - 1, NONCE_LEN, blob.len() - 1] {
            assert!(
                s.open("", "tmdb", "api_key", &blob[..cut]).is_err(),
                "cut {cut}"
            );
        }
    }

    #[tokio::test]
    async fn another_key_cannot_open_it() {
        let (_d, _db, s) = keyed().await;
        let (_d2, _db2, other) = keyed().await;
        let blob = s.seal("", "tmdb", "api_key", "hunter2").unwrap();
        assert!(other.open("", "tmdb", "api_key", &blob).is_err());
    }

    /// The key the hub did not create: `fs::copy` carries the source's mode,
    /// an object store carries none at all, and a `tar -x` carries the
    /// archive's. Every other file this hub owns is narrowed on open; the one
    /// the whole scheme rests on was the exception.
    #[tokio::test]
    async fn a_key_that_arrived_readable_is_narrowed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        Secrets::load_or_create(dir.path(), &db).await.unwrap();
        let path = dir.path().join(KEY_FILE);
        let key = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let secrets = Secrets::load_or_create(dir.path(), &db).await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a world-readable key stayed world-readable"
        );
        // Narrowed, not replaced: the credentials are sealed under this one.
        assert_eq!(std::fs::read(&path).unwrap(), key);
        let blob = secrets.seal(HUB, "tmdb", "api_key", "a-key").unwrap();
        assert_eq!(
            secrets.open(HUB, "tmdb", "api_key", &blob).unwrap(),
            "a-key"
        );
    }

    #[tokio::test]
    async fn the_key_is_generated_once_and_kept_private() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        // A wide umask must not widen it: open(2) can only clear bits.
        let previous = unsafe { libc::umask(0) };
        let first = Secrets::load_or_create(dir.path(), &db).await;
        let mode = std::fs::metadata(dir.path().join(KEY_FILE)).map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o777
        });
        unsafe { libc::umask(previous) };
        first.unwrap();
        assert_eq!(mode.unwrap(), 0o600);
        assert!(marker(&db).await.is_some(), "seeding was not recorded");

        let key = std::fs::read(dir.path().join(KEY_FILE)).unwrap();
        Secrets::load_or_create(dir.path(), &db).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(KEY_FILE)).unwrap(),
            key,
            "regenerating would orphan every stored credential"
        );
    }

    /// The reason the marker exists. Without it this is indistinguishable
    /// from a first start, and a first start mints a new key — silently
    /// making every credential sealed under the old one unreadable.
    /// A database paired with someone else's key. It has a key and a marker,
    /// so without recording WHICH key it would start and then fail to open
    /// every credential, one call at a time.
    #[tokio::test]
    async fn a_key_that_does_not_match_the_database_stops_the_hub() {
        let db = crate::db::open_in_memory().await.unwrap();
        let theirs = tempfile::tempdir().unwrap();
        Secrets::load_or_create(theirs.path(), &db).await.unwrap();

        // Same database, a different data directory with its own key.
        let ours = tempfile::tempdir().unwrap();
        std::fs::write(ours.path().join(KEY_FILE), [7u8; KEY_LEN]).unwrap();
        assert!(Secrets::load_or_create(ours.path(), &db).await.is_err());
    }

    /// Nonces must be drawn, not counted. A counter satisfies "two seals
    /// differ" while repeating every nonce a fresh process draws — and since
    /// a counter that lives in a `static` keeps climbing across instances,
    /// reloading the key in-process cannot tell the two apart either. What
    /// can: a counter leaves its high bytes constant, and random ones do not.
    #[tokio::test]
    async fn nonces_are_drawn_rather_than_counted() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut tails = std::collections::HashSet::new();
        for _ in 0..4 {
            let s = Secrets::load_or_create(dir.path(), &db).await.unwrap();
            for _ in 0..4 {
                let blob = s.seal("", "tmdb", "api_key", "same").unwrap();
                assert!(
                    seen.insert(blob[..NONCE_LEN].to_vec()),
                    "a nonce repeated under one key"
                );
                tails.insert(blob[NONCE_LEN - 4..NONCE_LEN].to_vec());
            }
        }
        // 16 draws sharing four bytes is a 1-in-2^96 accident, or a counter.
        assert!(
            tails.len() > 1,
            "every nonce had the same high bytes; these are counted, not drawn"
        );
    }

    /// The additional data is length-prefixed, so a NUL inside one component
    /// cannot shift the boundary into another and make two rows equivalent.
    #[tokio::test]
    async fn a_nul_cannot_shift_the_boundary_between_fields() {
        let (_d, _db, s) = keyed().await;
        let blob = s.seal("a\0b", "c", "field", "hunter2").unwrap();
        assert!(
            s.open("a", "b\0c", "field", &blob).is_err(),
            "two different rows shared their additional data"
        );
        assert_eq!(s.open("a\0b", "c", "field", &blob).unwrap(), "hunter2");
    }

    #[tokio::test]
    async fn a_key_that_went_missing_stops_the_hub() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        Secrets::load_or_create(dir.path(), &db).await.unwrap();
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();

        assert!(Secrets::load_or_create(dir.path(), &db).await.is_err());
        assert!(
            !dir.path().join(KEY_FILE).exists(),
            "a replacement was minted for a key that only went missing"
        );
    }

    /// The other half: a database restored without its key is the same
    /// state, and must refuse just as loudly.
    #[tokio::test]
    async fn a_database_that_remembers_a_key_it_does_not_have_stops_the_hub() {
        let db = crate::db::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, '1')")
            .bind(SEEDED_SETTING)
            .execute(&db)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(Secrets::load_or_create(dir.path(), &db).await.is_err());
    }

    #[tokio::test]
    async fn a_key_that_cannot_be_used_stops_the_hub() {
        // With something sealed under the real one, a short file is damage:
        // only a backup brings the credentials back.
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        Secrets::load_or_create(dir.path(), &db).await.unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"short").unwrap();

        let Err(e) = Secrets::load_or_create(dir.path(), &db).await else {
            panic!("a 5-byte key was accepted");
        };
        let said = format!("{e:#}");
        assert!(said.contains("Restore it from a backup"), "{said}");
    }

    #[tokio::test]
    async fn generating_the_key_leaves_nothing_beside_it() {
        let (dir, _db, _s) = keyed().await;
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != KEY_FILE)
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }

    /// The cost of writing in place: a crash mid-write leaves a short key.
    /// With nothing sealed under it the bytes mean nothing to anyone, so the
    /// next start replaces them rather than asking for help.
    #[tokio::test]
    async fn a_short_key_from_an_interrupted_first_start_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"half").unwrap();

        let secrets = Secrets::load_or_create(dir.path(), &db).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(KEY_FILE)).unwrap().len(),
            KEY_LEN
        );
        let blob = secrets.seal(HUB, "tmdb", "api_key", "a-key").unwrap();
        assert_eq!(
            secrets.open(HUB, "tmdb", "api_key", &blob).unwrap(),
            "a-key"
        );
        assert!(marker(&db).await.is_some(), "the new key was not recorded");
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Credentials) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        let c = Credentials::open(dir.path(), db).await.unwrap();
        (dir, c)
    }

    fn fields<'a>(pairs: &[(&'a str, &'a str)]) -> BTreeMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    async fn owned(db: &SqlitePool, owner_id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM credentials WHERE owner_id = ?")
            .bind(owner_id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn what_goes_in_comes_back() {
        let (_d, c) = store().await;
        c.set_provider(HUB, "tmdb", &fields(&[("api_key", "operator-key")]))
            .await
            .unwrap();
        c.set_provider(
            "u1",
            "opensubtitles",
            &fields(&[("username", "someone"), ("password", "hunter2")]),
        )
        .await
        .unwrap();

        assert_eq!(
            c.get_provider(HUB, "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), "operator-key".to_string())])
        );
        assert_eq!(
            c.get_provider("u1", "opensubtitles").await.unwrap(),
            BTreeMap::from([
                ("username".to_string(), "someone".to_string()),
                ("password".to_string(), "hunter2".to_string()),
            ])
        );
        // Another viewer's account of the same shape is a different row, and
        // a provider nobody configured is empty rather than an error.
        assert!(
            c.get_provider("u2", "opensubtitles")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(c.get_provider(HUB, "tvdb").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn setting_a_provider_replaces_everything_it_had() {
        let (_d, c) = store().await;
        c.set_provider(
            "u1",
            "opensubtitles",
            &fields(&[("username", "old"), ("password", "old")]),
        )
        .await
        .unwrap();
        // The new pair has no username at all: the old one must not survive.
        c.set_provider("u1", "opensubtitles", &fields(&[("password", "new")]))
            .await
            .unwrap();
        assert_eq!(
            c.get_provider("u1", "opensubtitles").await.unwrap(),
            BTreeMap::from([("password".to_string(), "new".to_string())])
        );
    }

    /// TVDB's pin is optional, so an empty one is a value like any other.
    /// What it means is the caller's business; absence is a missing key.
    #[tokio::test]
    async fn an_empty_field_round_trips() {
        let (_d, c) = store().await;
        c.set_provider(HUB, "tvdb", &fields(&[("api_key", "a-key"), ("pin", "")]))
            .await
            .unwrap();
        assert_eq!(
            c.get_provider(HUB, "tvdb").await.unwrap(),
            BTreeMap::from([
                ("api_key".to_string(), "a-key".to_string()),
                ("pin".to_string(), String::new()),
            ])
        );
    }

    /// A provider one owner holds must not be reachable through another,
    /// including the hub's own rows.
    #[tokio::test]
    async fn one_owners_provider_is_not_anothers() {
        let (_d, c) = store().await;
        c.set_provider(HUB, "tmdb", &fields(&[("api_key", "the-operators")]))
            .await
            .unwrap();
        c.set_provider("u1", "tmdb", &fields(&[("api_key", "the-viewers")]))
            .await
            .unwrap();
        assert_eq!(
            c.get_provider("u1", "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), "the-viewers".to_string())])
        );
        assert_eq!(
            c.get_provider(HUB, "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), "the-operators".to_string())])
        );
        assert!(c.get_provider("u2", "tmdb").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_database_does_not_hold_the_secret() {
        let (_d, c) = store().await;
        c.set_provider("u1", "opensubtitles", &fields(&[("password", "hunter2")]))
            .await
            .unwrap();
        let blob: Vec<u8> = sqlx::query_scalar("SELECT secret FROM credentials")
            .fetch_one(&c.db)
            .await
            .unwrap();
        assert!(!blob.windows(7).any(|w| w == b"hunter2"));
    }

    #[tokio::test]
    async fn a_row_moved_to_another_owner_will_not_open() {
        let (_d, c) = store().await;
        c.set_provider("u1", "opensubtitles", &fields(&[("password", "hunter2")]))
            .await
            .unwrap();
        sqlx::query("UPDATE credentials SET owner_id = 'u2'")
            .execute(&c.db)
            .await
            .unwrap();
        // An error, not an absence: the row is there and it is wrong.
        assert!(c.get_provider("u2", "opensubtitles").await.is_err());
    }

    #[tokio::test]
    async fn deleting_takes_one_provider_or_one_owner() {
        let (_d, c) = store().await;
        // Both viewers hold both providers, so a delete that forgot either
        // column would take one of u2's rows with it.
        for owner in ["u1", "u2"] {
            c.set_provider(
                owner,
                "opensubtitles",
                &fields(&[("username", "x"), ("password", "x")]),
            )
            .await
            .unwrap();
            c.set_provider(owner, "anidb", &fields(&[("password", "x")]))
                .await
                .unwrap();
        }

        assert_eq!(
            delete_provider(&c.db, "u1", "opensubtitles").await.unwrap(),
            2
        );
        assert!(
            c.get_provider("u1", "opensubtitles")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            c.get_provider("u2", "opensubtitles").await.unwrap().len(),
            2
        );
        assert_eq!(
            c.get_provider("u1", "anidb").await.unwrap().len(),
            1,
            "a different provider went with it"
        );

        // u1 still holds anidb AND is given opensubtitles back, so a
        // delete_owner that only took one provider, or took nothing and
        // reported a count, is visible.
        c.set_provider("u1", "opensubtitles", &fields(&[("password", "x")]))
            .await
            .unwrap();
        assert_eq!(delete_owner(&c.db, "u1").await.unwrap(), 2);
        assert_eq!(owned(&c.db, "u1").await, 0, "delete_owner left rows behind");
        assert_eq!(
            owned(&c.db, "u2").await,
            3,
            "delete_owner took another viewer's rows"
        );
    }

    /// `HUB` is the empty string, so an owner id that arrived empty would
    /// delete every provider key the operator has.
    #[tokio::test]
    async fn deleting_an_empty_owner_is_refused() {
        let (_d, c) = store().await;
        c.set_provider(HUB, "tmdb", &fields(&[("api_key", "operator-key")]))
            .await
            .unwrap();
        assert!(delete_owner(&c.db, "").await.is_err());
        assert_eq!(c.get_provider(HUB, "tmdb").await.unwrap().len(), 1);
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Credentials) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        let c = Credentials::open(dir.path(), db).await.unwrap();
        (dir, c)
    }

    async fn plaintext(c: &Credentials, key: &str, value: &str) {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&c.db)
            .await
            .unwrap();
    }

    async fn setting(c: &Credentials, key: &str) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&c.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn it_moves_the_plaintext_and_then_finds_nothing() {
        let (_d, c) = store().await;
        plaintext(&c, "tmdb_api_key", "operator-key").await;

        assert!(
            adopt_settings(&c, "tmdb", &[("tmdb_api_key", "api_key")])
                .await
                .unwrap()
        );
        assert_eq!(
            c.get_provider(HUB, "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), "operator-key".to_string())])
        );
        assert_eq!(setting(&c, "tmdb_api_key").await, None, "left in the clear");

        // A second pass has nothing to do and must not disturb what moved.
        assert!(
            !adopt_settings(&c, "tmdb", &[("tmdb_api_key", "api_key")])
                .await
                .unwrap()
        );
        assert_eq!(c.get_provider(HUB, "tmdb").await.unwrap().len(), 1);
    }

    /// Plaintext beside a sealed provider replaces it. Reaching this needs a
    /// binary older than the adoption to have written the row back, and then
    /// its value is the newer one.
    #[tokio::test]
    async fn plaintext_replaces_what_is_sealed() {
        let (_d, c) = store().await;
        c.set_provider(HUB, "tmdb", &BTreeMap::from([("api_key", "the-old-one")]))
            .await
            .unwrap();
        plaintext(&c, "tmdb_api_key", "from-the-old-binary").await;

        assert!(
            adopt_settings(&c, "tmdb", &[("tmdb_api_key", "api_key")])
                .await
                .unwrap()
        );
        assert_eq!(
            c.get_provider(HUB, "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), "from-the-old-binary".to_string())])
        );
        assert_eq!(setting(&c, "tmdb_api_key").await, None);
    }

    /// The plaintext one provider left behind is deleted the moment it moves,
    /// and deleted is not gone until the log is truncated. A LATER provider
    /// failing must not take the checkpoint down with it.
    #[tokio::test]
    async fn a_later_failure_still_truncates_what_already_moved() {
        const CANARY: &str = "canary-operator-key";
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let c = Credentials::open(dir.path(), db.clone()).await.unwrap();
        sqlx::query("INSERT INTO settings (key, value) VALUES ('tmdb_api_key', ?)")
            .bind(CANARY)
            .execute(&db)
            .await
            .unwrap();
        // The second provider's plaintext is not text, so reading it fails
        // AFTER the first has been sealed and its row deleted.
        sqlx::query("INSERT INTO settings (key, value) VALUES ('tvdb_api_key', X'ff')")
            .execute(&db)
            .await
            .unwrap();

        let outcome = adopt_all(
            &c,
            &[
                ("tmdb", &[("tmdb_api_key", "api_key")]),
                ("tvdb", &[("tvdb_api_key", "api_key")]),
            ],
        )
        .await;
        assert!(outcome.is_err(), "the unreadable setting was not fatal");

        assert_eq!(
            c.get_provider(HUB, "tmdb").await.unwrap(),
            BTreeMap::from([("api_key".to_string(), CANARY.to_string())]),
            "the first provider moved"
        );
        assert_eq!(setting(&c, "tmdb_api_key").await, None, "left in the clear");
        // The point of the whole exercise: the pre-delete page image is not
        // sitting in the log for the next person with the file.
        let wal = std::fs::metadata(dir.path().join("hub.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(wal, 0, "the write-ahead log still holds the plaintext");
        let bytes = std::fs::read(dir.path().join("hub.db")).unwrap();
        assert!(
            !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
            "the plaintext is still readable in hub.db"
        );
    }

    /// A one-way door: after the delete there is nothing to compare against
    /// and nothing to retry from, so the sealed row is read back first.
    #[tokio::test]
    async fn plaintext_survives_a_seal_that_does_not_read_back() {
        let (_d, c) = store().await;
        plaintext(&c, "tmdb_api_key", "operator-key").await;
        // Corrupt whatever gets written, the moment it is written: a trigger
        // is the one way to be inside the same transaction as the insert.
        sqlx::query(
            "CREATE TRIGGER spoil AFTER INSERT ON credentials BEGIN
               UPDATE credentials SET secret = randomblob(length(secret))
                WHERE owner_id = NEW.owner_id
                  AND provider = NEW.provider
                  AND field = NEW.field;
             END",
        )
        .execute(&c.db)
        .await
        .unwrap();

        let e = adopt_settings(&c, "tmdb", &[("tmdb_api_key", "api_key")])
            .await
            .expect_err("a credential that will not read back was accepted");
        assert!(format!("{e:#}").contains("tmdb"), "{e:#}");
        assert_eq!(
            setting(&c, "tmdb_api_key").await.as_deref(),
            Some("operator-key"),
            "the plaintext was deleted for a credential that cannot be read"
        );
    }

    #[tokio::test]
    async fn nothing_to_adopt_is_not_an_error() {
        let (_d, c) = store().await;
        assert!(
            !adopt_settings(&c, "tmdb", &[("tmdb_api_key", "api_key")])
                .await
                .unwrap()
        );
        assert!(c.get_provider(HUB, "tmdb").await.unwrap().is_empty());
    }
}
