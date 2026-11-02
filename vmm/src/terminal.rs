//! The host side of the serial console.
//!
//! When run interactively we put the terminal in raw mode, so every keystroke (including Ctrl-C)
//! goes to the guest instead of being handled by the host terminal. A background thread reads
//! stdin and feeds the bytes into the emulated UART, which interrupts the guest.
//!
//! Escape: Ctrl-A then x quits the VMM (like QEMU). Ctrl-A Ctrl-A sends a literal Ctrl-A.

use std::io::{self, Read, Stdout};
use std::sync::{Arc, Mutex, OnceLock};

use crate::serial::Serial;

const CTRL_A: u8 = 0x01;

static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Switch the terminal to raw mode if stdin is a terminal. Call `restore` before exiting.
pub fn enter_raw_mode() -> io::Result<()> {
    if !stdin_is_tty() {
        return Ok(());
    }
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = SAVED_TERMIOS.set(termios);
        libc::cfmakeraw(&mut termios);
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn restore() {
    if let Some(termios) = SAVED_TERMIOS.get() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios) };
    }
}

/// Forward stdin to the guest's serial port until EOF.
pub fn spawn_input_thread(serial: Arc<Mutex<Serial<Stdout>>>) {
    let interactive = stdin_is_tty();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 64];
        let mut escape = false;
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return, // EOF: the guest just stops getting input
                Ok(n) => n,
            };
            let mut bytes = Vec::with_capacity(n);
            for &b in &buf[..n] {
                if !interactive {
                    bytes.push(b);
                } else if escape {
                    escape = false;
                    match b {
                        b'x' | b'X' => {
                            restore();
                            eprintln!("\r\n[vmm] quit (Ctrl-A x)");
                            std::process::exit(0);
                        }
                        CTRL_A => bytes.push(CTRL_A),
                        _ => {}
                    }
                } else if b == CTRL_A {
                    escape = true;
                } else {
                    bytes.push(b);
                }
            }
            if let Err(e) = serial.lock().unwrap().enqueue_input(&bytes) {
                eprintln!("[vmm] serial input: {e}");
                return;
            }
        }
    });
}
