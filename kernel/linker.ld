/* The ELF entry is the VIRTUAL one, inside the segment's p_vaddr range: GRUB's multiboot2 ELF
   loader refuses an entry outside it ("entry point isn't in a segment") even though the mb2 tag
   names the real one. Every loader translates (entry - p_vaddr + p_paddr) or takes a physical
   symbol (the PVH note, the mb2 tag, tboot's `mb2_start_pa`): the image runs KOFF above where it
   is loaded (ARM64-PLAN §10.16 stage 7). */
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
  /* KOFF: the image's virtual address minus its physical one (stage 7; 0 — the image runs
     where it is loaded). Every section carries AT(): its load address is the physical one, so
     PT_LOAD's p_paddr stays what the loaders copy to. `kaddr::KIMG_OFF` must equal it (gate
     `kimg` checks `__kernel_pstart`). */
  KOFF = 0xFFFFFFFF80000000;   /* x86: the image at -2 GiB + its physical address (stage 7.2) */
  . = (DEFINED(KBASE) ? KBASE : 2M) + KOFF;
  __kernel_start = .;
  __kernel_pstart = __kernel_start - KOFF;
  /* .text.boot FIRST: the pvh_start entry must coincide with the segment's
     p_paddr — uefi-stub relies on that (entry - p_vaddr + p_paddr is the segment's start). The multiboot2
     header (§3.1) goes right after it: GRUB only requires it to be within the
     first 32 KB of the file and 8-byte aligned. */
  .text : AT(ADDR(.text) - KOFF) { *(.text.boot) . = ALIGN(8); KEEP(*(.mb2)) *(.text .text.*) } :text
  .rodata : AT(ADDR(.rodata) - KOFF) { *(.rodata .rodata.*) } :text
  .data : AT(ADDR(.data) - KOFF) { *(.data .data.*) } :text
  /* A9: the kernel ITSELF needs the .bss bounds — the entry zeroes it first thing.
     A loader (our tboot, GRUB, uefi-stub) may leave NOBITS unfilled, and on real
     hardware memory before us holds garbage, not zeros: statics then start with
     arbitrary values (locks taken, "already mounted", garbage pointers) and the
     kernel hangs silently. Under QEMU memory is zeroed, so this class of bugs is
     invisible locally. */
  __bss_start = .;
  .bss : AT(ADDR(.bss) - KOFF) { *(.bss .bss.*) *(COMMON) } :text
  __bss_end = .;
  PROVIDE(__kernel_end = .);
  /* The physical entries for the loaders' metadata (the ELF entry, the PVH note, the mb2 tag). */
  pvh_start_pa = pvh_start - KOFF;
  mb2_start_pa = mb2_start - KOFF;
  .note : AT(ADDR(.note) - KOFF) { *(.note .note.*) } :note
  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
/* KOFF puts GB0 at -2 GiB (kaddr::KIMG_VBASE): the image must stay in the first gigabyte (KBASE) */
ASSERT((__kernel_pstart & ~0x3FFFFFFF) + KOFF == 0xFFFFFFFF80000000, "the image is not in GB0: KOFF would not put it at KIMG_VBASE");
