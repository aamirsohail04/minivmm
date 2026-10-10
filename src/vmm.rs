// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Build a VM from a [`VmConfig`] and run it.
//!
//! Setup order (each step is one or two ioctls):
//!
//! 1. open `/dev/kvm`, check the API version and required capabilities
//! 2. `KVM_CREATE_VM`, then `KVM_SET_TSS_ADDR` (needed by Intel VMX for
//!    real-mode guests on CPUs without unrestricted guest support)
//! 3. create guest RAM, copy the image in, `KVM_SET_USER_MEMORY_REGION`
//! 4. put the serial console on the port I/O bus
//! 5. `KVM_CREATE_VCPU`, then set real-mode segment and general registers
//!
//! Then [`Vmm::run`] drives the vCPU on the calling thread. M1 has one vCPU
//! and no device threads, so nothing else runs concurrently yet.

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use kvm_ioctls::{Cap, Kvm, VmFd};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::errno;

use crate::bus::{Bus, BusError};
use crate::devices::serial::{COM1_BASE, COM1_LEN, IrqLine, SerialConsole};
use crate::guest::GuestImage;
use crate::memory::{self, GUEST_LOAD_ADDR, MemoryError};
use crate::vcpu::{ExitStats, StopReason, Vcpu, VcpuError};

/// The only KVM API version that has ever shipped.
const KVM_API_VERSION: i32 = 12;
/// Three pages KVM needs for the TSS on Intel; must lie outside guest RAM.
/// This is the address Firecracker and Cloud Hypervisor use.
const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

/// The console output sink: stdout for the binary, a buffer in tests.
pub type ConsoleOut = Box<dyn Write + Send>;

/// What to run.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Guest RAM size in MiB.
    pub mem_mib: u64,
    /// The program to load at [`GUEST_LOAD_ADDR`].
    pub image: GuestImage,
}

/// Errors from building or running the VM.
#[derive(Debug)]
pub enum VmmError {
    /// `/dev/kvm` could not be opened.
    OpenKvm(errno::Error),
    /// The kernel reports an unknown KVM API version.
    ApiVersion(i32),
    /// The kernel lacks a capability M1 needs.
    MissingCapability(&'static str),
    /// `KVM_CREATE_VM` failed.
    CreateVm(errno::Error),
    /// `KVM_SET_TSS_ADDR` failed.
    SetTss(errno::Error),
    /// Guest memory setup failed.
    Memory(MemoryError),
    /// Device registration failed.
    Bus(BusError),
    /// The serial interrupt eventfd could not be created.
    IrqLine(io::Error),
    /// vCPU creation, setup or execution failed.
    Vcpu(VcpuError),
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
            VmmError::Memory(e) => write!(f, "{e}"),
            VmmError::Bus(e) => write!(f, "device setup failed: {e}"),
            VmmError::IrqLine(e) => write!(f, "cannot create interrupt eventfd: {e}"),
            VmmError::Vcpu(e) => write!(f, "{e}"),
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
    pio_bus: Bus,
    serial: Arc<Mutex<SerialConsole<ConsoleOut>>>,
    _vm: VmFd,
    _kvm: Kvm,
    _guest_memory: GuestMemoryMmap,
}

impl Vmm {
    /// Build the VM. Guest console output is written to `console`.
    pub fn new(config: &VmConfig, console: ConsoleOut) -> Result<Self, VmmError> {
        let kvm = Kvm::new().map_err(VmmError::OpenKvm)?;
        let version = kvm.get_api_version();
        if version != KVM_API_VERSION {
            return Err(VmmError::ApiVersion(version));
        }
        if !kvm.check_extension(Cap::UserMemory) {
            return Err(VmmError::MissingCapability("KVM_CAP_USER_MEMORY"));
        }

        let vm = kvm.create_vm().map_err(VmmError::CreateVm)?;
        vm.set_tss_address(KVM_TSS_ADDRESS)
            .map_err(VmmError::SetTss)?;

        let guest_memory = memory::create_guest_memory(config.mem_mib)?;
        memory::load_image(&guest_memory, GUEST_LOAD_ADDR, config.image.bytes())?;
        memory::register_with_kvm(&vm, &guest_memory)?;

        let irq = IrqLine::new().map_err(VmmError::IrqLine)?;
        let serial = Arc::new(Mutex::new(SerialConsole::new(irq, console)));
        let mut pio_bus = Bus::new();
        pio_bus
            .insert("serial (COM1)", serial.clone(), COM1_BASE, COM1_LEN)
            .map_err(VmmError::Bus)?;

        let mut vcpu = Vcpu::new(&vm, 0)?;
        // The stack grows down from the load address, into free low memory.
        vcpu.setup_real_mode(GUEST_LOAD_ADDR, GUEST_LOAD_ADDR)?;

        Ok(Self {
            vcpu,
            pio_bus,
            serial,
            _vm: vm,
            _kvm: kvm,
            _guest_memory: guest_memory,
        })
    }

    /// Port I/O device map: (name, base, length).
    pub fn devices(&self) -> Vec<(&'static str, u64, u64)> {
        self.pio_bus.describe()
    }

    /// Run the guest on the calling thread until it halts or fails.
    pub fn run(&mut self) -> Result<RunSummary, VmmError> {
        let reason = self.vcpu.run(&self.pio_bus)?;
        let lost_console_bytes = match self.serial.lock() {
            Ok(s) => s.lost_bytes(),
            Err(p) => p.into_inner().lost_bytes(),
        };
        Ok(RunSummary {
            reason,
            stats: self.vcpu.stats(),
            lost_console_bytes,
        })
    }
}
