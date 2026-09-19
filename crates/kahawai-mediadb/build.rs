fn main() {
    // Stable Rust cannot discover a newly added file through migrate! alone.
    println!("cargo:rerun-if-changed=migrations");
}
