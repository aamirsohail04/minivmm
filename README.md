# minivmm

A small virtual machine monitor (VMM) written in Rust on the
[rust-vmm](https://github.com/rust-vmm) crates, built one milestone at a time
to learn how a KVM-based hypervisor works from the inside.

**Current milestone: M1.** A 16-bit real-mode guest runs on one vCPU, prints
through an emulated 16550A serial port, and stops with `HLT`.

| Milestone | Goal | Status |
| --- | --- | --- |
| M1 | Hello guest + serial console | Done |
| M2 | Boot a Linux kernel to a serial console | Next |
| M3 | virtio-blk root filesystem (virtio-mmio) | Planned |
| M4 | Robustness tests and fuzzing | Planned |
| M5 | virtio-net and multiple vCPUs | Planned |

## Quick start

On an x86_64 Linux machine (or VM with nested virtualization) with KVM:

```
./scripts/check-host.sh        # checks KVM, permissions and the Rust toolchain
cargo test                     # unit and end-to-end tests
cargo run --release            # run the built-in hello guest
```

Expected output:

```
Hello from the minivmm guest!
minivmm: guest halted after 31 VM exits
```

The first line comes from the guest (on stdout). The second comes from
minivmm (on stderr): 30 port writes, one per character, plus the `HLT`.

More detail with `-v`:

```
cargo run --release -- -v
```

## What happens when you run it

```
minivmm process                                   KVM (host kernel)       guest
---------------                                   -----------------       -----
open /dev/kvm, check API version 12  ------------>
KVM_CREATE_VM, KVM_SET_TSS_ADDR      ------------>
mmap 16 MiB, copy guest to 0x1000,
KVM_SET_USER_MEMORY_REGION           ------------>
put the UART on the port bus at 0x3f8
KVM_CREATE_VCPU, set CS/DS/... = 0,
RIP = 0x1000                         ------------>
KVM_RUN                              ------------>  enter guest  ------>  mov dx, 0x3f8
                                                                          lodsb ; out dx, al
                    exit: IO out, port 0x3f8, 'H' <-----------------------
bus -> UART -> stdout; KVM_RUN again ------------>  resume  ----------->  ... next byte ...
                    exit: HLT                     <-----------------------  hlt
print summary, exit 0
```

Only two things cause VM exits here: `out` to a port (the guest is not
allowed to touch real hardware) and `hlt`. Everything else, including every
memory access inside the 16 MiB of guest RAM, runs natively on the CPU.

## Code map

| File | What it does |
| --- | --- |
| `src/main.rs` | Command line, validation, summary output |
| `src/vmm.rs` | Builds the VM: KVM checks, memory, devices, vCPU |
| `src/memory.rs` | Guest RAM (`vm-memory`), image loading, KVM memory slots |
| `src/vcpu.rs` | Real-mode register setup and the `KVM_RUN` exit loop |
| `src/bus.rs` | Address range to device dispatch (port I/O now, MMIO later) |
| `src/devices/serial.rs` | 16550A UART (`vm-superio`) adapted to the bus |
| `src/guest.rs` | Guest images: the built-in hello program or a file |
| `guest/hello.asm` | NASM source of the built-in guest |
| `tests/hello.rs` | End-to-end tests that run the binary |
| `tests/guest_asm.rs` | Checks the embedded guest bytes match `hello.asm` |
| `scripts/check-host.sh` | Host readiness check |

Crates used: `kvm-ioctls` and `kvm-bindings` (KVM API), `vm-memory` (guest
memory), `vm-superio` (UART model), `vmm-sys-util` (eventfd, errno).

## Design choices worth knowing

- **No in-kernel interrupt controller in M1.** KVM returns `KVM_EXIT_HLT` to
  user space only when there is no in-kernel irqchip. Without one, `HLT`
  cleanly ends the guest. M2 adds the irqchip (Linux needs interrupts), and
  then `HLT` is handled inside KVM.
- **The UART already has an interrupt line.** It is an eventfd that nothing
  listens to yet. In M2 it is registered as an irqfd for IRQ 4, so a UART
  interrupt reaches the guest without a trip through user space.
- **Guest input is never trusted.** A port or MMIO access to an address no
  device answers is logged once per address, reads return `0xff` (like an
  empty ISA bus) and writes are dropped. The guest keeps running. Tests
  cover both cases.
- **Validate before touching KVM.** Image size, emptiness and the memory
  size are checked first, with error messages that say what to fix.
- **Drop order is deliberate.** `Vmm` declares the vCPU and VM before the
  guest memory, so KVM file descriptors close before the memory is unmapped.
- **A poisoned device lock is recovered, not re-panicked.** One failed
  device access must not take down every later access.
- **`unsafe` appears once**, around `KVM_SET_USER_MEMORY_REGION`, with a
  `SAFETY` comment explaining why the pointer stays valid.

## Tests

```
cargo test
```

- Unit tests (no KVM needed): bus dispatch, overlap and overflow checks,
  poisoned-lock recovery, UART output and register behaviour, guest image
  validation, memory bounds.
- End-to-end tests (need `/dev/kvm`, skip themselves otherwise): the hello
  guest, a guest loaded from a file, an unmapped port read, and a read just
  past the end of RAM (an unmapped MMIO read).
- CLI tests (no KVM needed): missing, empty and oversized images, and an
  out-of-range memory size.
- `tests/guest_asm.rs` assembles `guest/hello.asm` with `nasm` (if
  installed) and checks it matches the embedded bytes.

Run only the KVM tests with output visible:

```
cargo test --test hello -- --nocapture
```

## Write your own guest

```
sudo apt install nasm
nasm -f bin -o my.bin my.asm      # start with: bits 16 / org 0x1000
cargo run --release -- --guest my.bin
```

The image loads at `0x1000` with all segment bases at 0, so it must be
smaller than 60 KiB (it has to end below 64 KiB). End it with `hlt`.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `cannot open /dev/kvm: No such file or directory` | Enable AMD-V or VT-x in firmware. In VMware: VM settings, Processors, tick "Virtualize Intel VT-x/EPT or AMD-V/RVI". |
| `cannot open /dev/kvm: Permission denied` | `sudo usermod -aG kvm $USER`, then log out and back in. Check with `id`. |
| `guest shut down (triple fault)` | The guest crashed. Check its `org`, entry point and that it was assembled with `bits 16`. |
| A test hangs and times out after 10 s | The guest never executed `hlt`. |

## Next: M2

Boot a real Linux `bzImage` to a busybox shell on the serial console:
`linux-loader`, boot parameters and the e820 map, 64-bit page tables and
GDT, an in-kernel irqchip and PIT, and the UART interrupt as an irqfd.

## Licence

Apache-2.0 OR BSD-3-Clause, matching rust-vmm. Add the two licence files
(`LICENSE-APACHE`, `LICENSE-BSD-3-Clause`) when you publish the repository;
GitHub's "Add file, Create new file, LICENSE" offers both templates.
