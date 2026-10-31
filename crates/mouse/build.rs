// Makes a bare `cargo build` link on a machine without `libxdo-dev`.
//
// The Linux GUI links `-lxdo`, which needs a `libxdo.so` dev symlink that a stock Kali
// install does not have. The repo carries one at `.buildlibs/libxdo.so`, pointing at the
// runtime library. Without this script the only way to build was to remember to export
// `RUSTFLAGS=-L .xdo-shim` first, and a plain `cargo build` failed with
// "unable to find library -lxdo". Adding the directory here means the link finds the
// shim whatever the caller's environment. When `libxdo-dev` is installed the shim is
// simply unused.
/// Kept equal to the list in `src/glibc_compat.rs`.
const GLIBC_COMPAT: [&str; 6] = ["atan2f", "acosf", "asinf", "acoshf", "sinhf", "coshf"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let linux = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    if !linux {
        return;
    }

    // Older Linux (B47): glibc 2.43 gave six float functions new versions, so a build on
    // Kali would not start on Ubuntu 24.04 / Mint 22 (glibc 2.39). Every call is routed to
    // the long-standing version instead; see src/glibc_compat.rs.
    for f in GLIBC_COMPAT {
        println!("cargo:rustc-link-arg-bins=-Wl,--wrap={f}");
    }

    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/mouse/ -> repo root. `.buildlibs` is the tracked copy, so a fresh clone builds;
    // `.xdo-shim` is the older, untracked one some checkouts still have.
    let root = manifest.join("..").join("..");
    for dir in [".buildlibs", ".xdo-shim"] {
        let shim = root.join(dir);
        if shim.join("libxdo.so").exists() {
            println!("cargo:rustc-link-search=native={}", shim.display());
            return;
        }
    }
}
