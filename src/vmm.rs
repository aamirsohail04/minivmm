// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Build a VM from a [`VmConfig`] and run it.
//!
//! Setup order (each step is one or two ioctls):
//!
//! 1. open `/dev/kvm`, check the API version and required capabilities
//! 2. `KVM_CREATE_VM`, then `KVM_SET_TSS_ADDR` (needed by Intel VMX for
//!    real-mode guests on CPUs without unrestricted guest support)
//! 3. create guest RAM, load the payload into it (a flat real-mode image
//!    for M1, or a Linux kernel with its boot data for M2), then
//!    `KVM_SET_USER_MEMORY_REGION`
//! 4. Linux only: `KVM_CREATE_IRQCHIP` (PIC, IOAPIC and local APIC inside
//!    KVM) and `KVM_CREATE_PIT2` (the timer). This must come before the
//!    vCPU is created, so the vCPU gets a local APIC.
//! 5. the devices on the port I/O bus: the serial console (its interrupt
//!    registered with `KVM_IRQFD` for Linux) and, for Linux, the keyboard
//!    controller's reset line
//! 6. `KVM_CREATE_VCPU`, then its registers: real mode, or long mode at
//!    the kernel entry point
//!
//! Then [`Vmm::run`] starts the vCPU on its own thread and waits until the
//! guest stops, fails, or a stop is requested from the console.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use kvm_bindings::{KVM_PIT_SPEAKER_DUMMY, kvm_pit_config};
use kvm_ioctls::{Cap, Kvm, VmFd};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::errno;
use vmm_sys_util::signal::Killable;

use crate::boot::{self, BootError, BootInfo, LinuxBoot};
use crate::bus::{Bus, BusError};
use crate::console;
use crate::devices::i8042::{I8042_BASE, I8042_LEN, KeyboardController};
use crate::devices::serial::{COM1_BASE, COM1_IRQ, COM1_LEN, IrqLine, SerialConsole};
use crate::guest::GuestImage;
use crate::memory::{self, GUEST_LOAD_ADDR, MemoryError};
use crate::vcpu::{self, ExitControl, ExitStats, StopReason, UnmappedLog, Vcpu, VcpuError};

/// The only KVM API version that has ever shipped.
const KVM_API_VERSION: i32 = 12;
/// Three pages KVM needs for the TSS on Intel; must lie outside guest RAM.
/// This is the address Firecracker and Cloud Hypervisor use.
const KVM_TSS_ADDRESS: usize = 0xfffb_d000;
/// How often the main thread re-sends the kick while the vCPU winds down.
const KICK_INTERVAL: Duration = Duration::from_millis(1);

/// The console output sink: stdout for the binary, a buffer in tests.
pub type ConsoleOut = Box<dyn Write + Send>;

/// What runs in the VM.
#[derive(Debug, Clone)]
pub enum Payload {
    /// A flat 16-bit program, loaded at [`GUEST_LOAD_ADDR`] (milestone M1).
    RealMode(GuestImage),
    /// A Linux kernel, booted at its 64-bit entry point (milestone M2).
    Linux(LinuxBoot),
}

/// What to run.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Guest RAM size in MiB.
    pub mem_mib: u64,
    /// The program or kernel to run.
    pub payload: Payload,
    /// Print a warning for each unmapped address the guest touches. Useful
    /// for small test guests; Linux probes many legacy ports that do not
    /// exist here, so for Linux the accesses are usually only counted.
    pub warn_unmapped: bool,
}

/// Errors from building or running the VM.
#[derive(Debug)]
pub enum VmmError {
    /// `/dev/kvm` could not be opened.
    OpenKvm(errno::Error),
    /// The kernel reports an unknown KVM API version.
    ApiVersion(i32),
    /// The kernel lacks a capability minivmm needs.
    MissingCapability(&'static str),
    /// `KVM_CREATE_VM` failed.
    CreateVm(errno::Error),
    /// `KVM_SET_TSS_ADDR` failed.
    SetTss(errno::Error),
    /// `KVM_CREATE_IRQCHIP` failed.
    CreateIrqchip(errno::Error),
    /// `KVM_CREATE_PIT2` failed.
    CreatePit(errno::Error),
    /// `KVM_IRQFD` failed for the serial interrupt.
    RegisterIrqfd(errno::Error),
    /// Guest memory setup failed.
    Memory(MemoryError),
    /// Loading the Linux kernel, initrd or boot data failed.
    Boot(BootError),
    /// Device registration failed.
    Bus(BusError),
    /// An eventfd could not be created.
    EventFd(io::Error),
    /// vCPU creation, setup or execution failed.
    Vcpu(VcpuError),
    /// The vCPU kick signal handler could not be installed.
    Signal(errno::Error),
    /// A thread could not be started.
    Thread(io::Error),
    /// Waiting for the vCPU failed.
    Wait(io::Error),
    /// The vCPU thread panicked (a bug in minivmm).
    VcpuPanicked,
}

impl fmt::Display for VmmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmmError::OpenKvm(e) => {
                write!(f, "cannot open /dev/kvm: {e}")?;
                match e.errno() {
                    libc::ENOENT => write!(
                        f,
                        "\n  hint: KVM is not available. Enable AMD-V or VT-x in the firmware; \
                         in a VM, enable nested virtualization (VMware: \"Virtualize Intel \
                         VT-x/EPT or AMD-V/RVI\")"
                    ),
                    libc::EACCES | libc::EPERM => write!(
                        f,
                        "\n  hint: run `sudo usermod -aG kvm $USER`, then log out and back in \
                         (check with `id`)"
                    ),
                    _ => Ok(()),
                }
            }
            VmmError::ApiVersion(v) => write!(
                f,
                "unsupported KVM API version {v} (expected {KVM_API_VERSION})"
            ),
            VmmError::MissingCapability(c) => write!(f, "KVM lacks required capability {c}"),
            VmmError::CreateVm(e) => write!(f, "KVM_CREATE_VM failed: {e}"),
            VmmError::SetTss(e) => write!(f, "KVM_SET_TSS_ADDR failed: {e}"),
            VmmError::CreateIrqchip(e) => write!(f, "KVM_CREATE_IRQCHIP failed: {e}"),
            VmmError::CreatePit(e) => write!(f, "KVM_CREATE_PIT2 failed: {e}"),
            VmmError::RegisterIrqfd(e) => {
                write!(f, "KVM_IRQFD failed for the serial interrupt: {e}")
            }
            VmmError::Memory(e) => write!(f, "{e}"),
            VmmError::Boot(e) => write!(f, "{e}"),
            VmmError::Bus(e) => write!(f, "device setup failed: {e}"),
            VmmError::EventFd(e) => write!(f, "cannot create an eventfd: {e}"),
            VmmError::Vcpu(e) => write!(f, "{e}"),
            VmmError::Signal(e) => write!(f, "cannot install the vCPU kick handler: {e}"),
            VmmError::Thread(e) => write!(f, "cannot start a thread: {e}"),
            VmmError::Wait(e) => write!(f, "waiting for the vCPU failed: {e}"),
            VmmError::VcpuPanicked => write!(f, "the vCPU thread panicked (a minivmm bug)"),
        }
    }
}

impl std::error::Error for VmmError {}

impl From<MemoryError> for VmmError {
    fn from(e: MemoryError) -> Self {
        VmmError::Memory(e)
    }
}

impl From<VcpuError> for VmmError {
    fn from(e: VcpuError) -> Self {
        VmmError::Vcpu(e)
    }
}

impl From<BootError> for VmmError {
    fn from(e: BootError) -> Self {
        VmmError::Boot(e)
    }
}

/// The result of a completed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    /// Why the guest stopped.
    pub reason: StopReason,
    /// VM exit counts.
    pub stats: ExitStats,
    /// Console bytes the host side could not accept.
    pub lost_console_bytes: u64,
}

/// A built VM, ready to run.
///
/// Field order matters: Rust drops fields top to bottom, so the vCPU and
/// the VM file descriptors close before the guest memory is unmapped. KVM
/// must never be left holding a memory slot that points at freed pages.
pub struct Vmm {
    vcpu: Vcpu,
    pio_bus: Arc<Bus>,
    serial: Arc<Mutex<SerialConsole<ConsoleOut>>>,
    ctl: Arc<ExitControl>,
    boot: Option<BootInfo>,
    _vm: VmFd,
    _kvm: Kvm,
    _guest_memory: GuestMemoryMmap,
}

impl Vmm {
    /// Build the VM. Guest console output is written to `console`.
    pub fn new(config: &VmConfig, console: ConsoleOut) -> Result<Self, VmmError> {
        let is_linux = matches!(config.payload, Payload::Linux(_));

        let kvm = Kvm::new().map_err(VmmError::OpenKvm)?;
        let version = kvm.get_api_version();
        if version != KVM_API_VERSION {
            return Err(VmmError::ApiVersion(version));
        }
        let mut needed = vec![(Cap::UserMemory, "KVM_CAP_USER_MEMORY")];
        if is_linux {
            needed.extend([
                (Cap::Irqchip, "KVM_CAP_IRQCHIP"),
                (Cap::Irqfd, "KVM_CAP_IRQFD"),
                (Cap::Pit2, "KVM_CAP_PIT2"),
                (Cap::ExtCpuid, "KVM_CAP_EXT_CPUID"),
            ]);
        }
        for (cap, name) in needed {
            if !kvm.check_extension(cap) {
                return Err(VmmError::MissingCapability(name));
            }
        }

        let vm = kvm.create_vm().map_err(VmmError::CreateVm)?;
        vm.set_tss_address(KVM_TSS_ADDRESS)
            .map_err(VmmError::SetTss)?;

        let guest_memory = memory::create_guest_memory(config.mem_mib)?;
        let boot = match &config.payload {
            Payload::RealMode(image) => {
                memory::load_image(&guest_memory, GUEST_LOAD_ADDR, image.bytes())?;
                None
            }
            Payload::Linux(linux) => Some(boot::load_linux(&guest_memory, linux)?),
        };
        memory::register_with_kvm(&vm, &guest_memory)?;

        if is_linux {
            vm.create_irq_chip().map_err(VmmError::CreateIrqchip)?;
            // The dummy speaker makes KVM answer port 0x61 too, which Linux
            // reads while calibrating the timer.
            let pit = kvm_pit_config {
                flags: KVM_PIT_SPEAKER_DUMMY,
                ..Default::default()
            };
            vm.create_pit2(pit).map_err(VmmError::CreatePit)?;
        }

        let ctl = Arc::new(ExitControl::new().map_err(VmmError::EventFd)?);
        let irq = IrqLine::new().map_err(VmmError::EventFd)?;
        if is_linux {
            // KVM keeps its own reference to the eventfd, so moving `irq`
            // into the UART below does not affect the registration.
            vm.register_irqfd(irq.eventfd(), COM1_IRQ)
                .map_err(VmmError::RegisterIrqfd)?;
        }
        let serial = Arc::new(Mutex::new(SerialConsole::new(irq, console)));
        let mut pio_bus = Bus::new();
        pio_bus
            .insert("serial (COM1)", serial.clone(), COM1_BASE, COM1_LEN)
            .map_err(VmmError::Bus)?;
        if is_linux {
            let kbd = Arc::new(Mutex::new(KeyboardController::new(ctl.clone())));
            pio_bus
                .insert("i8042 (reset)", kbd, I8042_BASE, I8042_LEN)
                .map_err(VmmError::Bus)?;
        }

        let mut vcpu = Vcpu::new(&vm, 0)?;
        match &boot {
            // The stack grows down from the load address, into free low memory.
            None => vcpu.setup_real_mode(GUEST_LOAD_ADDR, GUEST_LOAD_ADDR)?,
            Some(info) => vcpu.setup_long_mode(&kvm, info)?,
        }
        vcpu.set_unmapped_log(if config.warn_unmapped {
            UnmappedLog::Warn
        } else {
            UnmappedLog::Count
        });

        Ok(Self {
            vcpu,
            pio_bus: Arc::new(pio_bus),
            serial,
            ctl,
            boot,
            _vm: vm,
            _kvm: kvm,
            _guest_memory: guest_memory,
        })
    }

    /// Port I/O device map: (name, base, length).
    pub fn devices(&self) -> Vec<(&'static str, u64, u64)> {
        self.pio_bus.describe()
    }

    /// Where the kernel, zero page and initrd were placed (Linux only).
    pub fn boot_info(&self) -> Option<BootInfo> {
        self.boot
    }

    /// Feed `input` to the guest's serial port on a background thread.
    ///
    /// The thread is not joined: it may be blocked reading a terminal when
    /// the guest stops, and it ends with the process.
    pub fn attach_console_input<R: Read + Send + 'static>(&self, input: R) -> io::Result<()> {
        console::spawn_input_thread(input, self.serial.clone(), self.ctl.clone()).map(drop)
    }

    /// Run the guest until it stops, fails, or a stop is requested.
    ///
    /// The vCPU runs on its own thread; this thread sleeps until something
    /// happens, then makes sure the vCPU has left `KVM_RUN` and its thread
    /// has ended before anything is torn down.
    pub fn run(self) -> Result<RunSummary, VmmError> {
        vcpu::register_kick_handler().map_err(VmmError::Signal)?;

        let bus = Arc::clone(&self.pio_bus);
        let ctl = Arc::clone(&self.ctl);
        // Move the vCPU out; the rest of `self` (VM, memory) stays here and
        // outlives the vCPU thread, which is joined below.
        let mut vcpu = self.vcpu;
        let handle = thread::Builder::new()
            .name(format!("vcpu{}", vcpu.id()))
            .spawn(move || {
                let result = vcpu.run(&bus, &ctl);
                ctl.wake_main();
                (vcpu, result)
            })
            .map_err(VmmError::Thread)?;

        let waited = self.ctl.wait();

        // Whatever woke us, the vCPU must be stopped before returning. If it
        // has not ended on its own, request a stop and kick it out of
        // KVM_RUN, where it may be sleeping on a guest HLT. Repeat the kick:
        // one sent just before the thread enters KVM_RUN would be missed.
        if !handle.is_finished() && self.ctl.requested().is_none() {
            self.ctl.request(StopReason::Quit);
        }
        while !handle.is_finished() {
            // Fails only if the thread has already exited; the loop then ends.
            let _ = handle.kill(vcpu::kick_signal());
            thread::sleep(KICK_INTERVAL);
        }
        let (vcpu, result) = handle.join().map_err(|_| VmmError::VcpuPanicked)?;
        waited.map_err(VmmError::Wait)?;
        let reason = result?;

        let lost_console_bytes = match self.serial.lock() {
            Ok(s) => s.lost_bytes(),
            Err(p) => p.into_inner().lost_bytes(),
        };
        let stats = vcpu.stats();
        // Close the vCPU before the remaining fields of `self` (VM, then
        // memory) are dropped when this function returns.
        drop(vcpu);
        Ok(RunSummary {
            reason,
            stats,
            lost_console_bytes,
        })
    }
}
