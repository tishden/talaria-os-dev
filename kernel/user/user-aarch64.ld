/* user.ld's arm64 twin: the load address must be RAM on the machine (see LDFA64 in the Makefile). */
ENTRY(_start)
SECTIONS {
  . = 0x50000000;
  .text : { *(.text*) *(.rodata*) *(.data*) }
  /DISCARD/ : { *(.note*) *(.comment) *(.eh_frame*) }
}
