//! Milestone 1: the smallest possible VMM.
//!
//! We ask KVM for an empty VM, give it 64 KiB of RAM, copy a tiny 16-bit real-mode program
//! into it, and run one vCPU. The guest writes a string to the serial port (I/O port 0x3f8),
//! one `out` instruction per byte. Each `out` traps back to us as KVM_EXIT_IO, and we play the
//! role of the serial port by printing the byte. Then the guest executes `hlt` and we stop.

mod kvm;

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;

use kvm::*;

/// Size of guest "physical" RAM. Guest physical addresses 0x0..0x10000 map onto this buffer.
const GUEST_MEM_SIZE: usize = 0x10000;
/// Guest physical address where we load the program and point the instruction pointer.
const CODE_ADDR: u64 = 0x1000;
/// COM1, the classic PC serial port. Linux will use the same port later (console=ttyS0).
const SERIAL_PORT: u16 = 0x3f8;

const MESSAGE: &[u8] = b"Hello from a KVM guest!\n\0";

/// Hand-assembled 16-bit guest. Equivalent to:
///
/// ```asm
///         mov  dx, 0x3f8      ; serial port
///         mov  si, msg        ; si = address of the string
/// next:   lodsb               ; al = [si], si++
///         test al, al
///         jz   done           ; stop at the NUL terminator
///         out  dx, al         ; write al to the port  -> VM exit!
///         jmp  next
/// done:   hlt                 ;                       -> VM exit!
/// msg:    db   "Hello from a KVM guest!", 10, 0
/// ```
fn guest_image() -> Vec<u8> {
    let mut code = vec![
        0xBA, 0xF8, 0x03, // mov dx, 0x3f8
        0xBE, 0x00, 0x00, // mov si, <patched below>
        0xAC, //             lodsb
        0x84, 0xC0, //       test al, al
        0x74, 0x03, //       jz +3 (to hlt)
        0xEE, //             out dx, al
        0xEB, 0xF8, //       jmp -8 (to lodsb)
        0xF4, //             hlt
    ];
    // The string sits right after the code; patch its address into `mov si, imm16`.
    let msg_addr = (CODE_ADDR as usize + code.len()) as u16;
    code[4..6].copy_from_slice(&msg_addr.to_le_bytes());
    code.extend_from_slice(MESSAGE);
    code
}

/// mmap anonymous, zeroed, read/write memory. Used for guest RAM.
fn mmap_anonymous(size: usize) -> io::Result<*mut u8> {
    let addr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(addr as *mut u8)
}

fn main() -> io::Result<()> {
    // 1. Open the KVM device and check we speak the same API.
    let kvm = OpenOptions::new().read(true).write(true).open("/dev/kvm")?;
    let version = ioctl(kvm.as_raw_fd(), KVM_GET_API_VERSION, 0)?;
    if version != KVM_API_VERSION {
        return Err(io::Error::other(format!("unexpected KVM API version {version}")));
    }

    // 2. Ask KVM for an empty VM. We get back a new fd that represents it.
    let vm = unsafe { File::from_raw_fd(ioctl(kvm.as_raw_fd(), KVM_CREATE_VM, 0)?) };

    // 3. Allocate guest RAM. It's ordinary memory in our process.
    let mem = mmap_anonymous(GUEST_MEM_SIZE)?;
    let guest_mem = unsafe { std::slice::from_raw_parts_mut(mem, GUEST_MEM_SIZE) };

    // 4. Register a memslot: guest physical 0x0.. is backed by `mem`. KVM builds EPT entries from this.
    let region = KvmUserspaceMemoryRegion {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: GUEST_MEM_SIZE as u64,
        userspace_addr: mem as u64,
    };
    ioctl(vm.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, &region as *const _ as u64)?;

    // 5. "Load" the guest program: just copy bytes into guest RAM at guest physical CODE_ADDR.
    let image = guest_image();
    guest_mem[CODE_ADDR as usize..CODE_ADDR as usize + image.len()].copy_from_slice(&image);

    // 6. Create vCPU 0, and map its kvm_run page: the shared struct KVM fills in on every exit.
    let vcpu = unsafe { File::from_raw_fd(ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU, 0)?) };
    let run_size = ioctl(kvm.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0)? as usize;
    let run_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            run_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            vcpu.as_raw_fd(),
            0,
        )
    };
    if run_ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let run = run_ptr as *mut KvmRun;

    // 7. Set the CPU state. A fresh vCPU is in 16-bit real mode, like a PC at power-on, but its
    //    code segment points at the BIOS reset vector (0xffff0). We have no BIOS, so make
    //    segments start at 0: real-mode address = segment base + offset = 0 + offset.
    let mut sregs = KvmSregs::default();
    ioctl(vcpu.as_raw_fd(), KVM_GET_SREGS, &mut sregs as *mut _ as u64)?;
    for seg in [&mut sregs.cs, &mut sregs.ds, &mut sregs.es, &mut sregs.ss] {
        seg.base = 0;
        seg.selector = 0;
    }
    ioctl(vcpu.as_raw_fd(), KVM_SET_SREGS, &sregs as *const _ as u64)?;

    let regs = KvmRegs {
        rip: CODE_ADDR,
        rflags: 0x2, // bit 1 is reserved and must be set; everything else (interrupts, etc.) off
        ..Default::default()
    };
    ioctl(vcpu.as_raw_fd(), KVM_SET_REGS, &regs as *const _ as u64)?;

    // 8. The run loop. KVM_RUN executes the guest natively until it does something we must handle.
    let mut stdout = io::stdout();
    let mut exits = 0u64;
    loop {
        ioctl(vcpu.as_raw_fd(), KVM_RUN, 0)?;
        exits += 1;

        let exit_reason = unsafe { (*run).exit_reason };
        match exit_reason {
            KVM_EXIT_IO => {
                let io_exit = unsafe { (*run).exit.io };
                // The bytes the guest wrote live inside the kvm_run page, at data_offset.
                let len = io_exit.size as usize * io_exit.count as usize;
                let data = unsafe {
                    std::slice::from_raw_parts((run_ptr as *const u8).add(io_exit.data_offset as usize), len)
                };
                if io_exit.port == SERIAL_PORT && io_exit.direction == KVM_EXIT_IO_OUT {
                    // We *are* the serial port: put the byte on our stdout.
                    stdout.write_all(data)?;
                    stdout.flush()?;
                } else {
                    eprintln!("[vmm] unhandled port I/O: {io_exit:?}");
                }
            }
            KVM_EXIT_HLT => {
                let mut regs = KvmRegs::default();
                ioctl(vcpu.as_raw_fd(), KVM_GET_REGS, &mut regs as *mut _ as u64)?;
                eprintln!("[vmm] guest halted after {exits} exits (rip={:#x})", regs.rip);
                return Ok(());
            }
            KVM_EXIT_MMIO => {
                let mmio = unsafe { (*run).exit.mmio };
                eprintln!("[vmm] unexpected MMIO exit: {mmio:?}");
                return Err(io::Error::other("unexpected MMIO"));
            }
            KVM_EXIT_SHUTDOWN => {
                // Usually a triple fault: the guest crashed so badly the CPU reset itself.
                return Err(io::Error::other("guest shut down (triple fault?)"));
            }
            KVM_EXIT_FAIL_ENTRY => {
                let fail = unsafe { (*run).exit.fail_entry };
                return Err(io::Error::other(format!("VM entry failed: {fail:?}")));
            }
            KVM_EXIT_INTERNAL_ERROR => {
                let internal = unsafe { (*run).exit.internal };
                return Err(io::Error::other(format!("KVM internal error: {internal:?}")));
            }
            other => return Err(io::Error::other(format!("unhandled exit reason {other}"))),
        }
    }
}
