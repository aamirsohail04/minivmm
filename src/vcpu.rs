// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! vCPU setup and the run loop.
//!
//! The run loop is the heart of a VMM: call `KVM_RUN`, the guest executes
//! natively until it does something KVM cannot finish in the kernel, then
//! `KVM_RUN` returns with an exit reason. The VMM handles the exit (here:
//! emulate a port access, or stop on `HLT`) and calls `KVM_RUN` again.
//!
//! Two ways to start a vCPU:
//! - [`Vcpu::setup_real_mode`] (M1): 16-bit real mode at a flat address.
//! - [`Vcpu::setup_long_mode`] (M2): 64-bit long mode at the Linux kernel's
//!   64-bit entry point, with paging on and a CPU model KVM supports.
//!
//! Note on `HLT`: KVM only returns `KVM_EXIT_HLT` to user space when there
//! is no in-kernel interrupt controller. A real-mode guest runs without
//! one, so a halt ends it. A Linux guest has the in-kernel irqchip, `HLT`
//! is handled inside KVM (the vCPU sleeps until an interrupt), and the
//! guest stops by rebooting, which [`ExitControl`] carries to the run loop.
//!
//! Stopping a vCPU from another thread: set the request in [`ExitControl`],
//! then send the vCPU thread [`kick_signal`]. The signal makes a blocked
//! `KVM_RUN` return `EINTR`, and the loop checks for the request before
//! every `KVM_RUN`.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::sync::Mutex;

use kvm_bindings::{
    CpuId, KVM_MAX_CPUID_ENTRIES, Msrs, kvm_fpu, kvm_lapic_state, kvm_msr_entry, kvm_regs,
    kvm_sregs,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use libc::{c_int, c_void, siginfo_t};
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::{errno, signal};

use crate::boot::{
    self, BOOT_STACK_POINTER, BootInfo, GDT_CODE, GDT_DATA, GDT_START, GDT_TABLE, GDT_TSS,
    IDT_START, PML4_START,
};
use crate::bus::Bus;

/// RFLAGS bit 1 is reserved and must always be set.
const RFLAGS_RESERVED: u64 = 0x2;
/// Value returned for reads nobody answers, like a floating ISA bus.
const UNMAPPED_READ_VALUE: u8 = 0xff;

// Control register and EFER bits for long mode.
const X86_CR0_PE: u64 = 1 << 0;
const X86_CR0_PG: u64 = 1 << 31;
const X86_CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

// Model-specific registers set at boot.
const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
/// MISC_ENABLE bit 0: fast string operations (`rep movs`) are allowed.
const MISC_ENABLE_FAST_STRING: u64 = 1;
/// MTRRs enabled (bit 11), default memory type write-back (6).
const MTRR_DEF_TYPE_ENABLED_WB: u64 = (1 << 11) | 6;

// CPUID.
const CPUID_LEAF_FEATURES: u32 = 0x1;
const CPUID_LEAF_EXT_TOPOLOGY: u32 = 0xb;
const CPUID_LEAF_V2_EXT_TOPOLOGY: u32 = 0x1f;
/// CPUID.1:ECX bit 31: "running under a hypervisor".
const CPUID_ECX_HYPERVISOR: u32 = 1 << 31;

// Local APIC registers (offsets into the 1 KiB `kvm_lapic_state`).
const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

/// Why the guest stopped normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The guest executed `HLT` (real-mode guests only).
    Halted,
    /// The guest asked for a reset through the keyboard controller: what
    /// Linux does on `reboot` and, with `panic=1`, after a kernel panic.
    Reset,
    /// The user asked minivmm to quit (Ctrl-A x on the console).
    Quit,
}

/// How other threads ask the vCPU to stop, and how the main thread waits.
///
/// The first request wins. Every request, and the vCPU thread finishing,
/// writes the eventfd that the main thread blocks on.
pub struct ExitControl {
    reason: Mutex<Option<StopReason>>,
    wake: EventFd,
}

impl ExitControl {
    /// Create a control with no request pending.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            reason: Mutex::new(None),
            // Blocking: the main thread sleeps in `wait` until something happens.
            wake: EventFd::new(0)?,
        })
    }

    /// Ask the vCPU to stop for `reason`, unless a request is already pending.
    pub fn request(&self, reason: StopReason) {
        let mut pending = self.reason.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_none() {
            *pending = Some(reason);
        }
        drop(pending);
        self.wake_main();
    }

    /// The pending stop request, if any.
    pub fn requested(&self) -> Option<StopReason> {
        *self.reason.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wake the main thread (used when the vCPU thread ends on its own).
    pub fn wake_main(&self) {
        // Can only fail if the counter would overflow, which a few writes
        // cannot cause. Nothing useful to do about it here anyway.
        let _ = self.wake.write(1);
    }

    /// Block until a request is made or the vCPU thread ends.
    pub fn wait(&self) -> io::Result<()> {
        loop {
            match self.wake.read() {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// The signal used to kick a vCPU thread out of `KVM_RUN`.
pub fn kick_signal() -> c_int {
    signal::SIGRTMIN()
}

extern "C" fn kick_handler(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
    // Nothing to do: the point of the signal is that it interrupts KVM_RUN.
}

/// Install the (empty) handler for [`kick_signal`]. Without a handler the
/// default action of a real-time signal would terminate the process.
pub fn register_kick_handler() -> errno::Result<()> {
    signal::register_signal_handler(kick_signal(), kick_handler)
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
    /// MMIO accesses (no MMIO devices exist yet).
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
    /// KVM accepted only some of the boot MSRs.
    Msrs { written: usize, expected: usize },
    /// `KVM_RUN` itself failed.
    Run(errno::Error),
    /// The guest triple-faulted (`KVM_EXIT_SHUTDOWN`).
    Shutdown { rip: Option<u64> },
    /// The CPU refused to enter the guest (`KVM_EXIT_FAIL_ENTRY`).
    FailEntry { reason: u64, cpu: u32 },
    /// KVM hit an internal error (`KVM_EXIT_INTERNAL_ERROR`).
    InternalError { rip: Option<u64> },
    /// An exit reason minivmm does not handle.
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
            VcpuError::Msrs { written, expected } => write!(
                f,
                "vCPU setup failed at KVM_SET_MSRS: KVM accepted {written} of {expected} MSRs"
            ),
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

/// How the run loop reports accesses to addresses no device answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmappedLog {
    /// Print a warning the first time each address is used.
    Warn,
    /// Only count them (Linux probes many legacy ports that do not exist).
    Count,
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
    unmapped_log: UnmappedLog,
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
            unmapped_log: UnmappedLog::Warn,
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

    /// Choose how unmapped accesses are reported.
    pub fn set_unmapped_log(&mut self, log: UnmappedLog) {
        self.unmapped_log = log;
    }

    /// Put the vCPU in 16-bit real mode, starting at `entry`.
    ///
    /// A new vCPU starts in the x86 reset state: real mode, with CS base
    /// 0xffff_0000 so the first instruction is fetched from the top of the
    /// address space, where firmware would be. There is no firmware here, so
    /// every segment base and selector is set to 0 and RIP to `entry`.
    /// Guest physical address = segment base (0) + offset (RIP, SI, SP...).
    pub fn setup_real_mode(&mut self, entry: u64, stack: u64) -> Result<(), VcpuError> {
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

    /// Put the vCPU in 64-bit long mode at the Linux kernel's entry point.
    ///
    /// The VM must already have its in-kernel irqchip (for the LAPIC), and
    /// `boot` must describe tables that [`boot::load_linux`] wrote.
    pub fn setup_long_mode(&mut self, kvm: &Kvm, boot: &BootInfo) -> Result<(), VcpuError> {
        self.setup_cpuid(kvm)?;
        self.setup_msrs()?;
        self.setup_fpu()?;
        self.setup_lapic()?;

        let mut sregs = self.fd.get_sregs().map_err(setup("KVM_GET_SREGS"))?;
        long_mode_sregs(&mut sregs);
        self.fd.set_sregs(&sregs).map_err(setup("KVM_SET_SREGS"))?;

        // The boot protocol's 64-bit entry contract: RSI points at the zero
        // page, interrupts are off, and there is a usable stack.
        let regs = kvm_regs {
            rip: boot.entry,
            rsi: boot.zero_page,
            rsp: BOOT_STACK_POINTER,
            rbp: BOOT_STACK_POINTER,
            rflags: RFLAGS_RESERVED,
            ..Default::default()
        };
        self.fd.set_regs(&regs).map_err(setup("KVM_SET_REGS"))?;
        Ok(())
    }

    /// Give the guest the CPU model KVM supports on this host, with this
    /// vCPU's APIC ID filled in and the "hypervisor present" bit set.
    fn setup_cpuid(&mut self, kvm: &Kvm) -> Result<(), VcpuError> {
        let mut cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(setup("KVM_GET_SUPPORTED_CPUID"))?;
        patch_cpuid(&mut cpuid, self.id);
        self.fd.set_cpuid2(&cpuid).map_err(setup("KVM_SET_CPUID2"))
    }

    /// MSRs the kernel expects in a defined state (the values Linux would
    /// find after firmware has run).
    fn setup_msrs(&mut self) -> Result<(), VcpuError> {
        let entries = boot_msrs();
        let msrs = Msrs::from_entries(&entries).map_err(|_| VcpuError::Msrs {
            written: 0,
            expected: entries.len(),
        })?;
        let written = self.fd.set_msrs(&msrs).map_err(setup("KVM_SET_MSRS"))?;
        if written != entries.len() {
            return Err(VcpuError::Msrs {
                written,
                expected: entries.len(),
            });
        }
        Ok(())
    }

    /// The x87 FPU and SSE control words in their power-on state.
    fn setup_fpu(&mut self) -> Result<(), VcpuError> {
        let fpu = kvm_fpu {
            fcw: 0x37f,
            mxcsr: 0x1f80,
            ..Default::default()
        };
        self.fd.set_fpu(&fpu).map_err(setup("KVM_SET_FPU"))
    }

    /// Wire the local APIC as firmware would ("virtual wire" mode): LINT0
    /// passes interrupts from the legacy PIC (ExtINT), LINT1 delivers NMIs.
    /// Without this the PIC's interrupts, including the timer and the UART,
    /// never reach the CPU.
    fn setup_lapic(&mut self) -> Result<(), VcpuError> {
        let mut lapic = self.fd.get_lapic().map_err(setup("KVM_GET_LAPIC"))?;
        set_lvt_delivery_mode(&mut lapic, APIC_LVT0, APIC_MODE_EXTINT);
        set_lvt_delivery_mode(&mut lapic, APIC_LVT1, APIC_MODE_NMI);
        self.fd.set_lapic(&lapic).map_err(setup("KVM_SET_LAPIC"))
    }

    /// Run the guest until it stops, fails, or `ctl` asks it to stop.
    pub fn run(&mut self, pio: &Bus, ctl: &ExitControl) -> Result<StopReason, VcpuError> {
        loop {
            if let Some(reason) = ctl.requested() {
                return Ok(reason);
            }
            let outcome = {
                // Split the borrows: `exit` borrows `fd` (its data lives in the
                // shared kvm_run page) while we update `warned` and `stats`.
                let Vcpu {
                    fd,
                    warned,
                    unmapped_log,
                    stats,
                    ..
                } = self;
                match fd.run() {
                    Ok(exit) => {
                        stats.total += 1;
                        handle_exit(exit, pio, warned, *unmapped_log, stats)
                    }
                    // A signal interrupted KVM_RUN (for example a kick), or KVM
                    // asks us to retry. The check at the top of the loop decides.
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

/// Map an ioctl error during setup to [`VcpuError::Setup`] for `step`.
fn setup(step: &'static str) -> impl Fn(errno::Error) -> VcpuError {
    move |source| VcpuError::Setup { step, source }
}

/// Segment, descriptor table and control registers for 64-bit mode with
/// the boot GDT and the identity-mapped page tables from [`boot`].
fn long_mode_sregs(sregs: &mut kvm_sregs) {
    let code = boot::segment_from_gdt(GDT_TABLE[GDT_CODE as usize], GDT_CODE);
    let data = boot::segment_from_gdt(GDT_TABLE[GDT_DATA as usize], GDT_DATA);
    let tss = boot::segment_from_gdt(GDT_TABLE[GDT_TSS as usize], GDT_TSS);

    sregs.gdt.base = GDT_START;
    sregs.gdt.limit = (std::mem::size_of_val(&GDT_TABLE) - 1) as u16;
    sregs.idt.base = IDT_START;
    sregs.idt.limit = (std::mem::size_of::<u64>() - 1) as u16;

    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = tss;

    // Protected mode with paging, PAE page tables, long mode active.
    sregs.cr3 = PML4_START;
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
}

/// Fill in the per-vCPU parts of the CPUID table KVM supports.
fn patch_cpuid(cpuid: &mut CpuId, id: u8) {
    for entry in cpuid.as_mut_slice() {
        match entry.function {
            CPUID_LEAF_FEATURES => {
                // EBX bits 31..24: initial APIC ID.
                entry.ebx = (entry.ebx & 0x00ff_ffff) | (u32::from(id) << 24);
                entry.ecx |= CPUID_ECX_HYPERVISOR;
            }
            // EDX: x2APIC ID of this logical processor.
            CPUID_LEAF_EXT_TOPOLOGY | CPUID_LEAF_V2_EXT_TOPOLOGY => entry.edx = u32::from(id),
            _ => {}
        }
    }
}

fn boot_msrs() -> Vec<kvm_msr_entry> {
    [
        (MSR_IA32_SYSENTER_CS, 0),
        (MSR_IA32_SYSENTER_ESP, 0),
        (MSR_IA32_SYSENTER_EIP, 0),
        (MSR_STAR, 0),
        (MSR_CSTAR, 0),
        (MSR_KERNEL_GS_BASE, 0),
        (MSR_SYSCALL_MASK, 0),
        (MSR_LSTAR, 0),
        (MSR_IA32_TSC, 0),
        (MSR_IA32_MISC_ENABLE, MISC_ENABLE_FAST_STRING),
        (MSR_MTRR_DEF_TYPE, MTRR_DEF_TYPE_ENABLED_WB),
    ]
    .into_iter()
    .map(|(index, data)| kvm_msr_entry {
        index,
        data,
        ..Default::default()
    })
    .collect()
}

/// Set the delivery mode (bits 8 to 10) of a local vector table register.
fn set_lvt_delivery_mode(lapic: &mut kvm_lapic_state, offset: usize, mode: u32) {
    let reg = &mut lapic.regs[offset..offset + 4];
    // `regs` is an array of C `char`, signed on x86; reinterpret each byte.
    let mut value = u32::from_le_bytes([reg[0] as u8, reg[1] as u8, reg[2] as u8, reg[3] as u8]);
    value = (value & !0x700) | (mode << 8);
    for (dst, src) in reg.iter_mut().zip(value.to_le_bytes()) {
        *dst = src as libc::c_char;
    }
}

/// Handle one VM exit.
fn handle_exit(
    exit: VcpuExit<'_>,
    pio: &Bus,
    warned: &mut HashSet<(Access, u64)>,
    log: UnmappedLog,
    stats: &mut ExitStats,
) -> Outcome {
    match exit {
        VcpuExit::IoOut(port, data) => {
            stats.io_out += 1;
            if pio.write(u64::from(port), data).is_err() {
                stats.unmapped += 1;
                warn_once(warned, log, Access::PortWrite, u64::from(port));
            }
            Outcome::Continue
        }
        VcpuExit::IoIn(port, data) => {
            stats.io_in += 1;
            if pio.read(u64::from(port), data).is_err() {
                stats.unmapped += 1;
                data.fill(UNMAPPED_READ_VALUE);
                warn_once(warned, log, Access::PortRead, u64::from(port));
            }
            Outcome::Continue
        }
        VcpuExit::MmioRead(addr, data) => {
            stats.mmio += 1;
            stats.unmapped += 1;
            data.fill(UNMAPPED_READ_VALUE);
            warn_once(warned, log, Access::MmioRead, addr);
            Outcome::Continue
        }
        VcpuExit::MmioWrite(addr, _) => {
            stats.mmio += 1;
            stats.unmapped += 1;
            warn_once(warned, log, Access::MmioWrite, addr);
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

fn warn_once(warned: &mut HashSet<(Access, u64)>, log: UnmappedLog, access: Access, addr: u64) {
    if warned.insert((access, addr)) && log == UnmappedLog::Warn {
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
        warn_once(&mut warned, UnmappedLog::Warn, Access::PortRead, 0x80);
        warn_once(&mut warned, UnmappedLog::Warn, Access::PortRead, 0x80);
        warn_once(&mut warned, UnmappedLog::Warn, Access::PortWrite, 0x80);
        assert_eq!(warned.len(), 2);
    }

    #[test]
    fn errors_explain_themselves() {
        let e = VcpuError::Shutdown { rip: Some(0x1234) };
        assert!(e.to_string().contains("triple fault"));
        assert!(e.to_string().contains("0x1234"));
        let e = VcpuError::FailEntry { reason: 7, cpu: 0 };
        assert!(e.to_string().contains("0x7"));
        let e = VcpuError::Msrs {
            written: 3,
            expected: 11,
        };
        assert!(e.to_string().contains("3 of 11"));
    }

    #[test]
    fn long_mode_registers_enable_paging_and_64_bit_code() {
        let mut sregs = kvm_sregs::default();
        long_mode_sregs(&mut sregs);
        assert_eq!(
            sregs.cr0 & (X86_CR0_PE | X86_CR0_PG),
            X86_CR0_PE | X86_CR0_PG
        );
        assert_ne!(sregs.cr4 & X86_CR4_PAE, 0);
        assert_eq!(sregs.efer & (EFER_LME | EFER_LMA), EFER_LME | EFER_LMA);
        assert_eq!(sregs.cr3, PML4_START);
        assert_eq!((sregs.cs.selector, sregs.cs.l), (0x08, 1));
        assert_eq!((sregs.ss.selector, sregs.tr.selector), (0x10, 0x18));
        assert_eq!(sregs.gdt.limit, 31, "four 8-byte descriptors");
    }

    #[test]
    fn lapic_lint_pins_get_their_delivery_modes() {
        let mut lapic = kvm_lapic_state::default();
        // A masked LVT0 with vector 0x30, as it might be after reset.
        let before = 0x0001_0030u32.to_le_bytes();
        for (i, b) in before.iter().enumerate() {
            lapic.regs[APIC_LVT0 + i] = *b as libc::c_char;
        }
        set_lvt_delivery_mode(&mut lapic, APIC_LVT0, APIC_MODE_EXTINT);
        let r = &lapic.regs[APIC_LVT0..APIC_LVT0 + 4];
        let after = u32::from_le_bytes([r[0] as u8, r[1] as u8, r[2] as u8, r[3] as u8]);
        assert_eq!(after, 0x0001_0730, "only bits 8 to 10 change");
    }

    #[test]
    fn cpuid_gets_the_apic_id_and_hypervisor_bit() {
        let mut cpuid = CpuId::new(2).unwrap();
        cpuid.as_mut_slice()[0].function = CPUID_LEAF_FEATURES;
        cpuid.as_mut_slice()[0].ebx = 0xff00_0800;
        cpuid.as_mut_slice()[1].function = CPUID_LEAF_EXT_TOPOLOGY;
        patch_cpuid(&mut cpuid, 3);
        let e = cpuid.as_slice();
        assert_eq!(e[0].ebx, 0x0300_0800);
        assert_ne!(e[0].ecx & CPUID_ECX_HYPERVISOR, 0);
        assert_eq!(e[1].edx, 3);
    }

    #[test]
    fn exit_control_keeps_the_first_request() {
        let ctl = ExitControl::new().unwrap();
        assert_eq!(ctl.requested(), None);
        ctl.request(StopReason::Reset);
        ctl.request(StopReason::Quit);
        assert_eq!(ctl.requested(), Some(StopReason::Reset));
        ctl.wait().unwrap(); // returns at once: requests wake the waiter
    }
}
