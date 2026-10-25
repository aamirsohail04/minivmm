// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Emulated devices.
//!
//! M2 has the legacy serial console and the keyboard controller's reset
//! line. The interrupt controllers and the timer (PIT) are emulated inside
//! KVM. virtio-mmio block and network devices arrive in M3 and M5.

pub mod i8042;
pub mod serial;
