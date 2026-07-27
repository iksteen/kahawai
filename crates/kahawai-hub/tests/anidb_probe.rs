//! A single, deliberate AniDB packet — run by hand, never in CI.
//!
//! AniDB bans are extended by contact, so after one there is no way to
//! learn whether it has lapsed except to ask exactly once. This asks
//! once: it reuses the stored session (no AUTH unless the session is
//! gone) and sends one FILE query, which is the smallest useful command.
//! `Anidb::login` refuses outright while a ban is on record, so the
//! probe cannot itself extend one.
//!
//!   cargo test -p kahawai-hub --test anidb_probe -- --ignored --nocapture
//!
//! Reads credentials from the live hub's database and speaks from its
//! data dir, so the session and port are the real ones.

use kahawai_hub::registry::Registry;

#[tokio::test]
#[ignore = "sends a real packet to AniDB; run by hand"]
async fn probe_anidb_once() {
    let data_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join(".local/share/kahawai");
    let db = kahawai_hub::db::open(&data_dir).await.expect("open hub db");
    let registry = Registry::new(db, Default::default());
    let user = registry.get_setting(kahawai_hub::anidb::USER_SETTING).await.unwrap();
    let pass = registry.get_setting(kahawai_hub::anidb::PASS_SETTING).await.unwrap();
    let key = registry.get_setting(kahawai_hub::anidb::APIKEY_SETTING).await.unwrap();
    let (Some(user), Some(pass)) = (user, pass) else {
        eprintln!("PROBE: no anidb credentials configured");
        return;
    };

    let started = std::time::Instant::now();
    let mut client =
        match kahawai_hub::anidb::Anidb::login(&data_dir, &user, &pass, key.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("PROBE: login refused after {:?}: {e:#}", started.elapsed());
                return;
            }
        };
    eprintln!("PROBE: session established in {:?}", started.elapsed());

    // One FILE query for a file we already have an answer for, so a hit
    // proves the round trip without asking AniDB anything new.
    let (size, ed2k) = (787_362_756u64, "83e2e65f3e9c5e1fd0cb8a65005d76ee");
    match client.file_by_ed2k(size, ed2k).await {
        Ok(Some(hit)) => eprintln!("PROBE: OK — AniDB answered: {hit:?}"),
        Ok(None) => eprintln!("PROBE: OK — AniDB answered, no file record for that hash"),
        Err(e) => eprintln!("PROBE: FAILED — {e:#}"),
    }
    client.finish().await;
    eprintln!("PROBE: one packet sent, total {:?}", started.elapsed());
}
