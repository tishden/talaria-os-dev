/* The ELF entry is the PHYSICAL one; the image's virtual base may sit KOFF above (stage 7). */
ENTRY(_head_pa)

PHDRS
{
  text PT_LOAD FLAGS(7);
}

SECTIONS
{
  /* The image base on arm64. QEMU `virt` puts RAM at 1 GiB, and a Linux-format
     `Image` is loaded at RAM base + `text_offset` (0x80000 in our header, boot.rs), so
     the ELF's addresses must be exactly that: 0x4008_0000. Overridable with
     `--defsym KBASE=<address>` as on x86 (linker.ld). On EC2 the UEFI stub relocates
     the image to this address (docs/ARM64-PLAN.md §4.2). */
  /* KOFF as on x86 (linker.ld): virtual minus physical, 0 for now; `kaddr::KIMG_OFF` equals it. */
  KOFF = 0x0000FFFF40000000;   /* the image's gigabyte (1 GiB) at TTBR0 L0[511]/L1[510] (stage 7.3) */
  . = (DEFINED(KBASE) ? KBASE : 0x40080000) + KOFF;
  __kernel_start = .;
  __kernel_pstart = __kernel_start - KOFF;
  /* .text.boot FIRST and unconditionally: the 64-byte Linux `Image` header must be
     the first bytes of the raw image (`objcopy -O binary`), and its `code0` is the
     branch to the entry. */
  .text : AT(ADDR(.text) - KOFF) { KEEP(*(.text.boot)) *(.text .text.*) } :text
  /* W^X at the high alias (stage 7.4): the code, the constants and the data each start on a 2 MiB
     boundary, so the alias's 2M leaves carry one section's rights each (paging::init). */
  . = ALIGN(0x200000);
  __text_end = .;
  .rodata : AT(ADDR(.rodata) - KOFF) { *(.rodata .rodata.*) } :text
  . = ALIGN(0x200000);
  __rodata_end = .;
  .data : AT(ADDR(.data) - KOFF) { *(.data .data.*) } :text
  /* The kernel zeroes its own .bss first thing (boot.rs): QEMU's loader of a raw
     image fills nothing beyond the file, and on a board the memory holds garbage —
     the same A9 lesson as on x86 (linker.ld). */
  . = ALIGN(4096);
  __bss_start = .;
  .bss : AT(ADDR(.bss) - KOFF) { *(.bss .bss.*) *(COMMON) } :text
  . = ALIGN(4096);
  __bss_end = .;
  PROVIDE(__kernel_end = .);
  _head_pa = _head - KOFF;
  /* `image_size` in the header: the loader reserves this much from the load address,
     .bss included, so nothing (the DTB, an initrd) is placed on top of our statics. */
  __kernel_size = __kernel_end - __kernel_start;
  /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note.GNU-stack) }
}
/* KOFF puts the image's GIGABYTE at 0xFFFF_8000_0000 (kaddr::KIMG_VBASE): a KBASE elsewhere needs another KOFF */
ASSERT((__kernel_pstart & ~0x3FFFFFFF) + KOFF == 0xFFFF80000000, "KOFF does not put the image's gigabyte at KIMG_VBASE");
