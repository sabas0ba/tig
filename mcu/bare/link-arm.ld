/* Cortex-M4 (thumbv7em) 向け。QEMU の mps2-an386 マシンのメモリマップに合わせる。
 * code 4 MiB @ 0x0、SRAM 4 MiB @ 0x2000_0000。reset 時に vector[0]=SP、
 * vector[1]=reset が読まれる。 */
MEMORY {
  FLASH (rx) : ORIGIN = 0x00000000, LENGTH = 4M
  RAM  (rwx) : ORIGIN = 0x20000000, LENGTH = 4M
}

ENTRY(reset)
_stack_top = ORIGIN(RAM) + LENGTH(RAM);

SECTIONS {
  .vector_table ORIGIN(FLASH) : {
    LONG(_stack_top);          /* 初期 MSP */
    LONG(reset);               /* reset ハンドラ (thumb bit は関数シンボルが持つ) */
  } > FLASH

  .text : {
    *(.text .text.*)
  } > FLASH

  .rodata : {
    *(.rodata .rodata.*)
    . = ALIGN(4);
  } > FLASH

  _sidata = LOADADDR(.data);
  .data : {
    . = ALIGN(4);
    _sdata = .;
    *(.data .data.*)
    . = ALIGN(4);
    _edata = .;
  } > RAM AT > FLASH

  .bss : {
    . = ALIGN(4);
    _sbss = .;
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(4);
    _ebss = .;
  } > RAM

  /DISCARD/ : {
    *(.ARM.exidx*)
    *(.ARM.extab*)
  }
}
