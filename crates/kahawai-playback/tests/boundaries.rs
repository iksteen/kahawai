//! The dependency rule, as a tripwire: this crate is linked into the
//! satellite daemons, so nothing only the hub needs may enter its
//! manifest. `scripts/kahawai-playback.sh lean` checks the resolved graph;
//! this catches the manifest edit before that runs.

#[test]
fn the_manifest_names_no_hub_only_dependency() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let forbidden = [
        "kahawai-hub",
        "kahawai-mediadb",
        "kahawai-sqlite",
        "kahawai-mediahost",
        "kahawai-transcoder",
        "kahawai-runtime",
        "sqlx",
        "axum",
        "leptess",
        "reqwest",
        "utoipa",
    ];
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let name = line.split(['=', ' ']).next().unwrap_or_default();
        assert!(
            !forbidden.contains(&name),
            "{name} would drag hub-only code into every satellite"
        );
    }
}
