//! A 16550A UART: the guest's serial console, in both directions.
//!
//! The chip exposes 8 one-byte registers at consecutive I/O ports (base..base+7). Linux's
//! 8250 driver probes them (write a value, read it back, check loopback wiring) before trusting
//! the port, so we must answer reads like real hardware would, not just print writes.
//!
//! Interrupts: during boot the kernel's console polls the "transmitter empty" bit, but once a
//! userspace program (the shell) opens the port, the driver becomes interrupt-driven. It sends a
//! burst of bytes, then waits for a "transmitter empty" interrupt before sending more, and waits
//! for a "data received" interrupt to read keystrokes.

use std::collections::VecDeque;
use std::io::{self, Write};

use crate::irq::IrqLine;

// Register offsets from the base port.
const DATA: u8 = 0; // THR (write) / RBR (read); divisor latch low when DLAB=1
const IER: u8 = 1; // interrupt enable; divisor latch high when DLAB=1
const IIR_FCR: u8 = 2; // interrupt identification (read) / FIFO control (write)
const LCR: u8 = 3; // line control
const MCR: u8 = 4; // modem control
const LSR: u8 = 5; // line status
const MSR: u8 = 6; // modem status
const SCR: u8 = 7; // scratch

const IER_RX_AVAILABLE: u8 = 0x01; // "interrupt me when a byte arrives"
const IER_THR_EMPTY: u8 = 0x02; // "interrupt me when you can take more output"
const LCR_DLAB: u8 = 0x80; // "divisor latch access": offsets 0/1 become the baud-rate divisor
const MCR_LOOP: u8 = 0x10; // loopback mode, used by the driver's self-test
const FCR_CLEAR_RX: u8 = 0x02;
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20; // ready to accept a byte
const LSR_IDLE: u8 = 0x40; // transmitter completely idle
const IIR_NO_INTERRUPT: u8 = 0x01;
const IIR_THR_EMPTY: u8 = 0x02;
const IIR_RX_AVAILABLE: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xC0; // makes Linux identify the chip as a 16550A
const MSR_DEFAULT: u8 = 0xB0; // carrier detect, data set ready, clear to send: "a cable is plugged in"

/// Input bytes we'll buffer for a guest that isn't reading. Beyond this we drop, like a real
/// UART overrunning (though real FIFOs are only 16 bytes).
const RX_BUFFER_MAX: usize = 4096;

pub struct Serial<W: Write> {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: [u8; 2],
    rx: VecDeque<u8>,
    /// Set when the transmitter becomes empty; cleared when the guest reads IIR and sees it.
    thr_empty_pending: bool,
    out: W,
    written: u64,
    irq: IrqLine,
}

impl<W: Write> Serial<W> {
    pub fn new(out: W, irq: IrqLine) -> Self {
        Self {
            ier: 0,
            lcr: 0x03,
            mcr: 0x08,
            scr: 0,
            divisor: [0x0C, 0x00],
            rx: VecDeque::new(),
            thr_empty_pending: false,
            out,
            written: 0,
            irq,
        }
    }

    /// Guest executed `in` on base+offset.
    pub fn read(&mut self, offset: u8) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor[0],
            IER if dlab => self.divisor[1],
            DATA => self.rx.pop_front().unwrap_or(0),
            IER => self.ier,
            IIR_FCR => {
                let iir = self.pending_interrupt();
                // Per the 16550 datasheet, reading IIR acknowledges a "transmitter empty" interrupt.
                if iir == IIR_THR_EMPTY {
                    self.thr_empty_pending = false;
                }
                IIR_FIFO_ENABLED | iir
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let ready = if self.rx.is_empty() { 0 } else { LSR_DATA_READY };
                LSR_THR_EMPTY | LSR_IDLE | ready
            }
            MSR if self.mcr & MCR_LOOP != 0 => {
                // In loopback the modem outputs are wired back to the inputs:
                // RTS->CTS, DTR->DSR, OUT1->RI, OUT2->DCD. The driver checks this wiring.
                let m = self.mcr;
                ((m & 0x02) << 3) | ((m & 0x01) << 5) | ((m & 0x04) << 4) | ((m & 0x08) << 4)
            }
            MSR => MSR_DEFAULT,
            SCR => self.scr,
            _ => 0xFF,
        }
    }

    /// Guest executed `out` on base+offset.
    pub fn write(&mut self, offset: u8, value: u8) -> io::Result<()> {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor[0] = value,
            IER if dlab => self.divisor[1] = value,
            // A transmitted byte: this is where guest console output reaches the host.
            // In loopback mode it would come back as input instead; we just drop it.
            DATA if self.mcr & MCR_LOOP == 0 => {
                self.out.write_all(&[value])?;
                self.out.flush()?; // shell prompts don't end in a newline
                self.written += 1;
                // We "transmit" instantly, so the transmitter is empty again right away.
                self.thr_empty_pending = true;
                self.raise_if_pending()?;
            }
            DATA => {}
            IER => {
                self.ier = value & 0x0F;
                if self.ier & IER_THR_EMPTY != 0 {
                    self.thr_empty_pending = true;
                }
                // Enabling interrupts with input already buffered must interrupt now.
                self.raise_if_pending()?;
            }
            IIR_FCR if value & FCR_CLEAR_RX != 0 => self.rx.clear(),
            IIR_FCR => {}
            LCR => self.lcr = value,
            MCR => self.mcr = value & 0x1F,
            SCR => self.scr = value,
            _ => {} // LSR/MSR are read-only
        }
        Ok(())
    }

    /// Bytes typed on the host, arriving at the guest's serial port.
    pub fn enqueue_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        let room = RX_BUFFER_MAX.saturating_sub(self.rx.len());
        self.rx.extend(&bytes[..bytes.len().min(room)]);
        self.raise_if_pending()
    }

    /// Bytes the guest has sent to the console so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// What IIR reports: the highest-priority enabled interrupt condition, if any.
    fn pending_interrupt(&self) -> u8 {
        if self.ier & IER_RX_AVAILABLE != 0 && !self.rx.is_empty() {
            IIR_RX_AVAILABLE
        } else if self.ier & IER_THR_EMPTY != 0 && self.thr_empty_pending {
            IIR_THR_EMPTY
        } else {
            IIR_NO_INTERRUPT
        }
    }

    fn raise_if_pending(&self) -> io::Result<()> {
        if self.pending_interrupt() != IIR_NO_INTERRUPT {
            self.irq.pulse()?;
        }
        Ok(())
    }
}
