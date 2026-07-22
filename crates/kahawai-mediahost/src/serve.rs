//! Byte-plane serving (MH-6): answer a hub OpenRead by opening a
//! ByteChannel and serving read requests until the hub closes it.
//! Read-only by construction — there is no write operation in the protocol.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{ByteChunk, OpenRead};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::wrappers::ReceiverStream;

use crate::scan::CollectionConfig;

const CHUNK: usize = 256 * 1024;

/// Resolve an OpenRead against the configured collections, refusing
/// anything that escapes a collection root (NFR-4).
pub fn resolve_path(collections: &[CollectionConfig], req: &OpenRead) -> Result<PathBuf> {
    let col = collections
        .iter()
        .find(|c| c.name == req.collection_id)
        .with_context(|| format!("unknown collection {}", req.collection_id))?;
    for root in &col.roots {
        let root = match std::fs::canonicalize(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Canonicalize the candidate too: symlinks and `..` both resolve,
        // so a path that lands outside the root is rejected regardless of
        // how it was spelled.
        if let Ok(candidate) = std::fs::canonicalize(root.join(&req.path_rel))
            && candidate.starts_with(&root)
            && candidate.is_file()
        {
            return Ok(candidate);
        }
    }
    bail!("path not found or outside collection roots: {}", req.path_rel)
}

/// Open the byte channel and serve read requests for one lease.
pub async fn serve_lease(
    channel: tonic::transport::Channel,
    lease_token: String,
    path: Result<PathBuf>,
) -> Result<()> {
    let mut client = MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel::<ByteChunk>(8);

    // First chunk binds the token; carry a resolution error if there is one.
    let (bind_error, file) = match path {
        Ok(p) => {
            let f = tokio::fs::File::open(&p).await.with_context(|| format!("opening {}", p.display()))?;
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
        let end = req.offset.saturating_add(req.len).min(size);
        let mut cur = req.offset.min(size);
        file.seek(std::io::SeekFrom::Start(cur)).await?;
        let mut buf = vec![0u8; CHUNK];
        while cur < end {
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

    fn req(collection: &str, path: &str) -> OpenRead {
        OpenRead {
            lease_token: "t".into(),
            collection_id: collection.into(),
            path_rel: path.into(),
        }
    }

    #[test]
    fn resolves_inside_root_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("../escape.mkv"), b"x").ok();

        assert!(resolve_path(&cols(dir.path()), &req("movies", "sub/a.mkv")).is_ok());
        assert!(resolve_path(&cols(dir.path()), &req("movies", "../escape.mkv")).is_err());
        assert!(resolve_path(&cols(dir.path()), &req("movies", "/etc/passwd")).is_err());
        assert!(resolve_path(&cols(dir.path()), &req("movies", "missing.mkv")).is_err());
        assert!(resolve_path(&cols(dir.path()), &req("other", "sub/a.mkv")).is_err());
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
        assert!(resolve_path(&cols(dir.path()), &req("movies", "link.mkv")).is_err());
    }
}
