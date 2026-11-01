//! De assembly van de stub: de PE32+-header, `_start_efi` en
//! `uefi_enter_kernel`. Wat ze doen en waarom, staat in [`crate::boot`].
//!
//! De header volgt het recept dat EDK2 aantoonbaar slikt (Linux'
//! `arch/arm64/kernel/efi-header.S`, en de Go-verpakking `mkkernel -pe`
//! die op QEMU/EDK2, de Altra en de O6N bootte; `OLD/docs/v1/archief/
//! uefi.md`):
//!
//! - DOS-stub: alleen `MZ` en `e_lfanew` op 0x3c;
//! - COFF: Machine 0xaa64, Characteristics 0x0206 (EXECUTABLE |
//!   LINE_NUMS_STRIPPED | DEBUG_STRIPPED), géén RELOCS_STRIPPED: dat
//!   betekent "alleen op ImageBase" en daar heeft de DXE-core geen
//!   terugval voor;
//! - PE32+ (0x20b), ImageBase 0, Section- en FileAlignment 0x1000 (RVA ==
//!   bestandsoffset, want we linken op 0), Subsystem 10 (EFI_APPLICATION);
//! - NumberOfRvaAndSizes = 6, nooit 5: EDK2 toetst `< 5` en leest bij 5 de
//!   sectietabel als relocatie-directory (Go, 13-07). SizeOfOptionalHeader
//!   is dan precies 0x70 + 8 * 6 = 0xa0;
//! - twee secties met echte rechten: `.text` (RX: code en rodata) en
//!   `.data` (RW: data, relocaties, en via VirtualSize de BSS en de stack,
//!   die de loader met nullen vult).
//!
//! Alle maten en RVA's komen als absolute symbolen uit `hopos/efi.ld`.

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .text.efihdr, "a"
    .global __efi_head
__efi_head:
    .ascii "MZ"
    .fill 0x3a, 1, 0
    .long 0x40
    .ascii "PE\0\0"
    .short 0xaa64
    .short 2
    .long 0
    .long 0
    .long 0
    .short 0xa0
    .short 0x0206
    .short 0x020b
    .byte 0, 0
    .long __efi_text_size
    .long __efi_data_raw
    .long __efi_bss_size
    .long __efi_entry_rva
    .long __efi_text_rva
    .quad 0
    .long 0x1000
    .long 0x1000
    .short 0, 0, 0, 0, 0, 0
    .long 0
    .long __efi_image_size
    .long __efi_text_rva
    .long 0
    .short 10
    .short 0
    .quad 0, 0, 0, 0
    .long 0
    .long 6
    .quad 0, 0, 0, 0, 0, 0
    .ascii ".text\0\0\0"
    .long __efi_text_size
    .long __efi_text_rva
    .long __efi_text_size
    .long __efi_text_rva
    .long 0, 0
    .short 0, 0
    .long 0x60000020
    .ascii ".data\0\0\0"
    .long __efi_data_vsize
    .long __efi_data_rva
    .long __efi_data_raw
    .long __efi_data_rva
    .long 0, 0
    .short 0, 0
    .long 0xc0000040

    .section .text.efientry, "ax"
    .global _start_efi
_start_efi:
    // x0 = de feitenpagina: een kern-flip (O6, crate::flip), geen
    // ImageHandle. Vóór de eerste store: de stack van de oude kern ligt
    // misschien waar nu onze code staat.
    ldr x9, ={head}
    cmp x0, x9
    b.eq 20f
    stp x29, x30, [sp, #-32]!
    mov x29, sp
    stp x19, x20, [sp, #16]
    mov x19, x0
    mov x20, x1
    bl 30f
    mov x0, x19
    mov x1, x20
    bl hopos_efi_main
    ldp x19, x20, [sp, #16]
    ldp x29, x30, [sp], #32
    ret

    // De flip-ingang: MMU uit, geen firmware. Eigen stack, dezelfde
    // relocatie en BSS als hierboven, en dan de weg van de koude stub naar
    // kmain, met de feiten van de koude boot (FLIP_FACTS_PA) en x0 = de
    // feitenpagina. Zonder feiten parkeert de core: een kern zonder console,
    // ACPI en kaart kan niets, en de guard van de oude generatie is er niet
    // meer.
20: adrp x9, __stack_top
    add x9, x9, :lo12:__stack_top
    mov sp, x9
    bl 30f
    ldr x9, ={facts}
    ldr x10, [x9]
    ldr x11, ={facts_magic}
    cmp x10, x11
    b.ne 29f
    mov x4, x9
    ldr x0, [x9, #{ttbr0}]
    ldr x1, [x9, #{ttbr0} + 8]
    ldr x2, [x9, #{ttbr0} + 16]
    ldr x3, [x9, #{ttbr0} + 24]
    b uefi_enter_flipped
29: wfe
    b 29b

    // De zelf-relocatie en de BSS (blad-routine: alleen x9 tot x14).
30: adrp x9, __efi_head
    add x9, x9, :lo12:__efi_head
    adrp x10, __rela_start
    add x10, x10, :lo12:__rela_start
    adrp x11, __rela_end
    add x11, x11, :lo12:__rela_end
1:  cmp x10, x11
    b.hs 2f
    ldp x12, x13, [x10], #16
    ldr x14, [x10], #8
    and x13, x13, #0xffffffff
    cmp x13, #0x403
    b.ne 1b
    add x14, x14, x9
    str x14, [x9, x12]
    b 1b
2:
    adrp x10, __bss_start
    add x10, x10, :lo12:__bss_start
    adrp x11, __bss_end
    add x11, x11, :lo12:__bss_end
3:  cmp x10, x11
    b.hs 4f
    str xzr, [x10], #8
    b 3b
4:  ret

    .global uefi_enter_kernel
uefi_enter_kernel:
    mov x23, xzr
    b 10f
    // Dezelfde weg voor een geflipte kern, met x4 = de feitenpagina: die
    // gaan vlak vóór kmain terug in de statics (hopos_efi_flip_facts).
uefi_enter_flipped:
    mov x23, x4
10: msr daifset, #0xf
    mov x19, x0
    mov x20, x1
    mov x21, x2
    mov x22, x3
    cmp x21, #2
    b.ne 8f

    mrs x9, sctlr_el2
    bic x9, x9, #1
    bic x9, x9, #(1 << 2)
    bic x9, x9, #(1 << 12)
    msr sctlr_el2, x9
    isb
    ic iallu
    tlbi alle2
    dsb sy
    isb

    // HCR_EL2 vers (crate::el2 `HCR`), met de MMU uit en vóór TCR en
    // SCTLR: onder VHE bepaalt E2H de lay-out van die twee. De nVHE-tak is
    // byte voor byte de ingang van vóór VHE.
    .if {vhe}
    ldr x9, ={hcr}
    .else
    mov x9, #(1 << 31)
    orr x9, x9, #(7 << 3)
    .endif
    msr hcr_el2, x9
    isb
    msr mdcr_el2, xzr
    msr hstr_el2, xzr
    .if {vhe}
    ldr x9, ={cptr}
    .else
    mov x9, #0x33ff
    .endif
    msr cptr_el2, x9
    msr cnthctl_el2, x22
    msr cntvoff_el2, xzr

    mrs x9, id_aa64mmfr0_el1
    ubfx x9, x9, #56, #4
    cbz x9, 5f
    msr S3_4_C1_C1_4, xzr
    msr S3_4_C1_C1_5, xzr
    msr S3_4_C1_C1_6, xzr
    msr S3_4_C3_C1_4, xzr
    msr S3_4_C3_C1_5, xzr
5:  mrs x9, id_aa64mmfr1_el1
    ubfx x9, x9, #40, #4
    cbz x9, 6f
    msr S3_4_C1_C2_2, xzr
6:  mrs x9, id_aa64pfr0_el1
    ubfx x10, x9, #40, #4
    cbz x10, 7f
    msr S3_4_C10_C5_0, xzr
    msr S3_4_C10_C4_0, xzr
7:  ubfx x10, x9, #24, #4
    cbz x10, 71f
    mov x10, #0xf
    msr S3_4_C12_C9_5, x10
    isb
    msr S3_4_C12_C11_0, xzr
71: mrs x9, midr_el1
    msr vpidr_el2, x9
    mrs x9, mpidr_el1
    msr vmpidr_el2, x9
    isb

    ldr x9, ={mair}
    msr mair_el2, x9
    msr tcr_el2, x20
    msr ttbr0_el2, x19
    isb
    tlbi alle2
    dsb ish
    isb
    .if {vhe}
    ldr x9, ={sctlr}
    .else
    ldr x9, =(0x30c50830 | (1 << 0) | (1 << 2) | (1 << 3) | (1 << 12))
    .endif
    msr sctlr_el2, x9
    isb
    adrp x9, __hopos_vectors
    add x9, x9, :lo12:__hopos_vectors
    msr vbar_el2, x9
    isb

8:  adrp x9, __stack_top
    add x9, x9, :lo12:__stack_top
    mov sp, x9
    mov x0, xzr
    cbz x23, 11f
    bl hopos_efi_flip_facts
    ldr x0, ={head}
11: mov x29, xzr
    mov x30, xzr
    mov x1, x21
    bl kmain
9:  wfe
    b 9b
    .ltorg
"#,
    mair = const cpu::boot::MAIR,
    vhe = const crate::el2::VHE as u8,
    hcr = const crate::el2::HCR,
    cptr = const crate::el2::CPTR,
    sctlr = const crate::el2::SCTLR,
    facts = const crate::slots::FLIP_FACTS_PA,
    head = const abi::layout::facts_pa(crate::slots::BOOT_SCRATCH_PA),
    facts_magic = const crate::flip::FACTS_MAGIC,
    ttbr0 = const crate::flip::W_TTBR0 * 8,
);
