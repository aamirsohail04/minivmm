# minivmm

A small virtual machine monitor (VMM) written in Rust on the
[rust-vmm](https://github.com/rust-vmm) crates, built one milestone at a time
to learn how a KVM-based hypervisor works from the inside.

**Current milestone: M2.** minivmm boots a Linux `bzImage` with a busybox
initramfs to an interactive shell on an emulated 16550A serial console. The
M1 mode (a 16-bit real-mode guest that prints and halts) is still there.

| Milestone | Goal | Status |
| --- | --- | --- |
| M1 | Hello guest + serial console | Done (`v0.1-m1`) |
| M2 | Boot a Linux kernel to a serial console | Done (`v0.2-m2`) |
| M3 | virtio-blk root filesystem (virtio-mmio) | Next |
| M4 | Robustness tests and fuzzing | Planned |
| M5 | virtio-net and multiple vCPUs | Planned |

## Quick start

On an x86_64 Linux machine (or VM with nested virtualization) with KVM:

```
./scripts/check-host.sh        # checks KVM, permissions and the tools
cargo test                     # unit and end-to-end tests
cargo run --release            # run the built-in M1 hello guest
```

## Boot Linux (M2)

You need a 64-bit kernel and the small initramfs this repository builds.
On Ubuntu, the host's own kernel works as the guest kernel:

```
sudo apt install busybox-static cpio
./scripts/make-initramfs.sh                    # writes guest/initramfs.cpio.gz
sudo cp /boot/vmlinuz-$(uname -r) guest/bzImage
sudo chown $USER: guest/bzImage                # /boot is readable by root only

cargo run --release -- --kernel guest/bzImage --initrd guest/initramfs.cpio.gz
```

The kernel log scrolls by, then:

```
minivmm guest: Linux 6.8.0-146-generic, 1 vCPU, 458 MiB RAM
Type 'reboot -f' to stop the VM, or press Ctrl-A x.

minivmm:~#
```

Every key goes to the guest, including Ctrl-C. To stop:

- `reboot -f` in the guest: minivmm prints `guest rebooted` and exits 0.
- **Ctrl-A x**: minivmm stops the VM from the outside (`stopped from the console`).

`poweroff` does not work yet: powering off needs ACPI, which minivmm does
not emulate, so the guest only halts its CPU. Use Ctrl-A x after it.

Useful options:

```
-v, --verbose        boot layout, device map, exit counts, every unmapped port
--mem-mib 1024       guest RAM (default 512 with --kernel)
--cmdline "..."      replace the kernel command line (default below)
```

Default command line: `console=ttyS0 reboot=k panic=1 pci=off nomodule
8250.nr_uarts=1 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd`. Each
part is explained in `src/boot.rs`.

## What happens when Linux boots

```
minivmm process                           KVM (host kernel)            guest
---------------                           -----------------            -----
check the bzImage header, initrd,
cmdline and RAM size (before KVM)
KVM_CREATE_VM, KVM_SET_TSS_ADDR  -------->
mmap RAM; copy kernel to 1 MiB, initrd
to the top of RAM, cmdline, zero page
(boot_params + e820), GDT, page tables
KVM_SET_USER_MEMORY_REGION       -------->
KVM_CREATE_IRQCHIP, KVM_CREATE_PIT2 ----->  PIC, IOAPIC, LAPIC, PIT
                                            now emulated in the kernel
KVM_IRQFD(serial eventfd, GSI 4) -------->
KVM_CREATE_VCPU, CPUID, MSRs, FPU, LAPIC
LINT0/LINT1, long-mode sregs,
RIP = 0x100200, RSI = 0x7000     -------->
vCPU thread: KVM_RUN             -------->  enter guest  ----------->  startup_64:
                                                                       decompress,
                                                                       start_kernel
             exit: IO out 0x3f8  <------------------------------------  printk
UART -> stdout; KVM_RUN again    -------->
             (timer, HLT, PIC, APIC: no exit, handled in KVM)
stdin thread: key -> UART FIFO,
  eventfd write                  -------->  irqfd raises IRQ 4  ----->  8250 driver
             exit: IO out 0x64 = 0xFE <-------------------------------  reboot
stop request; main thread joins the vCPU thread, prints the summary
```

## Guest memory layout (Linux)

| Address | Contents |
| --- | --- |
| `0x500` | GDT: null, 64-bit code, data, TSS |
| `0x520` | IDT: one empty entry (interrupts stay off until the kernel loads its own) |
| `0x7000` | `boot_params` (the "zero page"), including the e820 map |
| `0x8ff0` | initial stack pointer |
| `0x9000`, `0xa000`, `0xb000` | PML4, PDPT, PD: first 1 GiB identity mapped with 2 MiB pages |
| `0x20000` | kernel command line |
| `0x9fc00` to `0x100000` | EBDA, video and BIOS area: left out of the e820 map |
| `0x100000` | protected-mode kernel from the bzImage (entry at `+0x200`) |
| top of RAM | initrd, page aligned, below the kernel's `initrd_addr_max` |

## Code map

| File | What it does |
| --- | --- |
| `src/main.rs` | Command line, validation, summary output |
| `src/vmm.rs` | Builds the VM (KVM checks, memory, irqchip, devices, vCPU) and runs it |
| `src/boot.rs` | Linux boot protocol: bzImage and initrd checks, loading, zero page, e820, GDT, page tables |
| `src/vcpu.rs` | Real-mode and long-mode register setup, the `KVM_RUN` loop, stop requests and the kick signal |
| `src/memory.rs` | Guest RAM (`vm-memory`), real-mode image loading, KVM memory slots |
| `src/bus.rs` | Address range to device dispatch (port I/O now, MMIO later) |
| `src/devices/serial.rs` | 16550A UART (`vm-superio`) adapted to the bus, with input |
| `src/devices/i8042.rs` | Keyboard controller (`vm-superio`): its reset line stops the VM |
| `src/console.rs` | Host terminal: raw mode, stdin thread, Ctrl-A x |
| `src/guest.rs` | Real-mode guest images: the built-in hello program or a file |
| `guest/hello.asm` | NASM source of the built-in real-mode guest |
| `scripts/make-initramfs.sh` | Builds the busybox initramfs for the Linux guest |
| `scripts/check-host.sh` | Host readiness check |
| `tests/hello.rs` | End-to-end tests of the real-mode guest |
| `tests/boot.rs` | End-to-end Linux boot tests, and CLI validation tests |
| `tests/guest_asm.rs` | Checks the embedded guest bytes match `hello.asm` |

Crates used: `kvm-ioctls` and `kvm-bindings` (KVM API), `vm-memory` (guest
memory), `linux-loader` (bzImage loading, command line, zero page),
`vm-superio` (UART and i8042 models), `vmm-sys-util` (eventfd, signals,
errno).

What comes from crates and what is written here: `linux-loader` copies the
kernel out of the bzImage and writes the command line and zero page;
`vm-superio` provides the UART and i8042 register models. The memory
layout, e820 map, initrd placement, GDT, page tables, all vCPU state, the
bus, the run loop, the threading and stop logic, and the console are
minivmm's own code.

## Design choices worth knowing

- **Enter the kernel at its 64-bit entry point.** No firmware and no
  real-mode setup code run. minivmm builds what the boot protocol promises
  at `startup_64`: long mode, identity-mapped page tables, a flat GDT, RSI
  pointing at the zero page. The kernel replaces all of it early in boot.
- **Interrupt controllers and timer inside KVM.** `KVM_CREATE_IRQCHIP` and
  `KVM_CREATE_PIT2` mean a guest `HLT`, timer tick or EOI never leaves the
  kernel. The LAPIC's LINT0 is set to ExtINT ("virtual wire"), because
  without ACPI or MP tables Linux routes legacy interrupts through the PIC.
- **The UART interrupt is an irqfd.** The UART's interrupt line is an
  eventfd registered with `KVM_IRQFD` on GSI 4: writing the eventfd raises
  the guest interrupt without a round trip through the vCPU thread.
- **Reboot is the way out.** `reboot=k` makes Linux reset the CPU through
  the keyboard controller (port 0x64, command 0xFE). minivmm's i8042 turns
  that into a stop request, so `reboot` and a kernel panic (with `panic=1`)
  both end the VM cleanly.
- **The vCPU has its own thread, and stopping it is explicit.** An idle
  guest sleeps inside `KVM_RUN`, so setting a flag is not enough. A stop
  request is stored, then the vCPU thread is sent a real-time signal
  (SIGRTMIN) until it leaves: the signal makes `KVM_RUN` return `EINTR`, and
  the loop checks for a request before every `KVM_RUN`. The kick repeats
  because one that lands just before `KVM_RUN` starts would be missed. The
  main thread joins the vCPU thread before the VM and memory are dropped.
- **The terminal is always restored.** Raw mode saves the exact `termios`
  and a `Drop` guard puts it back, including when unwinding from a panic.
- **Guest input is never trusted.** A port or MMIO access to an address no
  device answers reads `0xff` (like an empty ISA bus) and drops writes; the
  guest keeps running. A real-mode guest gets a warning per address; Linux
  probes many legacy ports minivmm does not have (PCI config, CMOS), so for
  Linux they are counted, and printed only with `-v`.
- **Validate before touching KVM.** Kernel header (magic, boot protocol
  2.12+, 64-bit entry), initrd, command line (length and printable ASCII)
  and the RAM needed are all checked first, with errors that say what to
  fix.
- **Drop order is deliberate.** `Vmm` declares the vCPU and VM before the
  guest memory, so KVM file descriptors close before the memory is unmapped.
- **A poisoned device lock is recovered, not re-panicked.** One failed
  device access must not take down every later access.
- **`unsafe` appears three times**, each with a `SAFETY` comment:
  `KVM_SET_USER_MEMORY_REGION` (`src/memory.rs`), and `tcgetattr` and
  `tcsetattr` for the terminal (`src/console.rs`).

Kernel messages that look alarming but are expected (minivmm has no ACPI,
no CMOS clock, and an i8042 that only implements reset):
`ACPI BIOS Error (bug): A valid RSDP was not found`, `rtc_cmos rtc_cmos:
broken or not accessible`, `i8042: probe of i8042 failed with error -5`.

## Tests

```
cargo test
```

- Unit tests (no KVM needed): boot layout (GDT encoding and decoding, page
  tables, e820, initrd placement, zero page contents, kernel header and
  command line checks), long-mode registers, CPUID and LAPIC setup, stop
  requests, i8042 reset, UART input and FIFO limits, the Ctrl-A escape
  parser, bus dispatch, guest image validation, memory bounds.
- End-to-end real-mode tests (need `/dev/kvm`): the hello guest, a guest
  from a file, an unmapped port read, a read past the end of RAM.
- End-to-end Linux tests (need `/dev/kvm`, a kernel and the initramfs):
  boot to `/init` and reboot; type a command into the shell and read its
  result; Ctrl-A x stops an idle guest; a kernel panic ends the VM.
- CLI tests (no KVM needed): missing, empty, oversized or invalid images,
  kernels and initrds, too little RAM, an overlong command line, options
  that need `--kernel`.
- `tests/guest_asm.rs` assembles `guest/hello.asm` with `nasm` (if
  installed) and checks it matches the embedded bytes.

Tests that need something missing skip themselves with a note. The Linux
tests find the kernel at `guest/bzImage` and the initramfs at
`guest/initramfs.cpio.gz`, or at `$MINIVMM_KERNEL` and `$MINIVMM_INITRD`.
`$MINIVMM_BOOT_TIMEOUT` (seconds, default 60) helps on slow hosts.

Run only the Linux tests with output visible:

```
cargo test --test boot -- --nocapture
```

## Write your own real-mode guest

```
sudo apt install nasm
nasm -f bin -o my.bin my.asm      # start with: bits 16 / org 0x1000
cargo run --release -- --guest my.bin
```

The image loads at `0x1000` with all segment bases at 0, so it can be at
most 60 KiB (61,440 bytes): it has to end at or below 64 KiB. End it with
`hlt`.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `cannot open /dev/kvm: No such file or directory` | Enable AMD-V or VT-x in firmware. In VMware: VM settings, Processors, tick "Virtualize Intel VT-x/EPT or AMD-V/RVI". |
| `cannot open /dev/kvm: Permission denied` | `sudo usermod -aG kvm $USER`, then log out and back in. Check with `id`. |
| `cannot read guest/bzImage: Permission denied` | The kernel was copied from `/boot` as root: `sudo chown $USER: guest/bzImage`. |
| `guest RAM is too small` | Raise `--mem-mib` to at least the number in the message. |
| `Kernel panic - not syncing: VFS: Unable to mount root fs` | `--initrd` is missing or not a cpio archive. |
| The terminal behaves oddly after minivmm was killed | Raw mode was not undone (only happens on `kill -9`). Type `reset` and press Enter. |
| `guest shut down (triple fault)` | The guest crashed. For a real-mode guest, check its `org`, entry point and `bits 16`. |
| A test times out | On a slow host (nested virtualization), set `MINIVMM_BOOT_TIMEOUT=300`. |

## Next: M3

A virtio-blk disk over a hand-written virtio-mmio transport, so the guest
mounts a root filesystem from a disk image: feature negotiation and the
device status state machine, virtqueues with `virtio-queue`, an ioeventfd
for queue notifications and the existing irqfd path for completions.

## Licence

Apache-2.0 OR BSD-3-Clause, matching rust-vmm. See `LICENSE-APACHE` and
`LICENSE-BSD-3-Clause`.
