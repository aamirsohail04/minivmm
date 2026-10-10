// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! A bus maps address ranges to devices.
//!
//! In M1 there is one bus for x86 port I/O. The same type will serve MMIO
//! in M3, which is why addresses are `u64` rather than `u16`.
//!
//! Devices are shared as `Arc<Mutex<dyn BusDevice>>`, so a vCPU thread and
//! (from M3) a device event loop thread can both reach them safely.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

/// A device that can sit on a [`Bus`].
///
/// `offset` is relative to the base address the device was inserted at.
/// `data.len()` is the access size, or `size * count` for x86 string I/O
/// (`rep outsb`), where every byte targets the same port.
pub trait BusDevice: Send {
    /// Handle a read; the device fills `data`.
    fn read(&mut self, offset: u64, data: &mut [u8]);
    /// Handle a write of `data`.
    fn write(&mut self, offset: u64, data: &[u8]);
}

/// Errors from bus registration and dispatch.
#[derive(Debug, PartialEq, Eq)]
pub enum BusError {
    /// A device was registered with a length of zero.
    ZeroLength { name: &'static str },
    /// `base + len` does not fit in a `u64`.
    RangeOverflow { name: &'static str },
    /// The new range overlaps a device that is already registered.
    Overlap {
        name: &'static str,
        existing: &'static str,
    },
    /// No device is registered at this address.
    Unmapped { addr: u64 },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::ZeroLength { name } => write!(f, "device {name} has a zero-length range"),
            BusError::RangeOverflow { name } => {
                write!(f, "device {name} has a range that overflows u64")
            }
            BusError::Overlap { name, existing } => {
                write!(f, "device {name} overlaps device {existing}")
            }
            BusError::Unmapped { addr } => write!(f, "no device at address {addr:#x}"),
        }
    }
}

impl std::error::Error for BusError {}

struct BusEntry {
    base: u64,
    len: u64,
    name: &'static str,
    device: Arc<Mutex<dyn BusDevice>>,
}

impl BusEntry {
    /// Exclusive end of the range. Cannot overflow: checked on insert.
    fn end(&self) -> u64 {
        self.base + self.len
    }
}

/// An address space of non-overlapping device ranges.
#[derive(Default)]
pub struct Bus {
    /// Sorted by `base`.
    entries: Vec<BusEntry>,
}

impl Bus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `device` at `[base, base + len)`.
    pub fn insert(
        &mut self,
        name: &'static str,
        device: Arc<Mutex<dyn BusDevice>>,
        base: u64,
        len: u64,
    ) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::ZeroLength { name });
        }
        let end = base
            .checked_add(len)
            .ok_or(BusError::RangeOverflow { name })?;
        if let Some(existing) = self.entries.iter().find(|e| base < e.end() && e.base < end) {
            return Err(BusError::Overlap {
                name,
                existing: existing.name,
            });
        }
        let pos = self.entries.partition_point(|e| e.base < base);
        self.entries.insert(
            pos,
            BusEntry {
                base,
                len,
                name,
                device,
            },
        );
        Ok(())
    }

    /// Find the device covering `addr` and the offset of `addr` inside it.
    fn find(&self, addr: u64) -> Option<(&BusEntry, u64)> {
        // Last entry whose base is <= addr.
        let idx = self.entries.partition_point(|e| e.base <= addr);
        let entry = self.entries.get(idx.checked_sub(1)?)?;
        (addr < entry.end()).then(|| (entry, addr - entry.base))
    }

    /// Dispatch a read at `addr`. On `Unmapped`, `data` is left untouched.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> Result<(), BusError> {
        let (entry, offset) = self.find(addr).ok_or(BusError::Unmapped { addr })?;
        lock(&entry.device).read(offset, data);
        Ok(())
    }

    /// Dispatch a write at `addr`.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), BusError> {
        let (entry, offset) = self.find(addr).ok_or(BusError::Unmapped { addr })?;
        lock(&entry.device).write(offset, data);
        Ok(())
    }

    /// Names and ranges of the registered devices, for logging.
    pub fn describe(&self) -> Vec<(&'static str, u64, u64)> {
        self.entries
            .iter()
            .map(|e| (e.name, e.base, e.len))
            .collect()
    }
}

/// Lock a device, recovering from a poisoned mutex.
///
/// A panic in one device access must not turn every later access into a
/// second panic, so the guard is taken back from a poisoned lock.
fn lock(device: &Arc<Mutex<dyn BusDevice>>) -> MutexGuard<'_, dyn BusDevice + 'static> {
    match device.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the last access so tests can check offsets and data.
    #[derive(Default)]
    struct Recorder {
        last_write: Option<(u64, Vec<u8>)>,
        read_value: u8,
    }

    impl BusDevice for Recorder {
        fn read(&mut self, _offset: u64, data: &mut [u8]) {
            data.fill(self.read_value);
        }
        fn write(&mut self, offset: u64, data: &[u8]) {
            self.last_write = Some((offset, data.to_vec()));
        }
    }

    fn recorder() -> Arc<Mutex<Recorder>> {
        Arc::new(Mutex::new(Recorder::default()))
    }

    #[test]
    fn dispatches_with_offset_relative_to_base() {
        let dev = recorder();
        let mut bus = Bus::new();
        bus.insert("rec", dev.clone(), 0x3f8, 8).unwrap();

        bus.write(0x3fd, &[0xab]).unwrap();
        let last = dev.lock().unwrap().last_write.clone();
        assert_eq!(last, Some((5, vec![0xab])));
    }

    #[test]
    fn read_fills_data() {
        let dev = recorder();
        dev.lock().unwrap().read_value = 0x5a;
        let mut bus = Bus::new();
        bus.insert("rec", dev, 0x100, 4).unwrap();

        let mut data = [0u8; 2];
        bus.read(0x103, &mut data).unwrap();
        assert_eq!(data, [0x5a, 0x5a]);
    }

    #[test]
    fn unmapped_addresses_are_reported_and_data_untouched() {
        let mut bus = Bus::new();
        bus.insert("rec", recorder(), 0x3f8, 8).unwrap();

        let mut data = [0x11u8];
        assert_eq!(
            bus.read(0x3f7, &mut data),
            Err(BusError::Unmapped { addr: 0x3f7 })
        );
        assert_eq!(data, [0x11]);
        assert_eq!(
            bus.write(0x400, &[1]),
            Err(BusError::Unmapped { addr: 0x400 })
        );
    }

    #[test]
    fn rejects_overlapping_ranges() {
        let mut bus = Bus::new();
        bus.insert("a", recorder(), 0x10, 0x10).unwrap();
        let err = bus.insert("b", recorder(), 0x1f, 1).unwrap_err();
        assert_eq!(
            err,
            BusError::Overlap {
                name: "b",
                existing: "a"
            }
        );
        // Adjacent ranges are fine.
        bus.insert("c", recorder(), 0x20, 0x10).unwrap();
        bus.insert("d", recorder(), 0x0, 0x10).unwrap();
        let names: Vec<_> = bus.describe().into_iter().map(|d| d.0).collect();
        assert_eq!(names, ["d", "a", "c"]);
    }

    #[test]
    fn rejects_zero_length_and_overflow() {
        let mut bus = Bus::new();
        assert_eq!(
            bus.insert("z", recorder(), 0x10, 0),
            Err(BusError::ZeroLength { name: "z" })
        );
        assert_eq!(
            bus.insert("o", recorder(), u64::MAX, 2),
            Err(BusError::RangeOverflow { name: "o" })
        );
    }

    #[test]
    fn survives_a_poisoned_device_lock() {
        let dev = recorder();
        let mut bus = Bus::new();
        bus.insert("rec", dev.clone(), 0, 1).unwrap();

        let poisoner = dev.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("simulated device panic");
        })
        .join();
        assert!(dev.is_poisoned());

        bus.write(0, &[7]).unwrap();
        let last = match dev.lock() {
            Ok(g) => g.last_write.clone(),
            Err(p) => p.into_inner().last_write.clone(),
        };
        assert_eq!(last, Some((0, vec![7])));
    }
}
