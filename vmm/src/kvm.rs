//! Raw KVM bindings: only the ioctl numbers and `#[repr(C)]` structs we need so far.
//! Everything here mirrors <linux/kvm.h> on x86_64.

use std::io;
use std::os::fd::RawFd;

/// The only stable KVM API version. It has been 12 since 2007.
pub const KVM_API_VERSION: i32 = 12;

// ioctl request numbers. An ioctl number packs (direction, struct size, type 0xAE, nr):
//   _IO(0xAE, nr)     = 0x0000_AE00 | nr               (no argument struct)
//   _IOW(0xAE, nr, T) = 0x4000_0000 | size_of::<T>() << 16 | 0xAE00 | nr   (we write T to the kernel)
//   _IOR(0xAE, nr, T) = 0x8000_0000 | size_of::<T>() << 16 | 0xAE00 | nr   (kernel writes T to us)
pub const KVM_GET_API_VERSION: u64 = 0xAE00;
pub const KVM_CREATE_VM: u64 = 0xAE01;
pub const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xAE04;
pub const KVM_CREATE_VCPU: u64 = 0xAE41;
pub const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_AE46; // _IOW, 32-byte struct
pub const KVM_RUN: u64 = 0xAE80;
pub const KVM_GET_REGS: u64 = 0x8090_AE81; // _IOR, 144-byte struct
pub const KVM_SET_REGS: u64 = 0x4090_AE82; // _IOW, 144-byte struct
pub const KVM_GET_SREGS: u64 = 0x8138_AE83; // _IOR, 312-byte struct
pub const KVM_SET_SREGS: u64 = 0x4138_AE84; // _IOW, 312-byte struct

// Why KVM_RUN returned (kvm_run.exit_reason).
pub const KVM_EXIT_IO: u32 = 2;
pub const KVM_EXIT_HLT: u32 = 5;
pub const KVM_EXIT_MMIO: u32 = 6;
pub const KVM_EXIT_SHUTDOWN: u32 = 8;
pub const KVM_EXIT_FAIL_ENTRY: u32 = 9;
pub const KVM_EXIT_INTERNAL_ERROR: u32 = 17;

pub const KVM_EXIT_IO_OUT: u8 = 1;

/// A memslot: "guest physical range [guest_phys_addr, +memory_size) is backed by my memory at userspace_addr".
#[repr(C)]
#[derive(Debug, Default)]
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32,
    pub flags: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
}

/// General-purpose registers.
#[repr(C)]
#[derive(Debug, Default)]
pub struct KvmRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// A segment register (cs, ds, ...) including the hidden base/limit parts the CPU caches.
#[repr(C)]
#[derive(Debug, Default)]
pub struct KvmSegment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
    pub padding: u8,
}

/// GDT/IDT pointer.
#[repr(C)]
#[derive(Debug, Default)]
pub struct KvmDtable {
    pub base: u64,
    pub limit: u16,
    pub padding: [u16; 3],
}

/// "Special" registers: segments, control registers, EFER. This is where CPU modes live.
#[repr(C)]
#[derive(Debug, Default)]
pub struct KvmSregs {
    pub cs: KvmSegment,
    pub ds: KvmSegment,
    pub es: KvmSegment,
    pub fs: KvmSegment,
    pub gs: KvmSegment,
    pub ss: KvmSegment,
    pub tr: KvmSegment,
    pub ldt: KvmSegment,
    pub gdt: KvmDtable,
    pub idt: KvmDtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    pub interrupt_bitmap: [u64; 4],
}

/// Shared page between KVM and us, mmap'd from the vCPU fd. KVM fills it in on every exit.
#[repr(C)]
pub struct KvmRun {
    pub request_interrupt_window: u8,
    pub immediate_exit: u8,
    _padding1: [u8; 6],
    pub exit_reason: u32,
    pub ready_for_interrupt_injection: u8,
    pub if_flag: u8,
    pub flags: u16,
    pub cr8: u64,
    pub apic_base: u64,
    /// Exit details; which field is valid depends on `exit_reason`. Starts at offset 32.
    pub exit: KvmRunExit,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union KvmRunExit {
    pub io: KvmRunIo,
    pub mmio: KvmRunMmio,
    pub fail_entry: KvmRunFailEntry,
    pub internal: KvmRunInternal,
    _pad: [u8; 256],
}

/// KVM_EXIT_IO: the guest executed `in`/`out`. Data lives at (kvm_run base + data_offset).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmRunIo {
    pub direction: u8,
    pub size: u8,
    pub port: u16,
    pub count: u32,
    pub data_offset: u64,
}

/// KVM_EXIT_MMIO: the guest touched a guest-physical address with no memslot.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmRunMmio {
    pub phys_addr: u64,
    pub data: [u8; 8],
    pub len: u32,
    pub is_write: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmRunFailEntry {
    pub hardware_entry_failure_reason: u64,
    pub cpu: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmRunInternal {
    pub suberror: u32,
}

/// Thin wrapper over ioctl(2) that turns -1 into an io::Error.
pub fn ioctl(fd: RawFd, request: u64, arg: u64) -> io::Result<i32> {
    let ret = unsafe { libc::ioctl(fd, request as _, arg) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

// Catch layout mistakes at compile time: these must match the sizes encoded in the ioctl numbers.
const _: () = assert!(std::mem::size_of::<KvmUserspaceMemoryRegion>() == 32);
const _: () = assert!(std::mem::size_of::<KvmRegs>() == 144);
const _: () = assert!(std::mem::size_of::<KvmSregs>() == 312);
const _: () = assert!(std::mem::offset_of!(KvmRun, exit) == 32);
