//! Run the production remux graph over a local file, for diagnosing what
//! the hub's own pipeline does to a stream that survives a plain
//! gst-launch chain. `remux_probe <in.mkv> <out_dir> [audio_track]`.

use kahawai_media::remux::{
    AudioTarget, FileSource, RemuxPlan, SegmentFormat, StreamMode, VideoTarget,
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = std::path::PathBuf::from(args.next().expect("input file"));
    let out = std::path::PathBuf::from(args.next().expect("out dir"));
    let audio_track: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(0);
    std::fs::create_dir_all(&out)?;

    let plan = RemuxPlan {
        video: StreamMode::Copy,
        // The hub's real remux sessions encode audio to AAC whenever the
        // client cannot take the source track, and a probe that copies
        // instead is testing a different pipeline. Copying E-AC-3 into TS
        // hangs here with no error at all, which cost an hour: PROBE_AAC=1
        // reproduces what a session actually builds.
        audio: if std::env::var("PROBE_AAC").is_ok() {
            StreamMode::Encode
        } else {
            StreamMode::Copy
        },
        audio_track,
        video_track: 0,
        video_kbps: None,
        max_height: None,
        tone_map: false,
        deinterlace: false,
        burn_subtitle: None,
        burn_ass: None,
        max_channels: None,
        stereo_gain_db: None,
        native_gain_db: None,
        loudness_source_channels: None,
        loudness_gains: [None; kahawai_media::loudness::MAX_LAYOUT_GAINS],
        video_codec: VideoTarget::H264,
        audio_codec: AudioTarget::Aac,
        segment_format: std::env::var("PROBE_TS")
            .map(|_| SegmentFormat::Ts)
            .unwrap_or(SegmentFormat::Fmp4),
    };
    // PROBE_SINK=hlssink2 answers "does the other sink declare the same
    // segment durations?" — hlssink3's EXTINF values run ~10ms long per
    // segment, which is what strands a client's buffer estimate.
    let sink = std::env::var("PROBE_SINK").ok();
    let job = kahawai_media::remux::start_full(
        &out,
        plan,
        Box::new(FileSource::open(&input)?),
        0,
        sink.as_deref(),
    )?;
    while !job.finished() {
        if let Some(e) = job.failed() {
            anyhow::bail!("remux failed: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("done");
    Ok(())
}
