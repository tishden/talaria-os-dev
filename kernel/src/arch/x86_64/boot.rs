//! PVH boot: 32-bit entry from QEMU/Cloud Hypervisor, transition to long mode.
//!
//! Entry contract (PVH): protected mode, paging off, flat segments,
//! %ebx = phys addr of hvm_start_info. We identity-map the first 4 GiB with
//! 2 MiB pages (the last 32 MiB uncached for MMIO), enable SSE and jump to
//! `rust_main(start_info)`.

use core::arch::global_asm;

global_asm!(
    r#"
/* STAGE 7 (ARM64-PLAN §10.16): the image may be linked KOFF above where it is loaded. Until
   long mode is on and the jump below is taken, this code runs at the PHYSICAL address with
   paging off or identity, so every absolute address in it is `(sym - KOFF_A)`. */
.set KOFF_A, {koff}
/* ── §3.1 multiboot2 (a BIOS boot through GRUB) ────────────────────────────
   Native bare metal on EC2 is closed by the platform: not a single *.metal type
   accepts an AMI with boot-mode uefi. The BIOS bootloader = GRUB, and the kernel needs a second
   entry. The .mb2 section is linked right after .text.boot (linker.ld), so that the entry
   pvh_start stays equal to p_paddr — the uefi-stub requires that.
   The "entry address" tag (type 3) tells GRUB to jump into mb2_start rather than into e_entry:
   otherwise the bootloaders would have to be told apart by the eax register, whose value
   PVH does not stipulate. */
.section .mb2, "a"
.align 8
mb2_header:
    .long 0xE85250D6                       /* magic */
    .long 0                                /* the architecture: i386 */
    .long mb2_header_end - mb2_header
    .long -(0xE85250D6 + (mb2_header_end - mb2_header))
    .align 8
    .short 3                               /* the tag: the entry address */
    .short 0
    .long 12
    .long mb2_start_pa                     /* physical (linker.ld: mb2_start - KOFF) */
    .align 8
    .short 0                               /* the final tag */
    .short 0
    .long 8
mb2_header_end:

.section .note, "a"
.align 4
    .long 4                     /* namesz */
    .long 4                     /* descsz */
    .long 18                    /* XEN_ELFNOTE_PHYS32_ENTRY */
    .ascii "Xen\0"
    .long pvh_start_pa          /* desc: the 32-bit entry, physical (linker.ld) */

.section .bss
.align 4096
pml4:   .skip 4096
pdpt:   .skip 4096
pdpt_hi: .skip 4096
pds:    .skip 16384             /* 4 page directories = 4 GiB of 2M pages */
.align 16
stack_bottom:
        .skip 65536
stack_top:

.section .rodata
.align 8
gdt:
    .quad 0
    .quad 0x00209A0000000000    /* 64-bit code */
    .quad 0x0000920000000000    /* data */
gdt_end:
gdt_desc:
    .word gdt_end - gdt - 1
    .long gdt - KOFF_A

.section .text.boot, "ax"
.code32
/* A9: zero the .bss BEFORE the first access to any static.
   A bootloader is not obliged to fill NOBITS with zeroes (our tboot reads exactly
   p_filesz off the disk), and on real hardware the memory before us contains garbage — under QEMU it
   is zero, so the whole class of errors is invisible locally. The stack is not ours yet, so
   it is not a subroutine but a macro: there is nothing to do a `call` with here.
   The registers: edi/ecx/eax are free (we do not touch esi with the start_info). */
.macro ZERO_BSS
    mov $(__bss_start - KOFF_A), %edi
    mov $(__bss_end - KOFF_A), %ecx
    sub %edi, %ecx
    add $3, %ecx
    shr $2, %ecx
    xor %eax, %eax
    rep stosl
.endm
.global pvh_start
pvh_start:
    cli
    cld
    mov %ebx, %esi              /* save start_info */

    /* early debug: 'T' on COM1 before anything else */
    mov $0x3F8, %dx
    mov $0x54, %al
    out %al, %dx
    ZERO_BSS
boot_common:

    /* pml4[0] = pdpt | P|RW */
    mov $(pdpt - KOFF_A), %eax
    or $3, %eax
    mov %eax, (pml4 - KOFF_A)
    movl $0, (pml4 - KOFF_A + 4)

    /* The image's high alias (stage 7): pml4[511] = pdpt_hi, pdpt_hi[510] = the GB0 directory
       (pds' first), so the image is at -2 GiB + its physical address as well. */
    mov $(pdpt_hi - KOFF_A), %eax
    or $3, %eax
    mov %eax, (pml4 - KOFF_A + 511*8)
    movl $0, (pml4 - KOFF_A + 511*8 + 4)
    mov $(pds - KOFF_A), %eax
    or $3, %eax
    mov %eax, (pdpt_hi - KOFF_A + 510*8)
    movl $0, (pdpt_hi - KOFF_A + 510*8 + 4)

    /* pdpt[i] = &pds[i*512] | P|RW, i = 0..4 */
    xor %ecx, %ecx
1:  mov $(pds - KOFF_A), %eax
    mov %ecx, %edx
    shl $12, %edx
    add %edx, %eax
    or $3, %eax
    mov %eax, (pdpt - KOFF_A)(,%ecx,8)
    movl $0, (pdpt - KOFF_A + 4)(,%ecx,8)
    inc %ecx
    cmp $4, %ecx
    jb 1b

    /* pds[j] = (j << 21) | P|RW|PS ; PCD|PWT for >= 0xFE000000 (MMIO) */
    xor %ecx, %ecx
2:  mov %ecx, %eax
    shl $21, %eax
    or $0x83, %eax
    cmp $2032, %ecx             /* 0xFE000000 >> 21 */
    jb 3f
    or $0x18, %eax              /* PCD|PWT */
3:  mov %eax, (pds - KOFF_A)(,%ecx,8)
    movl $0, (pds - KOFF_A + 4)(,%ecx,8)
    inc %ecx
    cmp $2048, %ecx
    jb 2b

    mov $(pml4 - KOFF_A), %eax
    mov %eax, %cr3

    /* CR4: PAE | OSFXSR | OSXMMEXCPT (SSE for the Rust target) */
    mov %cr4, %eax
    or $0x620, %eax
    mov %eax, %cr4

    /* EFER.LME */
    mov $0xC0000080, %ecx
    rdmsr
    or $0x100, %eax
    wrmsr

    lgdt (gdt_desc - KOFF_A)

    /* CR0: clear EM, set MP, enable PG */
    mov %cr0, %eax
    and $0xFFFFFFFB, %eax
    or $0x80000003, %eax
    mov %eax, %cr0

    ljmp $0x08, $(long_start - KOFF_A)

.code32
.global mb2_start
mb2_start:
    /* The multiboot2 entry: eax = 0x36d76289, ebx = the MBI's address. Further on it is the same path
       as PVH's; the translation of the MBI into an hvm_start_info is done by Rust (boot::mb2_convert),
       when long mode and the serial line already work. */
    cli
    cld
    mov %ebx, %esi
    ZERO_BSS                    /* BEFORE the write of MB2_BOOT: the flag itself lies in the .bss */
    movl $1, (MB2_BOOT - KOFF_A)
    mov $0x3F8, %dx
    mov $0x4D, %al              /* 'M': the multiboot2 entry */
    out %al, %dx
    jmp boot_common

.code64
long_start:
    /* early debug: 'L' — long mode reached */
    mov $0x3F8, %dx
    mov $0x4C, %al
    out %al, %dx
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %ax, %ax
    mov %ax, %fs
    mov %ax, %gs
    /* into the image's linked address (the high half once KOFF is not 0) */
    movabs $long_high, %rax
    jmp *%rax
long_high:
    mov $stack_top, %rsp
    mov %esi, %edi              /* start_info (zero-extended) -> arg0 */
    xor %ebp, %ebp
    call rust_main
4:  hlt
    jmp 4b
"#,
    koff = const crate::mem::KIMG_OFF,
    options(att_syntax)
);

// ── §3.1: multiboot2 -> hvm_start_info ───────────────────────────────────────
// The kernel below in the stack knows exactly one format of boot information — the PVH
// hvm_start_info (mem::init, acpi::init and read_cmdline read it). So
// GRUB's MBI is translated into it HERE, and further on the boot path is common.

/// It is set by the 32-bit entry mb2_start (see the asm above).
#[unsafe(no_mangle)]
pub static mut MB2_BOOT: u32 = 0;

#[repr(C)]
struct StartInfo {
    magic: u32, version: u32, flags: u32, nr_modules: u32,
    modlist_paddr: u64, cmdline_paddr: u64, rsdp_paddr: u64,
    memmap_paddr: u64, memmap_entries: u32, _reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MemEntry { addr: u64, size: u64, typ: u32, _r: u32 }

const MM_MAX: usize = 128;
static mut MB_SI: StartInfo = StartInfo {
    magic: 0x336E_C578, version: 1, flags: 0, nr_modules: 0, modlist_paddr: 0,
    cmdline_paddr: 0, rsdp_paddr: 0, memmap_paddr: 0, memmap_entries: 0, _reserved: 0,
};
static mut MB_MM: [MemEntry; MM_MAX] = [MemEntry { addr: 0, size: 0, typ: 0, _r: 0 }; MM_MAX];

/// Whether we booted through multiboot2 (GRUB/BIOS).
pub fn is_multiboot2() -> bool { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(MB2_BOOT)) != 0 } }

/// Translate the MBI at the address `mbi` into an hvm_start_info and return its address.
/// We parse exactly what the kernel needs: the cmdline (tag 1), the memory map (tag 6) and
/// the RSDP (tags 14/15 — ACPI 1.0/2.0; the RSDP itself lies as a COPY inside the tag, and it is its
/// physical address that we give out).
pub fn mb2_convert(mbi: u64) -> u64 {
    unsafe {
        let total = core::ptr::read_volatile(crate::mem::kptr::<u32>(mbi)) as u64;
        let mut p = mbi + 8;
        let end = mbi + total;
        let mut n = 0usize;
        while p + 8 <= end {
            let ttype = core::ptr::read_volatile(crate::mem::kptr::<u32>(p));
            let tsize = core::ptr::read_volatile(crate::mem::kptr::<u32>(p + 4)) as u64;
            if ttype == 0 || tsize < 8 { break; }
            match ttype {
                1 => { MB_SI.cmdline_paddr = p + 8; }                       // the cmdline string
                14 | 15 => { if MB_SI.rsdp_paddr == 0 || ttype == 15 { MB_SI.rsdp_paddr = p + 8; } }
                6 => {                                                     // the memory map
                    let esz = core::ptr::read_volatile(crate::mem::kptr::<u32>(p + 8)) as u64;
                    let mut e = p + 16;
                    while e + esz <= p + tsize && n < MM_MAX {
                        let base = core::ptr::read_volatile(crate::mem::kptr::<u64>(e));
                        let len = core::ptr::read_volatile(crate::mem::kptr::<u64>(e + 8));
                        let mt = core::ptr::read_volatile(crate::mem::kptr::<u32>(e + 16));
                        // multiboot2: 1 available, 3 ACPI reclaimable, 4 NVS, otherwise occupied
                        let typ = match mt { 1 => 1u32, 3 => 3, 4 => 4, _ => 2 };
                        if len > 0 {
                            if n > 0 && MB_MM[n - 1].typ == typ && MB_MM[n - 1].addr + MB_MM[n - 1].size == base {
                                MB_MM[n - 1].size += len;
                            } else { MB_MM[n] = MemEntry { addr: base, size: len, typ, _r: 0 }; n += 1; }
                        }
                        e += esz;
                    }
                }
                _ => {}
            }
            p += (tsize + 7) & !7;
        }
        MB_SI.memmap_paddr = crate::mem::kpa_of(core::ptr::addr_of!(MB_MM));
        MB_SI.memmap_entries = n as u32;
        let rsdp = (*core::ptr::addr_of!(MB_SI)).rsdp_paddr;
        crate::println!("multiboot2: the MBI {:#x} ({} bytes) -> the start_info: the map has {} records, rsdp={:#x}",
            mbi, total, n, rsdp);
        crate::mem::kpa_of(core::ptr::addr_of!(MB_SI))
    }
}
