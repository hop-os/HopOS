//! De voorkant van het image: de bootstub (de relocatie en de brievenbus
//! voor cores uit reset), de ingang `_start_apple` en de vroege map.
//!
//! Waarom een stub bestaat. Zodra wij zélf het bootobject zijn (`kmutil
//! configure-boot --raw --entry-point 2048`) laadt iBoot dit bestand op een
//! adres van zijn keuze (GEMETEN 29-08: m1n1 stond de ene boot op
//! 0x100_2bb8_000, de andere op 0x100_3a90_000) en springt naar
//! bestandsoffset 0x800. Ons image is gelinkt op [`crate::RAM_BASE`] en is
//! niet verplaatsbaar, dus er moet iets tussen dat de rest naar zijn plek
//! kopieert. Precies twee dingen landen buiten hun linkadres:
//!
//! ```text
//! 0x0000  stub_reset  waar een core uit reset landt (RVBAR; iBoot zet hem op
//!                     het begin van het bootobject en vergrendelt hem, lock=1
//!                     op alle tien de cores, GEMETEN 29-08)
//! 0x0100  parameters  doel, grootte, entry (de linker vult ze in)
//! 0x0800  stub_entry  waar de firmware de boot-core aflevert, x0 = boot_args;
//!                     0x800 is de --entry-point 2048 van het installatiecommando
//! 0xE000  scratch     [`crate::SCRATCH`]: waar de stub vandaan kwam, x0, de
//!                     brievenbus
//! 0xE100  param-blok  van de m1n1-loader (spin-table, [`crate::fwinfo`])
//! 0xF000  config      hopos.cfg van de loader
//! 0x10000 de kern
//! ```
//!
//! Anders dan in Go (waar `mkkernel -apple` de stubs uit de ELF viste en
//! de parameters invulde) doet de linker hier alles: de stubs staan in de
//! eerste sectie van het image en de parameters zijn absolute symbolen uit
//! `hopos/link-apple.ld`. Twee regels uit Go blijven: geen absolute
//! constante in de stub (alles PC-relatief of uit het parameterblok, want de
//! stub draait niet op zijn linkadres), en geen exclusives (met de MMU uit is
//! geheugen Device-nGnRnE en zijn LDAXR/STLXR CONSTRAINED UNPREDICTABLE;
//! vandaar de brievenbus op MPIDR in plaats van een atomaire claim).
//!
//! De stubs draaien op EL2 met de MMU uit: m1n1's `mmu_init` begint met
//! "staat SCTLR.M al aan? dan klaar", en die tak wordt nooit genomen.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use dev::Pa;

/// De x0 van de firmware (boot_args): bij stroom aan wat de ingang zag, na
/// een sprong wat de feitenpagina draagt.
pub(crate) static FIRMWARE_X0: AtomicU64 = AtomicU64::new(0);

/// De dockchannel-basis van de t8132, voor de eerste regel vóór alles
/// (`aapl,dock-channels`, ADT reg + arm-io-range, GEMETEN 28-08). Een
/// foutmelder die van een boom afhangt, zwijgt juist wanneer het misgaat
/// (Go `cpuinit.s`, `DOCK_FALLBACK`).
pub(crate) const DOCK_FALLBACK: u64 = 0x3_8812_8000;

/// Wat de assembly na de BSS en de stack aanroept, met de MMU nog uit: de
/// eerste regel op de dockchannel en de identity map. Geeft TTBR0.
///
/// Eerste licht vóór de rest (Go 29-08): zonder dat regeltje was elke
/// mislukking "hij deed niets"; mét is het "hij kwam tot hier, met dít in
/// x0", en dat scheelt per stuk een bootcyclus.
#[cfg_attr(
    not(all(target_arch = "aarch64", target_os = "none")),
    allow(dead_code) // alleen de ingang roept dit aan
)]
extern "C" fn apple_early(x0: u64, pool: u64) -> u64 {
    // Na een sprong is x0 de feitenpagina (O6, `docs/flip.md`): kern A legde
    // de boot_args erin en veegde hem naar DRAM, dus hij leest met de MMU
    // uit. iBoot legt de boot_args onder het venster, nooit daar.
    let facts = abi::layout::facts_pa(crate::slots::BOOT_SCRATCH_PA);
    let x0 = if x0 == facts {
        dev::read64(Pa(facts + abi::layout::FACTS_FIRMWARE))
    } else {
        x0
    };
    FIRMWARE_X0.store(x0, Relaxed);
    let dock = Pa(DOCK_FALLBACK);
    crate::console::raw_line(dock, b"hopos x", x0);
    let actual = crate::fwinfo::early_mem_size(x0);
    // SAFETY: `pool` is `__apple_tables` in de BSS (`hopos/link-apple.ld`):
    // 4 KB-gealigneerd, `mmu::TABLES` pagina's, van niemand anders, en de
    // MMU staat nog uit.
    unsafe { crate::mmu::build(pool, actual, stack_guard()) }
}

/// Het adres van de wachtpagina onder de stack (`__stack_guard` in
/// `hopos/link-apple.ld`).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn stack_guard() -> u64 {
    unsafe extern "C" {
        safe static __stack_guard: u8;
    }
    (&raw const __stack_guard).addr() as u64
}

/// Host-stub: geen linkscript, dus geen wachtpagina.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn stack_guard() -> u64 {
    0
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .text.apple_head, "ax"
    .global __apple_head
__apple_head:
    // stub_reset (offset 0): een core uit reset. Hij wacht in WFE tot de
    // brievenbus in de scratch zijn aff1:aff0 draagt en een entry heeft,
    // leest het argument, bevestigt met -1 (niet 0: E-core 0 heeft aff
    // 0x0000), en springt.
    adr x10, __apple_head
    ldr x11, [x10, #0x108]
    add x11, x11, #0xE, lsl #12
    mrs x2, mpidr_el1
    and x2, x2, #0xffff
1:  ldr x14, [x11, #0x40]
    cmp x2, x14
    b.ne 2f
    ldr x15, [x11, #0x30]
    cbnz x15, 3f
2:  wfe
    b 1b
3:  ldr x0, [x11, #0x38]
    mov x14, #-1
    str x14, [x11, #0x40]
    dsb sy
    br x15

    // Het parameterblok op 0x100: magic, doel, grootte, entry.
    .space 0x100 - (. - __apple_head)
    .quad {magic}
    .quad __apple_head
    .quad __apple_image_size
    .quad _start_apple

    // stub_entry (offset 0x800): de boot-core, x0 = boot_args. Verplaatst
    // het image naar zijn linkadres als het er nog niet staat, en schrijft
    // op waar het vandaan kwam (dat is waar RVBAR van élke core naar wijst:
    // het antwoord op "zijn de cores van ons") en wat x0 was.
    .space 0x800 - (. - __apple_head)
    adr x10, __apple_head
    mov x9, x0
    ldr x11, [x10, #0x108]
    ldr x12, [x10, #0x110]
    ldr x13, [x10, #0x118]
    cbz x11, 9f
    cbz x13, 9f
    cmp x11, x10
    b.eq 8f
    // Naar beneden verplaatsen mag (voorwaarts kopiëren loopt voor zichzelf
    // uit); naar boven binnen het image niet.
    sub x1, x11, x10
    cmp x1, x12
    b.lo 9f
    // De cachelijn uit CTR_EL0 (DminLine = log2 van het aantal woorden).
    mrs x16, ctr_el0
    ubfx x16, x16, #16, #4
    mov x17, #4
    lsl x17, x17, x16
    // Bron en doel schoonvegen: de firmware schreef ons met caches aan, wij
    // lezen met de MMU uit rechtstreeks uit DRAM; en op het doel kan een
    // vuile regel van een vorig leven liggen.
    mov x14, x10
    add x15, x10, x12
4:  dc civac, x14
    add x14, x14, x17
    cmp x14, x15
    b.lo 4b
    mov x14, x11
    add x15, x11, x12
5:  dc civac, x14
    add x14, x14, x17
    cmp x14, x15
    b.lo 5b
    dsb sy
    mov x14, x10
    mov x15, x11
    add x16, x10, x12
6:  ldp x1, x2, [x14], #16
    stp x1, x2, [x15], #16
    cmp x14, x16
    b.lo 6b
    dsb sy
    ic iallu
    dsb sy
    isb
8:  // Pas nu opschrijven: de scratch ligt ín het image en is net
    // overschreven door de kopie.
    add x14, x11, #0xE, lsl #12
    str x10, [x14, #0x48]
    str x9, [x14, #0x50]
    dsb sy
    mov x0, x9
    br x13
9:  wfe
    b 9b

    // De ingang op het linkadres: de boot-core, x0 = boot_args, EL2, MMU uit.
    .section .text.apple_start, "ax"
    .global _start_apple
_start_apple:
    msr daifset, #0xf
    mov x20, x0
    mrs x21, CurrentEL
    lsr x21, x21, #2
    and x21, x21, #3

    // De vorige bewoner van deze regels (firmware, m1n1) schreef met caches
    // aan; wij schrijven BSS, stack en tabellen straks met de MMU uit. Een
    // vuile regel die later wordt uitgeschreven, zou onze bytes overschrijven.
    adrp x1, __bss_start
    add x1, x1, :lo12:__bss_start
    adrp x2, __stack_top
    add x2, x2, :lo12:__stack_top
    mrs x16, ctr_el0
    ubfx x16, x16, #16, #4
    mov x17, #4
    lsl x17, x17, x16
    bic x3, x1, #63
1:  dc civac, x3
    add x3, x3, x17
    cmp x3, x2
    b.lo 1b
    dsb sy

    mov sp, x2
    mov x3, x1
    adrp x2, __bss_end
    add x2, x2, :lo12:__bss_end
2:  cmp x3, x2
    b.hs 3f
    str xzr, [x3], #8
    b 2b
3:
    cmp x21, #2
    b.ne 5f

    // HCR_EL2: E2H (RES1 op dit silicium, GEMETEN 28-08), RW, en IMO, FMO en
    // AMO: fysieke IRQ, FIQ en SError komen naar EL2. De timer en de fast IPI
    // zijn FIQ's op Apple. TGE blijft 0.
    mov x1, #(1 << 31)
    orr x1, x1, #(7 << 3)
    orr x1, x1, #(1 << 34)
    msr hcr_el2, x1
    isb
    // CNTHCTL_EL2 in de VHE-vorm: EL1PCTEN en EL1PTEN (bits 10 en 11) voor de
    // bewoners, en de EL0-bits 0 en 1; de drop naar EL1 van een kooi leest
    // de teller dan zonder trap.
    mrs x1, cnthctl_el2
    orr x1, x1, #0x3
    orr x1, x1, #0xc00
    msr cnthctl_el2, x1
    msr cntvoff_el2, xzr

    mov x0, x20
    adrp x1, __apple_tables
    add x1, x1, :lo12:__apple_tables
    bl {early}
    mov x19, x0

    ldr x1, ={mair}
    msr mair_el2, x1
    mrs x2, id_aa64mmfr0_el1
    and x2, x2, #0xf
    cmp x2, #5
    mov x3, #5
    csel x2, x2, x3, ls
    ldr x1, ={tcr_base}
    orr x1, x1, x2, lsl #32
    msr tcr_el2, x1
    msr ttbr0_el2, x19
    isb
    tlbi alle2
    ic iallu
    dsb ish
    isb
    ldr x1, ={sctlr}
    msr sctlr_el2, x1
    isb
    adrp x1, __hopos_vectors
    add x1, x1, :lo12:__hopos_vectors
    msr vbar_el2, x1
    isb

5:  mov x0, x20
    mov x1, x21
    bl kmain
9:  wfe
    b 9b
    .ltorg

    .section .bss.apple_tables, "aw", %nobits
    .balign 4096
    .global __apple_tables
__apple_tables:
    .space {tables} * 4096
"#,
    early = sym apple_early,
    magic = const STUB_MAGIC,
    mair = const cpu::boot::MAIR,
    tcr_base = const crate::mmu::tcr(0),
    sctlr = const crate::mmu::SCTLR,
    tables = const crate::mmu::TABLES,
);

/// Het magic van het parameterblok op 0x100 ("HOPOSTUB"), waaraan
/// `cores::own_cores` een stub op RVBAR herkent.
pub(crate) const STUB_MAGIC: u64 = 0x4255_5453_4150_4f48;

/// De brievenbus in de scratch (pariteit met de stub hierboven).
pub(crate) const SCRATCH_PARK_PC: u64 = 0x30;
/// Het argument (x0) voor de core die de bus opent.
pub(crate) const SCRATCH_PARK_ARG: u64 = 0x38;
/// Voor wie: aff1:aff0; `u64::MAX` = vrij.
pub(crate) const SCRATCH_PARK_FOR: u64 = 0x40;
/// De x0 van de firmware, door de stub bewaard (op 0x48 staat waar hij het
/// image vond).
pub(crate) const SCRATCH_STUB_X0: u64 = 0x50;

const _: () = {
    assert!(crate::SCRATCH == crate::RAM_BASE + 0xE000);
    assert!(
        SCRATCH_STUB_X0 + 8 <= 0x100,
        "de scratch eindigt vóór het param-blok"
    );
};
