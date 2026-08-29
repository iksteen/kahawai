use kahawai_proto::v1 as pb;
use prost::Message as _;

pub async fn project_files(
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
}
