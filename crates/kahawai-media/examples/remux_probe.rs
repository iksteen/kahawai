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
        audio: StreamMode::Copy,
        audio_track,
        video_track: 0,
        video_kbps: None,
        max_height: None,
        tone_map: false,
        deinterlace: false,
        burn_subtitle: None,
        burn_ass: None,
        max_channels: None,
        video_codec: VideoTarget::H264,
        audio_codec: AudioTarget::Aac,
        segment_format: std::env::var("PROBE_TS")
            .map(|_| SegmentFormat::Ts)
            .unwrap_or(SegmentFormat::Fmp4),
    };
    let job = kahawai_media::remux::start(&out, plan, Box::new(FileSource::open(&input)?))?;
    while !job.finished() {
        if let Some(e) = job.failed() {
            anyhow::bail!("remux failed: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("done");
    Ok(())
}
