pub mod v1 {
    // prost decides the shape of these enums, so the size-difference
    // lint has nobody to talk to here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Hub accepts current and previous
/// minor (OPS-7).
pub const PROTOCOL_MAJOR: u32 = 2;
pub const PROTOCOL_MINOR: u32 = 4; // 4: additive exact-root source identity
// 2: Hello.build stamp; SessionReady facts (AR-13)
