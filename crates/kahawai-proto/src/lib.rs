pub mod v1 {
    // prost decides the shape of these enums, so the size-difference
    // lint has nobody to talk to here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("kahawai.v1");
}

/// Protocol 6 requires two independent source grants per dispatched part.
pub const PROTOCOL_MAJOR: u32 = 6;
pub const PROTOCOL_MINOR: u32 = 0; // Informational only; no minor negotiation.
pub const SEGMENT_COMPARISON_INSUFFICIENT: &str = "fewer than two readable episodes remain";

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

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn protocol_six_baseline() {
        assert_eq!((PROTOCOL_MAJOR, PROTOCOL_MINOR), (6, 0));
    }

    #[test]
    fn absent_measurements_do_not_become_unity_gain() {
        let bytes = v1::StartSession::default().encode_to_vec();
        let decoded = v1::StartSession::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.stereo_gain_db, None);
        assert_eq!(decoded.native_gain_db, None);
        assert_eq!(decoded.loudness_source_channels, None);
    }
}
