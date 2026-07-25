pub mod v1 {
    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Hub accepts current and previous
/// minor (OPS-7).
pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 1; // 1: attachments worklist/declarations (MH-4 backfill)
