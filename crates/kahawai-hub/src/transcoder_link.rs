//! TranscoderLink: the long-lived control stream from an enrolled
//! transcoder. Identity comes exclusively from the client certificate
//! (§3), same as the mediahost link; the capability report (TC-1) is
//! recorded on the registry for session placement.

use std::sync::Arc;

use kahawai_proto::v1::transcoder_link_server::{TranscoderLink, TranscoderLinkServer};
use kahawai_proto::v1::{HelloAck, HubToTc, Ping, TcToHub, hub_to_tc, tc_to_hub};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use kahawai_transport::mtls::peer_identity;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::registry::Registry;
use crate::sessions::Sessions;

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct TranscoderLinkService {
    registry: Arc<Registry>,
    sessions: Arc<Sessions>,
}

impl TranscoderLinkService {
    pub fn new(registry: Arc<Registry>, sessions: Arc<Sessions>) -> Self {
        Self { registry, sessions }
    }

    pub fn into_server(self) -> TranscoderLinkServer<Self> {
        // HUB-32b: display sets ride with StartSession and reach
        // megabytes; tonic's 4 MB default would drop the link.
        TranscoderLinkServer::new(self)
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024)
    }
}

#[tonic::async_trait]
impl TranscoderLink for TranscoderLinkService {
    type LinkStream = ReceiverStream<Result<HubToTc, Status>>;

    async fn link(
        &self,
        request: Request<Streaming<TcToHub>>,
    ) -> Result<Response<Self::LinkStream>, Status> {
        let peer = peer_identity(&request)
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        if peer.module_type != "transcoder" {
            return Err(Status::permission_denied("not a transcoder certificate"));
        }

        let mut inbound = request.into_inner();
        let hello = match inbound.message().await? {
            Some(TcToHub {
                msg: Some(tc_to_hub::Msg::Hello(h)),
            }) => h,
            _ => return Err(Status::failed_precondition("first message must be Hello")),
        };
        if hello.protocol_major != PROTOCOL_MAJOR {
            return Err(Status::failed_precondition(format!(
                "incompatible protocol {}.{} (hub speaks {}.{})",
                hello.protocol_major, hello.protocol_minor, PROTOCOL_MAJOR, PROTOCOL_MINOR
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let registry = self.registry.clone();
        let sessions = self.sessions.clone();
        let module_id = peer.module_id.clone();
        registry.connected(
            &module_id,
            &peer.module_type,
            &hello.name,
            &peer.fingerprint,
        );
        if let Err(e) = registry.settle_renewal(&module_id, &peer.fingerprint).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "renewal settlement failed");
        }
        registry.register_tc_link(&module_id, tx.clone());

        tokio::spawn(async move {
            let ack = HubToTc {
                msg: Some(hub_to_tc::Msg::HelloAck(HelloAck {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                })),
            };
            if tx.send(Ok(ack)).await.is_err() {
                registry.unregister_tc_link(&module_id);
                registry.disconnected(&module_id);
                return;
            }
            // Heartbeats arrive every 10 s; three missed = dead link.
            // (A vanished peer does not always surface as a stream error
            // — the byte-plane keepalive is ours to enforce.)
            // We also ping: a napped macOS transcoder's own ticker stalls,
            // but it answers inbound traffic promptly.
            let ping_tx = tx.clone();
            let pinger = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
                loop {
                    tick.tick().await;
                    let ping = HubToTc {
                        msg: Some(hub_to_tc::Msg::Ping(Ping {})),
                    };
                    if ping_tx.send(Ok(ping)).await.is_err() {
                        return;
                    }
                }
            });
            let _abort_pinger = AbortOnDrop(pinger);
            loop {
                let msg =
                    tokio::time::timeout(std::time::Duration::from_secs(35), inbound.message())
                        .await;
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::warn!(%module_id, "no heartbeat in 35s; declaring link dead");
                        break;
                    }
                };
                match msg {
                    Ok(Some(TcToHub { msg: Some(msg) })) => match msg {
                        tc_to_hub::Msg::Capabilities(caps) => {
                            tracing::info!(
                                %module_id,
                                encoders = ?caps.encoders.iter()
                                    .map(|e| format!("{}:{}", e.codec, e.element))
                                    .collect::<Vec<_>>(),
                                max_sessions = caps.max_sessions,
                                "transcoder registered"
                            );
                            registry.set_transcoder_caps(&module_id, &caps);
                        }
                        tc_to_hub::Msg::SessionReady(r) => {
                            sessions.transcode_verdict(&r.session_id, Ok(()));
                        }
                        tc_to_hub::Msg::SessionError(e) => {
                            // Pre-ready: fail the pending start. Post-ready
                            // (worker died mid-session): reschedule (AR-6).
                            if !sessions.transcode_verdict(&e.session_id, Err(e.error.clone())) {
                                let sessions = sessions.clone();
                                let registry = registry.clone();
                                tokio::spawn(async move {
                                    match sessions.reschedule(&registry, &e.session_id).await {
                                        Ok(tc) => tracing::info!(
                                            session = %e.session_id, to = %tc,
                                            "mid-session failure rescheduled"
                                        ),
                                        Err(err) => {
                                            tracing::warn!(
                                                session = %e.session_id,
                                                error = format!("{err:#}"),
                                                "mid-session reschedule failed; ending"
                                            );
                                            sessions.end(&e.session_id);
                                        }
                                    }
                                });
                            }
                        }
                        tc_to_hub::Msg::SourceRead(r) => {
                            let sessions = sessions.clone();
                            let registry = registry.clone();
                            let module_id = module_id.clone();
                            tokio::spawn(async move {
                                sessions
                                    .source_read(
                                        &registry,
                                        &module_id,
                                        &r.session_id,
                                        r.offset,
                                        r.len,
                                        r.req,
                                        r.part,
                                    )
                                    .await;
                            });
                        }
                        tc_to_hub::Msg::ArtifactData(d) => sessions.artifact_chunk(d),
                        tc_to_hub::Msg::Hello(_) | tc_to_hub::Msg::Heartbeat(_) => {}
                    },
                    Ok(Some(TcToHub { msg: None })) => {} // newer kind (OPS-7)
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(%module_id, error = %e, "link stream error");
                        break;
                    }
                }
            }
            // Unregister FIRST so rescheduling never re-picks the dead box.
            registry.unregister_tc_link(&module_id);
            registry.clear_transcoder_caps(&module_id);
            registry.disconnected(&module_id);
            let (moved, ended) = sessions
                .reschedule_for_transcoder(&registry, &module_id)
                .await;
            if moved + ended > 0 {
                tracing::warn!(%module_id, moved, ended, "transcoder lost; sessions rescheduled/ended");
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
