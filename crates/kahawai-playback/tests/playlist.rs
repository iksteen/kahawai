use kahawai_playback::playlist::{
    FLOOR_RUNWAY_SECS, MAX_RUNWAY_SECS, playlist_ready, playlist_span_secs, playlist_text_ready,
    runway_secs,
};

#[test]
fn runway_follows_the_declared_target_between_floor_and_cap() {
    assert_eq!(
        runway_secs(Some(2)),
        FLOOR_RUNWAY_SECS,
        "three of two is under the floor"
    );
    assert_eq!(runway_secs(Some(4)), 12.0);
    assert_eq!(
        runway_secs(Some(12)),
        MAX_RUNWAY_SECS,
        "a 12 s target would ask for 36"
    );
    assert_eq!(runway_secs(Some(66)), MAX_RUNWAY_SECS);
}

#[test]
fn an_unknown_target_uses_the_old_floor() {
    assert_eq!(runway_secs(None), FLOOR_RUNWAY_SECS);
    assert_eq!(runway_secs(Some(0)), FLOOR_RUNWAY_SECS);
}

#[test]
fn endlist_is_always_ready() {
    let short = "#EXTM3U\n#EXTINF:1.0,\nsegment00000.ts\n#EXT-X-ENDLIST\n";
    assert!(playlist_text_ready(short, Some(12)));
    assert!(playlist_text_ready(short, None));
}

#[test]
fn content_seconds_are_what_count_not_segments() {
    let three_short = "#EXTINF:1.5,\na\n#EXTINF:1.5,\nb\n#EXTINF:1.5,\nc\n";
    assert_eq!(playlist_span_secs(three_short), 4.5);
    assert!(
        !playlist_text_ready(three_short, Some(2)),
        "three segments can be too little"
    );
    let two_long = "#EXTINF:4.0,\na\n#EXTINF:3.0,\nb\n";
    assert!(playlist_text_ready(two_long, Some(2)));
    assert!(
        !playlist_text_ready(two_long, Some(4)),
        "a longer declaration wants more runway"
    );
}

#[test]
fn a_missing_playlist_is_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!playlist_ready(&dir.path().join("master.m3u8"), Some(2)));
    std::fs::write(dir.path().join("master.m3u8"), "#EXTINF:7.0,\na\n").unwrap();
    assert!(playlist_ready(&dir.path().join("master.m3u8"), Some(2)));
}
