// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! minivmm: a small learning VMM built on the rust-vmm crates.
//!
//! Milestone M1 runs a 16-bit real-mode guest on one vCPU. The guest writes
//! to a 16550A serial port (emulated with `vm-superio`) and stops with `HLT`.
//!
//! Milestone M2 boots a Linux `bzImage` (with an optional initramfs) to a
//! shell on that serial console: the kernel is entered at its 64-bit entry
//! point, with interrupt controllers and the timer emulated inside KVM.
//!
//! Module map:
//! - [`memory`]: guest RAM, image loading and KVM memory slots.
//! - [`guest`]: real-mode guest images (the built-in hello program, or a file).
//! - [`boot`]: Linux boot protocol: kernel, initrd, command line, zero page,
//!   e820 map, GDT and page tables.
//! - [`bus`]: dispatch of port I/O addresses to devices.
//! - [`devices`]: emulated devices (serial console, keyboard controller).
//! - [`console`]: host terminal input to the serial console.
//! - [`vcpu`]: vCPU register setup, the `KVM_RUN` loop and stop requests.
//! - [`vmm`]: builds the VM from a [`vmm::VmConfig`] and runs it.

#[cfg(not(target_arch = "x86_64"))]
compile_error!("minivmm supports x86_64 hosts only");

pub mod boot;
pub mod bus;
pub mod console;
pub mod devices;
pub mod guest;
pub mod memory;
pub mod vcpu;
pub mod vmm;
