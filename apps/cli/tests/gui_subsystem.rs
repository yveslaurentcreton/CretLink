//! ST-M009-001 spike evidence: a Windows GUI-subsystem build serves MCP
//! over redirected stdio (the conformance harness in `mcp_conformance.rs` runs
//! against the same binary) and is really built for the GUI subsystem.
//!
//! Run on Windows with `cargo test -p cretlink-cli --features gui-subsystem`.

#![cfg(all(windows, feature = "gui-subsystem"))]

use std::process::{Command, Stdio};

/// `IMAGE_SUBSYSTEM_WINDOWS_GUI` in the PE optional header.
const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

/// Reads the subsystem field of a PE image.
fn pe_subsystem(image: &[u8]) -> u16 {
    let at = |offset: usize, len: usize| &image[offset..offset + len];
    let pe = u32::from_le_bytes(at(0x3c, 4).try_into().expect("e_lfanew")) as usize;
    assert_eq!(at(pe, 4), b"PE\0\0", "PE signature");
    // Signature (4) + COFF file header (20), then the optional header, whose
    // subsystem field sits at offset 68 in both PE32 and PE32+.
    let optional = pe + 4 + 20;
    u16::from_le_bytes(at(optional + 68, 2).try_into().expect("subsystem"))
}

#[test]
fn the_binary_is_built_for_the_gui_subsystem() {
    let image = std::fs::read(env!("CARGO_BIN_EXE_cretlink-cli")).expect("read cretlink-cli.exe");
    assert_eq!(pe_subsystem(&image), IMAGE_SUBSYSTEM_WINDOWS_GUI);
}

#[test]
fn version_reaches_redirected_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_cretlink-cli"))
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .expect("run version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("cretlink "), "stdout: {stdout:?}");
}

#[test]
fn shell_is_refused_without_a_console_of_its_own() {
    let output = Command::new(env!("CARGO_BIN_EXE_cretlink-cli"))
        .arg("shell")
        .stdin(Stdio::null())
        .output()
        .expect("run shell");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cretlink-cli.exe"), "stderr: {stderr:?}");
}
