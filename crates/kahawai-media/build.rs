//! Tell the linker where libass is.
//!
//! `assraster.rs` declares `#[link(name = "ass")]`, which names the
//! library but not its location. On Linux that is a default search path
//! and nothing more is needed; Homebrew's prefix is not, so a macOS
//! build fails at the linker with `library 'ass' not found` even with
//! libass installed. pkg-config knows where it is on both.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    match pkg_config::Config::new()
        // The #[link] attribute already names the library; all that is
        // missing is the search path, so do not let pkg-config emit its
        // own link flags on top.
        .cargo_metadata(false)
        .probe("libass")
    {
        Ok(lib) => {
            for path in lib.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
        }
        // Not fatal: on a system where libass sits in the default path,
        // the link succeeds without any help from here.
        Err(e) => println!(
            "cargo:warning=libass not found by pkg-config ({e}); relying on the default library path"
        ),
    }
}
