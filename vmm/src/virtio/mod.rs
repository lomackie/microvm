//! virtio: the standard interface for paravirtualized devices.
//!
//! A virtio device has two halves:
//!   - a *transport*: how the guest finds and configures it. We use virtio-mmio, a page of
//!     registers at a fixed guest-physical address. The guest touches these rarely (setup, and a
//!     "doorbell" write when it has queued work), and each touch is a VM exit to us.
//!   - *virtqueues*: ring buffers in guest RAM (see queue.rs) where the actual requests go. We
//!     read and write them directly in memory, with no exits.
//!
//! So a disk read costs one exit for the doorbell plus one interrupt back, however many requests
//! the guest batched, instead of one exit per register access.

pub mod block;
pub mod queue;

use std::io;

use crate::irq::IrqLine;
use queue::Queue;

/// Feature bit every modern (virtio 1.0+) device must offer.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Device status bits, written by the driver as it initializes the device.
const STATUS_DRIVER_OK: u32 = 4;

/// What a specific device type (block, network, vsock, ...) provides to the transport.
pub trait VirtioDevice {
    /// virtio device ID (2 = block).
    fn device_id(&self) -> u32;
    /// Device-specific feature bits on offer.
    fn features(&self) -> u64;
    /// Max size of each of the device's queues; also how many queues it has.
    fn queue_max_sizes(&self) -> &[u16];
    /// Read from the device-specific configuration space (registers at 0x100 onwards).
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// The guest rang the doorbell for queue `index`: process its requests. Returns true if any
    /// were completed (and the guest should be interrupted).
    fn process_queue(&mut self, index: usize, queue: &mut Queue, mem: &mut [u8]) -> bool;
}

/// The virtio-mmio register page (virtio spec 1.2, section 4.2.2), version 2 ("modern").
pub struct MmioTransport<D: VirtioDevice> {
    pub device: D,
    queues: Vec<Queue>,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    status: u32,
    interrupt_status: u32,
    irq: IrqLine,
}

impl<D: VirtioDevice> MmioTransport<D> {
    pub fn new(device: D, irq: IrqLine) -> Self {
        let queues = device.queue_max_sizes().iter().map(|&max| Queue::new(max)).collect();
        Self {
            device,
            queues,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            status: 0,
            interrupt_status: 0,
            irq,
        }
    }

    fn offered_features(&self) -> u64 {
        self.device.features() | VIRTIO_F_VERSION_1
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Guest read `data.len()` bytes at `offset` within the register page.
    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= 0x100 {
            return self.device.read_config(offset - 0x100, data);
        }
        let value: u32 = match offset {
            0x000 => 0x7472_6976, // magic: "virt"
            0x004 => 2,           // transport version: modern
            0x008 => self.device.device_id(),
            0x00c => 0, // vendor ID
            0x010 => match self.device_features_sel {
                0 => self.offered_features() as u32,
                1 => (self.offered_features() >> 32) as u32,
                _ => 0,
            },
            0x034 => self.selected_queue().map_or(0, |q| q.max_size as u32),
            0x044 => self.selected_queue().map_or(0, |q| q.ready as u32),
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => 0, // config generation: our config never changes
            _ => 0,
        };
        let bytes = value.to_le_bytes();
        let n = data.len().min(4);
        data[..n].copy_from_slice(&bytes[..n]);
    }

    /// Guest wrote `data` at `offset` within the register page.
    pub fn write(&mut self, offset: u64, data: &[u8], mem: &mut [u8]) -> io::Result<()> {
        let mut bytes = [0u8; 4];
        let n = data.len().min(4);
        bytes[..n].copy_from_slice(&data[..n]);
        let value = u32::from_le_bytes(bytes);
        let set_low = |field: &mut u64, v: u32| *field = (*field & !0xffff_ffff) | v as u64;
        let set_high = |field: &mut u64, v: u32| *field = (*field & 0xffff_ffff) | (v as u64) << 32;

        match offset {
            0x014 => self.device_features_sel = value,
            0x020 => {
                // The driver says which offered features it will use.
                let offered = self.offered_features();
                match self.driver_features_sel {
                    0 => set_low(&mut self.driver_features, value),
                    1 => set_high(&mut self.driver_features, value),
                    _ => {}
                }
                self.driver_features &= offered;
            }
            0x024 => self.driver_features_sel = value,
            0x030 => self.queue_sel = value,
            0x038 => {
                if let Some(q) = self.selected_queue() {
                    q.size = value as u16;
                }
            }
            0x044 => {
                if let Some(q) = self.selected_queue() {
                    q.ready = value == 1;
                }
            }
            0x050 => {
                // The doorbell: "queue N has new requests".
                let index = value as usize;
                let active = self.status & STATUS_DRIVER_OK != 0;
                if let Some(q) = self.queues.get_mut(index).filter(|q| active && q.is_valid()) {
                    if self.device.process_queue(index, q, mem) {
                        self.interrupt_status |= 1; // "used buffer notification"
                        self.irq.pulse()?;
                    }
                }
            }
            0x064 => self.interrupt_status &= !value, // driver acknowledges interrupts
            0x070 if value == 0 => self.reset(),
            0x070 => self.status = value,
            0x080 => self.with_queue(|q| set_low(&mut q.desc_table, value)),
            0x084 => self.with_queue(|q| set_high(&mut q.desc_table, value)),
            0x090 => self.with_queue(|q| set_low(&mut q.avail_ring, value)),
            0x094 => self.with_queue(|q| set_high(&mut q.avail_ring, value)),
            0x0a0 => self.with_queue(|q| set_low(&mut q.used_ring, value)),
            0x0a4 => self.with_queue(|q| set_high(&mut q.used_ring, value)),
            _ => {}
        }
        Ok(())
    }

    fn with_queue(&mut self, f: impl FnOnce(&mut Queue)) {
        if let Some(q) = self.selected_queue() {
            f(q)
        }
    }

    fn reset(&mut self) {
        self.queues.iter_mut().for_each(Queue::reset);
        self.driver_features = 0;
        self.status = 0;
        self.interrupt_status = 0;
    }
}

// Bounds-checked little-endian access to guest memory. Guest physical address == offset into
// `mem`, because our single memslot starts at guest physical 0. Out-of-range reads give None and
// out-of-range writes are dropped: the guest can't make us touch memory outside its RAM.

fn guest_slice(mem: &[u8], addr: u64, len: usize) -> Option<&[u8]> {
    let start = usize::try_from(addr).ok()?;
    mem.get(start..start.checked_add(len)?)
}

fn guest_slice_mut(mem: &mut [u8], addr: u64, len: usize) -> Option<&mut [u8]> {
    let start = usize::try_from(addr).ok()?;
    mem.get_mut(start..start.checked_add(len)?)
}

fn read_u16(mem: &[u8], addr: u64) -> Option<u16> {
    Some(u16::from_le_bytes(guest_slice(mem, addr, 2)?.try_into().unwrap()))
}

fn read_u32(mem: &[u8], addr: u64) -> Option<u32> {
    Some(u32::from_le_bytes(guest_slice(mem, addr, 4)?.try_into().unwrap()))
}

fn read_u64(mem: &[u8], addr: u64) -> Option<u64> {
    Some(u64::from_le_bytes(guest_slice(mem, addr, 8)?.try_into().unwrap()))
}

fn write_u16(mem: &mut [u8], addr: u64, value: u16) {
    if let Some(s) = guest_slice_mut(mem, addr, 2) {
        s.copy_from_slice(&value.to_le_bytes());
    }
}

fn write_u32(mem: &mut [u8], addr: u64, value: u32) {
    if let Some(s) = guest_slice_mut(mem, addr, 4) {
        s.copy_from_slice(&value.to_le_bytes());
    }
}
