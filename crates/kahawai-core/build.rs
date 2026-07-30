// Build stamp for the AR-7 Hello: which commit a satellite binary was
// built from, answerable from the hub's log. Today's alternative was an
// ssh (and a smartcard) per box per question.
use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    // Date, not time: the stamp answers "which build?", and a
    // minute-resolution timestamp would rebuild this crate on every
    // `cargo build` for no added identity.
    let date = git(&["log", "-1", "--format=%cs", "HEAD"]).unwrap_or_default();
    let stamp = format!("{hash}{} {date}", if dirty { "+dirty" } else { "" });
    println!("cargo:rustc-env=KAHAWAI_BUILD={}", stamp.trim());
    // Re-stamp when the checked-out commit moves. .git/HEAD covers branch
    // switches; the ref file covers commits on the same branch.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=../../.git/{head}");
    }
}
