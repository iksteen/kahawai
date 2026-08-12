// rust_embed bakes web/dist into the binary but cannot tell cargo which
// files it read (proc-macro path tracking is nightly-only), so a rebuilt
// web/ left an unchanged Rust crate — and the hub kept serving the previous
// bundle with no sign that it was stale.
//
// So cargo builds the bundle rather than merely watching it: `cargo build`
// cannot embed a stale app, and neither can anything downstream of it, which
// is every restart script and every deploy. The npm build is a couple of
// seconds against a Rust build's minutes.
use std::path::Path;
use std::process::Command;

fn main() {
    let web = Path::new("../../web");

    // The bundle's INPUTS. web/dist is this script's output now: watching it
    // would arm a rebuild trigger with the script's own work.
    for input in [
        "src",
        "public",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
    ] {
        let path = web.join(input);
        // A missing path is "always dirty" to cargo, which would rebuild the
        // hub on every invocation.
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // A checkout with no node_modules is not a web-building environment, and
    // npm being on PATH does not make it one: `npm run build` is `tsc -b`,
    // which exits 127 on a missing tsc, and a non-zero exit is the panic below.
    // ci.yml's cargo jobs never install web dependencies and the ubuntu runner
    // image ships npm regardless, so the panic would fail every clippy and
    // check job. Those builds embed the tracked web/dist, exactly as they did
    // before this script existed.
    let modules = web.join("node_modules");
    if !modules.exists() {
        println!("cargo:warning=web/node_modules absent; embedding the checked-in web/dist");
        return;
    }

    // Present is not the same as current. package-lock.json is a rerun trigger
    // above, so the build that follows a dependency change is exactly the one
    // most likely to be built against the previous install: switching to a
    // branch that adds a dependency gives `tsc -b` an unresolvable import and
    // fails a RUST build with a TypeScript error, and a bump that still
    // type-checks quietly embeds a bundle nobody can reproduce. npm writes
    // .package-lock.json at install time, so the two mtimes answer it.
    let newer_than = |a: &Path, b: &Path| match (a.metadata(), b.metadata()) {
        (Ok(a), Ok(b)) => match (a.modified(), b.modified()) {
            (Ok(a), Ok(b)) => a > b,
            _ => false,
        },
        // No .package-lock.json means an install this script cannot reason
        // about (a hand-made node_modules, a pnpm store); leave it alone.
        _ => false,
    };
    if newer_than(
        &web.join("package-lock.json"),
        &modules.join(".package-lock.json"),
    ) {
        println!("cargo:warning=web/package-lock.json is newer than the install; running npm ci");
        match Command::new("npm").arg("ci").current_dir(web).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => panic!(
                "npm ci failed ({})\n{}{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ),
            Err(e) => panic!("npm ci could not be run: {e}"),
        }
    }

    // Captured rather than inherited: this script's stdout is the pipe cargo
    // reads directives from, and vite's progress does not belong in it. On
    // failure it is the only account of what went wrong, so print both halves.
    // `npm run build` is `tsc -b && vite build`, so a type error stops the
    // Rust build too.
    match Command::new("npm")
        .args(["run", "build"])
        .current_dir(web)
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => panic!(
            "the web bundle failed to build ({})\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ),
        // A machine with no node still builds the hub: web/dist is tracked, so
        // a checkout carries a real bundle rather than nothing. Cross-compile
        // hosts and deploy targets need no toolchain they would never use.
        Err(e) => {
            println!("cargo:warning=npm not runnable ({e}); embedding the checked-in web/dist")
        }
    }
}
