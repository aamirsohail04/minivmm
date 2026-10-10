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

use minivmm::guest::GuestImage;
use minivmm::memory::{MAX_MEM_MIB, MIN_MEM_MIB};
use minivmm::vmm::{VmConfig, Vmm};

/// A small learning VMM built on the rust-vmm crates (milestone M1: a
/// real-mode guest with a serial console).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Guest RAM size in MiB (1 to 3072).
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = 16,
        value_parser = clap::value_parser!(u64).range(MIN_MEM_MIB..=MAX_MEM_MIB)
    )]
    mem_mib: u64,

    /// Flat 16-bit binary to run instead of the built-in hello guest.
    /// It is loaded at 0x1000 and starts there.
    #[arg(long, value_name = "FILE")]
    guest: Option<PathBuf>,

    /// Print the device map and exit statistics.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Validate the image before touching KVM, so file problems are
    // reported the same way on any machine.
    let image = match &cli.guest {
        Some(path) => match GuestImage::from_file(path) {
            Ok(image) => image,
            Err(e) => {
                eprintln!("minivmm: error: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => GuestImage::hello(),
    };

    let config = VmConfig {
        mem_mib: cli.mem_mib,
        image,
    };

    let mut vmm = match Vmm::new(&config, Box::new(io::stdout())) {
        Ok(vmm) => vmm,
        Err(e) => {
            eprintln!("minivmm: error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if cli.verbose {
        eprintln!(
            "minivmm: guest {} ({} bytes), {} MiB RAM",
            config.image.name(),
            config.image.bytes().len(),
            config.mem_mib
        );
        for (name, base, len) in vmm.devices() {
            eprintln!("minivmm: port {base:#06x}..{:#06x} {name}", base + len - 1);
        }
    }

    match vmm.run() {
        Ok(summary) => {
            let s = summary.stats;
            eprintln!("minivmm: guest halted after {} VM exits", s.total);
            if cli.verbose {
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("minivmm: error: {e}");
            ExitCode::FAILURE
        }
    }
}
