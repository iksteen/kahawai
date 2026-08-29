#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::sync::{Arc, Barrier};

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn heavy_job_boundary_returns_free_arenas_to_linux() {
    const THREADS: usize = 8;
    const CHUNKS: usize = 256;
    const CHUNK_BYTES: usize = 64 * 1024;

    let baseline = rss_kib();
    let allocated = Arc::new(Barrier::new(THREADS + 1));
    let release = Arc::new(Barrier::new(THREADS + 1));
    let mut workers = Vec::new();
    for byte in 1..=THREADS {
        let allocated = allocated.clone();
        let release = release.clone();
        workers.push(std::thread::spawn(move || {
            let memory = (0..CHUNKS)
                .map(|_| vec![byte as u8; CHUNK_BYTES])
                .collect::<Vec<_>>();
            std::hint::black_box(&memory);
            allocated.wait();
            release.wait();
            drop(memory);
        }));
    }

    allocated.wait();
    let peak = rss_kib();
    release.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    kahawai_mediahost::release_background_memory("allocator regression");
    let after = rss_kib();

    assert!(
        peak >= baseline + 96 * 1024,
        "fixture did not commit enough memory: baseline={baseline} KiB peak={peak} KiB"
    );
    assert!(
        after <= baseline + 32 * 1024,
        "free arenas stayed resident: baseline={baseline} KiB peak={peak} KiB after={after} KiB"
    );
}
