// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! minivmm command-line entry point.
//!
//! Guest console output goes to stdout. Everything minivmm itself says
//! (warnings, errors, the summary line) goes to stderr, so the guest's
//! output can be captured or compared byte for byte.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use minivmm::boot::{DEFAULT_CMDLINE, LinuxBoot};
use minivmm::console::RawTerminal;
use minivmm::guest::GuestImage;
use minivmm::memory::{MAX_MEM_MIB, MIN_MEM_MIB};
use minivmm::vcpu::StopReason;
use minivmm::vmm::{Payload, RunSummary, VmConfig, Vmm};

/// Default RAM for the built-in or a file real-mode guest.
const DEFAULT_MEM_MIB_REAL_MODE: u64 = 16;
/// Default RAM for a Linux guest: room for a distribution kernel and an initramfs.
const DEFAULT_MEM_MIB_LINUX: u64 = 512;

/// A small learning VMM built on the rust-vmm crates.
///
/// Without --kernel it runs a 16-bit real-mode guest (milestone M1).
/// With --kernel it boots Linux to its serial console (milestone M2).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Linux kernel to boot (an x86_64 bzImage, boot protocol 2.12 or newer).
    #[arg(long, value_name = "FILE")]
    kernel: Option<PathBuf>,

    /// Initramfs for the kernel (a cpio archive, optionally compressed).
    #[arg(long, value_name = "FILE", requires = "kernel")]
    initrd: Option<PathBuf>,

    /// Kernel command line, replacing the default:
    /// "console=ttyS0 reboot=k panic=1 pci=off nomodule 8250.nr_uarts=1 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd".
    #[arg(long, value_name = "ARGS", requires = "kernel")]
    cmdline: Option<String>,

    /// Flat 16-bit binary to run instead of the built-in hello guest.
    /// It is loaded at 0x1000 and starts there.
    #[arg(long, value_name = "FILE", conflicts_with = "kernel")]
    guest: Option<PathBuf>,

    /// Guest RAM size in MiB (1 to 3072; default 16, or 512 with --kernel).
    #[arg(
        long,
        value_name = "MIB",
        value_parser = clap::value_parser!(u64).range(MIN_MEM_MIB..=MAX_MEM_MIB)
    )]
    mem_mib: Option<u64>,

    /// Print the device map, boot layout and exit statistics, and report
    /// every unmapped port a Linux guest touches.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("minivmm: error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    // Validate every input before touching KVM, so file problems are
    // reported the same way on any machine.
    let config = build_config(cli)?;
    let is_linux = matches!(config.payload, Payload::Linux(_));

    let vmm = Vmm::new(&config, Box::new(io::stdout())).map_err(|e| e.to_string())?;
    if cli.verbose {
        describe(&config, &vmm);
    }

    // A Linux guest gets keyboard input. With a terminal on stdin, keys go
    // to the guest as typed; `_raw` restores the terminal when `run` returns.
    let _raw = if is_linux {
        let raw = RawTerminal::enter().map_err(|e| format!("cannot set up the terminal: {e}"))?;
        if raw.is_some() {
            eprintln!("minivmm: console attached; press Ctrl-A x to quit");
        }
        vmm.attach_console_input(io::stdin())
            .map_err(|e| format!("cannot start console input: {e}"))?;
        raw
    } else {
        None
    };

    let summary = vmm.run().map_err(|e| e.to_string())?;
    report(&summary, cli.verbose);
    Ok(())
}

fn build_config(cli: &Cli) -> Result<VmConfig, String> {
    let (payload, default_mem) = match &cli.kernel {
        Some(kernel) => {
            let cmdline = cli.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE);
            let linux = LinuxBoot::new(kernel, cli.initrd.as_deref(), cmdline)
                .map_err(|e| e.to_string())?;
            (Payload::Linux(linux), DEFAULT_MEM_MIB_LINUX)
        }
        None => {
            let image = match &cli.guest {
                Some(path) => GuestImage::from_file(path).map_err(|e| e.to_string())?,
                None => GuestImage::hello(),
            };
            (Payload::RealMode(image), DEFAULT_MEM_MIB_REAL_MODE)
        }
    };
    let mem_mib = cli.mem_mib.unwrap_or(default_mem);
    if let Payload::Linux(linux) = &payload {
        linux.check_memory(mem_mib).map_err(|e| e.to_string())?;
    }
    Ok(VmConfig {
        mem_mib,
        warn_unmapped: !matches!(payload, Payload::Linux(_)) || cli.verbose,
        payload,
    })
}

fn describe(config: &VmConfig, vmm: &Vmm) {
    match &config.payload {
        Payload::RealMode(image) => eprintln!(
            "minivmm: guest {} ({} bytes), {} MiB RAM",
            image.name(),
            image.bytes().len(),
            config.mem_mib
        ),
        Payload::Linux(linux) => {
            let v = linux.kernel.protocol();
            eprintln!(
                "minivmm: kernel {} ({} bytes, boot protocol {}.{:02}), {} MiB RAM",
                linux.kernel.path().display(),
                linux.kernel.size(),
                v >> 8,
                v & 0xff,
                config.mem_mib
            );
            if let Some(initrd) = &linux.initrd {
                eprintln!(
                    "minivmm: initrd {} ({} bytes)",
                    initrd.path().display(),
                    initrd.size()
                );
            }
            eprintln!("minivmm: cmdline \"{}\"", linux.cmdline);
        }
    }
    if let Some(boot) = vmm.boot_info() {
        eprintln!(
            "minivmm: kernel at {:#x}, entry {:#x}, zero page {:#x}",
            boot.kernel_load, boot.entry, boot.zero_page
        );
        if let Some((addr, size)) = boot.initrd {
            eprintln!("minivmm: initrd at {addr:#x}..{:#x}", addr + size);
        }
    }
    for (name, base, len) in vmm.devices() {
        eprintln!("minivmm: port {base:#06x}..{:#06x} {name}", base + len - 1);
    }
}

fn report(summary: &RunSummary, verbose: bool) {
    let s = summary.stats;
    let what = match summary.reason {
        StopReason::Halted => "guest halted",
        StopReason::Reset => "guest rebooted",
        StopReason::Quit => "stopped from the console (Ctrl-A x)",
    };
    eprintln!("minivmm: {what} after {} VM exits", s.total);
    if verbose {
        eprintln!(
            "minivmm: exits: {} port writes, {} port reads, {} MMIO, {} unmapped",
            s.io_out, s.io_in, s.mmio, s.unmapped
        );
    }
    if summary.lost_console_bytes > 0 {
        eprintln!(
            "minivmm: warning: {} console bytes could not be written",
            summary.lost_console_bytes
        );
    }
}
