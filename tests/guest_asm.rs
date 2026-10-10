// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Checks that the embedded hello guest matches `guest/hello.asm`.
//!
//! Runs only when `nasm` is installed; otherwise it prints a note and passes.

use std::path::PathBuf;
use std::process::Command;

use minivmm::guest::GuestImage;

#[test]
fn embedded_hello_matches_the_assembly_source() {
    if Command::new("nasm").arg("-v").output().is_err() {
        eprintln!("skipping: nasm is not installed (sudo apt install nasm)");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = root.join("guest/hello.asm");
    let out = std::env::temp_dir().join(format!("minivmm-hello-{}.bin", std::process::id()));

    let status = Command::new("nasm")
        .args(["-f", "bin", "-o"])
        .arg(&out)
        .arg(&source)
        .status()
        .expect("run nasm");
    assert!(status.success(), "nasm failed on {}", source.display());

    let assembled = std::fs::read(&out).expect("read nasm output");
    let _ = std::fs::remove_file(&out);
    assert_eq!(
        assembled,
        GuestImage::hello().bytes(),
        "src/guest.rs HELLO_CODE is out of sync with guest/hello.asm"
    );
}
