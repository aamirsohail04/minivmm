// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! End-to-end tests for milestone M1: run the `minivmm` binary.
//!
//! Tests that need KVM skip themselves (with a note) when `/dev/kvm` cannot
//! be opened, so `cargo test` still passes on CI runners without KVM.
//! Tests of argument and image validation run everywhere.
//!
//! The small test guests are 16-bit machine code; the NASM source of each
//! is shown next to its bytes.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Can this user open /dev/kvm for reading and writing?
fn kvm_usable() -> bool {
    match OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) => true,
        Err(e) => {
            eprintln!("skipping: cannot open /dev/kvm ({e})");
            false
        }
    }
}

/// Run minivmm with `args`, killing it if it runs longer than `TIMEOUT`.
fn run_minivmm(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_minivmm"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start minivmm");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll minivmm") {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("minivmm did not stop within {TIMEOUT:?} (guest never halted?)");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // Outputs are small, so they fit in the pipe buffers while we wait.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

/// Write a guest image to a unique temporary file.
fn temp_guest(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("minivmm-{name}-{}.bin", std::process::id()));
    std::fs::write(&path, bytes).expect("write temp guest");
    path
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn hello_guest_prints_its_message_and_halts() {
    if !kvm_usable() {
        return;
    }
    let out = run_minivmm(&[]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, b"Hello from the minivmm guest!\n");
    assert!(stderr_of(&out).contains("guest halted"));
}

#[test]
fn verbose_mode_shows_devices_and_exit_counts() {
    if !kvm_usable() {
        return;
    }
    let out = run_minivmm(&["--verbose"]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("serial (COM1)"), "stderr: {err}");
    // One port write per message byte, plus the HLT exit.
    assert!(err.contains("30 port writes"), "stderr: {err}");
}

#[test]
fn runs_a_guest_from_a_file() {
    if !kvm_usable() {
        return;
    }
    // mov dx, 0x3f8 / mov al, 'O' / out dx, al / mov al, 'K' / out dx, al
    // mov al, 10 / out dx, al / .h: hlt / jmp .h
    let guest = temp_guest(
        "ok",
        &[
            0xBA, 0xF8, 0x03, 0xB0, 0x4F, 0xEE, 0xB0, 0x4B, 0xEE, 0xB0, 0x0A, 0xEE, 0xF4, 0xEB,
            0xFD,
        ],
    );
    let out = run_minivmm(&["--guest", guest.to_str().unwrap()]);
    let _ = std::fs::remove_file(&guest);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, b"OK\n");
}

#[test]
fn unmapped_port_read_returns_ff_and_the_guest_keeps_running() {
    if !kvm_usable() {
        return;
    }
    // mov dx, 0x3f8 / in al, 0x99 / out dx, al / .h: hlt / jmp .h
    let guest = temp_guest(
        "unmapped-port",
        &[0xBA, 0xF8, 0x03, 0xE4, 0x99, 0xEE, 0xF4, 0xEB, 0xFD],
    );
    let out = run_minivmm(&["--guest", guest.to_str().unwrap()]);
    let _ = std::fs::remove_file(&guest);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, [0xFF]);
    let err = stderr_of(&out);
    assert!(
        err.contains("port read at unmapped address 0x99"),
        "stderr: {err}"
    );
}

#[test]
fn read_beyond_ram_is_an_unmapped_mmio_read() {
    if !kvm_usable() {
        return;
    }
    // With 1 MiB of RAM, 0xffff:0x0010 = 0x100000 is just past the end of RAM,
    // so KVM reports it as MMIO.
    // mov dx, 0x3f8 / mov ax, 0xffff / mov ds, ax / mov al, [0x10]
    // out dx, al / .h: hlt / jmp .h
    let guest = temp_guest(
        "unmapped-mmio",
        &[
            0xBA, 0xF8, 0x03, 0xB8, 0xFF, 0xFF, 0x8E, 0xD8, 0xA0, 0x10, 0x00, 0xEE, 0xF4, 0xEB,
            0xFD,
        ],
    );
    let out = run_minivmm(&["--mem-mib", "1", "--guest", guest.to_str().unwrap()]);
    let _ = std::fs::remove_file(&guest);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(out.stdout, [0xFF]);
    let err = stderr_of(&out);
    assert!(
        err.contains("MMIO read at unmapped address 0x100000"),
        "stderr: {err}"
    );
}

// The tests below do not need KVM: the image is validated before
// /dev/kvm is opened.

#[test]
fn missing_guest_file_fails_with_its_path() {
    let out = run_minivmm(&["--guest", "/nonexistent/minivmm-guest.bin"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("/nonexistent/minivmm-guest.bin"));
}

#[test]
fn oversized_guest_is_rejected() {
    let guest = temp_guest("too-big", &vec![0x90; 70 * 1024]);
    let out = run_minivmm(&["--guest", guest.to_str().unwrap()]);
    let _ = std::fs::remove_file(&guest);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("too large"), "{}", stderr_of(&out));
}

#[test]
fn empty_guest_is_rejected() {
    let guest = temp_guest("empty", &[]);
    let out = run_minivmm(&["--guest", guest.to_str().unwrap()]);
    let _ = std::fs::remove_file(&guest);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("is empty"), "{}", stderr_of(&out));
}

#[test]
fn missing_kvm_gives_a_hint() {
    // Only meaningful where KVM is absent; elsewhere this test has nothing to check.
    if std::path::Path::new("/dev/kvm").exists() {
        return;
    }
    let out = run_minivmm(&[]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("cannot open /dev/kvm"), "{err}");
    assert!(err.contains("hint"), "{err}");
}

#[test]
fn memory_size_out_of_range_is_rejected_by_the_cli() {
    for bad in ["0", "3073"] {
        let out = run_minivmm(&["--mem-mib", bad]);
        assert!(!out.status.success());
        assert!(stderr_of(&out).contains("--mem-mib"), "{}", stderr_of(&out));
    }
}
