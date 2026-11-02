//! How emulated devices interrupt the guest.
//!
//! KVM emulates the interrupt controllers in the kernel; a device just tells KVM "my interrupt
//! line went high". Our devices sit on legacy PC interrupt lines (serial = 4, disk = 5) handled by
//! the 8259 PIC, which is edge-triggered: the guest is interrupted by a low-to-high transition. So
//! signalling an interrupt is a pulse, high then low.

use std::io;
use std::os::fd::RawFd;

use crate::kvm::{ioctl, KvmIrqLevel, KVM_IRQ_LINE};

#[derive(Clone, Copy, Debug)]
pub struct IrqLine {
    vm_fd: RawFd,
    irq: u32,
}

impl IrqLine {
    /// `vm_fd` must stay open for as long as this line is used.
    pub fn new(vm_fd: RawFd, irq: u32) -> Self {
        Self { vm_fd, irq }
    }

    pub fn pulse(&self) -> io::Result<()> {
        for level in [1, 0] {
            let line = KvmIrqLevel { irq: self.irq, level };
            ioctl(self.vm_fd, KVM_IRQ_LINE, &line as *const _ as u64)?;
        }
        Ok(())
    }
}
