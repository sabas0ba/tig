/* RISC-V (rv32/rv64) 向け。QEMU の virt マシンの DRAM は 0x8000_0000 から。
 * `-bios none -kernel <elf>` で entry (_start) へ直接ジャンプする。ELF の
 * PROGBITS は QEMU が RAM へ読み込むため .data の LMA==VMA、.bss は _start で
 * ゼロ埋めする (QEMU の RAM は 0 初期化だが移植性のため明示する)。 */
MEMORY {
  RAM (rwx) : ORIGIN = 0x80000000, LENGTH = 128M
}

ENTRY(_start)
_stack_top = ORIGIN(RAM) + 0x00400000;   /* 先頭 4 MiB の末尾を stack 頂点にする */

SECTIONS {
  .text : {
    *(.text.init)
    *(.text .text.*)
  } > RAM

  .rodata : {
    *(.rodata .rodata.*)
    . = ALIGN(8);
  } > RAM

  .data : {
    . = ALIGN(8);
    _sdata = .;
    *(.sdata .sdata.*)
    *(.data .data.*)
    . = ALIGN(8);
    _edata = .;
  } > RAM

  .bss : {
    . = ALIGN(8);
    _sbss = .;
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(8);
    _ebss = .;
  } > RAM
}
