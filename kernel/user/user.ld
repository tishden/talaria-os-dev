ENTRY(_start)
SECTIONS {
  . = 0x10000000;
  .text : { *(.text*) *(.rodata*) *(.data*) }
  /DISCARD/ : { *(.note*) *(.comment) *(.eh_frame*) }
}
