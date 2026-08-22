//! Files nobody but their owner may read: keys, secrets, the database.
//!
//! Two halves of one rule, because a file can be exposed at either end.
//!
//! **Creating.** `fs::write` asks for 0666, so under a typical umask the file
//! exists at 0644 until a following `set_permissions` lands — a window during
//! which a key is world-readable. [`create`] and [`write`] pass the mode to
//! `open(2)` instead, which applies `mode & ~umask`: umask can only make the
//! result stricter, and the file never exists at another mode.
//!
//! **Loading.** The copies this program did not make arrive however they were
//! stored: an object store carries no mode at all, `tar -x` carries the
//! archive's, `fs::copy` carries the source's. [`narrow`] is what a load path
//! calls to put that right, and it reports what it found so a caller can say
//! so — for a key, "it was readable" outlives "it is not now".
//!
//! Everything here is a no-op about modes off Unix, so callers need no `cfg`.

use std::fs::File;
use std::io;
use std::path::Path;

/// Create a new file that never exists at a wider mode. Fails if it is
/// already there — the caller decides what that means.
pub fn create(path: &Path) -> io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Create a directory only its owner may enter, so what lands inside it is
/// unreachable to anyone else whatever mode the writer gave it.
///
/// The leaf only: parents are created with the usual mode, because a snapshot
/// under `/srv/snapshots/today` has no business making `/srv/snapshots`
/// private. Fails if the leaf is already there — the caller knows whether
/// that is a problem.
pub fn create_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Write `bytes`, replacing whatever is there. The mode applies only when the
/// file is created, so an existing one is narrowed first: overwriting a
/// world-readable file otherwise leaves it world-readable.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    narrow(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    io::Write::write_all(&mut opts.open(path)?, bytes)
}

/// Restrict an existing file to 0600, returning the mode it replaced when it
/// had to do anything. A path that is not there is not an error: nothing is
/// exposed by a file that does not exist.
#[cfg(unix)]
pub fn narrow(path: &Path) -> io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    let found = match std::fs::metadata(path) {
        Ok(meta) => meta.permissions().mode() & 0o777,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if found & 0o077 == 0 {
        return Ok(None);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Some(found))
}

#[cfg(not(unix))]
pub fn narrow(_path: &Path) -> io::Result<Option<u32>> {
    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The window this exists to close: a wide umask must not widen the file,
    /// because `open(2)` can only clear bits.
    #[test]
    fn a_created_file_is_never_visible_at_a_wider_mode() {
        let dir = tempfile::tempdir().unwrap();
        let previous = unsafe { libc::umask(0) };
        let created = create(&dir.path().join("key")).map(drop);
        let written = write(&dir.path().join("secret"), b"s3cret");
        unsafe { libc::umask(previous) };
        created.unwrap();
        written.unwrap();
        assert_eq!(mode(&dir.path().join("key")), 0o600);
        assert_eq!(mode(&dir.path().join("secret")), 0o600);
    }

    #[test]
    fn creating_one_that_is_already_there_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        create(&path).unwrap();
        assert_eq!(
            create(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn overwriting_a_readable_file_does_not_leave_it_readable() {
        // The mode is only applied at creation, so this is the half that a
        // create-with-mode alone does not cover.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write(&path, b"new").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn narrowing_reports_what_it_replaced_and_nothing_when_it_need_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        create(&path).unwrap();
        assert_eq!(narrow(&path).unwrap(), None, "already tight");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o604)).unwrap();
        assert_eq!(
            narrow(&path).unwrap(),
            Some(0o604),
            "the mode it replaced is what a caller reports"
        );
        assert_eq!(mode(&path), 0o600);
        // Group-only counts too: "readable beyond its owner" is not "world".
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(narrow(&path).unwrap(), Some(0o640));
    }

    #[test]
    fn a_file_that_is_not_there_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(narrow(&dir.path().join("absent")).unwrap(), None);
    }
}
