//! One-time conversion of stored discoverer totals to component precision.
//! Operates on JSON to preserve unrelated and unrecognized probe fields. This
//! is deliberately separate from runtime deserialization and negotiation.

use serde_json::Value;

/// Returns whether a fresh probe is required. Compare the JSON before/after to
/// detect metadata changes. Legacy 16 is ambiguous (palette or true 16-bit);
/// padded formats can report 48 for 10, 12 or 16-bit components. Neither is safe
/// to convert arithmetically. Unknown depths remain unknown without a re-probe.
pub fn migrate(media: &mut Value) -> bool {
    let mut reprobe = false;
    if let Some(video) = media.get_mut("video").and_then(Value::as_array_mut) {
        for stream in video {
            let Some(depth) = stream.get_mut("bit_depth") else {
                continue;
            };
            if !depth.is_number() {
                continue;
            }
            *depth = match depth.as_u64() {
                Some(24) => 8.into(),
                Some(30) => 10.into(),
                Some(36) => 12.into(),
                Some(1..=15) => continue,
                _ => {
                    reprobe = true;
                    Value::Null
                }
            };
        }
    }
    reprobe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_only_unambiguous_totals_and_preserves_other_fields() {
        for (before, after, flagged) in [
            (24, Some(8), false),
            (30, Some(10), false),
            (36, Some(12), false),
            (8, Some(8), false),
            (10, Some(10), false),
            (12, Some(12), false),
            (16, None, true),
            (48, None, true),
            (0, None, true),
            (32, None, true),
        ] {
            let mut media = serde_json::json!({"video":[{"bit_depth":before,"future":42}],"audio":[{"bit_depth":24}]});
            assert_eq!(migrate(&mut media), flagged);
            assert_eq!(media["video"][0]["bit_depth"], serde_json::json!(after));
            assert_eq!(media["video"][0]["future"], 42);
            assert_eq!(media["audio"][0]["bit_depth"], 24);
            let once = media.clone();
            assert!(!migrate(&mut media));
            assert_eq!(media, once);
        }
        for mut media in [
            Value::Null,
            serde_json::json!({"video":[{}, {"bit_depth":null}]}),
        ] {
            let old = media.clone();
            assert!(!migrate(&mut media));
            assert_eq!(media, old);
        }
    }
}
