//! Transcoder module (§6, M3): enroll once, keep a control link to the
//! hub (AR-3: always dials out), and register what this box can encode —
//! every element dry-run-verified at startup (TC-1), so a broken driver
//! is discovered at registration, not mid-session.
//!
pub mod sessions;

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::transcoder_link_client::TranscoderLinkClient;
use kahawai_proto::v1::{hub_to_tc, tc_to_hub, CapabilityReport, EncoderCap, Heartbeat, Hello, TcToHub};
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
        type Msg2 =
            unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *mut c_void) -> *mut c_void;
        let msg0: Msg0 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg1: Msg1 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg2: Msg2 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());

        let pi = msg0(objc_getClass(c"NSProcessInfo".as_ptr()), sel_registerName(c"processInfo".as_ptr()));
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

    let capabilities = probe_capabilities(max_sessions)?;
    let scratch = state_dir.join("sessions");

    loop {
        // SEC-7: renew before (re)connecting when inside the window, and
        // bound the link's lifetime so a long-lived link still renews.
        match kahawai_transport::renew::maybe_renew(hub_addr, state_dir, "transcoder", name).await {
            Ok(Some(renewed)) => id = renewed,
            Ok(None) => {}
            Err(e) => tracing::warn!(error = format!("{e:#}"), "certificate renewal failed; retrying later"),
        }
        let tls = kahawai_transport::mtls::mtls_client_config(&id)?;
        let renewal_due = kahawai_transport::renew::seconds_until_renewal_due(&id.cert_pem)
            .unwrap_or(i64::MAX)
            .max(3600) as u64;
        tokio::select! {
            r = link_once(hub_addr, tls.clone(), name, capabilities.clone(), &scratch, &worker_exe) => match r {
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

/// TC-1 capability probe: what this machine can verifiably encode.
fn probe_capabilities(max_sessions: u32) -> Result<CapabilityReport> {
    let encoders: Vec<EncoderCap> = kahawai_media::remux::encoder_capabilities()
        .into_iter()
        .map(|(codec, element, hardware)| {
            tracing::info!(codec, element, hardware, "encoder verified");
            EncoderCap { codec: codec.into(), element: element.into(), hardware }
        })
        .collect();
    if encoders.is_empty() {
        bail!("no working encoders on this machine — see `kahawai doctor`");
    }
    let decode_caps = kahawai_media::remux::decoder_caps_names();
    tracing::info!(decoders = decode_caps.len(), "decoder inventory");
    Ok(CapabilityReport { encoders, max_sessions, decode_caps })
}

/// One link session: Hello/HelloAck, capability registration, then
/// heartbeats until the stream dies.
pub async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    capabilities: CapabilityReport,
    scratch: &Path,
    worker_exe: &Option<std::path::PathBuf>,
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    let mut client = TranscoderLinkClient::new(channel);

    let (tx, rx) = tokio::sync::mpsc::channel::<TcToHub>(16);
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Hello(Hello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            name: name.to_string(),
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

    tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Capabilities(capabilities)) })
        .await
        .context("link closed before capability report")?;

    let runner = sessions::Runner::new(scratch.to_path_buf(), worker_exe.clone(), tx.clone());
    let result = link_loop(&tx, &mut inbound, &runner).await;
    runner.end_all().await;
    result
}

async fn link_loop(
    tx: &tokio::sync::mpsc::Sender<TcToHub>,
    inbound: &mut tonic::Streaming<kahawai_proto::v1::HubToTc>,
    runner: &std::sync::Arc<sessions::Runner>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Heartbeat(Heartbeat {})) })
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
                                runner.start(s.session_id, s.size, &s.video, &s.audio, s.audio_track, s.video_track, s.start_ms, &s.sink).await;
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
