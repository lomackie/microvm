//! A virtio "split" virtqueue: how guest and device exchange work through shared memory.
//!
//! Three structures live in guest RAM, at addresses the driver tells us during setup:
//!
//!   descriptor table: [ {addr u64, len u32, flags u16, next u16} ; size ]
//!       Each descriptor points at a guest buffer. Descriptors chain via `next` to form one
//!       request (e.g. header -> data -> status byte).
//!   available ring:   { flags u16, idx u16, ring: [head u16 ; size] }
//!       Written by the guest: "here are new requests, starting at these descriptor heads".
//!   used ring:        { flags u16, idx u16, ring: [ {id u32, len u32} ; size ] }
//!       Written by us: "this request is done, and I wrote `len` bytes into its buffers".
//!
//! Everything here is controlled by the guest, which we don't trust: every address, length and
//! index is bounds-checked before use, and descriptor chains are length-limited so a malicious
//! guest can't make us loop forever.

use std::sync::atomic::{fence, Ordering};

use super::{read_u16, read_u32, read_u64, write_u16, write_u32};

const DESC_F_NEXT: u16 = 1; // chain continues at `next`
const DESC_F_WRITE: u16 = 2; // device writes to this buffer (otherwise it reads from it)
const DESC_F_INDIRECT: u16 = 4; // not supported: we don't offer VIRTIO_RING_F_INDIRECT_DESC

#[derive(Debug, Clone, Copy)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub writable: bool,
}

/// One request: the descriptors of a chain, in order, plus the head index to report back.
#[derive(Debug)]
pub struct DescriptorChain {
    pub head: u16,
    pub descriptors: Vec<Descriptor>,
}

#[derive(Debug, Default)]
pub struct Queue {
    pub max_size: u16,
    /// Negotiated size (a power of two, set by the driver).
    pub size: u16,
    pub ready: bool,
    pub desc_table: u64,
    pub avail_ring: u64,
    pub used_ring: u64,
    /// Next available-ring entry we haven't processed yet.
    next_avail: u16,
    /// Next used-ring entry we'll fill.
    next_used: u16,
}

impl Queue {
    pub fn new(max_size: u16) -> Self {
        Self { max_size, ..Default::default() }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub fn is_valid(&self) -> bool {
        self.ready && self.size > 0 && self.size <= self.max_size && self.size.is_power_of_two()
    }

    /// Take the next request the guest made available, if any. Returns Err for a malformed
    /// chain (which still consumes the entry, so the caller should complete it with len 0).
    pub fn pop(&mut self, mem: &[u8]) -> Option<Result<DescriptorChain, (u16, &'static str)>> {
        let avail_idx = read_u16(mem, self.avail_ring + 2)?;
        if avail_idx == self.next_avail {
            return None;
        }
        // Read the ring entry only after seeing the new index (pairs with the guest's barrier).
        fence(Ordering::Acquire);
        let slot = (self.next_avail % self.size) as u64;
        let head = read_u16(mem, self.avail_ring + 4 + slot * 2)?;
        self.next_avail = self.next_avail.wrapping_add(1);
        Some(self.read_chain(mem, head).map_err(|e| (head, e)))
    }

    fn read_chain(&self, mem: &[u8], head: u16) -> Result<DescriptorChain, &'static str> {
        let mut descriptors = Vec::new();
        let mut index = head;
        loop {
            if index >= self.size {
                return Err("descriptor index out of range");
            }
            if descriptors.len() >= self.size as usize {
                return Err("descriptor chain loops");
            }
            let base = self.desc_table + index as u64 * 16;
            let (Some(addr), Some(len), Some(flags), Some(next)) =
                (read_u64(mem, base), read_u32(mem, base + 8), read_u16(mem, base + 12), read_u16(mem, base + 14))
            else {
                return Err("descriptor table outside guest memory");
            };
            if flags & DESC_F_INDIRECT != 0 {
                return Err("indirect descriptors not supported");
            }
            descriptors.push(Descriptor { addr, len, writable: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                return Ok(DescriptorChain { head, descriptors });
            }
            index = next;
        }
    }

    /// Tell the guest a request is finished, having written `len` bytes into its buffers.
    pub fn add_used(&mut self, mem: &mut [u8], head: u16, len: u32) {
        let slot = (self.next_used % self.size) as u64;
        let entry = self.used_ring + 4 + slot * 8;
        write_u32(mem, entry, head as u32);
        write_u32(mem, entry + 4, len);
        self.next_used = self.next_used.wrapping_add(1);
        // The entry must be visible before the index that publishes it.
        fence(Ordering::Release);
        write_u16(mem, self.used_ring + 2, self.next_used);
    }
}
