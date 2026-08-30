pub mod v1 {
    // prost decides the shape of these enums, so the size-difference
    // lint has nobody to talk to here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Protocol 4 makes the mediahost's
/// durable local catalogue authoritative and deliberately rejects protocol 3
/// peers, whose hub-owned manifest/worklist contract has the reverse meaning.
pub const PROTOCOL_MAJOR: u32 = 4;
pub const PROTOCOL_MINOR: u32 = 1;
pub const SEGMENT_COMPARISON_INSUFFICIENT: &str = "fewer than two readable episodes remain";

/// Protocol features that may acquire minor-version gates after the 4.0
/// baseline. Every feature inherited at the breaking cutover starts at zero;
/// protocol-3 peers are rejected by the major handshake instead of degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFeature {
    SegmentDetection,
    AudioLoudnessScalars,
    ExactAudioLoudnessGains,
    AudioLoudnessAnalysis,
    RetryableSegmentResults,
    DiscoveryPriorityHints,
}

impl ProtocolFeature {
    pub const fn minimum_minor(self) -> u32 {
        match self {
            Self::DiscoveryPriorityHints => 1,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtocolFeatures {
    minor: u32,
}

impl ProtocolFeatures {
    pub const fn new(minor: u32) -> Self {
        Self { minor }
    }

    pub const fn current() -> Self {
        Self::new(PROTOCOL_MINOR)
    }

    pub const fn supports(self, feature: ProtocolFeature) -> bool {
        self.minor >= feature.minimum_minor()
    }
}

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
    fn protocol_four_one_keeps_inherited_gates_open_and_adds_hints() {
        assert_eq!(PROTOCOL_MINOR, 1);
        for feature in [
            ProtocolFeature::SegmentDetection,
            ProtocolFeature::AudioLoudnessScalars,
            ProtocolFeature::ExactAudioLoudnessGains,
            ProtocolFeature::AudioLoudnessAnalysis,
            ProtocolFeature::RetryableSegmentResults,
        ] {
            assert_eq!(feature.minimum_minor(), 0);
            assert!(ProtocolFeatures::new(0).supports(feature));
        }
        assert!(!ProtocolFeatures::new(0).supports(ProtocolFeature::DiscoveryPriorityHints));
        assert!(ProtocolFeatures::new(1).supports(ProtocolFeature::DiscoveryPriorityHints));
    }

    #[test]
    fn an_old_hubs_absent_gain_fields_do_not_become_unity_gain() {
        let bytes = v1::StartSession::default().encode_to_vec();
        let decoded = v1::StartSession::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.stereo_gain_db, None);
        assert_eq!(decoded.native_gain_db, None);
        assert_eq!(decoded.loudness_source_channels, None);
    }
}
