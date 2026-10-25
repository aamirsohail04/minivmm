// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! The guest's serial console: a 16550A UART at COM1.
//!
//! The UART register model comes from `vm-superio`. This module adapts it
//! to the [`Bus`](crate::bus::Bus) and gives it an interrupt line.

use std::io::{self, Write};

use vm_superio::serial::NoEvents;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::bus::BusDevice;

/// Base port of COM1.
pub const COM1_BASE: u64 = 0x3f8;
/// The UART has eight byte-wide registers.
pub const COM1_LEN: u64 = 8;
/// Legacy IRQ line of COM1.
pub const COM1_IRQ: u32 = 4;

/// The UART's interrupt output, as an eventfd.
///
/// For a Linux guest the eventfd is registered with KVM as an irqfd for
/// [`COM1_IRQ`], so a trigger becomes a guest interrupt without a trip
/// through user space: the write to the eventfd wakes KVM, which raises the
/// line on the in-kernel interrupt controller. A real-mode guest has no
/// interrupt controller, so there nothing listens to the eventfd.
pub struct IrqLine {
    evt: EventFd,
}

impl IrqLine {
    /// Create an interrupt line backed by a non-blocking eventfd.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            evt: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    /// The eventfd, for registering as an irqfd.
    pub fn eventfd(&self) -> &EventFd {
        &self.evt
    }
}

impl Trigger for IrqLine {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        self.evt.write(1)
    }
}

/// A serial console whose transmitted bytes go to `W` (stdout in the binary,
/// a buffer in tests).
pub struct SerialConsole<W: Write + Send> {
    uart: Serial<IrqLine, NoEvents, W>,
    /// Count of bytes the host side failed to accept. A failing writer
    /// (for example a closed stdout) must never stop the guest.
    lost_bytes: u64,
    warned_lost: bool,
    warned_irq: bool,
}

impl<W: Write + Send> SerialConsole<W> {
    /// Create a console that writes guest output to `out`.
    pub fn new(irq: IrqLine, out: W) -> Self {
        Self {
            uart: Serial::new(irq, out),
            lost_bytes: 0,
            warned_lost: false,
            warned_irq: false,
        }
    }

    /// Bytes the guest sent that could not be written to the host side.
    pub fn lost_bytes(&self) -> u64 {
        self.lost_bytes
    }

    /// The host-side writer (tests read it back).
    pub fn writer(&self) -> &W {
        self.uart.writer()
    }

    /// Queue bytes typed on the host for the guest to read, and raise the
    /// "data available" interrupt if the guest enabled it.
    ///
    /// Returns how many bytes were queued: fewer than `input.len()` (maybe
    /// 0) when the UART's 64-byte receive FIFO is full because the guest
    /// has not read it yet. The caller keeps the rest and tries again.
    pub fn enqueue_input(&mut self, input: &[u8]) -> usize {
        let n = input.len().min(self.uart.fifo_capacity());
        if n == 0 {
            return 0;
        }
        // With room checked above, the only possible error is from the
        // interrupt eventfd. The bytes are queued either way.
        if let Err(err) = self.uart.enqueue_raw_bytes(&input[..n]) {
            if !self.warned_irq {
                self.warned_irq = true;
                eprintln!("minivmm: serial interrupt failed ({err:?}); input may be delayed");
            }
        }
        n
    }
}

impl<W: Write + Send> BusDevice for SerialConsole<W> {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // The bus only routes offsets 0..COM1_LEN here, so the cast is lossless.
        let reg = offset as u8;
        for byte in data.iter_mut() {
            *byte = self.uart.read(reg);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let reg = offset as u8;
        for &byte in data {
            if let Err(err) = self.uart.write(reg, byte) {
                self.lost_bytes += 1;
                if !self.warned_lost {
                    self.warned_lost = true;
                    eprintln!(
                        "minivmm: serial output error ({err:?}); further errors are counted, not printed"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register offsets, from the 16550A datasheet.
    const THR: u64 = 0; // transmit holding register (DLAB = 0)
    const LCR: u64 = 3; // line control register
    const LSR: u64 = 5; // line status register
    const LSR_THR_EMPTY: u8 = 0x20;

    fn console() -> SerialConsole<Vec<u8>> {
        SerialConsole::new(IrqLine::new().unwrap(), Vec::new())
    }

    #[test]
    fn transmitted_bytes_reach_the_writer() {
        let mut c = console();
        for &b in b"hello" {
            c.write(THR, &[b]);
        }
        assert_eq!(c.writer().as_slice(), b"hello");
    }

    #[test]
    fn string_io_writes_every_byte() {
        // `rep outsb` arrives as one exit carrying several bytes for one port.
        let mut c = console();
        c.write(THR, b"abc");
        assert_eq!(c.writer().as_slice(), b"abc");
    }

    #[test]
    fn line_status_reports_transmitter_empty() {
        let mut c = console();
        let mut lsr = [0u8];
        c.read(LSR, &mut lsr);
        assert_ne!(lsr[0] & LSR_THR_EMPTY, 0);
    }

    #[test]
    fn divisor_latch_writes_are_not_output() {
        // With DLAB set, offset 0 is the divisor latch, not the data register.
        let mut c = console();
        c.write(LCR, &[0x80]);
        c.write(THR, &[0x01]);
        c.write(LCR, &[0x03]);
        c.write(THR, b"x");
        assert_eq!(c.writer().as_slice(), b"x");
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const RBR: u64 = 0; // receive buffer register (DLAB = 0)
    const IER: u64 = 1; // interrupt enable register
    const LSR_DATA_READY: u8 = 0x01;

    #[test]
    fn input_reaches_the_guest_and_raises_an_interrupt() {
        let mut c = console();
        c.write(IER, &[0x01]); // enable the "received data" interrupt
        assert_eq!(c.enqueue_input(b"ls\n"), 3);

        let irq = c.uart.interrupt_evt().eventfd().read().unwrap();
        assert!(irq >= 1, "the guest must be told data arrived");

        let mut lsr = [0u8];
        c.read(LSR, &mut lsr);
        assert_ne!(lsr[0] & LSR_DATA_READY, 0);
        let mut got = Vec::new();
        for _ in 0..3 {
            let mut b = [0u8];
            c.read(RBR, &mut b);
            got.push(b[0]);
        }
        assert_eq!(got, b"ls\n");
        c.read(LSR, &mut lsr);
        assert_eq!(lsr[0] & LSR_DATA_READY, 0, "FIFO drained");
    }

    #[test]
    fn a_full_receive_fifo_takes_only_what_fits() {
        let mut c = console();
        let big = [b'x'; 100];
        assert_eq!(c.enqueue_input(&big), 64);
        assert_eq!(c.enqueue_input(b"more"), 0);
        let mut b = [0u8];
        c.read(RBR, &mut b);
        assert_eq!(c.enqueue_input(b"more"), 1, "one byte read, one slot free");
    }

    #[test]
    fn a_failing_writer_is_counted_not_fatal() {
        let mut c = SerialConsole::new(IrqLine::new().unwrap(), FailingWriter);
        c.write(THR, b"lost");
        assert_eq!(c.lost_bytes(), 4);
    }
}
