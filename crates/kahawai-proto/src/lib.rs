pub mod v1 {
    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Hub accepts current and previous
/// minor (OPS-7).
pub const PROTOCOL_MAJOR: u32 = 2;
pub const PROTOCOL_MINOR: u32 = 3; // 3: keyframe-interval worklist + report
// 2: Hello.build stamp; SessionReady facts (AR-13)
