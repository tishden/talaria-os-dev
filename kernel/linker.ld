ENTRY(pvh_start)

PHDRS
{
  text PT_LOAD FLAGS(7);
  note PT_NOTE FLAGS(4);
}

SECTIONS
{
  /* The image base is a build parameter: `--defsym KBASE=<address>`. Default 2 MB
     (all loaders — PVH, multiboot2, tboot, uefi-stub — expect it).
     Why a parameter: a NON-RELOCATABLE binary (ET_EXEC) loads exactly at its
     addresses, and if they overlap the kernel image it cannot be started at
     all — only by moving the kernel higher. That is how the ClickHouse build
     is made (its segments occupy 2 MB..526 MB). */
  . = DEFINED(KBASE) ? KBASE : 2M;
  __kernel_start = .;
  /* .text.boot FIRST: the pvh_start entry must coincide with the segment's
     p_paddr — uefi-stub relies on that (it checks entry == paddr). The multiboot2
     header (§3.1) goes right after it: GRUB only requires it to be within the
     first 32 KB of the file and 8-byte aligned. */
  .text : { *(.text.boot) . = ALIGN(8); KEEP(*(.mb2)) *(.text .text.*) } :text
  .rodata : { *(.rodata .rodata.*) } :text
  .data : { *(.data .data.*) } :text
  /* A9: the kernel ITSELF needs the .bss bounds — the entry zeroes it first thing.
     A loader (our tboot, GRUB, uefi-stub) may leave NOBITS unfilled, and on real
     hardware memory before us holds garbage, not zeros: statics then start with
     arbitrary values (locks taken, "already mounted", garbage pointers) and the
     kernel hangs silently. Under QEMU memory is zeroed, so this class of bugs is
     invisible locally. */
  __bss_start = .;
  .bss : { *(.bss .bss.*) *(COMMON) } :text
  __bss_end = .;
  PROVIDE(__kernel_end = .);
  .note : { *(.note .note.*) } :note
  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
