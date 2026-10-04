//! OPS-RDY-4A: an exporter change must explicitly review its label dimensions.
//! Exercise all families, including measured and unmeasured fleet members;
//! library size and viewer activity may change values, never series identity.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use kahawai_hub::metrics::{EncoderSpeed, ModuleHealth, Snapshot, render};

// This policy is deliberately independent of the renderer: adding a family or
// dimension there must fail here until its cardinality/privacy is reviewed.
const LABEL_POLICY: &[(&str, &[&str])] = &[
    ("kahawai_build_info", &["version"]),
    ("kahawai_module_up", &["module", "name", "kind"]),
    ("kahawai_module_disabled", &["module", "name"]),
    (
        "kahawai_module_build_info",
        &["module", "name", "kind", "build"],
    ),
    (
        "kahawai_encoder_speed_realtime",
        &["module", "name", "codec", "element", "hardware", "height"],
    ),
    (
        "kahawai_tonemap_speed_realtime",
        &["module", "name", "height"],
    ),
    ("kahawai_sessions_active", &[]),
    ("kahawai_items", &[]),
    ("kahawai_files", &[]),
    ("kahawai_file_bytes", &[]),
    ("kahawai_subtitle_files", &[]),
    ("kahawai_enrichment_due", &[]),
    ("kahawai_items_unmatched", &[]),
    ("kahawai_anidb_ban_seconds", &[]),
];

type Series = BTreeSet<(String, BTreeMap<String, String>)>;

fn labels(mut text: &str) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    while !text.is_empty() {
        let (key, value) = text.split_once('=').context("label has no value")?;
        // Prometheus: "backslash, double-quote, and line feed must be escaped."
        // https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details
        // Those three quoted-string escapes also work in JSON. Consume one
        // string, not comma-separated tokens: names may contain commas/quotes.
        let mut quoted = serde_json::Deserializer::from_str(value).into_iter::<String>();
        let value = quoted.next().context("missing quoted label value")??;
        ensure!(
            result.insert(key.to_string(), value).is_none(),
            "duplicate label {key}"
        );
        text = &text[key.len() + 1 + quoted.byte_offset()..];
        if !text.is_empty() {
            text = text.strip_prefix(',').context("missing label separator")?;
        }
    }
    Ok(result)
}

fn check_policy(text: &str) -> Result<Series> {
    let policy: BTreeMap<_, BTreeSet<_>> = LABEL_POLICY
        .iter()
        .map(|(family, labels)| (*family, labels.iter().copied().collect()))
        .collect();
    let mut declared = BTreeSet::new();
    let mut series = Series::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        if let Some(header) = line.strip_prefix("# TYPE ") {
            let name = header.split_whitespace().next().context("missing family")?;
            ensure!(policy.contains_key(name), "unreviewed metric family {name}");
            ensure!(declared.insert(name), "duplicate family {name}");
        } else if !line.starts_with('#') {
            let (identity, value) = line.rsplit_once(' ').context("missing sample value")?;
            value.parse::<f64>().context("invalid sample value")?;
            let (name, labels) = match identity.split_once('{') {
                Some((name, block)) => (
                    name,
                    labels(block.strip_suffix('}').context("unclosed labels")?)?,
                ),
                None => (identity, BTreeMap::new()),
            };
            let expected = policy
                .get(name)
                .with_context(|| format!("unreviewed metric family {name}"))?;
            ensure!(declared.contains(name), "undeclared sample {name}");
            ensure!(
                labels.keys().map(String::as_str).collect::<BTreeSet<_>>() == *expected,
                "unreviewed label set for {name}: {:?}",
                labels.keys().collect::<Vec<_>>()
            );
            ensure!(
                series.insert((name.to_string(), labels)),
                "duplicate series for {name}"
            );
        }
    }
    ensure!(
        declared == policy.keys().copied().collect(),
        "metric family coverage changed"
    );
    Ok(series)
}

fn fleet(copies: usize) -> Snapshot {
    let mut modules = Vec::new();
    for n in 0..copies {
        modules.push(ModuleHealth {
            module_id: format!("host-{n}"),
            name: format!("Media, \\\"host {n}\\\""),
            kind: "mediahost".into(),
            connected: true,
            disabled: false,
            build: "fixture-build".into(),
            encoders: Vec::new(),
            tonemap_1080: None,
            tonemap_2160: None,
        });
        modules.push(ModuleHealth {
            module_id: format!("transcoder-{n}"),
            name: format!("Transcoder {n}"),
            kind: "transcoder".into(),
            connected: true,
            disabled: false,
            build: "fixture-build".into(),
            encoders: vec![
                EncoderSpeed {
                    codec: "h264".into(),
                    element: "x264enc".into(),
                    hardware: false,
                    s1080: Some(3.0),
                    s2160: Some(1.0),
                },
                EncoderSpeed {
                    codec: "hevc".into(),
                    element: "vah265enc".into(),
                    hardware: true,
                    s1080: Some(5.0),
                    s2160: Some(2.0),
                },
            ],
            tonemap_1080: Some(2.0),
            tonemap_2160: Some(0.75),
        });
        modules.push(ModuleHealth {
            module_id: format!("offline-{n}"),
            name: format!("Offline transcoder {n}"),
            kind: "transcoder".into(),
            connected: false,
            disabled: true,
            build: String::new(),
            encoders: vec![EncoderSpeed {
                codec: "h264".into(),
                element: "x264enc".into(),
                hardware: false,
                s1080: Some(0.0),
                s2160: None,
            }],
            tonemap_1080: None,
            tonemap_2160: Some(0.0),
        });
    }
    Snapshot {
        modules,
        sessions_active: 3,
        items: 10,
        files: 20,
        file_bytes: 1000,
        subtitle_files: 5,
        enrichment_due: 2,
        unmatched_items: 1,
        anidb_banned_secs: 0,
    }
}

#[test]
fn every_metric_family_has_only_fleet_bounded_labels() {
    for copies in [1, 10] {
        let mut snapshot = fleet(copies);
        let series = check_policy(&render(&snapshot)).unwrap();
        // Nine global samples; each three-box fleet contributes nine status/
        // build, five measured encoder and three measured tone-map samples.
        assert_eq!(series.len(), 9 + copies * 17);
        for &(family, _) in LABEL_POLICY {
            assert!(
                series.iter().any(|(name, _)| name == family),
                "{family} was not exercised"
            );
        }
        for (name, labels) in &series {
            if name.ends_with("speed_realtime") {
                assert!(["1080", "2160"].contains(&labels["height"].as_str()));
            }
        }
        snapshot.sessions_active = 100_000;
        snapshot.items = 1_000_000;
        snapshot.files = 10_000_000;
        snapshot.file_bytes = 1_000_000_000_000;
        snapshot.subtitle_files = 10_000_000;
        snapshot.enrichment_due = 1_000_000;
        snapshot.unmatched_items = 500_000;
        snapshot.anidb_banned_secs = 86_400;
        assert_eq!(
            check_policy(&render(&snapshot)).unwrap(),
            series,
            "catalogue/activity counters changed series identity"
        );
    }
    // The CLI companion can validate an actual HTTP scrape with this same
    // policy, without duplicating the validator in a shell/Python script.
    if let Some(path) = std::env::var_os("KAHAWAI_METRICS_SCRAPE_FILE") {
        let text = std::fs::read_to_string(path).expect("reading captured metrics scrape");
        let series = check_policy(&text).expect("live scrape violates the label policy");
        println!(
            "Captured scrape passed: {} reviewed time series",
            series.len()
        );
    }
}

#[test]
fn label_policy_rejects_sensitive_dimensions_and_unreviewed_families() {
    let text = render(&fleet(1));
    for key in [
        "user",
        "item",
        "session",
        "file",
        "request_id",
        "url",
        "provider_response",
        "version",
    ] {
        let changed = text.replace(
            "kahawai_sessions_active 3",
            &format!("kahawai_sessions_active{{{key}=\"canary\"}} 3"),
        );
        assert!(
            check_policy(&changed).is_err(),
            "accepted unreviewed label {key}"
        );
    }
    let changed = text.replace("kahawai_sessions_active", "kahawai_per_viewer_activity");
    assert!(
        check_policy(&changed).is_err(),
        "accepted an unreviewed metric family"
    );
}
