#![allow(dead_code)]
use kahawai_mediadb::*;
use kahawai_proto::v1 as p;
use prost::Message;

pub async fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create(&dir.path().join("mediadb.db")).await.unwrap();
    store.put_mediahost("host", "Fixture").await.unwrap();
    (dir, store)
}
pub fn offer(remote: &str, kind: MediaType, version: u64) -> p::CatalogCollection {
    p::CatalogCollection {
        id: remote.into(),
        media_type: kind.as_str().into(),
        roots: vec![p::CollectionRoot::new("root", format!("/media/{remote}"))],
        epoch: "epoch".into(),
        current_version: version,
        ..Default::default()
    }
}
pub fn file(version: u64, path: &str) -> p::CatalogRecord {
    file_media(version, path, kahawai_core::media::MediaInfo::default())
}
pub fn file_media(
    version: u64,
    path: &str,
    media: kahawai_core::media::MediaInfo,
) -> p::CatalogRecord {
    let source = p::SourcePath::new("root", path);
    let payload = p::FileUpsert {
        collection_id: String::new(),
        files: vec![p::FileRecord {
            source: Some(source),
            size: 123,
            mtime_unix: 456,
            head_xxh3: u64::MAX,
            tail_xxh3: 7,
            oshash: 8,
            streams_json: serde_json::to_string(&media).unwrap(),
        }],
    }
    .encode_to_vec();
    p::CatalogRecord {
        version,
        kind: "file".into(),
        key: format!("root\0{path}").into_bytes(),
        payload,
        deleted: false,
    }
}
pub fn delta(
    remote: &str,
    snapshot: bool,
    done: bool,
    through: u64,
    mut records: Vec<p::CatalogRecord>,
) -> p::CatalogDelta {
    for r in &mut records {
        if r.kind == "file" && !r.deleted {
            let mut f = p::FileUpsert::decode(r.payload.as_slice()).unwrap();
            f.collection_id = remote.into();
            r.payload = f.encode_to_vec();
        }
    }
    p::CatalogDelta {
        collection_id: remote.into(),
        epoch: "epoch".into(),
        records,
        through_version: through,
        snapshot,
        done,
    }
}
pub async fn collection(store: &Store, remote: &str, kind: MediaType, paths: &[&str]) -> String {
    let version = paths.len().max(1) as u64;
    let (id, _) = store
        .offer_collection("host", &offer(remote, kind, version))
        .await
        .unwrap();
    let records = paths
        .iter()
        .enumerate()
        .map(|(i, p)| file(i as u64 + 1, p))
        .collect();
    store
        .apply_catalogue("host", &delta(remote, true, true, version, records))
        .await
        .unwrap();
    id
}
pub fn record(
    provider: &str,
    external: &str,
    title: &str,
    year: Option<i32>,
    kind: MediaType,
) -> ProviderRecord {
    ProviderRecord {
        children: None,
        provider: provider.into(),
        namespace: kind.as_str().into(),
        external_id: external.into(),
        language: "en".into(),
        media_type: kind,
        title: title.into(),
        year,
        description: Description::default(),
    }
}
pub fn tagged(album: &str, disc: u32, track: u32) -> kahawai_core::media::MediaInfo {
    kahawai_core::media::MediaInfo {
        tags: std::collections::BTreeMap::from([
            ("album".into(), album.into()),
            ("album_artist".into(), "Album Artist".into()),
            ("artist".into(), "Guest".into()),
            ("title".into(), format!("Track {track}")),
            ("disc_number".into(), disc.to_string()),
            ("track_number".into(), track.to_string()),
            ("date".into(), "2001-02-03".into()),
        ]),
        ..Default::default()
    }
}
