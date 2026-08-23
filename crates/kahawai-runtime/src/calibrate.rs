//! OPS-9 remediation: write this box's decoder demotions into the global
//! GStreamer policy without rewriting the operator's TOML.
//!
//! Four rules govern the write:
//! 1. **Format-preserving.** `toml_edit` keeps comments and layout.
//! 2. **Additive only.** A human's entry is never removed.
//! 3. **Idempotent.** A second run changes nothing.
//! 4. **Backward-compatible.** Creating `[gstreamer]` first seeds it from the
//!    merged legacy mediahost/transcoder lists. An explicit global section is
//!    authoritative, so omitting that seed would silently discard old policy.
//!
//! Decoder ranks are process-global. A single section now describes that
//! reality; the role-local lists remain input only when `[gstreamer]` is absent.

use std::path::Path;

use anyhow::{Context, Result};

const GLOBAL: &str = "gstreamer";
const LEGACY_SECTIONS: [&str; 2] = ["mediahost", "transcoder"];

/// One change `--fix` made (or would make).
pub struct Written {
    pub section: &'static str,
    pub element: String,
    pub why: String,
}

/// Add measured demotions to `[gstreamer]`, seeding a newly-created section
/// from both legacy role lists. Returns what changed.
pub fn apply(path: &Path, demote: &[(String, String)]) -> Result<Vec<Written>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {} — fix it by hand first", path.display()))?;
    let inherited = if doc.contains_key(GLOBAL) {
        Vec::new()
    } else {
        legacy_demotions(&doc)
    };

    let mut written = Vec::new();
    for element in inherited {
        if add_one(&mut doc, GLOBAL, &element) {
            written.push(Written {
                section: GLOBAL,
                element,
                why: "inherited from legacy role policy".into(),
            });
        }
    }
    for (element, why) in demote {
        if add_one(&mut doc, GLOBAL, element) {
            written.push(Written {
                section: GLOBAL,
                element: element.clone(),
                why: why.clone(),
            });
        }
    }
    if written.is_empty() {
        return Ok(written);
    }
    let tmp = path.with_extension("toml.kahawai-new");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(written)
}

fn legacy_demotions(doc: &toml_edit::DocumentMut) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    LEGACY_SECTIONS
        .iter()
        .filter_map(|section| doc.get(section)?.as_table_like())
        .filter_map(|table| table.get("demote_decoders")?.as_array())
        .flat_map(|array| array.iter().filter_map(|value| value.as_str()))
        .filter(|name| seen.insert((*name).to_string()))
        .map(str::to_string)
        .collect()
}

/// True if the element was added — false if it was already there,
/// which is what makes a second run a no-op.
fn add_one(doc: &mut toml_edit::DocumentMut, section: &str, element: &str) -> bool {
    let table = doc
        .entry(section)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(table) = table.as_table_like_mut() else {
        return false; // the operator put something odd here; leave it alone
    };
    let list = table
        .entry("demote_decoders")
        .or_insert_with(|| toml_edit::value(toml_edit::Array::new()));
    let Some(array) = list.as_array_mut() else {
        return false;
    };
    if array.iter().any(|v| v.as_str() == Some(element)) {
        return false;
    }
    array.push(element);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kahawai.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    /// The whole point of `toml_edit` over parse-and-reserialize: an
    /// operator's comments explain WHY a demotion is there, which
    /// outlives the line itself. Losing them to a `--fix` run would
    /// make the tool worse than the hand edit it replaces.
    #[test]
    fn existing_comments_entries_and_formatting_survive() {
        let (_d, path) = write(
            "# top of file\n\
             [hub]\n\
             public_url = \"https://kahawai.example\"\n\
             \n\
             [transcoder]\n\
             # dtsdec only decodes the lossy core — demoted on purpose.\n\
             demote_decoders = [\"dtsdec\"]\n",
        );
        let out = apply(&path, &[("vah265dec".into(), "6 fps vs 121".into())]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert!(
            text.contains("# top of file"),
            "leading comment lost:\n{text}"
        );
        assert!(
            text.contains("# dtsdec only decodes the lossy core"),
            "the reason a human recorded was lost:\n{text}"
        );
        assert!(
            text.contains("public_url = \"https://kahawai.example\""),
            "unrelated key changed"
        );
        assert!(text.contains("\"dtsdec\""), "a human's entry was removed");
        assert!(
            text.contains("\"vah265dec\""),
            "the demotion was not written"
        );
        // The new global section is seeded with the legacy entry before the
        // measured one lands, or its presence would override and lose dtsdec.
        assert_eq!(out.len(), 2);
        assert!(
            text.contains("[gstreamer]"),
            "global section not created:\n{text}"
        );
    }

    /// Safe in a provisioning script, which is the difference between a
    /// remedy people run and one they read about.
    #[test]
    fn a_second_run_changes_nothing() {
        let (_d, path) = write("[transcoder]\ndemote_decoders = []\n");
        let demote = vec![("dtsdec".into(), "core only".into())];
        assert_eq!(apply(&path, &demote).unwrap().len(), 1);
        let after_first = std::fs::read_to_string(&path).unwrap();

        assert!(
            apply(&path, &demote).unwrap().is_empty(),
            "second run reported changes"
        );
        assert_eq!(
            after_first,
            std::fs::read_to_string(&path).unwrap(),
            "second run rewrote the file"
        );
    }

    #[test]
    fn an_explicit_global_section_does_not_inherit_legacy_policy() {
        let (_d, path) = write(
            "[gstreamer]\n\
             demote_decoders = []\n\
             [transcoder]\n\
             demote_decoders = [\"legacydec\"]\n",
        );
        apply(&path, &[("measureddec".into(), "slow".into())]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        let global = doc["gstreamer"]["demote_decoders"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert_eq!(global, ["measureddec"]);
    }

    /// A config we cannot parse is a config we must not overwrite.
    #[test]
    fn a_broken_config_is_left_alone() {
        let (_d, path) = write("[transcoder\nthis is not toml");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(apply(&path, &[("dtsdec".into(), "x".into())]).is_err());
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }
}
