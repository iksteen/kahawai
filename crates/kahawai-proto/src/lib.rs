pub mod v1 {
    // prost decides the shape of these enums, so the size-difference
    // lint has nobody to talk to here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("kahawai.v1");
}

/// Inter-module protocol version (AR-7). Protocol 3 deliberately rejects all
/// protocol 2 satellites: exact-root identity has one authoritative wire shape.
pub const PROTOCOL_MAJOR: u32 = 3;
pub const PROTOCOL_MINOR: u32 = 6;
pub const SEGMENT_COMPARISON_INSUFFICIENT: &str = "fewer than two readable episodes remain";

/// Additive protocol features whose minimum minor is part of the wire contract.
/// Keep version knowledge here rather than scattering numeric comparisons
/// through planners, registries, and satellites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFeature {
    SegmentDetection,
    AudioLoudnessScalars,
    ExactAudioLoudnessGains,
    AudioLoudnessAnalysis,
    RetryableSegmentResults,
}

impl ProtocolFeature {
    pub const fn minimum_minor(self) -> u32 {
        match self {
            Self::SegmentDetection => 1,
            Self::AudioLoudnessScalars => 4,
            Self::ExactAudioLoudnessGains | Self::AudioLoudnessAnalysis => 5,
            Self::RetryableSegmentResults => 6,
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
    fn feature_thresholds_have_one_authority() {
        let minor4 = ProtocolFeatures::new(4);
        assert!(minor4.supports(ProtocolFeature::AudioLoudnessScalars));
        assert!(!minor4.supports(ProtocolFeature::ExactAudioLoudnessGains));
        assert!(ProtocolFeatures::new(5).supports(ProtocolFeature::ExactAudioLoudnessGains));
        assert!(ProtocolFeatures::current().supports(ProtocolFeature::RetryableSegmentResults));
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
