//! Byte-plane serving (MH-6): answer a hub OpenRead by opening a
//! ByteChannel and serving read requests until the hub closes it.
//! Read-only by construction — there is no write operation in the protocol.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{ByteChunk, OpenRead};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::wrappers::ReceiverStream;

use crate::scan::CollectionConfig;
use crate::scheduler::{Priority, Scheduler};

const CHUNK: usize = 256 * 1024;

/// Resolve an OpenRead against the configured collections, refusing
/// anything that escapes a collection root (NFR-4).
pub fn resolve_path(collections: &[CollectionConfig], req: &OpenRead) -> Result<PathBuf> {
    let source = req
        .source
        .as_ref()
        .context("OpenRead missing exact source")?;
    resolve_rel(
        collections,
        &req.collection_id,
        &source.root_token,
        &source.path_rel,
    )
}

/// Resolve a collection-relative path against the collection's roots,
/// canonicalized and confined (shared by lease serving and the hasher).
pub fn resolve_rel(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
) -> Result<PathBuf> {
    let col = collections
        .iter()
        .find(|c| c.name == collection_id)
        .with_context(|| format!("unknown collection {collection_id}"))?;
    anyhow::ensure!(
        !root_token.is_empty(),
        "exact source has an empty root token"
    );
    let configured = col
        .resolved_roots()
        .find(|r| r.token == root_token)
        .with_context(|| {
            format!("unknown root token {root_token} in collection {collection_id}")
        })?;
    let root = std::fs::canonicalize(&configured.path)
        .with_context(|| format!("root unavailable: {}", configured.path.display()))?;
    // Canonicalize the candidate too: symlinks and `..` both resolve,
    // so a path that lands outside the exact root is rejected regardless of
    // how it was spelled.
    if let Ok(candidate) = std::fs::canonicalize(root.join(path_rel))
        && candidate.starts_with(&root)
        && candidate.is_file()
    {
        return Ok(candidate);
    }
    bail!("path not found or outside exact collection root: {path_rel}")
}

/// Compatibility helper for tests and protocol-3 fixtures that do not carry a
/// runtime scheduler. It still uses the scheduler, conservatively grouping the
/// lease into the fallback storage domain as foreground demand.
pub async fn serve_lease(
    channel: tonic::transport::Channel,
    lease_token: String,
    path: Result<PathBuf>,
) -> Result<()> {
    let scheduler = Scheduler::new(&[], &Default::default())?;
    serve_lease_scheduled(
        channel,
        lease_token,
        path,
        scheduler,
        String::new(),
        false,
        None,
    )
    .await
}

/// Resolve and serve a production request under the same resource admission.
/// Canonicalization can itself block on a network mount, so it must not happen
/// in the control-link task before the scheduler sees the operation.
pub async fn serve_request_scheduled(
    channel: tonic::transport::Channel,
    request: OpenRead,
    collections: Vec<CollectionConfig>,
    scheduler: Scheduler,
    owner: Option<String>,
) -> Result<()> {
    let source = request
        .source
        .as_ref()
        .context("OpenRead missing exact source")?;
    let root_token = source.root_token.clone();
    let background = request.background;
    let resources = scheduler.resources([root_token.as_str()], false);
    let _interactive = (!background).then(|| {
        scheduler.enter_interactive(
            resources.clone(),
            format!("viewer path resolution {root_token}"),
        )
    });
    let _resolution_permit = if background {
        Some(
            scheduler
                .acquire(
                    Priority::LocalMetadata,
                    resources,
                    owner.clone(),
                    format!("background path resolution {root_token}"),
                )
                .await?,
        )
    } else {
        None
    };
    let lease_token = request.lease_token.clone();
    let path = tokio::task::spawn_blocking(move || resolve_path(&collections, &request))
        .await
        .context("path resolution task failed")?;
    drop(_resolution_permit);
    drop(_interactive);
    serve_lease_scheduled(
        channel,
        lease_token,
        path,
        scheduler,
        root_token,
        background,
        owner,
    )
    .await
}

/// Open the byte channel and serve read requests for one scheduled lease.
pub async fn serve_lease_scheduled(
    channel: tonic::transport::Channel,
    lease_token: String,
    path: Result<PathBuf>,
    scheduler: Scheduler,
    root_token: String,
    background: bool,
    owner: Option<String>,
) -> Result<()> {
    let mut client = MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel::<ByteChunk>(8);
    let resources = scheduler.resources([root_token.as_str()], false);
    let _interactive = (!background).then(|| {
        scheduler.enter_interactive(resources.clone(), format!("viewer read {root_token}"))
    });

    // First chunk binds the token; carry a resolution error if there is one.
    let (bind_error, file) = match path {
        Ok(p) => {
            let _open_permit = if background {
                Some(
                    scheduler
                        .acquire(
                            Priority::LocalMetadata,
                            resources.clone(),
                            owner.clone(),
                            format!("background open {root_token}"),
                        )
                        .await?,
                )
            } else {
                None
            };
            let f = tokio::fs::File::open(&p)
                .await
                .with_context(|| format!("opening {}", p.display()))?;
            (String::new(), Some(f))
        }
        Err(e) => (format!("{e:#}"), None),
    };
    tx.send(ByteChunk {
        lease_token: lease_token.clone(),
        offset: 0,
        data: Vec::new(),
        eof: false,
        error: bind_error.clone(),
    })
    .await
    .ok();

    let mut requests = client
        .byte_channel(ReceiverStream::new(rx))
        .await
        .context("opening byte channel")?
        .into_inner();
    let Some(mut file) = file else {
        return Ok(()); // error delivered; hub will drop the lease
    };
    let size = file.metadata().await?.len();

    while let Some(req) = requests.message().await? {
        let permit = if background {
            Some(
                scheduler
                    .acquire(
                        Priority::LocalMetadata,
                        resources.clone(),
                        owner.clone(),
                        format!("background read {root_token}"),
                    )
                    .await?,
            )
        } else {
            None
        };
        let end = req.offset.saturating_add(req.len).min(size);
        let mut cur = req.offset.min(size);
        file.seek(std::io::SeekFrom::Start(cur)).await?;
        let mut buf = vec![0u8; CHUNK];
        while cur < end {
            if let Some(permit) = &permit {
                permit.checkpoint().await?;
            }
            let want = ((end - cur) as usize).min(CHUNK);
            let n = file.read(&mut buf[..want]).await?;
            if n == 0 {
                break;
            }
            let chunk = ByteChunk {
                lease_token: String::new(),
                offset: cur,
                data: buf[..n].to_vec(),
                eof: false,
                error: String::new(),
            };
            if tx.send(chunk).await.is_err() {
                return Ok(()); // hub dropped the lease
            }
            cur += n as u64;
        }
        let eof = ByteChunk {
            lease_token: String::new(),
            offset: cur,
            data: Vec::new(),
            eof: true,
            error: String::new(),
        };
        if tx.send(eof).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cols(root: &Path) -> Vec<CollectionConfig> {
        vec![CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![root.to_path_buf()],
        }]
    }

    fn req(collection: &str, root: &Path, path: &str) -> OpenRead {
        OpenRead {
            lease_token: "t".into(),
            collection_id: collection.into(),
            source: Some(kahawai_proto::v1::SourcePath {
                root_token: kahawai_core::media::root_token(root),
                path_rel: path.into(),
            }),
            background: false,
        }
    }

    #[test]
    fn resolves_inside_root_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("../escape.mkv"), b"x").ok();

        assert!(resolve_path(&cols(dir.path()), &req("movies", dir.path(), "sub/a.mkv")).is_ok());
        assert!(
            resolve_path(
                &cols(dir.path()),
                &req("movies", dir.path(), "../escape.mkv")
            )
            .is_err()
        );
        assert!(
            resolve_path(&cols(dir.path()), &req("movies", dir.path(), "/etc/passwd")).is_err()
        );
        assert!(
            resolve_path(&cols(dir.path()), &req("movies", dir.path(), "missing.mkv")).is_err()
        );
        assert!(resolve_path(&cols(dir.path()), &req("other", dir.path(), "sub/a.mkv")).is_err());
    }

    #[test]
    fn symlink_escape_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.mkv"), b"x").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.mkv"),
            dir.path().join("link.mkv"),
        )
        .unwrap();
        assert!(resolve_path(&cols(dir.path()), &req("movies", dir.path(), "link.mkv")).is_err());
    }
}
