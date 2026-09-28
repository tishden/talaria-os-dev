/* The layout of the vDSO image: the headers+the code in the first page, the data page
   right after it (the kernel knows its address as base+4096). */
PHDRS
{
	load	PT_LOAD FILEHDR PHDRS FLAGS(5);
	dynamic	PT_DYNAMIC FLAGS(4);
}
SECTIONS
{
	. = SIZEOF_HEADERS;
	.hash		: { *(.hash) }		:load
	.gnu.hash	: { *(.gnu.hash) }	:load
	.dynsym		: { *(.dynsym) }	:load
	.dynstr		: { *(.dynstr) }	:load
	.gnu.version	: { *(.gnu.version) }	:load
	.gnu.version_d	: { *(.gnu.version_d) }	:load
	.dynamic	: { *(.dynamic) }	:load :dynamic
	.rodata		: { *(.rodata*) *(.note*) *(.eh_frame*) } :load
	.text		: { *(.text*) }		:load
	. = ALIGN(4096);
	.vdso_data	: { *(.vdso_data) }	:load
	/DISCARD/	: { *(.comment) *(.debug*) *(.symtab) *(.strtab) }
}
