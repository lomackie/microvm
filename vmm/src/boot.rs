//! Booting Linux directly, the way Firecracker does: no BIOS, no bootloader.
//!
//! We do what firmware + GRUB would have done, then jump straight to the kernel's 64-bit entry
//! point. The contract is the Linux "64-bit boot protocol" (Documentation/arch/x86/boot.rst):
//!   - the kernel image is loaded at the physical addresses its ELF headers ask for
//!   - the CPU is already in 64-bit long mode, with page tables that identity-map memory
//!   - a GDT provides code segment 0x10 and data segment 0x18
//!   - interrupts are off
//!   - %rsi points at `struct boot_params` (the "zero page"): memory map, command line, etc.

use std::io;

use crate::kvm::{KvmRegs, KvmSegment, KvmSregs};

// Guest physical memory layout for boot structures. These live in low memory, below the
// kernel (which loads at 16 MiB), and match the addresses Firecracker uses.
const GDT_ADDR: u64 = 0x500;
const IDT_ADDR: u64 = 0x520;
const ZERO_PAGE_ADDR: u64 = 0x7000;
const BOOT_STACK: u64 = 0x8ff0;
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xa000;
const PD_ADDR: u64 = 0xb000;
const CMDLINE_ADDR: u64 = 0x20000;
const CMDLINE_MAX: usize = 0x10000;
/// Low RAM ends here on a PC; 0x9fc00..0x100000 is traditionally BIOS/VGA territory.
const EBDA_START: u64 = 0x9fc00;
/// Start of "high" memory (1 MiB).
const HIMEM_START: u64 = 0x100000;

/// Load a vmlinux ELF image into guest memory. Returns the 64-bit entry point.
pub fn load_kernel(mem: &mut [u8], image: &[u8]) -> io::Result<u64> {
    let bad = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, format!("kernel ELF: {msg}"));
    let u16_at = |off: usize| u16::from_le_bytes(image[off..off + 2].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(image[off..off + 4].try_into().unwrap());
    let u64_at = |off: usize| u64::from_le_bytes(image[off..off + 8].try_into().unwrap());

    if image.len() < 64 || &image[0..4] != b"\x7fELF" || image[4] != 2 {
        return Err(bad("not a 64-bit ELF (pass vmlinux, not bzImage)"));
    }
    let entry = u64_at(0x18);
    let phoff = u64_at(0x20) as usize;
    let phentsize = u16_at(0x36) as usize;
    let phnum = u16_at(0x38) as usize;

    // Copy every loadable segment to the guest *physical* address it asks for.
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        const PT_LOAD: u32 = 1;
        if u32_at(ph) != PT_LOAD {
            continue;
        }
        let offset = u64_at(ph + 8) as usize;
        let paddr = u64_at(ph + 24) as usize;
        let filesz = u64_at(ph + 32) as usize;
        let memsz = u64_at(ph + 40) as usize;
        if paddr + memsz > mem.len() {
            return Err(bad("segment doesn't fit in guest memory; give the VM more RAM"));
        }
        mem[paddr..paddr + filesz].copy_from_slice(&image[offset..offset + filesz]);
        // The rest (memsz - filesz, e.g. .bss) must be zero; fresh mmap'd memory already is.
    }

    if entry < HIMEM_START {
        return Err(bad("entry point below 1 MiB"));
    }
    Ok(entry)
}

/// Write the kernel command line and the zero page (struct boot_params).
pub fn write_boot_params(mem: &mut [u8], cmdline: &str) -> io::Result<()> {
    if cmdline.len() >= CMDLINE_MAX {
        return Err(io::Error::other("kernel command line too long"));
    }
    let cmd = CMDLINE_ADDR as usize;
    mem[cmd..cmd + cmdline.len()].copy_from_slice(cmdline.as_bytes());
    mem[cmd + cmdline.len()] = 0;

    let ram_end = mem.len() as u64;
    let zp = &mut mem[ZERO_PAGE_ADDR as usize..ZERO_PAGE_ADDR as usize + 4096];
    zp.fill(0);
    let put = |zp: &mut [u8], off: usize, bytes: &[u8]| zp[off..off + bytes.len()].copy_from_slice(bytes);

    // setup_header fields (offsets are within boot_params; see Documentation/arch/x86/boot.rst).
    put(zp, 0x1fe, &0xAA55u16.to_le_bytes()); // boot_flag: "this is a valid boot sector"
    put(zp, 0x202, b"HdrS"); // header magic
    put(zp, 0x210, &[0xff]); // type_of_loader: "unregistered bootloader"
    put(zp, 0x228, &(CMDLINE_ADDR as u32).to_le_bytes()); // cmd_line_ptr
    put(zp, 0x230, &0x0100_0000u32.to_le_bytes()); // kernel_alignment: 16 MiB
    put(zp, 0x238, &(cmdline.len() as u32).to_le_bytes()); // cmdline_size

    // E820 memory map: the list of usable RAM ranges. This is how the kernel learns how much
    // memory it has. Firmware normally provides it; we are the firmware.
    const E820_RAM: u32 = 1;
    let e820 = [(0, EBDA_START), (HIMEM_START, ram_end - HIMEM_START)];
    for (i, (addr, size)) in e820.iter().enumerate() {
        let off = 0x2d0 + i * 20; // e820_table: packed {u64 addr, u64 size, u32 type}
        put(zp, off, &addr.to_le_bytes());
        put(zp, off + 8, &size.to_le_bytes());
        put(zp, off + 16, &E820_RAM.to_le_bytes());
    }
    put(zp, 0x1e8, &[e820.len() as u8]); // e820_entries
    Ok(())
}

/// Put the vCPU straight into 64-bit long mode, skipping the 16 -> 32 -> 64-bit climb that
/// firmware normally does. Writes the GDT and page tables into guest memory, and fills in the
/// register state for the kernel's entry point.
pub fn setup_long_mode(mem: &mut [u8], sregs: &mut KvmSregs, entry: u64) -> KvmRegs {
    let put_u64 = |mem: &mut [u8], addr: u64, val: u64| {
        mem[addr as usize..addr as usize + 8].copy_from_slice(&val.to_le_bytes())
    };

    // GDT (segment descriptor table). In 64-bit mode segments are mostly vestigial, but the CPU
    // still requires valid descriptors. Selector = index * 8, so code = 0x10 and data = 0x18,
    // exactly what the boot protocol asks for.
    let gdt = [
        0,                                // 0x00: null descriptor (required)
        0,                                // 0x08: unused
        gdt_entry(0xa09b, 0, 0xfffff),    // 0x10: 64-bit code, kernel privilege
        gdt_entry(0xc093, 0, 0xfffff),    // 0x18: data, kernel privilege
    ];
    for (i, entry) in gdt.iter().enumerate() {
        put_u64(mem, GDT_ADDR + i as u64 * 8, *entry);
    }
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = (gdt.len() * 8 - 1) as u16;
    // Empty interrupt table: interrupts stay off until the kernel installs its own.
    put_u64(mem, IDT_ADDR, 0);
    sregs.idt.base = IDT_ADDR;
    sregs.idt.limit = 7;

    let code = KvmSegment {
        base: 0,
        limit: 0xffff_ffff,
        selector: 0x10,
        type_: 0xb, // execute/read
        present: 1,
        s: 1,
        l: 1, // 64-bit code
        g: 1,
        ..Default::default()
    };
    let data = KvmSegment { selector: 0x18, type_: 0x3, l: 0, db: 1, ..code };
    sregs.cs = code;
    for seg in [&mut sregs.ds, &mut sregs.es, &mut sregs.fs, &mut sregs.gs, &mut sregs.ss] {
        *seg = data;
    }

    // Page tables: identity-map the first 1 GiB (virtual address == physical address) using
    // 2 MiB pages. Long mode can't run with paging off; the kernel replaces these early on.
    put_u64(mem, PML4_ADDR, PDPT_ADDR | 0x3); // present | writable
    put_u64(mem, PDPT_ADDR, PD_ADDR | 0x3);
    for i in 0..512u64 {
        put_u64(mem, PD_ADDR + i * 8, (i << 21) | 0x83); // present | writable | 2 MiB page
    }
    sregs.cr3 = PML4_ADDR;

    // Flip the mode switches: protected mode + paging (CR0), 64-bit page tables (CR4.PAE),
    // and long mode enable/active (EFER.LME/LMA).
    const CR0_PE: u64 = 1 << 0;
    const CR0_PG: u64 = 1 << 31;
    const CR4_PAE: u64 = 1 << 5;
    const EFER_LME: u64 = 1 << 8;
    const EFER_LMA: u64 = 1 << 10;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.cr4 |= CR4_PAE;
    sregs.efer |= EFER_LME | EFER_LMA;

    KvmRegs {
        rip: entry,
        rsp: BOOT_STACK,
        rbp: BOOT_STACK,
        rsi: ZERO_PAGE_ADDR, // the boot protocol: rsi = &boot_params
        rflags: 0x2,         // reserved bit; interrupts off
        ..Default::default()
    }
}

/// Encode an x86 segment descriptor.
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let (flags, base, limit) = (flags as u64, base as u64, limit as u64);
    ((base & 0xff00_0000) << 32)
        | ((flags & 0xf0ff) << 40)
        | ((limit & 0x000f_0000) << 32)
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0x0000_ffff)
}
