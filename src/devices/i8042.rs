// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! The i8042 keyboard controller, used only for its CPU reset line.
//!
//! On x86, writing command 0xFE to the controller's command port (0x64)
//! pulses the CPU reset line. Linux booted with `reboot=k` does exactly
//! that on `reboot`, and after a panic with `panic=1`. There is no BIOS to
//! reset into, so minivmm turns the reset into a clean stop of the VM.
//!
//! The model comes from `vm-superio`: reads return 0 (no key data, nothing
//! busy) and every write except the reset command is ignored.

use std::convert::Infallible;
use std::sync::Arc;

use vm_superio::{I8042Device, Trigger};

use crate::bus::BusDevice;
use crate::vcpu::{ExitControl, StopReason};

/// First port of the controller: 0x60 is the data port.
pub const I8042_BASE: u64 = 0x60;
/// Ports 0x60 to 0x64. Port 0x61 (the PC speaker) is answered inside KVM
/// by the in-kernel PIT, so it never reaches this device.
pub const I8042_LEN: u64 = 5;

/// Turns the controller's reset line into a stop request.
pub struct ResetLine {
    ctl: Arc<ExitControl>,
}

impl Trigger for ResetLine {
    type E = Infallible;

    fn trigger(&self) -> Result<(), Infallible> {
        self.ctl.request(StopReason::Reset);
        Ok(())
    }
}

/// The keyboard controller on the port I/O bus.
pub struct KeyboardController {
    dev: I8042Device<ResetLine>,
}

impl KeyboardController {
    /// A controller whose reset command stops the VM through `ctl`.
    pub fn new(ctl: Arc<ExitControl>) -> Self {
        Self {
            dev: I8042Device::new(ResetLine { ctl }),
        }
    }
}

impl BusDevice for KeyboardController {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // The bus only routes offsets 0..I8042_LEN here, so the cast is lossless.
        let value = self.dev.read(offset as u8);
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for &byte in data {
            // The trigger cannot fail (its error type is `Infallible`).
            let _ = self.dev.write(offset as u8, byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: u64 = 4; // port 0x64
    const CMD_RESET_CPU: u8 = 0xfe;

    #[test]
    fn reset_command_requests_a_stop() {
        let ctl = Arc::new(ExitControl::new().unwrap());
        let mut kbd = KeyboardController::new(ctl.clone());
        kbd.write(COMMAND, &[0x20]); // "read command byte": ignored
        assert_eq!(ctl.requested(), None);
        kbd.write(COMMAND, &[CMD_RESET_CPU]);
        assert_eq!(ctl.requested(), Some(StopReason::Reset));
    }

    #[test]
    fn reads_report_an_idle_controller() {
        let ctl = Arc::new(ExitControl::new().unwrap());
        let mut kbd = KeyboardController::new(ctl);
        let mut status = [0xffu8];
        kbd.read(COMMAND, &mut status);
        assert_eq!(status, [0], "no data waiting, input buffer empty");
    }

    #[test]
    fn reset_written_to_the_data_port_is_ignored() {
        let ctl = Arc::new(ExitControl::new().unwrap());
        let mut kbd = KeyboardController::new(ctl.clone());
        kbd.write(0, &[CMD_RESET_CPU]);
        assert_eq!(ctl.requested(), None);
    }
}
