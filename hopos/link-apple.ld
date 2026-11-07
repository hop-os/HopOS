/*
 * Het image van HopOS op de Mac mini M4 (board/apple).
 *
 * Een raw image, gelinkt op RAM_BASE = DRAM + 4 GB = 0x101_0000_0000 (1 TiB
 * + 4 GB): daaronder woont de boot-keten (boot_args, de ADT, m1n1 met zijn
 * heap), en de bovenkant van het DRAM is van iBoot. De eerste 64 KB zijn de
 * voorkant van het bootobject (board/apple/src/head.rs):
 *
 *   0x0000  stub_reset   waar een core uit reset landt (RVBAR)
 *   0x0100  parameters   doel, grootte, entry (hieronder ingevuld)
 *   0x0800  stub_entry   waar iBoot of m1n1 de boot-core aflevert
 *   0xE000  scratch      waar de stub vandaan kwam, x0, de brievenbus
 *   0xE100  param-blok   van de m1n1-loader (spin-table)
 *   0xF000  config       hopos.cfg van de loader
 *   0x10000 de kern      _start_apple eerst
 *
 * De stub kopieert het bestand (tot __image_file_end) naar KERN_BASE als
 * iBoot het elders neerzette. De kern-RAM loopt tot KERN_END (256 MB,
 * board_apple::KERN_RAM); daarboven de DMA-regio. De heap is alles tussen
 * het einde van de stack en die grens, op de bovenste pagina na: dat is de
 * trampoline van de kern-flip (board_apple::slots::FLIP_TRAMP_PA).
 *
 * De `_start` van cpu::boot (`.text.boot`) blijft hier weg: de ingang is
 * `_start_apple`, en --gc-sections laat de rest vallen.
 *
 * Het contract met board-apple: __apple_head, __apple_image_size,
 * __bss_start, __bss_end (8-gealigneerd), __stack_top, __heap_start,
 * __heap_end, en de BSS-tabellen __apple_tables (4 KB-gealigneerd).
 */

ENTRY(_start_apple)

KERN_BASE = 0x10100000000;
KERN_END = KERN_BASE + 0x10000000;
STACK_SIZE = 0x40000;

SECTIONS
{
  . = KERN_BASE;

  .head : {
    KEEP(*(.text.apple_head))
    . = ALIGN(0x10000);
  }

  .text : ALIGN(4096) {
    KEEP(*(.text.apple_start))
    KEEP(*(.text.vectors))
    *(.text .text.*)
  }

  .rodata : ALIGN(4096) {
    KEEP(*(.rodata.pagetables))
    *(.rodata .rodata.*)
  }

  .data : ALIGN(4096) {
    *(.data .data.*)
    . = ALIGN(64);
  }

  __image_file_end = .;
  __apple_image_size = __image_file_end - KERN_BASE;

  .bss (NOLOAD) : ALIGN(4096) {
    __bss_start = .;
    *(.bss.apple_tables)
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(8);
    __bss_end = .;
  }

  /* De wachtpagina onder de stack (30-09): het board maakt hem ongeldig in
   * de map, zodat een overloop een luide fault is en niet stil het einde
   * van .bss overschrijft. Hij komt bovenop STACK_SIZE. */
  .stack (NOLOAD) : ALIGN(4096) {
    __stack_guard = .;
    . += 4096;
    . += STACK_SIZE;
    __stack_top = .;
  }

  . = ALIGN(4096);
  __heap_start = .;
  __heap_end = KERN_END - 0x1000;

  /DISCARD/ : {
    *(.comment)
    *(.note .note.*)
    *(.eh_frame .eh_frame_hdr)
  }
}

ASSERT(__apple_head == KERN_BASE, "de bootstub moet op offset 0 staan")
ASSERT(_start_apple == KERN_BASE + 0x10000, "de kern begint op 0x10000")
ASSERT(__heap_start < __heap_end, "het image loopt de DMA-regio in")
ASSERT(__heap_end <= __hopos_flip_tramp, "de heap loopt de trampoline van de flip in")
ASSERT(__hopos_flip_tramp + 0x1000 <= KERN_END, "de trampoline van de flip ligt buiten de kern-RAM")
ASSERT(__apple_tables % 4096 == 0, "de tabellen zijn 4 KB-gealigneerd")
