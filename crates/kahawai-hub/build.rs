// rust_embed bakes web/dist into the binary but cannot tell cargo which files
// it read (proc-macro path tracking is nightly-only). In a web development
// checkout, build the bundle here so every Rust rebuild embeds current assets.
// Rust-only and satellite checkouts may omit node_modules and build without a
// UI; artifact-producing paths prebuild with pinned Node and set
// KAHAWAI_REQUIRE_WEB=1.
use std::path::Path;
use std::process::Command;

fn main() {
    let web = Path::new("../../web");
    let dist = web.join("dist");
    let index = dist.join("index.html");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KAHAWAI_REQUIRE_WEB");
    println!("cargo:rerun-if-env-changed=KAHAWAI_SKIP_WEB_BUILD");

    // web/dist is output, so watch the bundle's inputs rather than arming the
    // build script with its own writes.
    for input in [
        "src",
        "public",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "vitest.config.ts",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
        // The client is generated from this on every build, so a hub change
        // that moves the API moves the bundle too.
        "openapi.json",
        "orval.config.ts",
    ] {
        let path = web.join(input);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let modules = web.join("node_modules");
    if modules.exists() && std::env::var_os("KAHAWAI_SKIP_WEB_BUILD").is_none() {
        // A changed lockfile with an old install should repair itself before
        // TypeScript sees an incomplete dependency tree.
        let installed_lock = modules.join(".package-lock.json");
        if newer_than(&web.join("package-lock.json"), &installed_lock) {
            run_npm(web, &["ci"], "npm ci");
        }
        run_npm(web, &["run", "build"], "the web bundle");
    }

    // The fresh crate build (CI) or watched source input (development) already
    // caused this script to run. Do not watch dist itself: this script writes
    // it and would otherwise retrigger on every Cargo run.
    if !index.exists() {
        if std::env::var_os("KAHAWAI_REQUIRE_WEB").is_some() {
            panic!(
                "KAHAWAI_REQUIRE_WEB is set but web/dist/index.html is missing; run `npm ci && npm run build` in web/ before building Kahawai"
            );
        }
        println!(
            "cargo:warning=web/node_modules and web/dist absent; building Kahawai without the embedded web UI"
        );
    }
}

fn newer_than(a: &Path, b: &Path) -> bool {
    match (a.metadata(), b.metadata()) {
        (Ok(a), Ok(b)) => match (a.modified(), b.modified()) {
            (Ok(a), Ok(b)) => a > b,
            _ => false,
        },
        _ => false,
    }
}

fn run_npm(web: &Path, args: &[&str], what: &str) {
    match Command::new("npm").args(args).current_dir(web).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => panic!(
            "{what} failed ({})\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ),
        Err(e) => panic!("{what} could not be run: {e}"),
    }
}
