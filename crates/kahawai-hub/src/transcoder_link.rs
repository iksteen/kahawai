//! TranscoderLink: the long-lived control stream from an enrolled
//! transcoder. Identity comes exclusively from the client certificate
//! (§3), same as the mediahost link; the capability report (TC-1) is
//! recorded on the registry for session placement.

use std::sync::Arc;

use kahawai_proto::v1::transcoder_link_server::{TranscoderLink, TranscoderLinkServer};
use kahawai_proto::v1::{hub_to_tc, tc_to_hub, HelloAck, HubToTc, TcToHub};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use kahawai_transport::mtls::peer_identity;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::registry::Registry;
use crate::sessions::Sessions;

pub struct TranscoderLinkService {
    registry: Arc<Registry>,
    sessions: Arc<Sessions>,
}

impl TranscoderLinkService {
    pub fn new(registry: Arc<Registry>, sessions: Arc<Sessions>) -> Self {
        Self { registry, sessions }
    }

    pub fn into_server(self) -> TranscoderLinkServer<Self> {
        TranscoderLinkServer::new(self)
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
            Some(TcToHub { msg: Some(tc_to_hub::Msg::Hello(h)) }) => h,
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
        registry.connected(&module_id, &peer.module_type, &hello.name, &peer.fingerprint);
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
            loop {
                match inbound.message().await {
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
                            sessions.transcode_verdict(&e.session_id, Err(e.error));
                        }
                        tc_to_hub::Msg::SourceRead(r) => {
                            let sessions = sessions.clone();
                            let registry = registry.clone();
                            let module_id = module_id.clone();
                            tokio::spawn(async move {
                                sessions
                                    .source_read(&registry, &module_id, &r.session_id, r.offset, r.len, r.req)
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
            let ended = sessions.end_for_transcoder(&module_id);
            if ended > 0 {
                tracing::warn!(%module_id, ended, "transcoder lost; sessions ended");
            }
            registry.unregister_tc_link(&module_id);
            registry.clear_transcoder_caps(&module_id);
            registry.disconnected(&module_id);
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
