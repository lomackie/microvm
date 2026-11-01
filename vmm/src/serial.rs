//! A minimal 16550A UART: just enough for Linux to detect it and use it as a console.
//!
//! The chip exposes 8 one-byte registers at consecutive I/O ports (base..base+7). Linux's
//! 8250 driver probes them (write a value, read it back, check loopback wiring) before trusting
//! the port, so we must answer reads like real hardware would, not just print writes.
//!
//! Not implemented yet: receiving input and raising interrupts. Linux's console output path
//! polls the "transmitter empty" bit, so neither is needed to see boot logs.

use std::io::Write;

// Register offsets from the base port.
const DATA: u8 = 0; // THR (write) / RBR (read); divisor latch low when DLAB=1
const IER: u8 = 1; // interrupt enable; divisor latch high when DLAB=1
const IIR_FCR: u8 = 2; // interrupt identification (read) / FIFO control (write)
const LCR: u8 = 3; // line control
const MCR: u8 = 4; // modem control
const LSR: u8 = 5; // line status
const MSR: u8 = 6; // modem status
const SCR: u8 = 7; // scratch

const LCR_DLAB: u8 = 0x80; // "divisor latch access": offsets 0/1 become the baud-rate divisor
const MCR_LOOP: u8 = 0x10; // loopback mode, used by the driver's self-test
const LSR_THR_EMPTY: u8 = 0x20; // ready to accept a byte
const LSR_IDLE: u8 = 0x40; // transmitter completely idle
const IIR_NO_INTERRUPT: u8 = 0x01;
const IIR_FIFO_ENABLED: u8 = 0xC0; // makes Linux identify the chip as a 16550A
const MSR_DEFAULT: u8 = 0xB0; // carrier detect, data set ready, clear to send: "a cable is plugged in"

pub struct Serial<W: Write> {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: [u8; 2],
    out: W,
    written: u64,
}

impl<W: Write> Serial<W> {
    pub fn new(out: W) -> Self {
        Self { ier: 0, lcr: 0x03, mcr: 0x08, scr: 0, divisor: [0x0C, 0x00], out, written: 0 }
    }

    /// Guest executed `in` on base+offset.
    pub fn read(&mut self, offset: u8) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor[0],
            IER if dlab => self.divisor[1],
            DATA => 0, // no input yet
            IER => self.ier,
            IIR_FCR => IIR_FIFO_ENABLED | IIR_NO_INTERRUPT,
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_THR_EMPTY | LSR_IDLE,
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
    pub fn write(&mut self, offset: u8, value: u8) -> std::io::Result<()> {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor[0] = value,
            IER if dlab => self.divisor[1] = value,
            // A transmitted byte: this is where guest console output reaches the host.
            // In loopback mode it would come back as input instead; we just drop it.
            DATA if self.mcr & MCR_LOOP == 0 => {
                self.out.write_all(&[value])?;
                self.written += 1;
            }
            DATA => {}
            IER => self.ier = value & 0x0F,
            IIR_FCR => {} // FIFO control: nothing to do, we have no real FIFOs
            LCR => self.lcr = value,
            MCR => self.mcr = value & 0x1F,
            SCR => self.scr = value,
            _ => {} // LSR/MSR are read-only
        }
        Ok(())
    }

    /// Bytes the guest has sent to the console so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}
