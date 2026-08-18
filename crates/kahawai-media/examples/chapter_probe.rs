//! What chapters a file declares, by both routes: the sparse container
//! read (`subindex::declare_chapters`) and the Discoverer's TOC.
//! `scripts/kahawai-chapters.sh` drives this against ffprobe.
//!
//! One TSV line per route: `path<TAB>route<TAB>start_ms:title,...`.

fn line(path: &str, route: &str, chapters: &[kahawai_core::media::Chapter]) {
    let listed = chapters
        .iter()
        .map(|c| {
            format!(
                "{}:{}",
                c.start_ms,
                c.title.as_deref().unwrap_or("").replace(',', " ")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    println!("{path}\t{route}\t{listed}");
}

fn main() {
    kahawai_media::init().unwrap();
    for path in std::env::args().skip(1) {
        let p = std::path::Path::new(&path);
        match kahawai_media::subindex::declare_chapters(p) {
            Ok(cs) => line(&path, "sparse", &cs),
            Err(e) => println!("{path}\tsparse\tERROR {e:#}"),
        }
        match kahawai_media::discover(p, std::time::Duration::from_secs(120)) {
            Ok(info) => line(&path, "discover", &info.chapters.unwrap_or_default()),
            Err(e) => println!("{path}\tdiscover\tERROR {e:#}"),
        }
    }
}
