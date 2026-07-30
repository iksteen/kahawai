//! Session facts (AR-13 honest degradation): things the pipeline learns
//! while prerolling that the negotiation could not know — the encoder
//! folded 7.1 to 5.1, a tier quietly changed shape. The negotiation's
//! verdict is a plan; these are what actually happened, and the 7.1 bug
//! proved the difference ships to users ("the file is corrupt") when
//! nothing carries it upward.
//!
//! The channel is a JSONL file in the session's run directory, written
//! by pipeline callbacks and read by the supervisor — transcoder or
//! hub, they run the same worker — when the session goes ready. A file
//! rather than process state, because the transcoder can run workers
//! in-process and concurrently: anything global would let one session's
//! facts land in another's verdict.

use std::io::Write;
use std::path::Path;

/// One fact: which verdict it amends ("audio" | "video") and a short
/// human phrase in the decision's terms ("7.1 → 5.1").
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Fact {
    pub kind: String,
    pub detail: String,
}

/// Report one fact into `dir`. Append-only, crash-tolerant: a fact is
/// one line, written before the pipeline can die of whatever prompted
/// it. Never fails — a fact is worth logging, not aborting a session.
pub fn report(dir: &Path, kind: &str, detail: impl Into<String>) {
    let fact = Fact {
        kind: kind.into(),
        detail: detail.into(),
    };
    tracing::info!(kind = %fact.kind, detail = %fact.detail, "session fact");
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("facts.jsonl"))
    else {
        return;
    };
    if let Ok(line) = serde_json::to_string(&fact) {
        let _ = writeln!(f, "{line}");
    }
}

/// Everything the session's worker reported, in order. Missing file =
/// no facts (the common case), never an error.
pub fn read(dir: &Path) -> Vec<Fact> {
    let Ok(body) = std::fs::read_to_string(dir.join("facts.jsonl")) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Vec::new());
        report(dir.path(), "audio", "7.1 → 5.1");
        report(dir.path(), "video", "tone-map fell back");
        let facts = read(dir.path());
        assert_eq!(facts.len(), 2);
        assert_eq!(
            facts[0],
            Fact {
                kind: "audio".into(),
                detail: "7.1 → 5.1".into()
            }
        );
        assert_eq!(facts[1].kind, "video");
    }
}
