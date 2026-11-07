/*
 * Het image van HopOS op de Radxa Zero 3E (RK3566), geboot door U-Boot's
 * `booti`.
 *
 * Het arm64-Image begint op IMAGE_BASE = 0x0220_0000 met de 64-byte
 * Linux-Image-header, die image/radxa-zero3.sh ervoor zet (de vorm van Go's
 * mkkernel): booti legt een Image met relocatable = 0 op bi_dram[0].start +
 * text_offset = 0x20_0000 + 0x0200_0000 (GEMETEN 05-08). De tekst begint
 * 64 KB verder, op KERN_BASE, zoals in Go (-T 0x02210000); de header-branch
 * springt naar _start.
 *
 * De kern-RAM loopt tot 0x0620_0000 (64 MB, board_rk3566::KERN_RAM);
 * daarboven begint het structurenvenster. De heap is alles tussen het einde
 * van de stack en die grens, op de bovenste pagina na: dat is de trampoline
 * van de kern-flip (board_rk3566::slots::FLIP_TRAMP_PA, het enige
 * uitvoerbare RAM buiten de pool).
 *
 * Het contract met cpu::boot: __stack_top, __stack_guard, __bss_start, __bss_end (8-
 * gealigneerd); met board-rk3566: __heap_start, __heap_end; met het
 * image-script: __image_end (image_size in de header = __image_end -
 * IMAGE_BASE, BSS en stack inbegrepen).
 */

ENTRY(_start)

IMAGE_BASE = 0x02200000;
KERN_BASE = IMAGE_BASE + 0x10000;
KERN_END = IMAGE_BASE + 0x04000000;
STACK_SIZE = 0x40000;

SECTIONS
{
  . = KERN_BASE;

  .text : ALIGN(4096) {
    KEEP(*(.text.boot))
    KEEP(*(.text.vectors))
    *(.text .text.*)
  }

  .rodata : ALIGN(4096) {
    KEEP(*(.rodata.pagetables))
    *(.rodata .rodata.*)
  }

  .data : ALIGN(4096) {
    *(.data .data.*)
  }

  .bss (NOLOAD) : ALIGN(4096) {
    __bss_start = .;
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
  __image_end = .;
  __heap_start = .;
  __heap_end = KERN_END - 0x1000;

  /DISCARD/ : {
    *(.comment)
    *(.note .note.*)
    *(.eh_frame .eh_frame_hdr)
  }
}

ASSERT(__heap_start < __heap_end, "het image loopt het structurenvenster in")
ASSERT(__heap_end <= __hopos_flip_tramp, "de heap loopt de trampoline van de flip in")
ASSERT(__hopos_flip_tramp + 0x1000 <= KERN_END, "de trampoline van de flip ligt buiten de kern-RAM")
ASSERT(_start == KERN_BASE, "_start moet vooraan staan: de header-branch springt erheen")
