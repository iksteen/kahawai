// rust_embed bakes web/dist into the binary but cannot tell cargo which
// files it read (proc-macro path tracking is nightly-only), so a rebuilt
// web/ leaves an unchanged Rust crate — and the hub keeps serving the
// previous bundle with no sign that it is stale. Cargo scans a directory
// argument recursively, so this covers every asset.
fn main() {
    println!("cargo:rerun-if-changed=../../web/dist");
}
