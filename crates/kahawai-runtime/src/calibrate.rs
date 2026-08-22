//! OPS-9 remediation: write the decoder demotions this box needs into
//! this box's own config file.
//!
//! The requirement exists because the DTS check already warned, in
//! exactly the words that describe the bug, on a box nobody was reading
//! the output of — and the library silently accumulated 312 wrongly
//! filed channel counts. A check whose remedy is a hand-edited TOML on
//! each satellite is a check that will be ignored.
//!
//! Four rules govern the write, and each one is a way this could
//! quietly destroy an operator's file:
//!
//! 1. **Format-preserving.** Read-modify-serialize would reflow the
//!    document and drop every comment — including the three-line note
//!    beside the existing `demote_decoders` explaining WHY dtsdec is
//!    demoted, which is worth more than the line it annotates. So the
//!    edit goes through `toml_edit`, which keeps the original bytes of
//!    everything it does not touch.
//! 2. **Additive only.** A human's entry is never removed, even when
//!    this box cannot reproduce the reason for it. The measurement runs
//!    on one box at one moment; the human may know something it does
//!    not, and the cost of being wrong is asymmetric — a spurious
//!    demotion loses some speed, a removed one silently files 312 files
//!    wrong again.
//! 3. **Idempotent.** Running twice changes nothing the second time,
//!    which is what makes it safe to put in a provisioning script.
//! 4. **Per box.** A demotion is calibration of one machine's hardware
//!    and drivers, so `--fix` only ever writes the config of the box it
//!    runs on, and only for elements it measured or holds on the
//!    known-bad list.
//!
//! Both `[transcoder]` and `[mediahost]` get the demotion because the
//! two lists exist precisely because a decoder can be right for one job
//! and wrong for the other — and `dtsdec` is wrong for both: it decodes
//! the wrong thing AND, through discovery, files the wrong thing.

use std::path::Path;

use anyhow::{Context, Result};

/// The sections a demotion is written to. Both, always: see the module
/// doc — a decoder that decodes the wrong thing also files it.
const SECTIONS: [&str; 2] = ["transcoder", "mediahost"];

/// One change `--fix` made (or would make).
pub struct Written {
    pub section: &'static str,
    pub element: String,
    pub why: String,
}

/// Add `demote` to `[transcoder]` and `[mediahost] demote_decoders` in
/// `path`, preserving everything else byte for byte. Returns what
/// changed; an empty result means the file already said it.
pub fn apply(path: &Path, demote: &[(String, String)]) -> Result<Vec<Written>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {} — fix it by hand first", path.display()))?;

    let mut written = Vec::new();
    for section in SECTIONS {
        for (element, why) in demote {
            if add_one(&mut doc, section, element) {
                written.push(Written {
                    section,
                    element: element.clone(),
                    why: why.clone(),
                });
            }
        }
    }
    if written.is_empty() {
        return Ok(written);
    }
    // Write through a temp file in the same directory: a half-written
    // config is a box that will not start, and this runs on satellites
    // reachable only over ssh.
    let tmp = path.with_extension("toml.kahawai-new");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(written)
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
        // Both sections, because a decoder that decodes wrong also files
        // wrong — [mediahost] did not exist and had to be created.
        assert_eq!(
            out.len(),
            2,
            "expected one write per section, got {}",
            out.len()
        );
        assert!(
            text.contains("[mediahost]"),
            "mediahost section not created:\n{text}"
        );
    }

    /// Safe in a provisioning script, which is the difference between a
    /// remedy people run and one they read about.
    #[test]
    fn a_second_run_changes_nothing() {
        let (_d, path) = write("[transcoder]\ndemote_decoders = []\n");
        let demote = vec![("dtsdec".into(), "core only".into())];
        assert_eq!(apply(&path, &demote).unwrap().len(), 2);
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

    /// A config we cannot parse is a config we must not overwrite.
    #[test]
    fn a_broken_config_is_left_alone() {
        let (_d, path) = write("[transcoder\nthis is not toml");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(apply(&path, &[("dtsdec".into(), "x".into())]).is_err());
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }
}
