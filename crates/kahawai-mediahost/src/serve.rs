//! Byte-plane serving (MH-6): answer a hub OpenRead by opening a
//! ByteChannel and serving read requests until the hub closes it.
//! Read-only by construction — there is no write operation in the protocol.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{ByteChunk, OpenRead};
use kahawai_transport::source_stream::{self, FileReader};
use tokio_stream::wrappers::ReceiverStream;

use crate::scan::CollectionConfig;
use crate::scheduler::{Priority, Scheduler};

async fn enter_operation(
    scheduler: &Scheduler,
    resources: &crate::scheduler::Resources,
    background: bool,
    owner: Option<String>,
    label: String,
) -> Result<Box<dyn Send + Sync>> {
    if background {
        Ok(Box::new(
            scheduler
                .acquire(Priority::LocalMetadata, resources.clone(), owner, label)
                .await?,
        ))
    } else {
        Ok(Box::new(
            scheduler.enter_interactive(resources.clone(), label),
        ))
    }
}

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
    let resolution_permit = enter_operation(
        &scheduler,
        &resources,
        background,
        owner.clone(),
        format!("path resolution {root_token}"),
    )
    .await?;
    let lease_token = request.lease_token.clone();
    let path = tokio::task::spawn_blocking(move || resolve_path(&collections, &request))
        .await
        .context("path resolution task failed")?;
    drop(resolution_permit);
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
    // Keep playback demand present between disk reads and while the network
    // applies backpressure. Brief per-read guards can disappear before an
    // analyzer reaches its next checkpoint, leaving it running during playback.
    // Reserve CPU only; storage guards below cover actual filesystem work.
    // Dropping the lease resumes scheduled CPU work.
    let _playback = (!background).then(|| scheduler.enter_playback("playback byte lease"));

    let admission: source_stream::Admission = std::sync::Arc::new(move || {
        let scheduler = scheduler.clone();
        let resources = resources.clone();
        let owner = owner.clone();
        let label = format!("source read {root_token}");
        Box::pin(
            async move { enter_operation(&scheduler, &resources, background, owner, label).await },
        )
    });
    let opened = match path {
        Ok(path) => FileReader::open(path, Some(admission)).await,
        Err(e) => Err(e),
    };
    // Opening errors must bind the lease too, otherwise the hub waits ten
    // seconds for a byte channel whose failure never reached it.
    tx.send(ByteChunk {
        lease_token,
        error: opened
            .as_ref()
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default(),
        ..Default::default()
    })
    .await
    .ok();
    let mut requests = client
        .byte_channel(ReceiverStream::new(rx))
        .await
        .context("opening byte channel")?
        .into_inner();
    let Ok(file) = opened else {
        return Ok(());
    };
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(2);
    let commands = async {
        while let Some(req) = requests.message().await? {
            if command_tx.send(req).await.is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = commands => result?,
        _ = source_stream::serve(command_rx, tx, file.size, |offset, len| file.read(offset, len)) => {},
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
