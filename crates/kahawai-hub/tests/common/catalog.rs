use kahawai_proto::v1 as pb;
use prost::Message as _;

pub async fn project_files(
    registry: &kahawai_hub::registry::Registry,
    tx: &tokio::sync::mpsc::Sender<pb::HostToHub>,
    inbound: &mut tonic::Streaming<pb::HubToHost>,
    collection_id: &str,
    media_type: &str,
    roots: Vec<pb::CollectionRoot>,
    files: Vec<pb::FileRecord>,
) {
    let current_version = files.len() as u64;
    tx.send(pb::HostToHub {
        msg: Some(pb::host_to_hub::Msg::CatalogOffer(pb::CatalogOffer {
            collections: vec![pb::CatalogCollection {
                id: collection_id.into(),
                media_type: media_type.into(),
                roots,
                epoch: "fixture".into(),
                current_version,
                oldest_replayable_version: 0,
                scanning: false,
            }],
        })),
    })
    .await
    .unwrap();
    let cursor = inbound.message().await.unwrap().unwrap();
    let Some(pb::hub_to_host::Msg::CatalogCursor(cursor)) = cursor.msg else {
        panic!("expected catalogue cursor")
    };
    let records = files
        .into_iter()
        .enumerate()
        .map(|(index, file)| {
            let source = file.source.as_ref().expect("fixture file has no source");
            let mut key = source.root_token.clone().into_bytes();
            key.push(0);
            key.extend_from_slice(source.path_rel.as_bytes());
            pb::CatalogRecord {
                version: index as u64 + 1,
                kind: "file".into(),
                key,
                payload: pb::FileUpsert {
                    collection_id: collection_id.into(),
                    files: vec![file],
                }
                .encode_to_vec(),
                deleted: false,
            }
        })
        .collect();
    tx.send(pb::HostToHub {
        msg: Some(pb::host_to_hub::Msg::CatalogDelta(pb::CatalogDelta {
            collection_id: collection_id.into(),
            epoch: "fixture".into(),
            records,
            through_version: current_version,
            snapshot: cursor.snapshot,
            done: true,
        })),
    })
    .await
    .unwrap();
    let ack = inbound.message().await.unwrap().unwrap();
    assert!(matches!(ack.msg, Some(pb::hub_to_host::Msg::CatalogAck(_))));
    seed_legacy_fixture(registry).await;
}

/// Explicit arrangement for the retired consumer regression suites. This is
/// test-only: production ingestion never writes a second catalogue.
pub async fn seed_legacy_fixture(registry: &kahawai_hub::registry::Registry) {
    for summary in registry.catalogue().collection_summaries().await.unwrap() {
        let c = summary.collection;
        let roots: Vec<String> = summary
            .roots
            .into_iter()
            .filter(|r| r.active)
            .map(|r| r.path)
            .collect();
        registry
            .announce_collection(&c.mediahost_id, &c.remote_id, c.media_type.as_str(), &roots)
            .await
            .unwrap();
        let files = registry
            .catalogue()
            .files(&c.id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|f| {
                Some(kahawai_hub::registry::FileUpsertRecord {
                    root_token: f.root_token,
                    path_rel: f.path,
                    size: f.size?,
                    mtime_unix: f.mtime?,
                    head_xxh3: f.head_hash?,
                    tail_xxh3: f.tail_hash?,
                    oshash: f.oshash?,
                    streams_json: serde_json::to_string(&f.media?).unwrap(),
                })
            })
            .collect();
        registry
            .upsert_files(&c.mediahost_id, &c.remote_id, files)
            .await
            .unwrap();
    }
}
