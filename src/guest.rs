// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Guest images for M1: flat 16-bit real-mode binaries.
//!
//! An image is copied to [`GUEST_LOAD_ADDR`] and the vCPU starts there with
//! every segment base at 0. That means the whole image must sit below
//! 64 KiB, where a 16-bit instruction pointer can reach it.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::memory::{GUEST_LOAD_ADDR, REAL_MODE_LIMIT};

/// Largest image that fits between the load address and the 64 KiB limit.
pub const MAX_IMAGE_SIZE: u64 = REAL_MODE_LIMIT - GUEST_LOAD_ADDR;

/// What the built-in hello guest prints.
pub const HELLO_MESSAGE: &[u8] = b"Hello from the minivmm guest!\n";

/// Machine code of the built-in hello guest, assembled for `org 0x1000`.
///
/// The NASM source is `guest/hello.asm`; `tests/guest_asm.rs` checks that
/// these bytes match it whenever `nasm` is installed.
///
/// ```text
/// 0x1000  BA F8 03    mov  dx, 0x3f8     ; COM1 data register
/// 0x1003  BE 11 10    mov  si, 0x1011    ; address of the message
/// 0x1006  AC          lodsb              ; al = [ds:si], si += 1
/// 0x1007  84 C0       test al, al
/// 0x1009  74 03       jz   0x100e        ; NUL terminator: stop
/// 0x100B  EE          out  dx, al        ; VM exit: KVM_EXIT_IO
/// 0x100C  EB F8       jmp  0x1006
/// 0x100E  F4          hlt                ; VM exit: KVM_EXIT_HLT
/// 0x100F  EB FD       jmp  0x100e        ; never run past the halt
/// 0x1011  "Hello from the minivmm guest!\n", 0
/// ```
const HELLO_CODE: [u8; 0x11] = [
    0xBA, 0xF8, 0x03, // mov dx, 0x3f8
    0xBE, 0x11, 0x10, // mov si, 0x1011
    0xAC, // lodsb
    0x84, 0xC0, // test al, al
    0x74, 0x03, // jz +3
    0xEE, // out dx, al
    0xEB, 0xF8, // jmp -8
    0xF4, // hlt
    0xEB, 0xFD, // jmp -3
];

/// Errors from building a guest image.
#[derive(Debug)]
pub enum ImageError {
    /// The image file could not be read.
    Read { path: String, source: io::Error },
    /// The image has no bytes.
    Empty { name: String },
    /// The image does not fit below the 64 KiB real-mode limit.
    TooLarge { name: String, size: u64 },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Read { path, source } => {
                write!(f, "cannot read guest image {path}: {source}")
            }
            ImageError::Empty { name } => write!(f, "guest image {name} is empty"),
            ImageError::TooLarge { name, size } => write!(
                f,
                "guest image {name} is too large: {size} bytes, the maximum is {MAX_IMAGE_SIZE} \
                 (it loads at {GUEST_LOAD_ADDR:#x} and must end below {REAL_MODE_LIMIT:#x})"
            ),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImageError::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A validated guest image: non-empty and small enough to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestImage {
    name: String,
    bytes: Vec<u8>,
}

impl GuestImage {
    /// The built-in program that prints [`HELLO_MESSAGE`] and halts.
    pub fn hello() -> Self {
        let mut bytes = HELLO_CODE.to_vec();
        bytes.extend_from_slice(HELLO_MESSAGE);
        bytes.push(0);
        Self {
            name: "built-in hello".to_string(),
            bytes,
        }
    }

    /// Validate an image held in memory.
    pub fn from_bytes(name: impl Into<String>, bytes: Vec<u8>) -> Result<Self, ImageError> {
        let name = name.into();
        if bytes.is_empty() {
            return Err(ImageError::Empty { name });
        }
        let size = bytes.len() as u64;
        if size > MAX_IMAGE_SIZE {
            return Err(ImageError::TooLarge { name, size });
        }
        Ok(Self { name, bytes })
    }

    /// Read and validate an image file.
    ///
    /// The size is checked from the file's metadata first, so pointing the
    /// VMM at a huge file fails fast instead of reading it all into memory.
    pub fn from_file(path: &Path) -> Result<Self, ImageError> {
        let shown = path.display().to_string();
        let read_err = |source| ImageError::Read {
            path: shown.clone(),
            source,
        };
        let size = fs::metadata(path).map_err(read_err)?.len();
        if size > MAX_IMAGE_SIZE {
            return Err(ImageError::TooLarge { name: shown, size });
        }
        let bytes = fs::read(path).map_err(read_err)?;
        Self::from_bytes(shown, bytes)
    }

    /// A human-readable name (the path, or "built-in hello").
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The raw machine code.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_message_address_points_at_the_message() {
        let image = GuestImage::hello();
        let bytes = image.bytes();
        // `mov si, imm16` is BE lo hi.
        assert_eq!(bytes[3], 0xBE);
        let si = u16::from_le_bytes([bytes[4], bytes[5]]) as u64;
        let offset = (si - GUEST_LOAD_ADDR) as usize;
        assert_eq!(offset, HELLO_CODE.len());
        assert_eq!(&bytes[offset..offset + HELLO_MESSAGE.len()], HELLO_MESSAGE);
        assert_eq!(bytes.last(), Some(&0), "message must be NUL-terminated");
    }

    #[test]
    fn hello_jumps_land_on_instruction_starts() {
        // jz at 0x09: target = next instruction (0x0b) + rel8.
        assert_eq!(HELLO_CODE[9], 0x74);
        assert_eq!(0x0b + HELLO_CODE[10] as i8 as isize, 0x0e);
        assert_eq!(HELLO_CODE[0x0e], 0xF4, "jz target must be the hlt");
        // jmp at 0x0c: back to lodsb at 0x06.
        assert_eq!(HELLO_CODE[0x0c], 0xEB);
        assert_eq!(0x0e + HELLO_CODE[0x0d] as i8 as isize, 0x06);
        assert_eq!(HELLO_CODE[0x06], 0xAC);
        // jmp at 0x0f: back to the hlt.
        assert_eq!(0x11 + HELLO_CODE[0x10] as i8 as isize, 0x0e);
    }

    #[test]
    fn hello_fits() {
        assert!((GuestImage::hello().bytes().len() as u64) <= MAX_IMAGE_SIZE);
    }

    #[test]
    fn empty_image_is_rejected() {
        let err = GuestImage::from_bytes("e", vec![]).unwrap_err();
        assert!(matches!(err, ImageError::Empty { .. }));
    }

    #[test]
    fn oversized_image_is_rejected() {
        let err =
            GuestImage::from_bytes("big", vec![0x90; MAX_IMAGE_SIZE as usize + 1]).unwrap_err();
        assert!(matches!(err, ImageError::TooLarge { size, .. } if size == MAX_IMAGE_SIZE + 1));
        // Exactly the maximum is fine.
        GuestImage::from_bytes("max", vec![0x90; MAX_IMAGE_SIZE as usize]).unwrap();
    }

    #[test]
    fn missing_file_names_the_path() {
        let err = GuestImage::from_file(Path::new("/nonexistent/guest.bin")).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/guest.bin"));
    }
}
