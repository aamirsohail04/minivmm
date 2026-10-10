// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! minivmm: a small learning VMM built on the rust-vmm crates.
//!
//! Milestone M1 runs a 16-bit real-mode guest on one vCPU. The guest writes
//! to a 16550A serial port (emulated with `vm-superio`) and stops with `HLT`.
//!
//! Module map:
//! - [`memory`]: guest RAM, image loading and KVM memory slots.
//! - [`guest`]: guest images (the built-in hello program, or a file).
//! - [`bus`]: dispatch of port I/O addresses to devices.
//! - [`devices`]: emulated devices (only the serial console in M1).
//! - [`vcpu`]: vCPU register setup and the `KVM_RUN` loop.
//! - [`vmm`]: builds the VM from a [`vmm::VmConfig`] and runs it.

#[cfg(not(target_arch = "x86_64"))]
compile_error!("minivmm supports x86_64 hosts only");

pub mod bus;
pub mod devices;
pub mod guest;
pub mod memory;
pub mod vcpu;
pub mod vmm;
