//! Transcoder module (§6, M3): enroll once, keep a control link to the
//! hub (AR-3: always dials out), and register what this box can encode —
//! every element dry-run-verified at startup (TC-1), so a broken driver
//! is discovered at registration, not mid-session.
//!
pub mod sessions;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::transcoder_link_client::TranscoderLinkClient;
use kahawai_proto::v1::{
    CapabilityReport, EncoderCap, Heartbeat, Hello, TcToHub, hub_to_tc, tc_to_hub,
};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use tokio_stream::wrappers::ReceiverStream;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Enroll (or load identity) and keep the hub link up forever.
/// macOS parks session-less idle processes so hard that even kevent
/// wakeups (timers AND socket readiness) defer for minutes — the link
/// heartbeat dies no matter how it is launched (caffeinate and nice do
/// not help; launchd agents cannot load headless). NSProcessInfo's
/// activity assertion is the documented opt-out.
#[cfg(target_os = "macos")]
fn prevent_app_nap() {
    use std::ffi::c_void;
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    // NSActivityUserInitiated | NSActivityLatencyCritical
    const OPTIONS: u64 = 0x00FF_FFFF | (1 << 20) | 0xFF_0000_0000;
    unsafe {
        type Msg0 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
        type Msg1 =
            unsafe extern "C" fn(*mut c_void, *mut c_void, *const std::ffi::c_char) -> *mut c_void;
        type Msg2 = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *mut c_void) -> *mut c_void;
        let msg0: Msg0 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg1: Msg1 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg2: Msg2 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());

        let pi = msg0(
            objc_getClass(c"NSProcessInfo".as_ptr()),
            sel_registerName(c"processInfo".as_ptr()),
        );
        let reason = msg1(
            objc_getClass(c"NSString".as_ptr()),
            sel_registerName(c"stringWithUTF8String:".as_ptr()),
            c"kahawai transcoder link liveness".as_ptr(),
        );
        let token = msg2(
            pi,
            sel_registerName(c"beginActivityWithOptions:reason:".as_ptr()),
            OPTIONS,
            reason,
        );
        // Held for the life of the process: retain and never release.
        msg0(token, sel_registerName(c"retain".as_ptr()));
    }
    tracing::info!("macOS App Nap disabled (NSProcessInfo activity assertion)");
}

pub async fn run(
    hub_addr: &str,
    state_dir: &Path,
    name: &str,
    max_sessions: u32,
    worker_exe: Option<std::path::PathBuf>,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    prevent_app_nap();
    let mut id =
        kahawai_transport::enroll::ensure_identity(hub_addr, state_dir, "transcoder", name).await?;

    let capabilities = probe_capabilities(max_sessions, state_dir)?;
    // HUB-36: the report is live state, not a constant — the background
    // benchmark republishes it when measured speeds drift, and every
    // reconnect picks up the freshest one.
    let (caps_tx, caps_rx) = tokio::sync::watch::channel(capabilities);
    spawn_benchmark(state_dir, max_sessions, caps_tx);
    let scratch = state_dir.join("sessions");

    loop {
        // SEC-7: renew before (re)connecting when inside the window, and
        // bound the link's lifetime so a long-lived link still renews.
        match kahawai_transport::renew::maybe_renew(hub_addr, state_dir, "transcoder", name).await {
            Ok(Some(renewed)) => id = renewed,
            Ok(None) => {}
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "certificate renewal failed; retrying later"
            ),
        }
        let tls = kahawai_transport::mtls::mtls_client_config(&id)?;
        let renewal_due = kahawai_transport::renew::seconds_until_renewal_due(&id.cert_pem)
            .unwrap_or(i64::MAX)
            .max(3600) as u64;
        tokio::select! {
            r = link_once(hub_addr, tls.clone(), name, caps_rx.clone(), &scratch, &worker_exe) => match r {
                Ok(()) => tracing::warn!("hub closed the link; reconnecting"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed; reconnecting"),
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(renewal_due)) => {
                tracing::info!("certificate renewal due; cycling the link");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Where this box remembers what it measured about itself (HUB-36).
fn bench_cache(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("benchmarks.json")
}

/// TC-1 capability probe: what this machine can verifiably encode, and
/// HUB-36: how fast, from the on-disk benchmark cache. A cache miss
/// reports zeros — "unmeasured", which every consumer reads as "no
/// data, assume sufficient" — and the background re-measure fills them
/// in a minute later. Link-up is never delayed by benchmarking.
fn probe_capabilities(max_sessions: u32, state_dir: &Path) -> Result<CapabilityReport> {
    let bench = kahawai_media::bench::load(&bench_cache(state_dir)).unwrap_or_default();
    let encoders: Vec<EncoderCap> = kahawai_media::remux::encoder_capabilities()
        .into_iter()
        // An element that CRASHED the benchmark is not a capability.
        // The startup dry-run only proves it loads: svtav1enc passes at
        // 320x240 on the J5005 and segfaults at 1080p, so without this
        // the box advertises av1 and takes a real session down with it.
        .filter(|(codec, element, _)| {
            let dead = bench.crashed(element);
            if dead {
                tracing::warn!(
                    codec,
                    element,
                    "not advertising: this element crashed the benchmark, so it \
                     cannot be trusted with a session"
                );
            }
            !dead
        })
        .map(|(codec, element, hardware)| {
            let s = bench.encoders.get(element).copied().unwrap_or_default();
            tracing::info!(
                codec,
                element,
                hardware,
                at_1080 = s.s1080,
                at_2160 = s.s2160,
                "encoder verified"
            );
            EncoderCap {
                codec: codec.into(),
                element: element.into(),
                hardware,
                speed_1080: s.s1080,
                speed_2160: s.s2160,
            }
        })
        .collect();
    if encoders.is_empty() {
        bail!("no working encoders on this machine — see `kahawai doctor`");
    }
    let decode_caps = kahawai_media::remux::decoder_caps_names();
    tracing::info!(decoders = decode_caps.len(), "decoder inventory");
    let tonemap = kahawai_media::remux::tonemap_available();
    let ass_burn = kahawai_media::remux::ass_burn_available();
    let tm = bench.tonemap.unwrap_or_default();
    tracing::info!(
        tonemap,
        at_1080 = tm.s1080,
        at_2160 = tm.s2160,
        "HDR tone-map segment (HUB-15a)"
    );
    tracing::info!(ass_burn, "ASS burn-in (HUB-32a)");
    Ok(CapabilityReport {
        encoders,
        max_sessions,
        decode_caps,
        tonemap,
        tonemap_speed_1080: tm.s1080,
        tonemap_speed_2160: tm.s2160,
        ass_burn,
    })
}

/// HUB-36 cache-but-verify: measure in the background, store, and
/// publish a refreshed report when reality has drifted from what the
/// cache claimed. Runs once per process, after a settle delay so a
/// session started right at boot is not fighting the benchmark for the
/// encoder.
/// ponytail: fixed settle instead of gating on Runner idleness —
/// sessions in the first minute of a satellite's life are rare; gate
/// on idleness if one ever gets starved.
fn spawn_benchmark(
    state_dir: &Path,
    max_sessions: u32,
    tx: tokio::sync::watch::Sender<CapabilityReport>,
) {
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);
    /// Generous: a weak box measuring software AV1 is slow but not
    /// broken. Past this it is presumed wedged and killed.
    const BENCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
    let (path, state_dir) = (bench_cache(state_dir), state_dir.to_path_buf());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        let cached = kahawai_media::bench::load(&path);
        // In a CHILD process: this is GStreamer work and GStreamer work
        // takes processes down. Measured live — svtav1enc on the J5005
        // killed the transcoder outright, silently, mid-benchmark. A
        // dead child costs a measurement; a dead satellite is an
        // outage (§1.1, same reason pipelines are supervised children).
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        // ONE CHILD PER PIECE. A segfault then costs exactly that
        // measurement: silence's svtav1enc dies at 1080p (exit 139) and
        // everything else on the box is still measured, in any order.
        let mut jobs: Vec<Vec<String>> = vec![vec!["--tonemap".into()]];
        jobs.extend(
            kahawai_media::remux::encoder_capabilities()
                .iter()
                .filter(|(c, _, _)| ["h264", "hevc", "av1"].contains(c))
                .map(|(_, el, _)| vec!["--only".into(), (*el).to_string()]),
        );
        for args in jobs {
            let child = tokio::process::Command::new(&exe)
                .arg("benchmark")
                .arg("--cache")
                .arg(&path)
                .args(&args)
                .kill_on_drop(true)
                .status();
            match tokio::time::timeout(BENCH_BUDGET, child).await {
                Ok(Ok(st)) if st.success() => {}
                Ok(Ok(st)) => tracing::warn!(
                    ?args,
                    status = ?st,
                    "benchmark child died; continuing with the rest"
                ),
                Ok(Err(e)) => tracing::warn!(?args, error = %e, "benchmark child failed to run"),
                Err(_) => tracing::warn!(
                    ?args,
                    budget_s = BENCH_BUDGET.as_secs(),
                    "benchmark child exceeded its budget; continuing"
                ),
            }
        }
        let Some(measured) = kahawai_media::bench::load(&path) else {
            tracing::warn!("benchmark child wrote no usable cache");
            return;
        };
        let news = cached
            .as_ref()
            .is_none_or(|c| kahawai_media::bench::drifted(c, &measured));
        if !news {
            tracing::debug!("benchmarks unchanged; cached report stands");
            return;
        }
        match probe_capabilities(max_sessions, &state_dir) {
            Ok(fresh) => {
                tracing::info!("measured speeds changed; refreshing the capability report");
                let _ = tx.send(fresh);
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "re-probe after benchmark failed"),
        }
    });
}

/// One link session: Hello/HelloAck, capability registration, then
/// heartbeats until the stream dies.
pub async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    capabilities: tokio::sync::watch::Receiver<CapabilityReport>,
    scratch: &Path,
    worker_exe: &Option<std::path::PathBuf>,
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    // HUB-32b: a StartSession carries the display sets to burn, which
    // run to megabytes on a feature film — well past tonic's 4 MB
    // default, which would drop the link instead of the session.
    let mut client = TranscoderLinkClient::new(channel)
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024);

    let (tx, rx) = tokio::sync::mpsc::channel::<TcToHub>(16);
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Hello(Hello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            name: name.to_string(),
            build: kahawai_core::build_stamp().into(),
        })),
    })
    .await
    .ok();

    let mut inbound = client
        .link(ReceiverStream::new(rx))
        .await
        .context("opening link")?
        .into_inner();

    match inbound.message().await.context("awaiting HelloAck")? {
        Some(m) => match m.msg {
            Some(hub_to_tc::Msg::HelloAck(ack)) => {
                tracing::info!(
                    hub_protocol = format!("{}.{}", ack.protocol_major, ack.protocol_minor),
                    "link established"
                );
            }
            _ => bail!("hub did not open with HelloAck"),
        },
        None => bail!("hub closed the link before HelloAck"),
    }

    let mut capabilities = capabilities;
    // Clone out of the watch BEFORE awaiting: the borrow guard is not
    // Send, and holding it across the send would poison the future.
    let current = {
        capabilities.mark_unchanged();
        capabilities.borrow().clone()
    };
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Capabilities(current)),
    })
    .await
    .context("link closed before capability report")?;

    let runner = sessions::Runner::new(scratch.to_path_buf(), worker_exe.clone(), tx.clone());
    let result = link_loop(&tx, &mut inbound, &runner, &mut capabilities).await;
    runner.end_all().await;
    result
}

async fn link_loop(
    tx: &tokio::sync::mpsc::Sender<TcToHub>,
    inbound: &mut tonic::Streaming<kahawai_proto::v1::HubToTc>,
    runner: &std::sync::Arc<sessions::Runner>,
    capabilities: &mut tokio::sync::watch::Receiver<CapabilityReport>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            // HUB-36: the background benchmark measured something the
            // cache did not know. The hub re-applies every report it
            // receives, so re-sending IS the refresh.
            Ok(()) = capabilities.changed() => {
                let fresh = capabilities.borrow_and_update().clone();
                if tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Capabilities(fresh)) })
                    .await
                    .is_err()
                {
                    bail!("link sender closed");
                }
            }
            _ = ticker.tick() => {
                if tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    bail!("link sender closed");
                }
                // HUB-36: rides the existing tick, and only when there
                // is something to say. Sent AFTER the heartbeat so a
                // link that is dying still gets its keepalive first.
                if let Some(report) = runner.take_pace_report()
                    && tx.send(TcToHub { msg: Some(tc_to_hub::Msg::PaceReport(report)) })
                        .await
                        .is_err()
                {
                    bail!("link sender closed");
                }
            }
            msg = inbound.message() => {
                match msg {
                    Ok(Some(m)) => match m.msg {
                        Some(hub_to_tc::Msg::StartSession(s)) => {
                            let runner = runner.clone();
                            tokio::spawn(async move {
                                runner.start(s.session_id, s.size, &s.video, &s.audio, s.audio_track, s.video_track, s.start_ms, &s.sink, s.tail_sizes, (s.video_kbps, s.max_height, s.max_channels, s.tone_map, s.burn_subtitle), (s.video_codec, s.audio_codec, s.container), s.burn_sets).await;
                            });
                        }
                        // Inline, not spawned: EndSession→StartSession
                        // ordering on the link is the seek-restart
                        // contract — a spawned end can outrun the new
                        // run's registration and kill it.
                        Some(hub_to_tc::Msg::EndSession(e)) => runner.end(&e.session_id).await,
                        Some(hub_to_tc::Msg::SourceData(d)) => {
                            runner.source_data(d.req, d.data);
                        }
                        Some(hub_to_tc::Msg::ViewerPosition(v)) => {
                            runner.viewer_position(&v.session_id, v.position_ms);
                        }
                        // OPS-10: diagnostics for a session still running.
                        // Answered inline — reading a run dir is a few
                        // files and the reply is ~27 KB, so a spawn would
                        // only risk racing the session's own teardown.
                        Some(hub_to_tc::Msg::CollectLogs(c)) => {
                            if let Some(body) = runner.collect_logs(&c.session_id)
                                && tx
                                    .send(TcToHub {
                                        msg: Some(tc_to_hub::Msg::SessionLogs(
                                            kahawai_proto::v1::SessionLogs {
                                                session_id: c.session_id,
                                                body,
                                            },
                                        )),
                                    })
                                    .await
                                    .is_err()
                            {
                                bail!("link sender closed");
                            }
                        }
                        // Reactive liveness: our own ticker stalls under
                        // macOS App Nap, but inbound traffic wakes us.
                        Some(hub_to_tc::Msg::Ping(_)) => {
                            if tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Heartbeat(Heartbeat {})) })
                                .await
                                .is_err()
                            {
                                bail!("link sender closed");
                            }
                        }
                        Some(hub_to_tc::Msg::FetchArtifact(f)) => {
                            let runner = runner.clone();
                            tokio::spawn(async move {
                                runner.fetch_artifact(&f.session_id, &f.name).await;
                            });
                        }
                        _ => {} // HelloAck handled above; newer kinds (OPS-7)
                    },
                    Ok(None) => return Ok(()),
                    Err(e) => bail!("link stream error: {e}"),
                }
            }
        }
    }
}
