// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! End-to-end tests for milestone M2: boot Linux in the `minivmm` binary.
//!
//! The boot tests need `/dev/kvm`, a kernel and the initramfs from
//! `scripts/make-initramfs.sh`. They look for:
//!
//! - the kernel in `$MINIVMM_KERNEL`, else `guest/bzImage`
//! - the initramfs in `$MINIVMM_INITRD`, else `guest/initramfs.cpio.gz`
//!
//! and skip themselves (with a note) when any of the three is missing, so
//! `cargo test` still passes on CI runners. `$MINIVMM_BOOT_TIMEOUT` (seconds,
//! default 60) raises the time limit on slow hosts, for example nested
//! virtualization without hardware support for nested paging.
//!
//! The command-line validation tests at the end need none of this.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use minivmm::boot::DEFAULT_CMDLINE;

/// The prompt the initramfs shell prints (`PS1='minivmm:\w# '` in /root).
const PROMPT: &str = "minivmm:~# ";
/// Printed by the initramfs /init when the command line has `minivmm.selftest`.
const BOOT_MARKER: &str = "MINIVMM-BOOT-OK";

fn manifest_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn timeout() -> Duration {
    let secs = std::env::var("MINIVMM_BOOT_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Kernel and initramfs paths, or `None` (with a note) if a test must skip.
fn guest_files() -> Option<(PathBuf, PathBuf)> {
    if let Err(e) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        eprintln!("skipping: cannot open /dev/kvm ({e})");
        return None;
    }
    let kernel = std::env::var_os("MINIVMM_KERNEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_path("guest/bzImage"));
    let initrd = std::env::var_os("MINIVMM_INITRD")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_path("guest/initramfs.cpio.gz"));
    for path in [&kernel, &initrd] {
        if !path.exists() {
            eprintln!(
                "skipping: {} not found (see \"Boot Linux\" in README.md)",
                path.display()
            );
            return None;
        }
    }
    Some((kernel, initrd))
}

/// A running minivmm whose stdout is collected as it arrives.
struct Vm {
    child: Child,
    stdout: Receiver<Vec<u8>>,
    seen: String,
    deadline: Instant,
}

impl Vm {
    fn start(args: &[&str]) -> Vm {
        let mut child = Command::new(env!("CARGO_BIN_EXE_minivmm"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start minivmm");
        let mut out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Vm {
            child,
            stdout: rx,
            seen: String::new(),
            deadline: Instant::now() + timeout(),
        }
    }

    /// Wait until the guest's output contains `text`.
    fn expect(&mut self, text: &str) {
        while !self.seen.contains(text) {
            let left = self.deadline.saturating_duration_since(Instant::now());
            match self.stdout.recv_timeout(left) {
                Ok(chunk) => self.seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(_) => {
                    let _ = self.child.kill();
                    let stderr = self.stderr();
                    panic!(
                        "guest output never showed {text:?} within {:?}\n--- guest output:\n{}\n--- stderr:\n{stderr}",
                        timeout(),
                        self.seen
                    );
                }
            }
        }
    }

    /// Type `keys` on the guest's console.
    fn send(&mut self, keys: &[u8]) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(keys).unwrap();
        stdin.flush().unwrap();
    }

    /// Wait for minivmm to exit; returns its status and stderr.
    fn finish(mut self) -> (ExitStatus, String) {
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status, self.stderr());
            }
            if Instant::now() > self.deadline {
                let _ = self.child.kill();
                panic!(
                    "minivmm did not exit within {:?}\n--- guest output:\n{}",
                    timeout(),
                    self.seen
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stderr(&mut self) -> String {
        let mut err = String::new();
        if let Some(mut s) = self.child.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        err
    }
}

#[test]
fn linux_boots_to_init_and_reboots_cleanly() {
    let Some((kernel, initrd)) = guest_files() else {
        return;
    };
    let cmdline = format!("{DEFAULT_CMDLINE} minivmm.selftest");
    let mut vm = Vm::start(&[
        "--kernel",
        kernel.to_str().unwrap(),
        "--initrd",
        initrd.to_str().unwrap(),
        "--cmdline",
        &cmdline,
    ]);
    vm.expect("Linux version");
    vm.expect(BOOT_MARKER);
    let (status, stderr) = vm.finish();
    assert!(status.success(), "stderr: {stderr}");
    assert!(stderr.contains("guest rebooted"), "stderr: {stderr}");
}

#[test]
fn shell_runs_commands_typed_on_the_console() {
    let Some((kernel, initrd)) = guest_files() else {
        return;
    };
    let mut vm = Vm::start(&[
        "--kernel",
        kernel.to_str().unwrap(),
        "--initrd",
        initrd.to_str().unwrap(),
    ]);
    vm.expect(PROMPT);
    // The guest computes 6 * 7, so "42" only appears if the command ran: the
    // echoed input shows "$((6 * 7))", not the result. Input reaching the
    // shell means the UART receive interrupt got through the irqfd.
    vm.send(b"echo answer=$((6 * 7))\r");
    vm.expect("answer=42");
    vm.send(b"reboot -f\r");
    let (status, stderr) = vm.finish();
    assert!(status.success(), "stderr: {stderr}");
    assert!(stderr.contains("guest rebooted"), "stderr: {stderr}");
}

#[test]
fn ctrl_a_x_stops_a_running_guest() {
    let Some((kernel, initrd)) = guest_files() else {
        return;
    };
    let mut vm = Vm::start(&[
        "--kernel",
        kernel.to_str().unwrap(),
        "--initrd",
        initrd.to_str().unwrap(),
    ]);
    vm.expect(PROMPT);
    // The guest is idle in the shell, so its vCPU sleeps inside KVM_RUN:
    // stopping it needs the kick signal, not just a flag.
    vm.send(b"\x01x");
    let (status, stderr) = vm.finish();
    assert!(status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("stopped from the console"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_kernel_panic_ends_the_vm_instead_of_hanging() {
    let Some((kernel, _)) = guest_files() else {
        return;
    };
    // No initramfs and no root device: the kernel panics with "VFS: Unable
    // to mount root fs", then panic=1 reboots it through the i8042.
    let mut vm = Vm::start(&["--kernel", kernel.to_str().unwrap()]);
    vm.expect("Kernel panic");
    let (status, stderr) = vm.finish();
    assert!(status.success(), "stderr: {stderr}");
    assert!(stderr.contains("guest rebooted"), "stderr: {stderr}");
}

// The tests below do not need KVM or a kernel: every input is checked
// before /dev/kvm is opened.

/// Run minivmm to completion with no stdin; returns (success, stderr).
fn run_cli(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_minivmm"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run minivmm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A file with a valid 64-bit bzImage setup header and nothing else.
fn fake_kernel(name: &str) -> PathBuf {
    let mut image = vec![0u8; 0x1000];
    image[0x1f1] = 1; // setup_sects
    image[0x202..0x206].copy_from_slice(b"HdrS");
    image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes()); // protocol 2.15
    image[0x211] = 0x1; // loadflags: LOADED_HIGH
    image[0x214..0x218].copy_from_slice(&0x10_0000u32.to_le_bytes()); // code32_start
    image[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes()); // initrd_addr_max
    image[0x236..0x238].copy_from_slice(&0x1u16.to_le_bytes()); // xloadflags: 64-bit entry
    image[0x238..0x23c].copy_from_slice(&2047u32.to_le_bytes()); // cmdline_size
    image[0x258..0x260].copy_from_slice(&0x100_0000u64.to_le_bytes()); // pref_address
    image[0x260..0x264].copy_from_slice(&(48u32 << 20).to_le_bytes()); // init_size
    let path = std::env::temp_dir().join(format!("minivmm-{name}-{}", std::process::id()));
    std::fs::write(&path, image).unwrap();
    path
}

#[test]
fn a_file_that_is_not_a_kernel_is_rejected() {
    let path = std::env::temp_dir().join(format!("minivmm-notkernel-{}", std::process::id()));
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    let (ok, err) = run_cli(&["--kernel", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert!(!ok);
    assert!(err.contains("is not a Linux bzImage"), "{err}");
}

#[test]
fn a_missing_kernel_or_initrd_names_the_file() {
    let (ok, err) = run_cli(&["--kernel", "/nonexistent/bzImage"]);
    assert!(!ok);
    assert!(err.contains("/nonexistent/bzImage"), "{err}");

    let kernel = fake_kernel("kernel-missing-initrd");
    let (ok, err) = run_cli(&[
        "--kernel",
        kernel.to_str().unwrap(),
        "--initrd",
        "/nonexistent/initrd",
    ]);
    let _ = std::fs::remove_file(&kernel);
    assert!(!ok);
    assert!(err.contains("/nonexistent/initrd"), "{err}");
}

#[test]
fn too_little_memory_is_reported_before_booting() {
    let kernel = fake_kernel("kernel-small-ram");
    let (ok, err) = run_cli(&["--kernel", kernel.to_str().unwrap(), "--mem-mib", "32"]);
    let _ = std::fs::remove_file(&kernel);
    assert!(!ok);
    // 16 MiB preferred load address + 48 MiB init_size + one page.
    assert!(err.contains("need at least 65 MiB"), "{err}");
    assert!(err.contains("--mem-mib"), "{err}");
}

#[test]
fn an_overlong_command_line_is_rejected() {
    let kernel = fake_kernel("kernel-long-cmdline");
    let long = "x".repeat(3000);
    let (ok, err) = run_cli(&["--kernel", kernel.to_str().unwrap(), "--cmdline", &long]);
    let _ = std::fs::remove_file(&kernel);
    assert!(!ok);
    assert!(err.contains("3000 bytes, the maximum is 2047"), "{err}");
}

#[test]
fn linux_options_need_a_kernel_and_exclude_a_real_mode_guest() {
    let (ok, err) = run_cli(&["--initrd", "x.cpio"]);
    assert!(!ok);
    assert!(err.contains("--kernel"), "{err}");

    let (ok, err) = run_cli(&["--kernel", "k", "--guest", "g.bin"]);
    assert!(!ok);
    assert!(err.contains("cannot be used with"), "{err}");
}
