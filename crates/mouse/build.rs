// Makes a bare `cargo build` link on a machine without `libxdo-dev`.
//
// The Linux GUI links `-lxdo`, which needs a `libxdo.so` dev symlink that a stock Kali
// install does not have. The repo carries one at `.buildlibs/libxdo.so`, pointing at the
// runtime library. Without this script the only way to build was to remember to export
// `RUSTFLAGS=-L .xdo-shim` first, and a plain `cargo build` failed with
// "unable to find library -lxdo". Adding the directory here means the link finds the
// shim whatever the caller's environment. When `libxdo-dev` is installed the shim is
// simply unused.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let linux = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    if !linux {
        return;
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
