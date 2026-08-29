use kahawai_hub::registry::{LoudnessPreference, Registry};
use kahawai_proto::v1::{AudioLayoutLoudness, AudioLoudnessTrack, FileLoudness, SourcePath};

async fn fixture() -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::raw_sql(
        r#"
        INSERT INTO collections(module_id,collection_id,media_type)
          VALUES('m','c','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
          VALUES('m','c','r','/movies');
        INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                          head_xxh3,tail_xxh3,oshash,streams_json)
          SELECT 'm','c',id,'movie.mkv',10,1,0,0,0,
                 '{"audio":[{"codec":"dts","channels":8,"sample_rate":48000,"language":null,"bitrate_kbps":null,"layout":null},{"codec":"aac","channels":2,"sample_rate":48000,"language":null,"bitrate_kbps":null,"layout":"0x3"}]}'
            FROM collection_roots;
        "#,
    )
    .execute(&db)
    .await
    .unwrap();
    (dir, Registry::new(db, Default::default()))
}

fn result(mtime_unix: i64) -> FileLoudness {
    FileLoudness {
        collection_id: "c".into(),
        source: Some(SourcePath::new("r", "movie.mkv")),
        size: 10,
        mtime_unix,
        analyzer: kahawai_media::loudness::ANALYZER,
        tracks: vec![
            AudioLoudnessTrack {
                stream_index: 0,
                integrated_lufs: -27.0,
                true_peak_dbtp: -6.0,
                source_channels: 8,
                source_channel_mask: 0xc3f,
                native_integrated_lufs: -24.0,
                native_true_peak_dbtp: -5.0,
                layouts: vec![
                    AudioLayoutLoudness {
                        channels: 8,
                        channel_mask: 0xc3f,
                        integrated_lufs: -24.0,
                        true_peak_dbtp: -5.0,
                    },
                    AudioLayoutLoudness {
                        channels: 8,
                        channel_mask: 0xff,
                        integrated_lufs: -23.0,
                        true_peak_dbtp: -4.0,
                    },
                    AudioLayoutLoudness {
                        channels: 6,
                        channel_mask: 0x3f,
                        integrated_lufs: -25.0,
                        true_peak_dbtp: -5.5,
                    },
                    AudioLayoutLoudness {
                        channels: 2,
                        channel_mask: 0x3,
                        integrated_lufs: -27.0,
                        true_peak_dbtp: -6.0,
                    },
                    AudioLayoutLoudness {
                        channels: 1,
                        channel_mask: 0x4,
                        integrated_lufs: -30.0,
                        true_peak_dbtp: -9.0,
                    },
                ],
            },
            AudioLoudnessTrack {
                stream_index: 1,
                integrated_lufs: -19.0,
                true_peak_dbtp: -2.0,
                source_channels: 2,
                native_integrated_lufs: -19.0,
                native_true_peak_dbtp: -2.0,
                source_channel_mask: 0x3,
                layouts: vec![
                    AudioLayoutLoudness {
                        channels: 2,
                        channel_mask: 0x3,
                        integrated_lufs: -19.0,
                        true_peak_dbtp: -2.0,
                    },
                    AudioLayoutLoudness {
                        channels: 1,
                        channel_mask: 0x4,
                        integrated_lufs: -22.0,
                        true_peak_dbtp: -5.0,
                    },
                ],
            },
        ],
        error: String::new(),
    }
}

#[tokio::test]
async fn measurements_are_complete_revision_guarded_source_facts() {
    let (_dir, registry) = fixture().await;
    assert_eq!(registry.loudness_worklist("m", "c").await.unwrap().len(), 1);

    assert!(
        registry
            .record_file_loudness("m", &result(1))
            .await
            .unwrap()
    );
    assert!(
        registry
            .loudness_worklist("m", "c")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        registry.audio_loudness(1, 0).await.unwrap(),
        Some(kahawai_media::loudness::AudioLoudnessMeasurement {
            source: kahawai_media::loudness::AudioLayout::new(8, 0xc3f),
            layouts: vec![
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(8, 0xff),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -23.0,
                        true_peak_dbtp: -4.0,
                    },
                },
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(8, 0xc3f),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -24.0,
                        true_peak_dbtp: -5.0,
                    },
                },
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(6, 0x3f),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -25.0,
                        true_peak_dbtp: -5.5,
                    },
                },
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(2, 0x3),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -27.0,
                        true_peak_dbtp: -6.0,
                    },
                },
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(1, 0x4),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -30.0,
                        true_peak_dbtp: -9.0,
                    },
                },
            ],
        })
    );

    sqlx::query("UPDATE files SET mtime_unix=2")
        .execute(registry.db())
        .await
        .unwrap();
    assert_eq!(registry.loudness_worklist("m", "c").await.unwrap().len(), 1);
    assert!(registry.audio_loudness(1, 0).await.unwrap().is_none());

    let mut failed = result(2);
    failed.tracks.clear();
    failed.error = "decoder failed".into();
    assert!(registry.record_file_loudness("m", &failed).await.unwrap());
    assert!(
        registry
            .loudness_worklist("m", "c")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(registry.audio_loudness(1, 0).await.unwrap().is_none());
}

#[tokio::test]
async fn positionless_source_keeps_exact_layouts_without_fabricating_native_loudness() {
    let (_dir, registry) = fixture().await;
    let mut measured = result(1);
    measured.tracks[0].source_channel_mask = 0;
    measured.tracks[0].native_integrated_lufs = f64::NAN;
    measured.tracks[0].native_true_peak_dbtp = f64::NAN;

    assert!(registry.record_file_loudness("m", &measured).await.unwrap());
    let stored = registry.audio_loudness(1, 0).await.unwrap().unwrap();
    assert_eq!(
        stored.source,
        kahawai_media::loudness::AudioLayout::new(8, 0)
    );
    assert!(
        stored
            .get(kahawai_media::loudness::AudioLayout::new(6, 0x3f))
            .is_some(),
        "canonical exact gains must remain available"
    );
    let native: Option<f64> = sqlx::query_scalar(
        "SELECT native_integrated_lufs FROM audio_loudness
          WHERE file_id=1 AND stream_index=0",
    )
    .fetch_one(registry.db())
    .await
    .unwrap();
    assert_eq!(native, None, "positionless input has no exact native key");
}

#[tokio::test]
async fn one_bad_audio_stream_preserves_its_successful_siblings() {
    let (_dir, registry) = fixture().await;
    let mut partial = result(1);
    partial.tracks.retain(|track| track.stream_index == 0);
    partial.error = "audio stream 1: decoder failed".into();

    assert!(registry.record_file_loudness("m", &partial).await.unwrap());
    assert!(
        registry.audio_loudness(1, 0).await.unwrap().is_some(),
        "the successful main track was discarded with its failed sibling"
    );
    assert!(registry.audio_loudness(1, 1).await.unwrap().is_none());
    let errors: Vec<(i64, String)> = sqlx::query_as(
        "SELECT stream_index,error FROM audio_loudness
          WHERE file_id=1 ORDER BY stream_index",
    )
    .fetch_all(registry.db())
    .await
    .unwrap();
    assert_eq!(
        errors,
        vec![
            (0, String::new()),
            (1, "audio stream 1: decoder failed".into()),
        ]
    );
    assert!(
        registry
            .loudness_worklist("m", "c")
            .await
            .unwrap()
            .is_empty(),
        "the terminal failed stream queued the whole file again"
    );
}

#[tokio::test]
async fn loudness_normalization_has_encoded_force_and_off_modes() {
    let (_dir, registry) = fixture().await;
    sqlx::query("INSERT INTO users(id,username,password_hash) VALUES('u','viewer','x')")
        .execute(registry.db())
        .await
        .unwrap();
    assert_eq!(
        registry.loudness_normalization("u").await.unwrap(),
        LoudnessPreference::Encoded
    );

    for (value, expected) in [
        ("force", LoudnessPreference::Force),
        ("off", LoudnessPreference::Off),
        ("unknown", LoudnessPreference::Encoded),
    ] {
        sqlx::query(
            "INSERT INTO user_prefs(user_id,scope,key,value)
             VALUES('u','','loudness_normalization',?)
             ON CONFLICT(user_id,scope,key) DO UPDATE SET value=excluded.value",
        )
        .bind(value)
        .execute(registry.db())
        .await
        .unwrap();
        assert_eq!(
            registry.loudness_normalization("u").await.unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn music_is_excluded_from_loudness_analysis() {
    let (_dir, registry) = fixture().await;
    sqlx::raw_sql(
        r#"
        INSERT INTO collections(module_id,collection_id,media_type)
          VALUES('m','music','music');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
          VALUES('m','music','music-root','/music');
        INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                          head_xxh3,tail_xxh3,oshash,streams_json)
          SELECT 'm','music',id,'album.flac',10,1,0,0,0,
                 '{"audio":[{"codec":"flac","channels":2,"sample_rate":48000,"language":null,"bitrate_kbps":null,"layout":"0x3"}]}'
            FROM collection_roots WHERE collection_id='music';
        "#,
    )
    .execute(registry.db())
    .await
    .unwrap();
    let file_id: i64 = sqlx::query_scalar("SELECT id FROM files WHERE collection_id='music'")
        .fetch_one(registry.db())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO audio_loudness
           (file_id,stream_index,analyzer,size,mtime_unix,integrated_lufs,
            true_peak_dbtp,source_channels,native_integrated_lufs,
            native_true_peak_dbtp,error,measured_at)
         VALUES(?,0,?,10,1,-18,-1,2,-18,-1,'',unixepoch())",
    )
    .bind(file_id)
    .bind(kahawai_media::loudness::ANALYZER)
    .execute(registry.db())
    .await
    .unwrap();

    assert!(
        registry
            .loudness_worklist("m", "music")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        registry.audio_loudness(file_id, 0).await.unwrap().is_none(),
        "music must never receive Kahawai loudness gain"
    );
}
