// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! vCPU setup and the run loop.
//!
//! The run loop is the heart of a VMM: call `KVM_RUN`, the guest executes
//! natively until it does something KVM cannot finish in the kernel, then
//! `KVM_RUN` returns with an exit reason. The VMM handles the exit (here:
//! emulate a port access, or stop on `HLT`) and calls `KVM_RUN` again.
//!
//! Note on `HLT`: KVM only returns `KVM_EXIT_HLT` to user space when there
//! is no in-kernel interrupt controller. M1 deliberately creates none, so a
//! halt ends the guest. From M2 the in-kernel irqchip handles `HLT` inside
//! KVM (the vCPU sleeps until an interrupt) and the guest stops another way.

use std::collections::HashSet;
use std::fmt;

use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use vmm_sys_util::errno;

use crate::bus::Bus;

/// RFLAGS bit 1 is reserved and must always be set.
const RFLAGS_RESERVED: u64 = 0x2;
/// Value returned for reads nobody answers, like a floating ISA bus.
const UNMAPPED_READ_VALUE: u8 = 0xff;

/// Why the guest stopped normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The guest executed `HLT`.
    Halted,
}

/// Counts of VM exits, for the summary line and for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExitStats {
    /// Every return from `KVM_RUN` with an exit to handle.
    pub total: u64,
    /// Port writes (`out`).
    pub io_out: u64,
    /// Port reads (`in`).
    pub io_in: u64,
    /// MMIO accesses (no MMIO devices exist in M1).
    pub mmio: u64,
    /// Accesses to addresses no device answers.
    pub unmapped: u64,
}

/// Errors from creating or running a vCPU.
#[derive(Debug)]
pub enum VcpuError {
    /// `KVM_CREATE_VCPU` failed.
    Create(errno::Error),
    /// Reading or writing vCPU registers failed during setup.
    Setup {
        step: &'static str,
        source: errno::Error,
    },
    /// `KVM_RUN` itself failed.
    Run(errno::Error),
    /// The guest triple-faulted (`KVM_EXIT_SHUTDOWN`).
    Shutdown { rip: Option<u64> },
    /// The CPU refused to enter the guest (`KVM_EXIT_FAIL_ENTRY`).
    FailEntry { reason: u64, cpu: u32 },
    /// KVM hit an internal error (`KVM_EXIT_INTERNAL_ERROR`).
    InternalError { rip: Option<u64> },
    /// An exit reason M1 does not handle.
    UnexpectedExit { exit: String, rip: Option<u64> },
}

fn fmt_rip(rip: &Option<u64>) -> String {
    rip.map(|r| format!(" at rip {r:#x}")).unwrap_or_default()
}

impl fmt::Display for VcpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcpuError::Create(e) => write!(f, "cannot create vCPU: {e}"),
            VcpuError::Setup { step, source } => {
                write!(f, "vCPU setup failed at {step}: {source}")
            }
            VcpuError::Run(e) => write!(f, "KVM_RUN failed: {e}"),
            VcpuError::Shutdown { rip } => write!(
                f,
                "guest shut down (triple fault){}; check the guest code and its entry point",
                fmt_rip(rip)
            ),
            VcpuError::FailEntry { reason, cpu } => write!(
                f,
                "hardware refused to enter the guest on cpu {cpu} (reason {reason:#x}); \
                 the vCPU register state is invalid"
            ),
            VcpuError::InternalError { rip } => {
                write!(f, "KVM internal error{}", fmt_rip(rip))
            }
            VcpuError::UnexpectedExit { exit, rip } => {
                write!(f, "unhandled VM exit {exit}{}", fmt_rip(rip))
            }
        }
    }
}

impl std::error::Error for VcpuError {}

/// Kinds of access, used to report each unmapped address once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Access {
    PortRead,
    PortWrite,
    MmioRead,
    MmioWrite,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::PortRead => "port read",
            Access::PortWrite => "port write",
            Access::MmioRead => "MMIO read",
            Access::MmioWrite => "MMIO write",
        })
    }
}

/// What the run loop does after handling one exit.
enum Outcome {
    Continue,
    Stop(StopReason),
    Fail(VcpuError),
}

/// One virtual CPU.
pub struct Vcpu {
    id: u8,
    fd: VcpuFd,
    /// Unmapped addresses already reported, so a guest polling a missing
    /// device cannot flood the log.
    warned: HashSet<(Access, u64)>,
    stats: ExitStats,
}

impl Vcpu {
    /// Create vCPU `id` in `vm`.
    pub fn new(vm: &VmFd, id: u8) -> Result<Self, VcpuError> {
        let fd = vm.create_vcpu(u64::from(id)).map_err(VcpuError::Create)?;
        Ok(Self {
            id,
            fd,
            warned: HashSet::new(),
            stats: ExitStats::default(),
        })
    }

    /// The vCPU index.
    pub fn id(&self) -> u8 {
        self.id
    }

    /// Exit counts so far.
    pub fn stats(&self) -> ExitStats {
        self.stats
    }

    /// Put the vCPU in 16-bit real mode, starting at `entry`.
    ///
    /// A new vCPU starts in the x86 reset state: real mode, with CS base
    /// 0xffff_0000 so the first instruction is fetched from the top of the
    /// address space, where firmware would be. There is no firmware here, so
    /// every segment base and selector is set to 0 and RIP to `entry`.
    /// Guest physical address = segment base (0) + offset (RIP, SI, SP...).
    pub fn setup_real_mode(&mut self, entry: u64, stack: u64) -> Result<(), VcpuError> {
        let setup = |step| move |source| VcpuError::Setup { step, source };

        let mut sregs = self.fd.get_sregs().map_err(setup("KVM_GET_SREGS"))?;
        for seg in [
            &mut sregs.cs,
            &mut sregs.ds,
            &mut sregs.es,
            &mut sregs.fs,
            &mut sregs.gs,
            &mut sregs.ss,
        ] {
            seg.base = 0;
            seg.selector = 0;
        }
        self.fd.set_sregs(&sregs).map_err(setup("KVM_SET_SREGS"))?;

        let mut regs = self.fd.get_regs().map_err(setup("KVM_GET_REGS"))?;
        regs.rip = entry;
        regs.rsp = stack;
        // Interrupts off, direction flag clear (string instructions count up).
        regs.rflags = RFLAGS_RESERVED;
        self.fd.set_regs(&regs).map_err(setup("KVM_SET_REGS"))?;
        Ok(())
    }

    /// Run the guest until it halts or fails.
    pub fn run(&mut self, pio: &Bus) -> Result<StopReason, VcpuError> {
        loop {
            let outcome = {
                // Split the borrows: `exit` borrows `fd` (its data lives in the
                // shared kvm_run page) while we update `warned` and `stats`.
                let Vcpu {
                    fd, warned, stats, ..
                } = self;
                match fd.run() {
                    Ok(exit) => {
                        stats.total += 1;
                        handle_exit(exit, pio, warned, stats)
                    }
                    // A signal interrupted KVM_RUN, or KVM asks us to retry.
                    Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                        Outcome::Continue
                    }
                    Err(e) => Outcome::Fail(VcpuError::Run(e)),
                }
            };
            match outcome {
                Outcome::Continue => {}
                Outcome::Stop(reason) => return Ok(reason),
                Outcome::Fail(err) => return Err(self.add_rip(err)),
            }
        }
    }

    /// Attach the guest's RIP to errors where it helps debugging.
    fn add_rip(&self, err: VcpuError) -> VcpuError {
        let rip = self.fd.get_regs().ok().map(|r| r.rip);
        match err {
            VcpuError::Shutdown { .. } => VcpuError::Shutdown { rip },
            VcpuError::InternalError { .. } => VcpuError::InternalError { rip },
            VcpuError::UnexpectedExit { exit, .. } => VcpuError::UnexpectedExit { exit, rip },
            other => other,
        }
    }
}

/// Handle one VM exit.
fn handle_exit(
    exit: VcpuExit<'_>,
    pio: &Bus,
    warned: &mut HashSet<(Access, u64)>,
    stats: &mut ExitStats,
) -> Outcome {
    match exit {
        VcpuExit::IoOut(port, data) => {
            stats.io_out += 1;
            if pio.write(u64::from(port), data).is_err() {
                stats.unmapped += 1;
                warn_once(warned, Access::PortWrite, u64::from(port));
            }
            Outcome::Continue
        }
        VcpuExit::IoIn(port, data) => {
            stats.io_in += 1;
            if pio.read(u64::from(port), data).is_err() {
                stats.unmapped += 1;
                data.fill(UNMAPPED_READ_VALUE);
                warn_once(warned, Access::PortRead, u64::from(port));
            }
            Outcome::Continue
        }
        VcpuExit::MmioRead(addr, data) => {
            stats.mmio += 1;
            stats.unmapped += 1;
            data.fill(UNMAPPED_READ_VALUE);
            warn_once(warned, Access::MmioRead, addr);
            Outcome::Continue
        }
        VcpuExit::MmioWrite(addr, _) => {
            stats.mmio += 1;
            stats.unmapped += 1;
            warn_once(warned, Access::MmioWrite, addr);
            Outcome::Continue
        }
        VcpuExit::Hlt => Outcome::Stop(StopReason::Halted),
        VcpuExit::Shutdown => Outcome::Fail(VcpuError::Shutdown { rip: None }),
        VcpuExit::FailEntry(reason, cpu) => Outcome::Fail(VcpuError::FailEntry { reason, cpu }),
        VcpuExit::InternalError => Outcome::Fail(VcpuError::InternalError { rip: None }),
        other => Outcome::Fail(VcpuError::UnexpectedExit {
            exit: format!("{other:?}"),
            rip: None,
        }),
    }
}

fn warn_once(warned: &mut HashSet<(Access, u64)>, access: Access, addr: u64) {
    if warned.insert((access, addr)) {
        eprintln!(
            "minivmm: warning: guest {access} at unmapped address {addr:#x}; \
             reads return {UNMAPPED_READ_VALUE:#04x}, writes are dropped; \
             further accesses there are not reported"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_once_per_address_and_kind() {
        let mut warned = HashSet::new();
        warn_once(&mut warned, Access::PortRead, 0x80);
        warn_once(&mut warned, Access::PortRead, 0x80);
        warn_once(&mut warned, Access::PortWrite, 0x80);
        assert_eq!(warned.len(), 2);
    }

    #[test]
    fn errors_explain_themselves() {
        let e = VcpuError::Shutdown { rip: Some(0x1234) };
        assert!(e.to_string().contains("triple fault"));
        assert!(e.to_string().contains("0x1234"));
        let e = VcpuError::FailEntry { reason: 7, cpu: 0 };
        assert!(e.to_string().contains("0x7"));
    }
}
