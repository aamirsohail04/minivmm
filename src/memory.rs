// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Guest physical memory.
//!
//! M1 uses one RAM region starting at guest physical address 0. The region
//! is an anonymous host mapping created by `vm-memory`; KVM is told about it
//! with `KVM_SET_USER_MEMORY_REGION`, after which the guest's accesses to
//! that range hit the host pages directly, with no VM exit.
//!
//! 0x0000_0000  real-mode interrupt vector table and BIOS data area (unused)
//! 0x0000_1000  GUEST_LOAD_ADDR: the guest image and its entry point
//! 0x0001_0000  REAL_MODE_LIMIT: end of what CS = 0 can execute
//! 0x0001_0000+ rest of RAM, up to --mem-mib

use std::fmt;

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{
    Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
};

/// Where the guest image is loaded and where the vCPU starts.
pub const GUEST_LOAD_ADDR: u64 = 0x1000;
/// With all segment bases at 0, 16-bit code can only run below 64 KiB.
pub const REAL_MODE_LIMIT: u64 = 0x1_0000;
/// Smallest RAM size accepted (the image must fit below 64 KiB anyway).
pub const MIN_MEM_MIB: u64 = 1;
/// Largest RAM size accepted. RAM must stay below the 4 GiB boundary, where
/// the KVM TSS and (from M3) the device MMIO windows live.
pub const MAX_MEM_MIB: u64 = 3072;

/// Errors from building or registering guest memory.
#[derive(Debug)]
pub enum MemoryError {
    /// The requested size is outside `MIN_MEM_MIB..=MAX_MEM_MIB`.
    InvalidSize { mib: u64 },
    /// The host mapping could not be created.
    Create(String),
    /// The image does not fit in guest memory below `REAL_MODE_LIMIT`.
    ImageOutOfRange { addr: u64, len: u64 },
    /// Copying the image into guest memory failed.
    Write(GuestMemoryError),
    /// A region's host address could not be found.
    HostAddress(GuestMemoryError),
    /// KVM refused the memory slot.
    Register {
        slot: u32,
        source: vmm_sys_util::errno::Error,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::InvalidSize { mib } => write!(
                f,
                "memory size {mib} MiB is out of range ({MIN_MEM_MIB} to {MAX_MEM_MIB} MiB)"
            ),
            MemoryError::Create(e) => write!(f, "cannot create guest memory: {e}"),
            MemoryError::ImageOutOfRange { addr, len } => write!(
                f,
                "image of {len} bytes at {addr:#x} does not fit in guest memory below {REAL_MODE_LIMIT:#x}"
            ),
            MemoryError::Write(e) => write!(f, "cannot copy the image into guest memory: {e}"),
            MemoryError::HostAddress(e) => write!(f, "cannot find a region's host address: {e}"),
            MemoryError::Register { slot, source } => {
                write!(f, "KVM refused memory slot {slot}: {source}")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

/// Create guest RAM of `mib` MiB starting at guest physical address 0.
pub fn create_guest_memory(mib: u64) -> Result<GuestMemoryMmap, MemoryError> {
    if !(MIN_MEM_MIB..=MAX_MEM_MIB).contains(&mib) {
        return Err(MemoryError::InvalidSize { mib });
    }
    let size = usize::try_from(mib << 20).map_err(|_| MemoryError::InvalidSize { mib })?;
    GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), size)])
        .map_err(|e| MemoryError::Create(format!("{e:?}")))
}

/// Copy `image` into guest memory at `addr`.
///
/// The image must end at or below [`REAL_MODE_LIMIT`] and inside RAM.
pub fn load_image(mem: &GuestMemoryMmap, addr: u64, image: &[u8]) -> Result<(), MemoryError> {
    let len = image.len() as u64;
    let fits = addr
        .checked_add(len)
        .is_some_and(|end| end <= REAL_MODE_LIMIT && end <= mem.last_addr().0.saturating_add(1));
    if !fits {
        return Err(MemoryError::ImageOutOfRange { addr, len });
    }
    // `write_slice` is bounds-checked by vm-memory as a second line of defence.
    mem.write_slice(image, GuestAddress(addr))
        .map_err(MemoryError::Write)
}

/// Tell KVM about every RAM region, one memory slot per region.
pub fn register_with_kvm(vm: &VmFd, mem: &GuestMemoryMmap) -> Result<(), MemoryError> {
    for (index, region) in mem.iter().enumerate() {
        // The VMM creates a handful of regions at most, so this never truncates.
        let slot = index as u32;
        let host_addr = mem
            .get_host_address(region.start_addr())
            .map_err(MemoryError::HostAddress)?;
        let slot_desc = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: region.start_addr().0,
            memory_size: region.len(),
            userspace_addr: host_addr as u64,
        };
        // SAFETY: `host_addr` is the start of a live mapping of exactly
        // `region.len()` bytes owned by `mem`. The caller keeps `mem` alive
        // for as long as the VM exists (the `Vmm` struct drops the VM before
        // the memory), so KVM never sees a dangling mapping.
        unsafe { vm.set_user_memory_region(slot_desc) }
            .map_err(|source| MemoryError::Register { slot, source })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_memory_of_the_requested_size() {
        let mem = create_guest_memory(2).unwrap();
        assert_eq!(mem.last_addr(), GuestAddress((2 << 20) - 1));
    }

    #[test]
    fn rejects_sizes_out_of_range() {
        assert!(matches!(
            create_guest_memory(0),
            Err(MemoryError::InvalidSize { mib: 0 })
        ));
        assert!(matches!(
            create_guest_memory(MAX_MEM_MIB + 1),
            Err(MemoryError::InvalidSize { .. })
        ));
    }

    #[test]
    fn loads_an_image_where_asked() {
        let mem = create_guest_memory(1).unwrap();
        load_image(&mem, GUEST_LOAD_ADDR, &[1, 2, 3]).unwrap();
        let mut back = [0u8; 3];
        mem.read_slice(&mut back, GuestAddress(GUEST_LOAD_ADDR))
            .unwrap();
        assert_eq!(back, [1, 2, 3]);
    }

    #[test]
    fn rejects_an_image_crossing_the_real_mode_limit() {
        let mem = create_guest_memory(1).unwrap();
        let image = vec![0u8; 0x10];
        let err = load_image(&mem, REAL_MODE_LIMIT - 8, &image).unwrap_err();
        assert!(matches!(err, MemoryError::ImageOutOfRange { .. }));
    }

    #[test]
    fn rejects_an_address_that_overflows() {
        let mem = create_guest_memory(1).unwrap();
        let err = load_image(&mem, u64::MAX, &[0, 0]).unwrap_err();
        assert!(matches!(err, MemoryError::ImageOutOfRange { .. }));
    }
}
