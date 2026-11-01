//! A minimal VMM: boots a Linux kernel in a KVM virtual machine with a serial console.
//!
//! Setup, in order:
//!   1. ask KVM for an empty VM, and turn on its in-kernel interrupt controllers + timer
//!   2. give the VM RAM (ordinary memory in this process, registered as a memslot)
//!   3. act as firmware + bootloader: load the kernel, write the memory map and command line
//!   4. create a vCPU already in 64-bit mode, pointed at the kernel's entry point
//!   5. run it: every time the guest touches hardware we emulate, KVM_RUN returns to us
//!
//! Usage: vmm --kernel <vmlinux> [--mem-mib N] [--cmdline "..."] [--debug] [--break <addr>]

mod boot;
mod kvm;
mod serial;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kvm::*;
use serial::Serial;

const DEFAULT_MEM_MIB: usize = 256;
/// console=ttyS0: log to our serial port. reboot=t: reboot via triple fault, which reaches us as
/// KVM_EXIT_SHUTDOWN. panic=-1: reboot immediately on panic, so a panic ends the VM.
/// pci=off: we have no PCI bus.
const DEFAULT_CMDLINE: &str = "console=ttyS0 earlyprintk=serial reboot=t panic=-1 pci=off";
/// COM1. The UART occupies 8 consecutive ports from here.
const COM1: u16 = 0x3f8;
/// Intel VT-x needs KVM to reserve 3 pages of guest-physical space for its own use. Put them
/// near the top of the 32-bit address space, far away from guest RAM.
const KVM_TSS_ADDR: u64 = 0xfffb_d000;

struct Args {
    kernel: String,
    mem_mib: usize,
    cmdline: String,
    /// Report where the guest is executing when it stops exiting for a while.
    debug: bool,
    /// Stop and dump guest state when the guest executes this (virtual) address.
    break_at: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { kernel: String::new(), mem_mib: DEFAULT_MEM_MIB, cmdline: DEFAULT_CMDLINE.into(), debug: false, break_at: None };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--kernel" => args.kernel = value()?,
            "--mem-mib" => args.mem_mib = value()?.parse().map_err(|e| format!("--mem-mib: {e}"))?,
            "--cmdline" => args.cmdline = value()?,
            "--debug" => args.debug = true,
            "--break" => {
                let v = value()?;
                let hex = v.trim_start_matches("0x");
                args.break_at = Some(u64::from_str_radix(hex, 16).map_err(|e| format!("--break: {e}"))?);
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    if args.kernel.is_empty() {
        return Err("usage: vmm --kernel <vmlinux> [--mem-mib N] [--cmdline \"...\"] [--debug] [--break <addr>]".into());
    }
    if args.mem_mib < 32 || args.mem_mib > 1024 {
        // The boot page tables identity-map 1 GiB; enough for now.
        return Err("--mem-mib must be between 32 and 1024".into());
    }
    Ok(args)
}

fn main() {
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    if let Err(e) = run(args) {
        eprintln!("[vmm] error: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> io::Result<()> {
    let start = Instant::now();

    // 1. KVM, the VM, and KVM's built-in interrupt controllers (PIC, IOAPIC, per-CPU APIC) and
    //    timer (PIT). These must exist before any vCPU is created.
    let kvm = OpenOptions::new().read(true).write(true).open("/dev/kvm")?;
    if ioctl(kvm.as_raw_fd(), KVM_GET_API_VERSION, 0)? != KVM_API_VERSION {
        return Err(io::Error::other("unexpected KVM API version"));
    }
    let vm = unsafe { File::from_raw_fd(ioctl(kvm.as_raw_fd(), KVM_CREATE_VM, 0)?) };
    ioctl(vm.as_raw_fd(), KVM_SET_TSS_ADDR, KVM_TSS_ADDR)?;
    ioctl(vm.as_raw_fd(), KVM_CREATE_IRQCHIP, 0)?;
    let pit = KvmPitConfig { flags: KVM_PIT_SPEAKER_DUMMY, ..Default::default() };
    ioctl(vm.as_raw_fd(), KVM_CREATE_PIT2, &pit as *const _ as u64)?;

    // 2. Guest RAM: one memslot covering guest physical 0..mem_size.
    let mem_size = args.mem_mib << 20;
    let mem_ptr = mmap(None, mem_size, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE)?;
    let mem = unsafe { std::slice::from_raw_parts_mut(mem_ptr, mem_size) };
    let region = KvmUserspaceMemoryRegion {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: mem_size as u64,
        userspace_addr: mem_ptr as u64,
    };
    ioctl(vm.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, &region as *const _ as u64)?;

    // 3. Firmware + bootloader duties.
    let image = fs::read(&args.kernel).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", args.kernel)))?;
    let entry = boot::load_kernel(mem, &image)?;
    boot::write_boot_params(mem, &args.cmdline)?;
    drop(image);

    // 4. vCPU 0: tell it which CPU features it has, and start it in 64-bit mode at the kernel.
    let vcpu = unsafe { File::from_raw_fd(ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU, 0)?) };

    // CPUID is how software asks "what CPU am I on, what features do I have?". KVM proposes a
    // list it can support on this host; we pass it through unchanged for now.
    let mut cpuid: Box<KvmCpuid2> = Box::new(unsafe { std::mem::zeroed() });
    cpuid.nent = KVM_MAX_CPUID_ENTRIES as u32;
    ioctl(kvm.as_raw_fd(), KVM_GET_SUPPORTED_CPUID, &mut *cpuid as *mut _ as u64)?;
    ioctl(vcpu.as_raw_fd(), KVM_SET_CPUID2, &*cpuid as *const _ as u64)?;

    // MSRs (model-specific registers) that firmware normally configures before the OS runs.
    // IA32_MISC_ENABLE.FAST_STRING: without it Linux disables fast `rep movs`, yet still trusts
    // CPUID's FSRM bit; on this kernel that combination patches memmove into a version with no
    // length check, which corrupts memory on the first short copy.
    const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
    const MISC_ENABLE_FAST_STRING: u64 = 1 << 0;
    let msrs = KvmMsrs::<1> {
        nmsrs: 1,
        pad: 0,
        entries: [KvmMsrEntry { index: MSR_IA32_MISC_ENABLE, data: MISC_ENABLE_FAST_STRING, ..Default::default() }],
    };
    if ioctl(vcpu.as_raw_fd(), KVM_SET_MSRS, &msrs as *const _ as u64)? != 1 {
        return Err(io::Error::other("KVM refused to set MSRs"));
    }

    let mut sregs = KvmSregs::default();
    ioctl(vcpu.as_raw_fd(), KVM_GET_SREGS, &mut sregs as *mut _ as u64)?;
    let regs = boot::setup_long_mode(mem, &mut sregs, entry);
    ioctl(vcpu.as_raw_fd(), KVM_SET_SREGS, &sregs as *const _ as u64)?;
    ioctl(vcpu.as_raw_fd(), KVM_SET_REGS, &regs as *const _ as u64)?;

    let run_size = ioctl(kvm.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0)? as usize;
    let run_ptr = mmap(Some(vcpu.as_raw_fd()), run_size, libc::MAP_SHARED)?;
    let run = run_ptr as *mut KvmRun;

    // 5. The run loop.
    let mut serial = Serial::new(io::stdout());
    let mut exits: HashMap<String, u64> = HashMap::new();
    let mut first_output: Option<Duration> = None;
    eprintln!("[vmm] booting {} with {} MiB RAM, entry {entry:#x}", args.kernel, args.mem_mib);
    let exit_count = Arc::new(AtomicU64::new(0));
    if args.debug {
        spawn_stall_watchdog(exit_count.clone());
    }
    if let Some(addr) = args.break_at {
        // Hardware breakpoint 0: DR0 = address, DR7 bit 0 = "enable DR0, break on execute".
        let mut dbg = KvmGuestDebug { control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP, ..Default::default() };
        dbg.debugreg[0] = addr;
        dbg.debugreg[7] = 0x1;
        ioctl(vcpu.as_raw_fd(), KVM_SET_GUEST_DEBUG, &dbg as *const _ as u64)?;
    }

    loop {
        match ioctl(vcpu.as_raw_fd(), KVM_RUN, 0) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                // The watchdog kicked us out of the guest. Report where it was, then resume.
                dump_guest_state(&vcpu, mem, "stall: no exits for 3s")?;
                continue;
            }
            Err(e) => return Err(e),
        }
        exit_count.fetch_add(1, Ordering::Relaxed);

        match unsafe { (*run).exit_reason } {
            KVM_EXIT_IO => {
                let io_exit = unsafe { (*run).exit.io };
                let len = io_exit.size as usize * io_exit.count as usize;
                let data = unsafe { std::slice::from_raw_parts_mut(run_ptr.add(io_exit.data_offset as usize), len) };
                let is_out = io_exit.direction == KVM_EXIT_IO_OUT;
                *exits.entry(io_exit_label(io_exit.port, is_out)).or_default() += 1;

                match io_exit.port {
                    p if (COM1..COM1 + 8).contains(&p) => {
                        let offset = (p - COM1) as u8;
                        for byte in data.iter_mut() {
                            if is_out {
                                serial.write(offset, *byte)?;
                            } else {
                                *byte = serial.read(offset);
                            }
                        }
                        if is_out && first_output.is_none() && serial.bytes_written() > 0 {
                            first_output = Some(start.elapsed());
                        }
                    }
                    // Nothing at this port. A real PC's bus reads back all ones from empty
                    // ports, and the kernel's probes understand that as "no device here".
                    _ if !is_out => data.fill(0xff),
                    _ => {}
                }
            }
            KVM_EXIT_MMIO => {
                // No MMIO devices yet. Like empty ports: reads return all ones, writes vanish.
                let mmio = unsafe { &mut (*run).exit.mmio };
                let label = format!("mmio {} {:#x}", if mmio.is_write != 0 { "write" } else { "read" }, mmio.phys_addr);
                *exits.entry(label).or_default() += 1;
                if mmio.is_write == 0 {
                    mmio.data = [0xff; 8];
                }
            }
            KVM_EXIT_DEBUG => {
                serial.flush()?;
                dump_guest_state(&vcpu, mem, "breakpoint hit")?;
                print_summary(start.elapsed(), first_output, &exits);
                return Ok(());
            }
            KVM_EXIT_SHUTDOWN => {
                // The guest reset the machine (with reboot=t, that's how Linux reboots or ends
                // a panic). We treat "reboot" as "the VM is done".
                *exits.entry("shutdown".into()).or_default() += 1;
                serial.flush()?;
                print_summary(start.elapsed(), first_output, &exits);
                return Ok(());
            }
            KVM_EXIT_HLT => {
                // With KVM's in-kernel interrupt controller, an idle guest's `hlt` is handled
                // inside KVM (the vCPU sleeps until an interrupt). We only see this if the guest
                // halted with interrupts off, i.e. it stopped for good.
                serial.flush()?;
                print_summary(start.elapsed(), first_output, &exits);
                return Err(io::Error::other("guest halted with interrupts disabled"));
            }
            KVM_EXIT_FAIL_ENTRY => {
                let fail = unsafe { (*run).exit.fail_entry };
                return Err(io::Error::other(format!("VM entry failed (bad CPU state?): {fail:?}")));
            }
            KVM_EXIT_INTERNAL_ERROR => {
                let internal = unsafe { (*run).exit.internal };
                return Err(io::Error::other(format!("KVM internal error: {internal:?}")));
            }
            other => return Err(io::Error::other(format!("unhandled exit reason {other}"))),
        }
    }
}

/// Debug aid: print the guest's registers, the top of its stack, and any kernel code addresses
/// further up the stack page (a crude backtrace, newest first; map them with `nm vmlinux`).
fn dump_guest_state(vcpu: &File, mem: &[u8], why: &str) -> io::Result<()> {
    let mut regs = KvmRegs::default();
    ioctl(vcpu.as_raw_fd(), KVM_GET_REGS, &mut regs as *mut _ as u64)?;
    let mut sregs = KvmSregs::default();
    ioctl(vcpu.as_raw_fd(), KVM_GET_SREGS, &mut sregs as *mut _ as u64)?;
    let interrupts = if regs.rflags & 0x200 != 0 { "on" } else { "off" };
    eprintln!(
        "[vmm] {why}: rip={:#x} rsp={:#x} cr2={:#x} cs={:#x} interrupts={interrupts}",
        regs.rip, regs.rsp, sregs.cr2, sregs.cs.selector
    );
    eprintln!(
        "[vmm]   rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x}",
        regs.rax, regs.rbx, regs.rcx, regs.rdx, regs.rsi, regs.rdi
    );
    let read_u64 = |virt: u64| {
        let p = guest_virt_to_phys(mem, sregs.cr3, virt)?;
        Some(u64::from_le_bytes(mem.get(p..p + 8)?.try_into().unwrap()))
    };
    // Right after an exception, the top of the stack is the frame the CPU pushed:
    // [error code,] rip, cs, rflags, rsp, ss.
    let top: Vec<String> = (0..6).filter_map(|i| read_u64(regs.rsp + i * 8)).map(|w| format!("{w:#x}")).collect();
    eprintln!("[vmm]   stack top: {}", top.join(" "));
    let mut addr = regs.rsp & !7;
    while addr < (regs.rsp | 0xfff) + 1 {
        if let Some(word) = read_u64(addr).filter(|w| (0xffff_ffff_8100_0000..0xffff_ffff_8300_0000).contains(w)) {
            eprintln!("[vmm]   [{addr:#x}] {word:#x}");
        }
        addr += 8;
    }
    Ok(())
}

/// Debug aid: if the guest stops exiting for 3 seconds, interrupt the vCPU with a signal so the
/// run loop can report the guest's instruction pointer. A busy guest that never touches emulated
/// hardware is otherwise invisible to the VMM. (An idle guest also stops exiting, so only use
/// this while investigating a hang.)
fn spawn_stall_watchdog(exit_count: Arc<AtomicU64>) {
    extern "C" fn ignore(_: i32) {}
    unsafe {
        // No SA_RESTART: we want KVM_RUN to return EINTR rather than be silently restarted.
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = ignore as usize;
        libc::sigaction(libc::SIGUSR1, &action, ptr::null_mut());
    }
    let vcpu_thread = unsafe { libc::pthread_self() };
    std::thread::spawn(move || {
        let mut last = exit_count.load(Ordering::Relaxed);
        loop {
            std::thread::sleep(Duration::from_secs(3));
            let now = exit_count.load(Ordering::Relaxed);
            if now == last {
                unsafe { libc::pthread_kill(vcpu_thread, libc::SIGUSR1) };
            }
            last = now;
        }
    });
}

/// Translate a guest virtual address to guest physical by walking the guest's own 4-level page
/// tables, exactly as the CPU's MMU would. Debug aid; returns None if the address isn't mapped.
fn guest_virt_to_phys(mem: &[u8], cr3: u64, virt: u64) -> Option<usize> {
    const PRESENT: u64 = 1;
    const HUGE: u64 = 1 << 7;
    const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
    let read = |phys: u64| -> Option<u64> {
        let p = phys as usize;
        Some(u64::from_le_bytes(mem.get(p..p + 8)?.try_into().unwrap()))
    };
    let mut table = cr3 & ADDR_MASK;
    // Levels: PML4 (bits 47..39), PDPT (38..30), PD (29..21), PT (20..12).
    for (level, shift) in [39u32, 30, 21, 12].into_iter().enumerate() {
        let entry = read(table + ((virt >> shift) & 0x1ff) * 8)?;
        if entry & PRESENT == 0 {
            return None;
        }
        // A "huge" entry at the PDPT (1 GiB) or PD (2 MiB) level maps the rest directly.
        if level == 3 || (level > 0 && entry & HUGE != 0) {
            let page_mask = (1u64 << shift) - 1;
            return Some(((entry & ADDR_MASK & !page_mask) | (virt & page_mask)) as usize);
        }
        table = entry & ADDR_MASK;
    }
    None
}

/// mmap read/write memory: anonymous (guest RAM) or backed by an fd (the vCPU's kvm_run page).
fn mmap(fd: Option<i32>, size: usize, flags: i32) -> io::Result<*mut u8> {
    let addr = unsafe {
        libc::mmap(ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, flags, fd.unwrap_or(-1), 0)
    };
    if addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(addr as *mut u8)
}

/// Human-readable name for an I/O port exit, for the summary.
fn io_exit_label(port: u16, is_out: bool) -> String {
    let name = match port {
        0x3f8..=0x3ff => "COM1 serial",
        0x2f8..=0x2ff | 0x3e8..=0x3ef | 0x2e8..=0x2ef => "COM2-4 serial (absent)",
        0x60 | 0x64 => "i8042 keyboard controller (absent)",
        0x70 | 0x71 => "CMOS/RTC clock (absent)",
        0x80 => "POST delay port",
        0xcf8..=0xcff => "PCI config (absent)",
        _ => "unknown",
    };
    format!("io {} {port:#06x} {name}", if is_out { "out" } else { "in " })
}

fn print_summary(elapsed: Duration, first_output: Option<Duration>, exits: &HashMap<String, u64>) {
    let first = first_output.map_or("never".into(), |d| format!("{:.3}s", d.as_secs_f64()));
    eprintln!();
    eprintln!("[vmm] guest reset after {:.3}s (first console output at {first})", elapsed.as_secs_f64());
    let total: u64 = exits.values().sum();
    eprintln!("[vmm] {total} VM exits to userspace, by cause:");
    let mut sorted: Vec<_> = exits.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (label, count) in sorted.iter().take(12) {
        eprintln!("[vmm]   {count:>8}  {label}");
    }
}
