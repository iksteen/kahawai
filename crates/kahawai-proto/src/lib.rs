pub mod v1 {
    // prost decides the shape of these enums, so the size-difference
    // lint has nobody to talk to here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Protocol 3 deliberately rejects all
/// protocol 2 satellites: exact-root identity has one authoritative wire shape.
pub const PROTOCOL_MAJOR: u32 = 3;
pub const PROTOCOL_MINOR: u32 = 4;

impl v1::SourcePath {
    pub fn new(root_token: impl Into<String>, path_rel: impl Into<String>) -> Self {
        Self {
            root_token: root_token.into(),
            path_rel: path_rel.into(),
        }
    }
}

impl v1::CollectionRoot {
    pub fn new(root_token: impl Into<String>, normalized_path: impl Into<String>) -> Self {
        Self {
            root_token: root_token.into(),
            normalized_path: normalized_path.into(),
        }
    }
}
