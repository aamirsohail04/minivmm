// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Booting a Linux `bzImage` (milestone M2).
//!
//! This follows the x86 Linux boot protocol (Documentation/arch/x86/boot.rst
//! in the kernel tree) and enters the kernel at its 64-bit entry point, so
//! no firmware and no real-mode setup code runs. Before the first `KVM_RUN`
//! the VMM puts everything the kernel expects into guest memory:
//!
//! ```text
//! 0x0000_0500  GDT: null, 64-bit code, data, TSS          (GDT_START)
//! 0x0000_0520  IDT: one empty entry, interrupts stay off  (IDT_START)
//! 0x0000_7000  boot_params, the "zero page"               (ZERO_PAGE_START)
//! 0x0000_8ff0  initial stack pointer, grows down          (BOOT_STACK_POINTER)
//! 0x0000_9000  page tables: PML4, PDPT, PD                (PML4_START..)
//! 0x0002_0000  kernel command line, NUL-terminated        (CMDLINE_START)
//! 0x0009_fc00  start of the EBDA: not RAM for the guest   (EBDA_START)
//! 0x0010_0000  protected-mode kernel from the bzImage     (HIMEM_START)
//! top of RAM   initrd, page aligned, below initrd_addr_max
//! ```
//!
//! `linux-loader` reads the bzImage and copies the kernel in, and writes
//! the command line and the zero page. The layout, the e820 map, the initrd
//! placement, the GDT and the page tables are this module's own code.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use kvm_bindings::kvm_segment;
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bootparam::{boot_e820_entry, boot_params, setup_header};
use linux_loader::loader::{self, BzImage, KernelLoader};
use vm_memory::{
    ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryError, GuestMemoryMmap,
};

/// The default kernel command line.
///
/// - `console=ttyS0`: kernel messages and the console go to the COM1 UART.
/// - `reboot=k`: reboot through the keyboard controller (port 0x64), which
///   minivmm turns into a clean exit.
/// - `panic=1`: on a kernel panic, reboot after one second, so a broken
///   guest stops the VM instead of hanging.
/// - `pci=off`: there is no PCI bus, so do not probe for one.
/// - `nomodule`: no loadable modules (there is nowhere to load them from).
/// - `8250.nr_uarts=1`: only COM1 exists, so do not probe COM2 to COM4.
/// - `i8042.*`: minivmm's keyboard controller only implements reset, so
///   skip the probes for a mouse port, multiplexing and plug and play.
pub const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off nomodule \
     8250.nr_uarts=1 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd";

/// Global descriptor table.
pub const GDT_START: u64 = 0x500;
/// Interrupt descriptor table (a single empty entry).
pub const IDT_START: u64 = 0x520;
/// The `boot_params` structure, known as the zero page.
pub const ZERO_PAGE_START: u64 = 0x7000;
/// Initial stack pointer for the kernel's 64-bit entry code.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// Page map level 4 table.
pub const PML4_START: u64 = 0x9000;
/// Page directory pointer table.
pub const PDPTE_START: u64 = 0xa000;
/// Page directory (512 entries of 2 MiB pages).
pub const PDE_START: u64 = 0xb000;
/// Kernel command line.
pub const CMDLINE_START: u64 = 0x2_0000;
/// Longest command line minivmm accepts, not counting the NUL.
pub const CMDLINE_MAX_LEN: usize = 4095;
/// Start of the extended BIOS data area; low RAM for the guest ends here.
pub const EBDA_START: u64 = 0x9_fc00;
/// Where the protected-mode kernel is loaded: 1 MiB.
pub const HIMEM_START: u64 = 0x10_0000;

/// Offset of the 64-bit entry point (`startup_64`) from the load address.
const ENTRY_64_OFFSET: u64 = 0x200;
/// Boot protocol 2.12 added `xloadflags`, which says if the 64-bit entry exists.
const MIN_BOOT_PROTOCOL: u16 = 0x020c;
/// `HdrS`, the magic number of the setup header.
const SETUP_HEADER_MAGIC: u32 = 0x5372_6448;
/// Offset of the setup header inside the bzImage (and the zero page).
const SETUP_HEADER_OFFSET: usize = 0x1f1;
/// `loadflags` bit 0: the protected-mode kernel is loaded at 1 MiB.
const LOADED_HIGH: u8 = 0x1;
/// `xloadflags` bit 0: the kernel has a 64-bit entry point at +0x200.
const XLF_KERNEL_64: u16 = 0x1;
/// `type_of_loader` value for a boot loader without an assigned ID.
const LOADER_TYPE_UNDEFINED: u8 = 0xff;
/// e820 type for usable RAM.
const E820_RAM: u32 = 1;
/// Page table entry bits: present and writable.
const PTE_PRESENT_RW: u64 = 0x3;
/// Page directory entry bit 7: this entry maps a 2 MiB page.
const PDE_PAGE_SIZE: u64 = 0x80;
/// Size of a large page, mapped by one page directory entry.
const LARGE_PAGE_SIZE: u64 = 0x20_0000;
const PAGE_SIZE: u64 = 0x1000;
const MIB: u64 = 1 << 20;

/// GDT index of the 64-bit code segment (selector 0x08).
pub const GDT_CODE: u8 = 1;
/// GDT index of the data segment (selector 0x10).
pub const GDT_DATA: u8 = 2;
/// GDT index of the task state segment (selector 0x18).
pub const GDT_TSS: u8 = 3;

/// The boot GDT. The flags words are the access byte (low 8 bits) and the
/// granularity nibble (bits 12 to 15) of a segment descriptor:
/// - `0xa09b`: present, ring 0, code, execute/read, long mode (L), 4 KiB granularity
/// - `0xc093`: present, ring 0, data, read/write, 32-bit (D/B), 4 KiB granularity
/// - `0x808b`: present, ring 0, busy 64-bit TSS
pub const GDT_TABLE: [u64; 4] = [
    gdt_entry(0, 0, 0),
    gdt_entry(0xa09b, 0, 0xfffff),
    gdt_entry(0xc093, 0, 0xfffff),
    gdt_entry(0x808b, 0, 0xfffff),
];

/// Errors from validating or loading a Linux guest.
#[derive(Debug)]
pub enum BootError {
    /// A kernel or initrd file could not be read.
    Read { path: PathBuf, source: io::Error },
    /// The kernel file is not a bzImage.
    NotBzImage { path: PathBuf },
    /// The kernel's boot protocol is too old for a 64-bit entry.
    ProtocolTooOld { path: PathBuf, version: u16 },
    /// The kernel has no 64-bit entry point (a 32-bit kernel).
    No64BitEntry { path: PathBuf },
    /// The initrd file is empty.
    EmptyInitrd { path: PathBuf },
    /// The command line is longer than the kernel or minivmm accepts.
    CmdlineTooLong { len: usize, max: usize },
    /// The command line has a character outside printable ASCII.
    CmdlineNotAscii,
    /// Guest RAM is too small for the kernel, or for the kernel and initrd.
    NotEnoughMemory { need_mib: u64, have_mib: u64 },
    /// `linux-loader` could not load the kernel.
    Load(loader::Error),
    /// `linux-loader` refused the command line.
    Cmdline(linux_loader::cmdline::Error),
    /// `linux-loader` could not write the zero page.
    BootParams(linux_loader::configurator::Error),
    /// Writing boot data into guest memory failed.
    Memory(GuestMemoryError),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            BootError::NotBzImage { path } => write!(
                f,
                "{} is not a Linux bzImage (no \"HdrS\" setup header at 0x202)",
                path.display()
            ),
            BootError::ProtocolTooOld { path, version } => write!(
                f,
                "{} uses boot protocol {}.{:02}; minivmm needs 2.12 or newer (Linux 3.8+)",
                path.display(),
                version >> 8,
                version & 0xff
            ),
            BootError::No64BitEntry { path } => write!(
                f,
                "{} has no 64-bit entry point; build an x86_64 kernel",
                path.display()
            ),
            BootError::EmptyInitrd { path } => write!(f, "initrd {} is empty", path.display()),
            BootError::CmdlineTooLong { len, max } => write!(
                f,
                "kernel command line is {len} bytes, the maximum is {max}"
            ),
            BootError::CmdlineNotAscii => write!(
                f,
                "kernel command line may only contain printable ASCII characters"
            ),
            BootError::NotEnoughMemory { need_mib, have_mib } => write!(
                f,
                "guest RAM is too small: the kernel and initrd need at least {need_mib} MiB, \
                 the VM has {have_mib} MiB (use --mem-mib)"
            ),
            BootError::Load(e) => write!(f, "cannot load the kernel: {e}"),
            BootError::Cmdline(e) => write!(f, "invalid kernel command line: {e}"),
            BootError::BootParams(e) => write!(f, "cannot write the zero page: {e}"),
            BootError::Memory(e) => write!(f, "cannot write boot data into guest memory: {e}"),
        }
    }
}

impl std::error::Error for BootError {}

impl From<GuestMemoryError> for BootError {
    fn from(e: GuestMemoryError) -> Self {
        BootError::Memory(e)
    }
}

/// A kernel file whose setup header has been checked.
#[derive(Debug, Clone)]
pub struct KernelImage {
    path: PathBuf,
    size: u64,
    header: setup_header,
}

impl KernelImage {
    /// Open `path` and check that it is a 64-bit bzImage minivmm can boot.
    pub fn open(path: &Path) -> Result<Self, BootError> {
        let read_err = |source| BootError::Read {
            path: path.to_path_buf(),
            source,
        };
        let mut file = File::open(path).map_err(read_err)?;
        let size = file.metadata().map_err(read_err)?.len();

        let mut head = vec![0u8; SETUP_HEADER_OFFSET + std::mem::size_of::<setup_header>()];
        match file.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(BootError::NotBzImage {
                    path: path.to_path_buf(),
                });
            }
            Err(e) => return Err(read_err(e)),
        }
        let header = Self::check_header(path, &head)?;
        Ok(Self {
            path: path.to_path_buf(),
            size,
            header,
        })
    }

    /// Validate the setup header in the first bytes of a bzImage.
    fn check_header(path: &Path, head: &[u8]) -> Result<setup_header, BootError> {
        let not_bzimage = || BootError::NotBzImage {
            path: path.to_path_buf(),
        };
        // `setup_header` is packed (alignment 1), so any slice offset works.
        let header = *head
            .get(SETUP_HEADER_OFFSET..)
            .and_then(setup_header::from_slice)
            .ok_or_else(not_bzimage)?;

        // Fields of a packed struct are copied out before use: taking a
        // reference to an unaligned field is not allowed in Rust.
        let (magic, version, loadflags, xloadflags) = (
            header.header,
            header.version,
            header.loadflags,
            header.xloadflags,
        );
        if magic != SETUP_HEADER_MAGIC || loadflags & LOADED_HIGH == 0 {
            return Err(not_bzimage());
        }
        if version < MIN_BOOT_PROTOCOL {
            return Err(BootError::ProtocolTooOld {
                path: path.to_path_buf(),
                version,
            });
        }
        if xloadflags & XLF_KERNEL_64 == 0 {
            return Err(BootError::No64BitEntry {
                path: path.to_path_buf(),
            });
        }
        Ok(header)
    }

    /// The file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Boot protocol version, for example `0x020f` for 2.15.
    pub fn protocol(&self) -> u16 {
        self.header.version
    }

    /// Lowest end of RAM the kernel can boot with: it decompresses itself to
    /// `max(1 MiB, pref_address)` and needs `init_size` bytes from there.
    pub fn min_ram_end(&self) -> u64 {
        let pref = self.header.pref_address;
        let init_size = u64::from(self.header.init_size);
        HIMEM_START.max(pref).saturating_add(init_size)
    }

    /// Longest command line the kernel accepts, not counting the NUL.
    pub fn max_cmdline_len(&self) -> usize {
        // Protocol 2.06 and later report it; the header is at least 2.12 here.
        let kernel_max = self.header.cmdline_size as usize;
        kernel_max.min(CMDLINE_MAX_LEN)
    }

    /// Highest address the initrd may end at, plus one.
    fn initrd_limit(&self) -> u64 {
        u64::from(self.header.initrd_addr_max) + 1
    }
}

/// An initrd file that exists and is not empty.
#[derive(Debug, Clone)]
pub struct InitrdImage {
    path: PathBuf,
    size: u64,
}

impl InitrdImage {
    /// Check that `path` can be read and is not empty.
    pub fn open(path: &Path) -> Result<Self, BootError> {
        let size = std::fs::metadata(path)
            .map_err(|source| BootError::Read {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        if size == 0 {
            return Err(BootError::EmptyInitrd {
                path: path.to_path_buf(),
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            size,
        })
    }

    /// The file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Everything needed to boot Linux, validated before KVM is touched.
#[derive(Debug, Clone)]
pub struct LinuxBoot {
    /// The kernel to boot.
    pub kernel: KernelImage,
    /// Optional initial RAM filesystem.
    pub initrd: Option<InitrdImage>,
    /// The kernel command line.
    pub cmdline: String,
}

impl LinuxBoot {
    /// Validate the kernel, the initrd and the command line.
    pub fn new(kernel: &Path, initrd: Option<&Path>, cmdline: &str) -> Result<Self, BootError> {
        let kernel = KernelImage::open(kernel)?;
        let initrd = initrd.map(InitrdImage::open).transpose()?;
        if !cmdline.chars().all(|c| matches!(c, ' '..='~')) {
            return Err(BootError::CmdlineNotAscii);
        }
        let max = kernel.max_cmdline_len();
        if cmdline.len() > max {
            return Err(BootError::CmdlineTooLong {
                len: cmdline.len(),
                max,
            });
        }
        Ok(Self {
            kernel,
            initrd,
            cmdline: cmdline.to_string(),
        })
    }

    /// Smallest end of guest RAM that holds the decompressed kernel, the
    /// initrd above it and one spare page for alignment.
    pub fn ram_needed(&self) -> u64 {
        let initrd_size = self.initrd.as_ref().map_or(0, |i| i.size());
        self.kernel
            .min_ram_end()
            .saturating_add(initrd_size)
            .saturating_add(PAGE_SIZE)
    }

    /// Check that a VM with `mem_mib` MiB of RAM is big enough.
    pub fn check_memory(&self, mem_mib: u64) -> Result<(), BootError> {
        let need = self.ram_needed();
        let have = mem_mib.saturating_mul(MIB);
        if need > have {
            return Err(BootError::NotEnoughMemory {
                need_mib: need.div_ceil(MIB),
                have_mib: mem_mib,
            });
        }
        Ok(())
    }
}

/// Where things ended up in guest memory, for the vCPU setup and for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInfo {
    /// The kernel's 64-bit entry point (RIP at the first `KVM_RUN`).
    pub entry: u64,
    /// The zero page (RSI at the first `KVM_RUN`).
    pub zero_page: u64,
    /// Where the protected-mode kernel was copied.
    pub kernel_load: u64,
    /// Initrd start address and size, if there is one.
    pub initrd: Option<(u64, u64)>,
}

/// Load `boot` into guest memory and prepare everything the kernel's 64-bit
/// entry point expects.
pub fn load_linux(mem: &GuestMemoryMmap, boot: &LinuxBoot) -> Result<BootInfo, BootError> {
    let ram_end = mem.last_addr().0 + 1;

    // 1. Check the memory budget first, so a too-small VM gets a clear message
    //    instead of a kernel that fails while decompressing itself.
    boot.check_memory(ram_end / MIB)?;
    let kernel_area_end = boot.kernel.min_ram_end();

    // 2. The kernel: linux-loader skips the real-mode setup sectors and copies
    //    the protected-mode part to 1 MiB.
    let path = boot.kernel.path();
    let mut file = File::open(path).map_err(|source| BootError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let loaded = BzImage::load(
        mem,
        Some(GuestAddress(HIMEM_START)),
        &mut file,
        Some(GuestAddress(HIMEM_START)),
    )
    .map_err(BootError::Load)?;
    let kernel_load = loaded.kernel_load.0;

    // 3. The command line.
    let cmdline =
        Cmdline::try_from(&boot.cmdline, boot.cmdline.len() + 1).map_err(BootError::Cmdline)?;
    loader::load_cmdline(mem, GuestAddress(CMDLINE_START), &cmdline).map_err(BootError::Load)?;

    // 4. The initrd, at the top of usable memory.
    let initrd = match &boot.initrd {
        None => None,
        Some(image) => {
            let addr = initrd_address(
                ram_end,
                boot.kernel.initrd_limit(),
                kernel_area_end,
                image.size(),
            )
            .ok_or(BootError::NotEnoughMemory {
                need_mib: boot.ram_needed().div_ceil(MIB),
                have_mib: ram_end / MIB,
            })?;
            load_initrd(mem, image, addr)?;
            Some((addr, image.size()))
        }
    };

    // 5. The zero page: start from the kernel's own setup header, as the boot
    //    protocol asks, then fill in what the boot loader is responsible for.
    let mut params = boot_params {
        hdr: loaded.setup_header.unwrap_or(boot.kernel.header),
        ..Default::default()
    };
    params.hdr.type_of_loader = LOADER_TYPE_UNDEFINED;
    params.hdr.cmd_line_ptr = CMDLINE_START as u32;
    if let Some((addr, size)) = initrd {
        // Both fit in 32 bits: RAM ends below 4 GiB (see memory::MAX_MEM_MIB).
        params.hdr.ramdisk_image = addr as u32;
        params.hdr.ramdisk_size = size as u32;
    }
    let e820 = e820_map(ram_end);
    for (i, entry) in e820.iter().enumerate() {
        params.e820_table[i] = *entry;
    }
    params.e820_entries = e820.len() as u8;
    LinuxBootConfigurator::write_bootparams::<GuestMemoryMmap>(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE_START)),
        mem,
    )
    .map_err(BootError::BootParams)?;

    // 6. The descriptor tables and the identity-mapped page tables.
    write_gdt_and_idt(mem)?;
    write_page_tables(mem)?;

    Ok(BootInfo {
        entry: kernel_load + ENTRY_64_OFFSET,
        zero_page: ZERO_PAGE_START,
        kernel_load,
        initrd,
    })
}

/// Pick a page-aligned initrd address as high as possible: below the end of
/// RAM and below the kernel's `initrd_addr_max`, and above the area the
/// kernel decompresses into. `None` if it does not fit.
pub fn initrd_address(ram_end: u64, limit: u64, kernel_area_end: u64, size: u64) -> Option<u64> {
    let top = ram_end.min(limit);
    let addr = top.checked_sub(size)? & !(PAGE_SIZE - 1);
    (addr >= kernel_area_end).then_some(addr)
}

fn load_initrd(mem: &GuestMemoryMmap, image: &InitrdImage, addr: u64) -> Result<(), BootError> {
    let read_err = |source| BootError::Read {
        path: image.path().to_path_buf(),
        source,
    };
    let mut file = File::open(image.path()).map_err(read_err)?;
    // The size comes from the earlier check and fits in RAM, so it fits in usize.
    let len = image.size() as usize;
    mem.read_exact_volatile_from(GuestAddress(addr), &mut file, len)
        .map_err(|e| match e {
            GuestMemoryError::IOError(source) => read_err(source),
            other => BootError::Memory(other),
        })
}

/// The e820 memory map: what the guest may use as RAM.
///
/// Low RAM ends at the EBDA; 640 KiB to 1 MiB is the legacy video and BIOS
/// area, so it is left out (not RAM). Everything from 1 MiB to the end of
/// guest memory is usable.
pub fn e820_map(ram_end: u64) -> Vec<boot_e820_entry> {
    let mut map = vec![boot_e820_entry {
        addr: 0,
        size: EBDA_START,
        r#type: E820_RAM,
    }];
    if ram_end > HIMEM_START {
        map.push(boot_e820_entry {
            addr: HIMEM_START,
            size: ram_end - HIMEM_START,
            r#type: E820_RAM,
        });
    }
    map
}

/// Build a segment descriptor. `flags` holds the access byte (bits 0 to 7)
/// and the flags nibble (bits 12 to 15); see [`GDT_TABLE`].
pub const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let flags = flags as u64;
    let base = base as u64;
    let limit = limit as u64;
    ((base & 0xff00_0000) << (56 - 24))
        | ((flags & 0x0000_f0ff) << 40)
        | ((limit & 0x000f_0000) << (48 - 16))
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0x0000_ffff)
}

/// Decode a GDT entry into the form `KVM_SET_SREGS` takes. KVM needs the
/// hidden part of each segment register (base, limit, attributes) spelled
/// out, because no instruction will load it from the table.
pub fn segment_from_gdt(entry: u64, index: u8) -> kvm_segment {
    let base = ((entry & 0xff00_0000_0000_0000) >> 32) | ((entry & 0x0000_00ff_ffff_0000) >> 16);
    let raw_limit = (((entry & 0x000f_0000_0000_0000) >> 32) | (entry & 0xffff)) as u32;
    let bit = |n: u32| ((entry >> n) & 1) as u8;
    let g = bit(55);
    let limit = if g == 1 {
        (raw_limit << 12) | 0xfff
    } else {
        raw_limit
    };
    let present = bit(47);
    kvm_segment {
        base,
        limit,
        selector: u16::from(index) * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present,
        dpl: ((entry >> 45) & 0x3) as u8,
        db: bit(54),
        s: bit(44),
        l: bit(53),
        g,
        avl: bit(52),
        unusable: u8::from(present == 0),
        padding: 0,
    }
}

fn write_gdt_and_idt(mem: &GuestMemoryMmap) -> Result<(), BootError> {
    for (i, entry) in GDT_TABLE.iter().enumerate() {
        mem.write_obj(*entry, GuestAddress(GDT_START + (i as u64) * 8))?;
    }
    // One empty gate. The kernel loads its own IDT before it enables
    // interrupts, so this one is never used.
    mem.write_obj(0u64, GuestAddress(IDT_START))?;
    Ok(())
}

/// Identity-map the first 1 GiB with 2 MiB pages: virtual address equals
/// physical address, which is what the kernel's 64-bit entry requires.
/// The kernel replaces these tables with its own early in boot.
fn write_page_tables(mem: &GuestMemoryMmap) -> Result<(), BootError> {
    mem.write_obj(PDPTE_START | PTE_PRESENT_RW, GuestAddress(PML4_START))?;
    mem.write_obj(PDE_START | PTE_PRESENT_RW, GuestAddress(PDPTE_START))?;
    for i in 0..512u64 {
        let entry = (i * LARGE_PAGE_SIZE) | PDE_PAGE_SIZE | PTE_PRESENT_RW;
        mem.write_obj(entry, GuestAddress(PDE_START + i * 8))?;
    }
    Ok(())
}

// The zero page is exactly one page; linux-loader relies on that too.
const _: () = assert!(std::mem::size_of::<boot_params>() == 4096);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::create_guest_memory;

    /// A minimal fake bzImage: just a setup header with the given fields.
    fn fake_bzimage(version: u16, xloadflags: u16) -> Vec<u8> {
        let mut image = vec![0u8; 0x1000];
        let header = setup_header {
            setup_sects: 1,
            boot_flag: 0xaa55,
            header: SETUP_HEADER_MAGIC,
            version,
            loadflags: LOADED_HIGH,
            code32_start: HIMEM_START as u32,
            initrd_addr_max: 0x7fff_ffff,
            xloadflags,
            cmdline_size: 2047,
            init_size: 0x40_0000,
            pref_address: 0x100_0000,
            ..Default::default()
        };
        image[SETUP_HEADER_OFFSET..SETUP_HEADER_OFFSET + std::mem::size_of::<setup_header>()]
            .copy_from_slice(header.as_slice());
        image
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("minivmm-boot-{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn gdt_entries_match_known_descriptors() {
        // The 64-bit code descriptor every x86_64 boot loader uses.
        assert_eq!(GDT_TABLE[1], 0x00af_9b00_0000_ffff);
        assert_eq!(GDT_TABLE[2], 0x00cf_9300_0000_ffff);
        assert_eq!(GDT_TABLE[3], 0x008f_8b00_0000_ffff);
    }

    #[test]
    fn code_segment_decodes_as_64_bit() {
        let cs = segment_from_gdt(GDT_TABLE[GDT_CODE as usize], GDT_CODE);
        assert_eq!(cs.selector, 0x08);
        assert_eq!(cs.l, 1, "long mode bit");
        assert_eq!(cs.db, 0, "D/B must be 0 when L is 1");
        assert_eq!(cs.type_, 0xb, "execute/read, accessed");
        assert_eq!(cs.limit, 0xffff_ffff);
        assert_eq!((cs.present, cs.s, cs.g, cs.unusable), (1, 1, 1, 0));

        let ds = segment_from_gdt(GDT_TABLE[GDT_DATA as usize], GDT_DATA);
        assert_eq!((ds.selector, ds.type_, ds.db, ds.l), (0x10, 0x3, 1, 0));

        let tr = segment_from_gdt(GDT_TABLE[GDT_TSS as usize], GDT_TSS);
        assert_eq!((tr.selector, tr.type_, tr.s), (0x18, 0xb, 0));

        let null = segment_from_gdt(GDT_TABLE[0], 0);
        assert_eq!(null.unusable, 1);
    }

    #[test]
    fn segment_base_and_limit_round_trip() {
        let entry = gdt_entry(0xc093, 0x1234_5678, 0xabcde);
        let seg = segment_from_gdt(entry, 5);
        assert_eq!(seg.base, 0x1234_5678);
        assert_eq!(seg.limit, 0xabcd_efff, "4 KiB granularity scales the limit");
        assert_eq!(seg.selector, 40);
    }

    #[test]
    fn e820_leaves_out_the_legacy_hole() {
        let map = e820_map(64 * MIB);
        assert_eq!(map.len(), 2);
        let (a0, s0, a1, s1) = (map[0].addr, map[0].size, map[1].addr, map[1].size);
        assert_eq!((a0, s0), (0, EBDA_START));
        assert_eq!((a1, s1), (HIMEM_START, 63 * MIB));
    }

    #[test]
    fn initrd_goes_to_the_top_page_aligned() {
        let addr = initrd_address(64 * MIB, u64::MAX, 32 * MIB, 0x1234).unwrap();
        assert_eq!(addr, 64 * MIB - 0x2000);
        assert_eq!(addr % PAGE_SIZE, 0);
        assert!(addr + 0x1234 <= 64 * MIB);
    }

    #[test]
    fn initrd_respects_initrd_addr_max() {
        let addr = initrd_address(3072 * MIB, 0x8000_0000, 32 * MIB, MIB).unwrap();
        assert_eq!(addr, 0x8000_0000 - MIB);
    }

    #[test]
    fn initrd_never_overlaps_the_kernel() {
        assert_eq!(initrd_address(64 * MIB, u64::MAX, 60 * MIB, 8 * MIB), None);
        assert_eq!(
            initrd_address(MIB, u64::MAX, 0, 2 * MIB),
            None,
            "no underflow"
        );
    }

    #[test]
    fn page_tables_identity_map_the_first_gigabyte() {
        let mem = create_guest_memory(16).unwrap();
        write_page_tables(&mem).unwrap();
        let pml4: u64 = mem.read_obj(GuestAddress(PML4_START)).unwrap();
        let pdpt: u64 = mem.read_obj(GuestAddress(PDPTE_START)).unwrap();
        assert_eq!(pml4, PDPTE_START | 0x3);
        assert_eq!(pdpt, PDE_START | 0x3);
        let first: u64 = mem.read_obj(GuestAddress(PDE_START)).unwrap();
        let last: u64 = mem.read_obj(GuestAddress(PDE_START + 511 * 8)).unwrap();
        assert_eq!(first, 0x83);
        assert_eq!(last, (511 << 21) | 0x83);
    }

    #[test]
    fn accepts_a_64_bit_bzimage() {
        let path = temp_file("ok", &fake_bzimage(0x020f, XLF_KERNEL_64));
        let kernel = KernelImage::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(kernel.protocol(), 0x020f);
        assert_eq!(kernel.max_cmdline_len(), 2047);
        assert_eq!(kernel.min_ram_end(), 0x100_0000 + 0x40_0000);
    }

    #[test]
    fn rejects_files_that_are_not_bzimages() {
        let path = temp_file("short", b"not a kernel");
        let err = KernelImage::open(&path).unwrap_err();
        assert!(matches!(err, BootError::NotBzImage { .. }), "{err}");

        let mut image = fake_bzimage(0x020f, XLF_KERNEL_64);
        image[0x202] = b'X'; // break the magic
        std::fs::write(&path, &image).unwrap();
        let err = KernelImage::open(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, BootError::NotBzImage { .. }), "{err}");
    }

    #[test]
    fn rejects_old_or_32_bit_kernels() {
        let path = temp_file("old", &fake_bzimage(0x020a, XLF_KERNEL_64));
        let err = KernelImage::open(&path).unwrap_err();
        assert!(err.to_string().contains("2.10"), "{err}");

        std::fs::write(&path, fake_bzimage(0x020f, 0)).unwrap();
        let err = KernelImage::open(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, BootError::No64BitEntry { .. }), "{err}");
    }

    #[test]
    fn checks_the_command_line() {
        let path = temp_file("cmdline", &fake_bzimage(0x020f, XLF_KERNEL_64));
        let long = "a".repeat(2048);
        let err = LinuxBoot::new(&path, None, &long).unwrap_err();
        assert!(
            matches!(
                err,
                BootError::CmdlineTooLong {
                    len: 2048,
                    max: 2047
                }
            ),
            "{err}"
        );
        let err = LinuxBoot::new(&path, None, "console=ttyS0\n").unwrap_err();
        assert!(matches!(err, BootError::CmdlineNotAscii));
        LinuxBoot::new(&path, None, DEFAULT_CMDLINE).unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_an_empty_or_missing_initrd() {
        let path = temp_file("empty-initrd", b"");
        let err = InitrdImage::open(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, BootError::EmptyInitrd { .. }));
        let err = InitrdImage::open(Path::new("/nonexistent/initrd")).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/initrd"));
    }

    #[test]
    fn too_little_memory_is_reported_in_mib() {
        let path = temp_file("small-ram", &fake_bzimage(0x020f, XLF_KERNEL_64));
        let boot = LinuxBoot::new(&path, None, DEFAULT_CMDLINE).unwrap();
        let mem = create_guest_memory(16).unwrap();
        let err = load_linux(&mem, &boot).unwrap_err();
        let _ = std::fs::remove_file(&path);
        // 16 MiB pref_address + 4 MiB init_size + one page, rounded up.
        assert!(
            matches!(
                err,
                BootError::NotEnoughMemory {
                    need_mib: 21,
                    have_mib: 16
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn loads_a_kernel_and_initrd_and_fills_the_zero_page() {
        let kernel_path = temp_file("load-kernel", &fake_bzimage(0x020f, XLF_KERNEL_64));
        let initrd_path = temp_file("load-initrd", &[0x5a; 5000]);
        let boot = LinuxBoot::new(&kernel_path, Some(&initrd_path), "console=ttyS0").unwrap();
        let mem = create_guest_memory(64).unwrap();
        let info = load_linux(&mem, &boot).unwrap();
        let _ = std::fs::remove_file(&kernel_path);
        let _ = std::fs::remove_file(&initrd_path);

        assert_eq!(info.kernel_load, HIMEM_START);
        assert_eq!(info.entry, HIMEM_START + 0x200);
        let (initrd_addr, initrd_size) = info.initrd.unwrap();
        assert_eq!(initrd_size, 5000);
        assert_eq!(initrd_addr, 64 * MIB - 0x2000);
        let byte: u8 = mem.read_obj(GuestAddress(initrd_addr + 4999)).unwrap();
        assert_eq!(byte, 0x5a);

        let params: boot_params = mem.read_obj(GuestAddress(ZERO_PAGE_START)).unwrap();
        let hdr = params.hdr;
        let (loader, cmd_ptr, rd_image, rd_size, magic) = (
            hdr.type_of_loader,
            hdr.cmd_line_ptr,
            hdr.ramdisk_image,
            hdr.ramdisk_size,
            hdr.header,
        );
        assert_eq!(loader, 0xff);
        assert_eq!(cmd_ptr as u64, CMDLINE_START);
        assert_eq!((rd_image as u64, rd_size), (initrd_addr, 5000));
        assert_eq!(magic, SETUP_HEADER_MAGIC);
        assert_eq!(params.e820_entries, 2);

        let mut cmdline = [0u8; 14];
        mem.read_slice(&mut cmdline, GuestAddress(CMDLINE_START))
            .unwrap();
        assert_eq!(&cmdline, b"console=ttyS0\0");
    }
}
