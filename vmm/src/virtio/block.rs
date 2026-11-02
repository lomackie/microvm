//! virtio-blk: a disk backed by a plain file on the host.
//!
//! Each request is a descriptor chain:
//!   [ header: {type u32, reserved u32, sector u64} ]  (guest -> us)
//!   [ data buffer(s) ]                                 (either direction)
//!   [ status: u8 ]                                     (us -> guest)
//! The guest's filesystem (ext4) runs inside the guest; we only see "read/write N bytes at
//! sector S" and turn that into pread/pwrite on the image file.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use super::queue::{Descriptor, Queue};
use super::{guest_slice, guest_slice_mut, read_u32, read_u64, VirtioDevice};

const SECTOR_SIZE: u64 = 512;
const QUEUE_SIZE: u16 = 256;

const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9; // we honour "flush to stable storage" requests

const T_IN: u32 = 0; // read from disk
const T_OUT: u32 = 1; // write to disk
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8; // disk serial number

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const DISK_ID: &[u8] = b"microvm-disk";

pub struct Block {
    file: File,
    capacity_bytes: u64,
}

impl Block {
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        // Round down: the guest only sees whole sectors.
        Ok(Self { file, capacity_bytes: len / SECTOR_SIZE * SECTOR_SIZE })
    }

    /// Execute one request. Returns (status, bytes written into guest buffers).
    fn execute(&self, request_type: u32, sector: u64, data: &[Descriptor], mem: &mut [u8]) -> (u8, u32) {
        let total: u64 = data.iter().map(|d| d.len as u64).sum();
        let Some(start) = sector.checked_mul(SECTOR_SIZE) else { return (S_IOERR, 0) };
        let in_bounds = start.checked_add(total).is_some_and(|end| end <= self.capacity_bytes);
        match request_type {
            T_IN | T_OUT if !in_bounds => (S_IOERR, 0),
            T_IN => {
                let mut offset = start;
                for d in data {
                    let Some(buf) = guest_slice_mut(mem, d.addr, d.len as usize).filter(|_| d.writable) else {
                        return (S_IOERR, 0);
                    };
                    if self.file.read_exact_at(buf, offset).is_err() {
                        return (S_IOERR, 0);
                    }
                    offset += d.len as u64;
                }
                (S_OK, total as u32)
            }
            T_OUT => {
                let mut offset = start;
                for d in data {
                    let Some(buf) = guest_slice(mem, d.addr, d.len as usize).filter(|_| !d.writable) else {
                        return (S_IOERR, 0);
                    };
                    if self.file.write_all_at(buf, offset).is_err() {
                        return (S_IOERR, 0);
                    }
                    offset += d.len as u64;
                }
                (S_OK, 0)
            }
            T_FLUSH => match self.file.sync_data() {
                Ok(()) => (S_OK, 0),
                Err(_) => (S_IOERR, 0),
            },
            T_GET_ID => {
                let Some(d) = data.first().filter(|d| d.writable) else { return (S_IOERR, 0) };
                let n = DISK_ID.len().min(d.len as usize);
                match guest_slice_mut(mem, d.addr, n) {
                    Some(buf) => {
                        buf.copy_from_slice(&DISK_ID[..n]);
                        (S_OK, n as u32)
                    }
                    None => (S_IOERR, 0),
                }
            }
            _ => (S_UNSUPP, 0),
        }
    }
}

impl VirtioDevice for Block {
    fn device_id(&self) -> u32 {
        2
    }

    fn features(&self) -> u64 {
        VIRTIO_BLK_F_FLUSH
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config space starts with the capacity in 512-byte sectors (u64). We advertise nothing
        // else, so the rest reads as zero.
        let mut config = [0u8; 8];
        config.copy_from_slice(&(self.capacity_bytes / SECTOR_SIZE).to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn process_queue(&mut self, _index: usize, queue: &mut Queue, mem: &mut [u8]) -> bool {
        let mut completed = false;
        while let Some(popped) = queue.pop(mem) {
            completed = true;
            let chain = match popped {
                Ok(chain) => chain,
                Err((head, why)) => {
                    eprintln!("[vmm] virtio-blk: bad request from guest: {why}");
                    queue.add_used(mem, head, 0);
                    continue;
                }
            };
            let descs = &chain.descriptors;
            // Need at least a header and a status byte; the status buffer must be writable.
            let (Some(header), Some(status)) = (descs.first(), descs.last().filter(|d| d.writable && d.len >= 1))
            else {
                eprintln!("[vmm] virtio-blk: malformed request chain");
                queue.add_used(mem, chain.head, 0);
                continue;
            };
            let (Some(request_type), Some(sector)) = (read_u32(mem, header.addr), read_u64(mem, header.addr + 8))
            else {
                queue.add_used(mem, chain.head, 0);
                continue;
            };
            let data = if descs.len() > 2 { &descs[1..descs.len() - 1] } else { &[][..] };

            let (result, written) = self.execute(request_type, sector, data, mem);
            if let Some(byte) = guest_slice_mut(mem, status.addr, 1) {
                byte[0] = result;
            }
            queue.add_used(mem, chain.head, written + 1);
        }
        completed
    }
}
