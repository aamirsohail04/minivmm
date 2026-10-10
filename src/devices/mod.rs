// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Emulated devices.
//!
//! M1 has only the legacy serial console. virtio-mmio block and network
//! devices arrive in M3 and M5.

pub mod serial;
