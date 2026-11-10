//! De boot-stub: van de eerste instructie tot `kmain`.
//!
//! `_start` doet precies wat Rust nog niet kan: alleen core 0 gaat door (of
//! de core van een sprong, [`admits`]),
//! de stack komt op zijn plek, BSS wordt gewist, en op EL2 gaan de vectoren
//! en de MMU aan. Daarna `kmain(dtb, el)`, dat de binary levert.
//!
//! HopOS eist EL2 (de stage-2-kooi is een invariant, geen optie). Wie ons
//! op een ander niveau aflevert, krijgt toch een stack en BSS maar géén
//! MMU (dat zijn andere registers), en `kmain` meldt de reden op de UART en
//! parkeert: stil parkeren vóór de console is een fout die niemand vindt.
//! Zo deed de Go-kern het ook (`board.RequireEL2` in de main).
//!
//! Het linkcontract (de binary levert het, `hopos/link.ld`):
//!
//! - `__stack_top`: de top van de boot-stack, 16-gealigneerd;
//! - `__bss_start`, `__bss_end`: 8-gealigneerd;
//! - `__boot_ttbr0`: de niveau-1-tabel van de identity map (4 KB-korrel,
//!   39-bit VA). Het board levert hem (`global_asm!`), want welk bereik
//!   Device en welk Normal is, is board-kennis;
//! - `__stack_guard`: de pagina direct onder de boot-stack (4 KB-gealigneerd,
//!   de stack begint erboven). De stub maakt hem ongeldig in de map (zie
//!   de wachtpagina hieronder);
//! - `kmain`: `extern "C" fn(x0: u64, el: u64) -> !`;
//! - `__hopos_facts`: het adres van de feitenpagina van de flip (de binary,
//!   `hopos/src/flip.rs`), een absoluut symbool voor de poort hieronder;
//! - `__heap_start`, `__heap_end`: de heap ([`heap_bounds`]), achter image,
//!   BSS en stack. Ook in de riscv-linkscripts; de UEFI-boards nemen hun
//!   heap uit de EFI-allocatie en noemen deze symbolen niet.
//!
//! # De wachtpagina
//!
//! Les van 30-09: de stack (256 KB, direct boven `.bss`) had geen
//! wachtpagina, en een plaatsing piekte op 186 KB (het boot-frame van
//! 125 KB bleef onder de executor staan). Een overloop schreef stil in het
//! einde van `.bss`: de switch-tabel, de deuren van de system-API. Nu maakt
//! de stub, met de MMU nog uit (dus zonder break-before-make: er is nog geen
//! TLB die het oude blok kent), van het 2 MB-blok onder `__stack_guard` een
//! niveau-3-tabel (`__guard_l3`, in de BSS) met dezelfde attributen, en zet
//! de ene pagina van `__stack_guard` op ongeldig. Een overloop is dan een
//! luide data-abort (`HOPOS_EXCEPTION`, ESR 0x96000007: translation fault
//! op niveau 3, gemeten 30-09 op QEMU virt met een lees op de pagina) in
//! plaats van corruptie. Een board waarvan de kern-RAM niet via een
//! niveau-2-blok gemapt is, houdt zijn map zoals hij is (de stub slaat de
//! splitsing dan over). Een board met een eigen map doet hetzelfde met
//! [`guard_page`] (de Mac mini). De UEFI-boards nog niet: een geflipte kern
//! erft daar de map van de koude boot (`board/uefi/src/flip.rs`), dus de
//! wachtpagina van kern A zou midden in kern B kunnen vallen; dat vraagt
//! eerst een vrijgave vóór de sprong.
//!
//! De MMU-attributen staan hier vast, zodat elk board dezelfde indexen
//! gebruikt: [`ATTR_DEVICE`], [`ATTR_NORMAL`], [`ATTR_NORMAL_NC`].

/// MAIR-index 0: Device-nGnRnE (MMIO).
pub const ATTR_DEVICE: u64 = 0;
/// MAIR-index 1: Normal, write-back, read/write-allocate (de kern-RAM).
pub const ATTR_NORMAL: u64 = 1;
/// MAIR-index 2: Normal, non-cacheable (DMA-regio's zonder cache-onderhoud).
pub const ATTR_NORMAL_NC: u64 = 2;

/// MAIR_EL2: index 0 = 0x00 (Device-nGnRnE), 1 = 0xff (Normal WB RWA),
/// 2 = 0x44 (Normal NC).
pub const MAIR: u64 =
    (0x00 << (8 * ATTR_DEVICE)) | (0xff << (8 * ATTR_NORMAL)) | (0x44 << (8 * ATTR_NORMAL_NC));

/// Een blokbeschrijving (niveau 1: 1 GB, niveau 2: 2 MB) voor `pa` met
/// MAIR-index `attr`, voor een kern onder HCR_EL2.E2H = 0: zie
/// [`block_e2h`].
#[must_use]
pub const fn block(pa: u64, attr: u64) -> u64 {
    block_e2h(pa, attr, false)
}

/// Een blokbeschrijving voor `pa` met MAIR-index `attr` in het regime van
/// een kern met HCR_EL2.E2H = `e2h`: AF gezet, inner shareable voor Normal,
/// en [`xn`] voor alles wat geen gecachte kern-RAM is (code draait alleen
/// uit de kern).
#[must_use]
pub const fn block_e2h(pa: u64, attr: u64, e2h: bool) -> u64 {
    const VALID_BLOCK: u64 = 0b01;
    const AF: u64 = 1 << 10;
    const SH_INNER: u64 = 0b11 << 8;
    let mut d = pa | VALID_BLOCK | (attr << 2) | AF;
    if attr != ATTR_DEVICE {
        d |= SH_INNER;
    }
    if attr != ATTR_NORMAL {
        d |= xn(e2h);
    }
    d
}

/// Execute-never voor de kern zelf. Onder E2H = 0 (het EL2-regime, één
/// privilegeniveau) is dat bit 54 en is bit 53 RES0. Onder E2H = 1 (het
/// EL2&0-regime: Apple, de O6N, board-uefi met `vhe`) is bit 54 alleen UXN
/// en verbiedt pas PXN (bit 53) het de kern; zonder PXN mag EL2 er
/// speculatief instructies uit halen, en voor Device is dat precies wat XN
/// moest voorkomen (Linux: `PTE_PXN | PTE_UXN` op elke niet-code-mapping).
#[must_use]
pub const fn xn(e2h: bool) -> u64 {
    const XN: u64 = 1 << 54;
    const PXN: u64 = 1 << 53;
    if e2h { XN | PXN } else { XN }
}

/// Maakt de 4 KB-pagina van `guard` ongeldig in een map die de MMU nog niet
/// gebruikt: de Rust-vorm van de wachtpagina van de stub (zie de moduledoc),
/// voor een board dat zijn map zelf bouwt (de Mac mini). Staat er in
/// `l2_entry` een blok van 2 MB dat `guard` dekt, dan wordt het een
/// niveau-3-tabel in `l3` met dezelfde attributen, op die ene pagina na.
/// Anders verandert er niets en is het antwoord `false`.
///
/// # Safety
///
/// De map is van geen enkele MMU in gebruik (dus geen break-before-make
/// nodig), `l2_entry` is een niveau-2-ingang van die map, en `l3` is een
/// 4 KB-gealigneerde pagina die van niemand anders is.
pub unsafe fn guard_page(l2_entry: dev::Pa, l3: dev::Pa, guard: u64) -> bool {
    const BLOCK_ADDR: u64 = 0x0000_ffff_ffe0_0000;
    const PAGE: u64 = 4096;
    let e = dev::read64(l2_entry);
    let base = e & BLOCK_ADDR;
    if e & 0b11 != 0b01 || guard & BLOCK_ADDR != base {
        return false;
    }
    let attrs = e & !BLOCK_ADDR & !0b11;
    let hole = guard & !(PAGE - 1);
    for i in 0..512u64 {
        let pa = base + i * PAGE;
        let d = if pa == hole { 0 } else { pa | attrs | 0b11 };
        dev::write64(l3.add(i * 8), d);
    }
    dev::write64(l2_entry, l3.0 | 0b11);
    true
}

/// De poort van `_start`, als Rust: gaat een core met MPIDR-affiniteit
/// `aff` en `x0` bij de ingang door naar `kmain`, op een board met de
/// feitenpagina op `facts`? De assembly hieronder is dezelfde beslissing;
/// deze vorm is er voor de host-tests.
///
/// Bij stroom aan alleen de core met affiniteit 0: een firmware die alle
/// cores op de ingang loslaat, krijgt zo één kern en een parkeerlus. Een
/// geflipte kern springt vanaf de OS-core, en die is niet per se core 0
/// (`hopos.system.core`, PORT.md beslissing 2), met x0 = de feitenpagina (O6,
/// `docs/flip.md`): dat adres geeft geen firmware (het Linux-bootprotocol
/// zet daar de DTB, of 0), dus elke core met dat adres mag door. Er komt er
/// ook maar één, want de app-cores draaien in de switch-code in de
/// plan-regio en raken het beeld nooit.
#[must_use]
pub const fn admits(aff: u64, x0: u64, facts: u64) -> bool {
    aff & 0xff_ffff == 0 || x0 == facts
}

/// Parkeert deze core voor altijd: een WFE-lus, zodat hij niets verbruikt
/// en niets meer aanraakt.
pub fn park() -> ! {
    loop {
        arch::wfe();
    }
}

/// Hard falen (docs/sequence-hopos.md §13.2): één regel `HOPOS_FATAL
/// <reden>` over de noodconsole, en deze core parkeert. Op de OS-core is
/// dat ook het einde van de executor en dus van de pets: de watchdog
/// reset de node. Geen retry en geen terugvalstand; de koude boot is de
/// enige retry. Dezelfde weg als de panic-handler van de binary.
pub fn fatal(why: core::fmt::Arguments<'_>) -> ! {
    crate::console::emergency(format_args!("HOPOS_FATAL {why}"));
    park()
}

/// [`fatal`] met de vorm van `println!`.
#[macro_export]
macro_rules! fatal {
    ($($arg:tt)*) => {
        $crate::boot::fatal(::core::format_args!($($arg)*))
    };
}

/// Het heap-bereik uit het linkscript: `[__heap_start, __heap_end)`. Op de
/// host leeg.
#[must_use]
pub fn heap_bounds() -> (usize, usize) {
    heap::bounds()
}

#[cfg(target_os = "none")]
mod heap {
    unsafe extern "C" {
        /// Het begin van de heap (het linkscript van het board).
        static __heap_start: u8;
        /// Het einde van de heap.
        static __heap_end: u8;
    }

    pub(super) fn bounds() -> (usize, usize) {
        (
            (&raw const __heap_start) as usize,
            (&raw const __heap_end) as usize,
        )
    }
}

#[cfg(not(target_os = "none"))]
mod heap {
    //! Host-stub: geen linkscript.
    pub(super) fn bounds() -> (usize, usize) {
        (0, 0)
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod arch {
    /// Eén WFE.
    #[inline]
    pub(super) fn wfe() {
        // SAFETY: WFE wacht op een event; geen geheugeneffect.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) }
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod arch {
    /// Host-stub.
    #[inline]
    pub(super) fn wfe() {
        core::hint::spin_loop();
    }
}

// De stub. Registers: x19 = MPIDR-affiniteit, x20 = x0 van de ingang (de
// DTB van de firmware, of na een sprong de feitenpagina), x21 = het EL.
// x0 = `__hopos_facts` laat elke core door ([`admits`]): een geflipte kern
// komt op de OS-core binnen.
//
// TCR_EL2 (niet-VHE): T0SZ = 25 (39-bit VA, start op niveau 1), IRGN0 en
// ORGN0 = WB-WA, SH0 = inner, TG0 = 4 KB, PS uit ID_AA64MMFR0_EL1.PARange,
// en de RES1-bits 23 en 31.
//
// SCTLR_EL2 (niet-VHE): de RES1-set 0x30c50830, plus M, C, I en SA; A, WXN
// en EE uit (geen uitlijncontrole, little-endian).
//
// HCR_EL2: RW (een lagere EL is AArch64), en IMO, FMO en AMO: fysieke
// IRQ, FIQ en SError komen naar EL2. Zonder IMO is een IRQ op EL2
// "gericht op EL1" en dus nooit genomen.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
_start:
    mrs x19, mpidr_el1
    and x19, x19, #0xffffff
    ldr x9, =__hopos_facts
    cmp x0, x9
    b.eq 8f
    cbnz x19, 9f
8:
    mov x20, x0
    mrs x21, CurrentEL
    lsr x21, x21, #2
    and x21, x21, #3

    adrp x1, __stack_top
    add x1, x1, :lo12:__stack_top
    mov sp, x1

    adrp x1, __bss_start
    add x1, x1, :lo12:__bss_start
    adrp x2, __bss_end
    add x2, x2, :lo12:__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:
    cmp x21, #2
    b.ne 3f

    adrp x1, __hopos_vectors
    add x1, x1, :lo12:__hopos_vectors
    msr vbar_el2, x1

    mov x1, #(1 << 31)
    orr x1, x1, #(7 << 3)
    msr hcr_el2, x1

    ldr x1, ={mair}
    msr mair_el2, x1
    mrs x2, id_aa64mmfr0_el1
    and x2, x2, #7
    cmp x2, #5
    mov x3, #5
    csel x2, x2, x3, ls
    ldr x1, =((1 << 31) | (1 << 23) | (3 << 12) | (1 << 10) | (1 << 8) | 25)
    orr x1, x1, x2, lsl #16
    msr tcr_el2, x1
    // De wachtpagina (zie de moduledoc), met de MMU uit. x1 = L1, x2 = de
    // wachtpagina, x6 = de L2-ingang, x7 = de blokbasis, x8 = de
    // attributen als paginadescriptor, x9 = de L3-tabel.
    adrp x1, __boot_ttbr0
    add x1, x1, :lo12:__boot_ttbr0
    adrp x2, __stack_guard
    add x2, x2, :lo12:__stack_guard
    ubfx x3, x2, #30, #9
    ldr x4, [x1, x3, lsl #3]
    and x5, x4, #3
    cmp x5, #3
    b.ne 5f
    and x4, x4, #0x0000fffffffff000
    ubfx x3, x2, #21, #9
    add x6, x4, x3, lsl #3
    ldr x5, [x6]
    and x7, x5, #3
    cmp x7, #1
    b.ne 5f
    and x7, x5, #0x0000ffffffe00000
    and x8, x5, #0xffff0000001fffff
    and x8, x8, #0xfffffffffffffffc
    orr x8, x8, #3
    adrp x9, __guard_l3
    add x9, x9, :lo12:__guard_l3
    mov x10, #0
4:  add x11, x7, x10, lsl #12
    orr x11, x11, x8
    str x11, [x9, x10, lsl #3]
    add x10, x10, #1
    cmp x10, #512
    b.lo 4b
    ubfx x3, x2, #12, #9
    str xzr, [x9, x3, lsl #3]
    orr x9, x9, #3
    str x9, [x6]
    dsb sy
5:
    msr ttbr0_el2, x1
    dsb ish
    isb
    tlbi alle2
    dsb ish
    isb
    ldr x1, =(0x30c50830 | (1 << 0) | (1 << 2) | (1 << 3) | (1 << 12))
    msr sctlr_el2, x1
    isb

3:
    mov x0, x20
    mov x1, x21
    bl kmain
9:
    wfe
    b 9b
    .ltorg
"#,
    mair = const MAIR,
);

// De niveau-3-tabel van de wachtpagina: 4 KB in de BSS, gevuld door de stub
// na het wissen van de BSS (zie de moduledoc).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .bss.hopos_guard_l3, "aw", %nobits
    .balign 4096
    .global __guard_l3
__guard_l3:
    .skip 4096
"#
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_descriptors_carry_the_right_bits() {
        // Device: geldig blok, AF, XN, geen shareability.
        assert_eq!(block(0, ATTR_DEVICE), 0x0040_0000_0000_0401);
        // Kern-RAM: index 1, inner shareable, uitvoerbaar.
        assert_eq!(block(0x4000_0000, ATTR_NORMAL), 0x4000_0705);
        // DMA: index 2, inner shareable, XN.
        assert_eq!(block(0x4f00_0000, ATTR_NORMAL_NC), 0x0040_0000_4f00_0709);
    }

    #[test]
    fn only_core_zero_boots_cold_and_any_core_lands_a_flip() {
        let facts = 0xB000_0080;
        // Koud: de core met affiniteit 0, en alleen die, wat x0 ook is.
        assert!(admits(0, 0, facts));
        assert!(admits(0, 0x4000_0000, facts), "the DTB of the firmware");
        assert!(
            !admits(1, 0, facts),
            "a second core must park on a cold boot"
        );
        assert!(
            !admits(0x100, 0x4000_0000, facts),
            "Pi 5: core 1 is aff1 = 1"
        );
        // Een flip vanaf de OS-core: core 2 op virt, core 1 op de Pi 5.
        assert!(admits(2, facts, facts));
        assert!(admits(0x100, facts, facts));
        // Alleen het adres zelf; rommel in x0 is geen flip.
        assert!(!admits(3, facts + 8, facts));
        // De bovenste bits van MPIDR (U, MT, RES1) tellen niet mee.
        assert!(admits(0x8000_0000, 0, facts));
    }

    #[test]
    fn a_guard_page_splits_its_block_and_leaves_the_rest() {
        let mut mem = vec![0u64; 1024 + 512];
        let base = mem.as_mut_ptr() as u64;
        let l3 = dev::Pa(base.next_multiple_of(4096));
        let entry = dev::Pa(l3.0 + 4096);
        let blk = block(0x4020_0000, ATTR_NORMAL);
        dev::write64(entry, blk);
        let guard = 0x4032_1000;
        // SAFETY: `mem` is van de test en geen MMU kijkt ernaar.
        assert!(unsafe { guard_page(entry, l3, guard) });
        assert_eq!(dev::read64(entry), l3.0 | 0b11, "het blok is een tabel");
        let at = |i: u64| dev::read64(l3.add(i * 8));
        assert_eq!(at(0x121), 0, "de wachtpagina is ongeldig");
        assert_eq!(at(0), 0x4020_0000 | (blk & 0xffc) | 0b11);
        assert_eq!(at(0x122), 0x4032_2000 | (blk & 0xffc) | 0b11);
        // Een ingang die geen blok is, of een blok elders: niets.
        // SAFETY: zie hierboven.
        assert!(!unsafe { guard_page(entry, l3, guard) });
        dev::write64(entry, blk);
        // SAFETY: zie hierboven.
        assert!(!unsafe { guard_page(entry, l3, 0x4060_0000) });
        assert_eq!(dev::read64(entry), blk);
    }

    #[test]
    fn mair_indexes_match_the_constants() {
        assert_eq!((MAIR >> (8 * ATTR_DEVICE)) & 0xff, 0x00);
        assert_eq!((MAIR >> (8 * ATTR_NORMAL)) & 0xff, 0xff);
        assert_eq!((MAIR >> (8 * ATTR_NORMAL_NC)) & 0xff, 0x44);
    }
}
