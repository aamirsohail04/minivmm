// Copyright 2026 Aamir Sohail
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! The host side of the serial console: keyboard input for the guest.
//!
//! Guest output already reaches stdout through the UART. This module adds
//! the other direction:
//!
//! - [`RawTerminal`] puts the host terminal in raw mode, so every key goes
//!   straight to the guest (including Ctrl-C, which the guest's shell
//!   handles) instead of being line-edited by the host. The original
//!   settings are restored when it is dropped.
//! - [`spawn_input_thread`] reads stdin on its own thread and feeds the
//!   bytes into the UART's receive FIFO, which raises the guest's serial
//!   interrupt.
//! - [`EscapeParser`] watches for Ctrl-A x, the key sequence to quit
//!   minivmm (the same one QEMU uses), because Ctrl-C now belongs to the
//!   guest.

use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::devices::serial::SerialConsole;
use crate::vcpu::{ExitControl, StopReason};

/// Ctrl-A: the first key of an escape sequence.
pub const ESCAPE_KEY: u8 = 0x01;
/// After Ctrl-A, this key quits minivmm.
pub const QUIT_KEY: u8 = b'x';
/// How long the input thread waits for the guest to drain a full FIFO.
const FIFO_RETRY: Duration = Duration::from_millis(5);

/// Separates keys meant for the guest from the quit sequence.
#[derive(Debug, Default)]
pub struct EscapeParser {
    after_escape: bool,
}

impl EscapeParser {
    /// Feed typed bytes. Bytes for the guest are appended to `out`.
    /// Returns `true` when the user typed Ctrl-A x; bytes after it are dropped.
    ///
    /// Ctrl-A Ctrl-A sends one Ctrl-A to the guest. Ctrl-A followed by any
    /// other key sends both keys unchanged.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> bool {
        for &byte in input {
            if self.after_escape {
                self.after_escape = false;
                match byte {
                    QUIT_KEY => return true,
                    ESCAPE_KEY => out.push(ESCAPE_KEY),
                    other => out.extend_from_slice(&[ESCAPE_KEY, other]),
                }
            } else if byte == ESCAPE_KEY {
                self.after_escape = true;
            } else {
                out.push(byte);
            }
        }
        false
    }
}

/// Start the thread that copies `input` (stdin in the binary) to the guest.
///
/// The thread ends at end of input, on a read error, or after Ctrl-A x
/// (which it reports through `ctl`). It never stops the guest on its own
/// otherwise: with stdin closed (as in tests) the guest simply gets no input.
pub fn spawn_input_thread<R, W>(
    mut input: R,
    serial: Arc<Mutex<SerialConsole<W>>>,
    ctl: Arc<ExitControl>,
) -> io::Result<JoinHandle<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    thread::Builder::new()
        .name("console-in".to_string())
        .spawn(move || {
            let mut parser = EscapeParser::default();
            let mut buf = [0u8; 64];
            let mut pending = Vec::with_capacity(buf.len());
            loop {
                let n = match input.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                pending.clear();
                if parser.feed(&buf[..n], &mut pending) {
                    ctl.request(StopReason::Quit);
                    return;
                }
                let mut rest = pending.as_slice();
                while !rest.is_empty() {
                    let queued = serial
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .enqueue_input(rest);
                    rest = &rest[queued..];
                    if queued == 0 {
                        if ctl.requested().is_some() {
                            return;
                        }
                        // The guest has not read the FIFO yet. Wait without
                        // holding the lock, so the vCPU can drain it.
                        thread::sleep(FIFO_RETRY);
                    }
                }
            }
        })
}

/// The host terminal in raw mode; dropping it restores the saved settings.
pub struct RawTerminal {
    saved: libc::termios,
}

impl RawTerminal {
    /// Switch stdin to raw mode if it is a terminal. Returns `None` when
    /// stdin is a pipe or a file, where there is nothing to switch.
    pub fn enter() -> io::Result<Option<Self>> {
        if !io::stdin().is_terminal() {
            return Ok(None);
        }
        let mut termios = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: fd 0 is open (it is a terminal, checked above) and
        // `termios` points to writable memory of the right size. On success
        // tcgetattr has written every field, so `assume_init` is sound.
        let saved = unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            termios.assume_init()
        };

        let mut raw = saved;
        // No line editing, no echo (the guest echoes), no signals from
        // Ctrl-C or Ctrl-Z (they go to the guest), no Ctrl-S/Ctrl-Q flow
        // control, and Enter sends CR as a real serial terminal would.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        set_stdin_termios(&raw)?;
        Ok(Some(Self { saved }))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Runs on every way out of main, including a panic unwinding.
        let _ = set_stdin_termios(&self.saved);
    }
}

fn set_stdin_termios(termios: &libc::termios) -> io::Result<()> {
    // SAFETY: fd 0 is a terminal (only called after `enter` checked it)
    // and `termios` is a valid, fully initialised struct that tcsetattr
    // only reads.
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::serial::IrqLine;

    fn parse(input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let quit = EscapeParser::default().feed(input, &mut out);
        (out, quit)
    }

    #[test]
    fn plain_keys_pass_through() {
        assert_eq!(parse(b"ls -l\r\x03"), (b"ls -l\r\x03".to_vec(), false));
    }

    #[test]
    fn ctrl_a_x_quits_and_drops_the_rest() {
        assert_eq!(parse(b"ab\x01xcd"), (b"ab".to_vec(), true));
    }

    #[test]
    fn ctrl_a_twice_sends_one_ctrl_a() {
        assert_eq!(parse(b"\x01\x01x"), (b"\x01x".to_vec(), false));
    }

    #[test]
    fn ctrl_a_then_another_key_sends_both() {
        assert_eq!(parse(b"\x01q"), (b"\x01q".to_vec(), false));
    }

    #[test]
    fn the_escape_can_span_two_reads() {
        let mut p = EscapeParser::default();
        let mut out = Vec::new();
        assert!(!p.feed(b"a\x01", &mut out));
        assert!(p.feed(b"x", &mut out));
        assert_eq!(out, b"a");
    }

    fn serial() -> Arc<Mutex<SerialConsole<Vec<u8>>>> {
        Arc::new(Mutex::new(SerialConsole::new(
            IrqLine::new().unwrap(),
            Vec::new(),
        )))
    }

    #[test]
    fn input_thread_feeds_the_uart_and_stops_at_end_of_input() {
        let serial = serial();
        let ctl = Arc::new(ExitControl::new().unwrap());
        let input: &'static [u8] = b"echo hi\r";
        spawn_input_thread(input, serial.clone(), ctl.clone())
            .unwrap()
            .join()
            .unwrap();
        let mut s = serial.lock().unwrap();
        let mut got = Vec::new();
        for _ in 0..8 {
            let mut b = [0u8];
            crate::bus::BusDevice::read(&mut *s, 0, &mut b);
            got.push(b[0]);
        }
        assert_eq!(got, b"echo hi\r");
        assert_eq!(ctl.requested(), None, "end of input does not stop the VM");
    }

    #[test]
    fn input_thread_reports_ctrl_a_x() {
        let ctl = Arc::new(ExitControl::new().unwrap());
        let input: &'static [u8] = b"\x01x";
        spawn_input_thread(input, serial(), ctl.clone())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(ctl.requested(), Some(StopReason::Quit));
    }
}
