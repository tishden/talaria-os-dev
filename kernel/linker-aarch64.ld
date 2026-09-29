ENTRY(_head)

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
  . = DEFINED(KBASE) ? KBASE : 0x40080000;
  __kernel_start = .;
  /* .text.boot FIRST and unconditionally: the 64-byte Linux `Image` header must be
     the first bytes of the raw image (`objcopy -O binary`), and its `code0` is the
     branch to the entry. */
  .text : { KEEP(*(.text.boot)) *(.text .text.*) } :text
  .rodata : { *(.rodata .rodata.*) } :text
  .data : { *(.data .data.*) } :text
  /* The kernel zeroes its own .bss first thing (boot.rs): QEMU's loader of a raw
     image fills nothing beyond the file, and on a board the memory holds garbage —
     the same A9 lesson as on x86 (linker.ld). */
  . = ALIGN(4096);
  __bss_start = .;
  .bss : { *(.bss .bss.*) *(COMMON) } :text
  . = ALIGN(4096);
  __bss_end = .;
  PROVIDE(__kernel_end = .);
  /* `image_size` in the header: the loader reserves this much from the load address,
     .bss included, so nothing (the DTB, an initrd) is placed on top of our statics. */
  __kernel_size = __kernel_end - __kernel_start;
  /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note.GNU-stack) }
}
