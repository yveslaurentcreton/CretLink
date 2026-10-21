//! Build script for CretLink Support (ST-M002-001, REQ-F-002).
//!
//! On a Windows *target* it embeds the application manifest that requests
//! administrator elevation (`requestedExecutionLevel level="requireAdministrator"`).
//! Declining the resulting UAC prompt means the process simply never starts
//! elevated, so the agent gains no administrator capability without visible
//! consent (REQ-F-002). For a non-Windows *target* this script is a clean no-op
//! so the whole workspace still builds on every CI host and cross-compiles
//! cleanly; the `embed-manifest` build-dependency is scoped to `cfg(windows)`.
//!
//! Because `embed-manifest` is a `cfg(windows)` build-dependency, only a Windows
//! *host* can embed the manifest. A Windows *target* cross-built from a
//! non-Windows host therefore cannot be given its elevation manifest; rather
//! than silently ship a Windows binary that never requests elevation, this
//! script fails the build loudly in that case (fail-closed). Official Windows
//! artifacts are built on Windows (ADR-0001), where this path never triggers.

fn main() {
    println!("cargo:rerun-if-changed=cretlink-support.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    embed_manifest_for_windows_target();
}

/// Whether the compile *target* is Windows, regardless of the build host.
fn target_is_windows() -> bool {
    std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
}

// The `embed-manifest` build-dependency is present only when the build *host*
// is Windows (Cargo matches `[target.'cfg(windows)'.build-dependencies]`
// against the host for build scripts), so it may only be named under
// `#[cfg(windows)]`. Inside, gate on the compile *target* OS so a Windows host
// cross-compiling to another platform does not try (and fail) to embed a
// Windows manifest.
#[cfg(windows)]
fn embed_manifest_for_windows_target() {
    if !target_is_windows() {
        return;
    }
    if let Err(error) = embed_manifest::embed_manifest_file("cretlink-support.manifest") {
        // Without the manifest the binary would silently fail to request
        // elevation, so fail the build loudly rather than ship that.
        println!("cargo:warning=CretLink Support manifest embed failed: {error}");
        std::process::exit(1);
    }
}

// On a non-Windows host the `embed-manifest` dependency is absent, so a Windows
// *target* cross-built here would ship with no elevation manifest and never
// request UAC (REQ-F-002). Fail closed rather than silently emit that binary; a
// non-Windows target stays a clean no-op.
#[cfg(not(windows))]
fn embed_manifest_for_windows_target() {
    if target_is_windows() {
        println!(
            "cargo:warning=CretLink Support: a Windows target cannot embed its \
             requireAdministrator elevation manifest (REQ-F-002) when built on a non-Windows \
             host; build official Windows artifacts on Windows (ADR-0001)."
        );
        std::process::exit(1);
    }
}
