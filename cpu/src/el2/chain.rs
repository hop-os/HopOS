//! De sprong van de kern-flip: de nieuwe kern relokeren, en er met de MMU
//! uit in springen (`OLD/metal/cpu/el2/chain.go` en `chain_arm64.s`).
//!
//! Dit bezit twee dingen en verder niets: [`relocate`], dat de
//! HOPRELO1-tabel van de bundel toepast op het platte beeld dat de kern al
//! neerlegde (`kern::kernflip::flatten`), en [`chain`], de sprong zelf. Wat
//! er wordt overgedragen (het handoff-blob), waar het beeld ligt en wanneer
//! er gesprongen wordt, is van de kern-binary (`hopos/src/flip.rs`).
//!
//! # De vorm van de sprong in v3
//!
//! De Go-kern leende een venster uit de pool en sprong daarin; de nieuwe
//! kern kreeg `RamStart`/`RamSize` gepatcht. Een kern van v3 linkt op een
//! vast adres met een vaste identity map, dus de nieuwe kern gaat op het
//! KOUDE adres van de oude: precies waar de firmware hem ook zou laden. Dat
//! kan de oude kern niet zelf (hij zou zijn eigen code overschrijven), dus
//! de laatste meters loopt een kleine trampoline buiten het kern-RAM:
//!
//! 1. interrupts dicht, MMU, D- en I-cache uit (lezen-en-maskeren, m1n1's
//!    `mmu_disable`, zoals de Go-sprong op de M4);
//! 2. het kern-RAM en het beeld by-VA vegen (`dc civac`, twee vensters: op
//!    UEFI ligt het beeld buiten het kernvenster): elke dirty regel van de
//!    oude kern (stack, heap, .data) moet naar DRAM vóór de kopie, anders
//!    drukt een latere eviction oude bytes over de nieuwe kern heen;
//! 3. het platte beeld woordgewijs van de staging naar het koude adres, met
//!    de MMU uit (dus Device-nGnRnE: gealigneerd, ongecached, geen
//!    speculatie die een regel terugbrengt);
//! 4. de I-hygiëne (`ic iallu`, de les van de eerste vier ijzer-flips van
//!    01-09: het venster was net nog code van een ander), en `br` naar de
//!    entry met x0 = de firmware-x0 die de oude kern ooit kreeg.
//!
//! Vóór stap 1 gaat VBAR_EL2 naar een kale vectortabel die met de
//! trampoline meereist (2 KB verder op dezelfde pagina): een fault tijdens
//! de kopie, of in de nieuwe kern voordat die zijn eigen vectoren zet, landt
//! anders in de vectoren van de oude kern, die op dat moment half
//! overschreven zijn. GEMETEN 01-09, de zevende ijzer-flip: een trace met
//! oude nummers en oude adressen in plaats van de echte ESR/ELR/FAR (Go:
//! `Chainload` met `layout.TrapVecPA`). De kale vector legt ESR, ELR en FAR
//! in een record op de pagina, veegt dat naar DRAM en wacht; de kern die na
//! de reset boot (koud, of adopterend want het blob ligt er) leest het
//! ([`take_trap`]).
//!
//! De nieuwe kern komt zo binnen op exact de conditie van een koude boot
//! (EL2, MMU uit, caches schoon), en zijn boot-stub zet alles zelf weer op.
//! Eén verschil: x3 draagt [`crate::boot::FLIP_ENTRY`]. De sprong komt
//! van de OS-core, en die is niet per se core 0; zonder merkteken parkeert
//! `_start` elke core behalve core 0 (de koude-boot-poort, `cpu::boot`).
//!
//! # riscv64
//!
//! Dezelfde sprong in machine mode. Er is
//! geen MMU om uit te zetten; de trampoline zet de interrupts dicht, veegt op
//! de C906 de hele D-cache naar DRAM (`th.dcache.ciall`, feature `thead`),
//! kopieert het beeld, veegt opnieuw, maakt de I-cache leeg en springt naar
//! `_start` met a0 = 0 en a1 = de a1 die de firmware (QEMU: de DTB) de oude
//! kern gaf, zoals bij de boot (`cpu::riscv::boot`). Nog zonder gebruiker:
//! riscv64 flipt niet zolang de switch-code uit het kern-image draait
//! (`hopos/src/flip.rs`, `FLIPS`).

use dev::Pa;

/// Waarom de sprong niet door kan gaan. Alles wat hier faalt, faalt VÓÓR
/// er iets onherroepelijks gebeurde: de oude kern draait gewoon door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// Een relocatie-offset die niet 8-uitgelijnd is of buiten het beeld
    /// valt.
    Reloc {
        /// De offset uit de tabel.
        off: u32,
        /// De maat van het platte beeld.
        size: u64,
    },
    /// Het beeld, de staging of de trampoline overlappen elkaar of het
    /// geveegde venster op de verkeerde manier.
    Layout {
        /// Wat er botst.
        what: &'static str,
        /// Het adres.
        pa: u64,
    },
    /// Deze build heeft geen trampoline (de host).
    NoTrampoline,
}

impl core::fmt::Display for ChainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reloc { off, size } => write!(
                f,
                "relocation at {off:#x} is unaligned or outside the {size:#x}-byte image"
            ),
            Self::Layout { what, pa } => write!(f, "chain layout: {what} at {pa:#x}"),
            Self::NoTrampoline => write!(f, "this build carries no chain trampoline"),
        }
    }
}

/// Past een relocatietabel toe op het platte beeld op `flat` (`size`
/// bytes): elk woord op een offset draagt een absoluut adres op de
/// linkbasis van de bundel en krijgt `delta` erbij (modulo 2^64: een
/// nieuwe basis onder de linkbasis is een negatieve delta). Eerst wordt de
/// hele tabel getoetst, dan pas geschreven: een kapotte tabel laat het beeld
/// ongemoeid. Geeft het aantal relocaties.
///
/// Het beeld moet van de aanroeper zijn (de staging van de flip); `flat`
/// komt uit het plan van het board, zoals elk adres dat `dev` krijgt.
pub fn relocate(
    flat: Pa,
    size: u64,
    offs: impl Iterator<Item = u32> + Clone,
    delta: u64,
) -> Result<usize, ChainError> {
    let mut n = 0usize;
    for off in offs.clone() {
        if !off.is_multiple_of(8) || u64::from(off).saturating_add(8) > size {
            return Err(ChainError::Reloc { off, size });
        }
        n += 1;
    }
    if delta != 0 {
        for off in offs {
            let a = flat.add(u64::from(off));
            dev::write64(a, dev::read64(a).wrapping_add(delta));
        }
    }
    Ok(n)
}

/// Alles wat de sprong nodig heeft.
#[derive(Debug, Clone, Copy)]
pub struct Jump {
    /// Het koude adres van de kern: waar het beeld heen gaat.
    pub dst: Pa,
    /// De staging met het gerelokeerde platte beeld.
    pub src: Pa,
    /// De maat van het beeld (8-uitgelijnd).
    pub len: u64,
    /// Het fysieke entrypoint van de nieuwe kern (binnen `[dst, dst+len)`).
    pub entry: u64,
    /// x0 voor de nieuwe kern: wat de firmware óns gaf (de DTB-pointer).
    pub x0: u64,
    /// Het cacheable RAM dat geveegd wordt: twee vensters `[a, b)` (een
    /// leeg venster is `a == b`). Samen omvatten ze het kern-RAM en het
    /// hele beeld op `dst`: op virt en de Pi's ligt het beeld in het
    /// kern-RAM en is het tweede venster leeg; op UEFI koos de firmware
    /// een plek buiten het kernvenster, en is het tweede venster het oude
    /// beeld.
    pub sweep: [(Pa, Pa); 2],
    /// Waar de trampoline heen gaat: buiten het beeld en de staging, in
    /// RAM dat de identity map uitvoerbaar mapt. Hij mag in een geveegd
    /// venster liggen (de heap-top op de Radxa): na zijn kopie draait er
    /// geen Rust meer, en het vegen van zijn eigen regels (die al in DRAM
    /// staan) verandert niets aan wat hij uitvoert.
    pub tramp: Pa,
}

/// De trampoline als bytes, of `None` op de host.
#[must_use]
pub fn trampoline() -> Option<&'static [u8]> {
    arch::trampoline()
}

/// "HOPTRAP1": het record van de kale vector van de trampoline is gevuld.
const TRAP_MAGIC: u64 = u64::from_le_bytes(*b"HOPTRAP1");

/// Wat de kale vector van de trampoline op `tramp` ([`Jump::tramp`])
/// vastlegde: `(ESR, ELR, FAR)` van een fault tijdens de sprong of in de
/// eerste stappen van de nieuwe kern, en daarna leeg. `None` zonder record
/// (de gewone koude boot) en op een build zonder die vector (riscv64, de
/// host). Voor de boot ná een flip die niet landde.
pub fn take_trap(tramp: Pa) -> Option<(u64, u64, u64)> {
    take_trap_at(tramp.add(arch::trap_off()?))
}

fn take_trap_at(rec: Pa) -> Option<(u64, u64, u64)> {
    if dev::read64(rec) != TRAP_MAGIC {
        return None;
    }
    let t = (
        dev::read64(rec.add(8)),
        dev::read64(rec.add(16)),
        dev::read64(rec.add(24)),
    );
    dev::write64(rec, 0);
    dev::push(rec, 8);
    Some(t)
}

fn overlaps(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    a < b.saturating_add(blen) && b < a.saturating_add(alen)
}

impl Jump {
    /// Toetst de indeling: dit is de laatste plek waar "nee" nog kan.
    pub fn check(&self) -> Result<(), ChainError> {
        let code = trampoline().ok_or(ChainError::NoTrampoline)?;
        let t = code.len() as u64;
        let bad = |what, pa| Err(ChainError::Layout { what, pa });
        if !self.len.is_multiple_of(8)
            || !self.dst.0.is_multiple_of(8)
            || !self.src.0.is_multiple_of(8)
        {
            return bad("unaligned image", self.dst.0);
        }
        if self.entry < self.dst.0 || self.entry >= self.dst.0.saturating_add(self.len) {
            return bad("entry outside the image", self.entry);
        }
        let end = self.dst.0.saturating_add(self.len);
        let covered = self
            .sweep
            .iter()
            .any(|(a, b)| self.dst.0 >= a.0 && end <= b.0);
        if !covered {
            return bad("image outside the swept RAM", self.dst.0);
        }
        for (a, b) in self.sweep {
            if b.0 < a.0 {
                return bad("sweep window ends before it starts", a.0);
            }
            if overlaps(self.src.0, self.len, a.0, b.0 - a.0) {
                return bad("staging inside the swept RAM", self.src.0);
            }
        }
        if overlaps(self.tramp.0, t, self.dst.0, self.len)
            || overlaps(self.tramp.0, t, self.src.0, self.len)
        {
            return bad("trampoline overlaps the image or the staging", self.tramp.0);
        }
        // Zijn kale vectoren liggen 2 KB verder, en VBAR_EL2 eist 2 KB.
        if !self.tramp.0.is_multiple_of(0x800) {
            return bad("trampoline not 2 KB aligned", self.tramp.0);
        }
        Ok(())
    }
}

/// Springt in de nieuwe kern. Keert alleen terug met een fout, en dan is er
/// niets veranderd behalve de kopie van de trampoline.
///
/// # Safety
///
/// De aanroeper maakt waar dat:
///
/// - dit de OS-core is (de enige core met de kern erop) en er op deze core
///   niets meer hoeft te gebeuren: na de sprong bestaat de oude kern niet
///   meer (geen executor, geen stack);
/// - `[src, src+len)` het complete, gerelokeerde beeld is van een bundel
///   waarvan de som getoetst is (`kern::kernflip::Bundle`, `sha256`), en dat
///   niemand anders dat bereik of `tramp` nog beschrijft;
/// - alles wat de nieuwe kern moet lezen buiten het beeld (het handoff-blob,
///   het pointer/magic-paar, de recorder) al naar DRAM geveegd is;
/// - geen andere core in de vensters van `sweep` schrijft of er code
///   uitvoert (de app-cores draaien in hun partities en in de switch-code
///   in de plan-regio, nooit in het kern-RAM of het kern-beeld).
pub unsafe fn chain(j: &Jump) -> ChainError {
    if let Err(e) = j.check() {
        return e;
    }
    let Some(code) = trampoline() else {
        return ChainError::NoTrampoline;
    };
    // De trampoline naar zijn plek en naar DRAM: hij draait straks met de
    // I-cache uit, dus hij moet in het geheugen staan, niet alleen in een
    // regel. De staging idem: de kopie leest met de MMU uit.
    dev::copy_in(j.tramp, code);
    dev::push(j.tramp, code.len());
    if let Ok(n) = usize::try_from(j.len) {
        dev::push(j.src, n);
    }
    dev::mb();
    // SAFETY: de indeling is net getoetst (`check`); de rest is het
    // contract van deze functie, dat de aanroeper waarmaakt.
    unsafe { arch::jump(j) }
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    use super::{Jump, TRAP_MAGIC};
    use crate::boot::FLIP_ENTRY;

    // De trampoline. Positie-onafhankelijk (alleen registers en relatieve
    // sprongen en adressen), want hij draait op een adres dat hij niet kent.
    // Op 2 KB, zodat zijn vectortabel (2 KB verder) dat op de kopie ook is.
    //
    // In: x0 = dst, x1 = src, x2 = len (8-voud), x3 = entry, x4 = x0 van de
    // nieuwe kern, x5/x6 en x7/x8 = de twee te vegen vensters. Uit: x0 =
    // de x0 van de firmware, x1 = x2 = 0, x3 = het flip-merkteken
    // (`crate::boot::FLIP_ENTRY`: elke core mag door `_start`).
    core::arch::global_asm!(
        r#"
    .pushsection .text.hopos_chain, "ax"
    .balign 2048
    .global hopos_chain_tramp
hopos_chain_tramp:
    msr daifset, #0xf
    // De vectoren op de kale dumper hieronder, vóór de kopie de oude
    // overschrijft (Go 01-09).
    adr x9, hopos_chain_vec
    msr vbar_el2, x9
    isb
    mov x16, x3
    // MMU, D- en I-cache uit: lezen en maskeren, geen gegokte vaste waarde.
    mrs x9, sctlr_el2
    bic x9, x9, #1
    bic x9, x9, #(1 << 2)
    bic x9, x9, #(1 << 12)
    msr sctlr_el2, x9
    isb
    // Het kern-RAM naar DRAM en uit de cache, regelgrootte uit CTR_EL0.
    mrs x9, ctr_el0
    ubfx x9, x9, #16, #4
    mov x10, #4
    lsl x10, x10, x9
    sub x11, x10, #1
    bic x5, x5, x11
1:  cmp x5, x6
    b.hs 2f
    dc civac, x5
    add x5, x5, x10
    b 1b
2:  bic x7, x7, x11
5:  cmp x7, x8
    b.hs 6f
    dc civac, x7
    add x7, x7, x10
    b 5b
6:  dsb sy
    // Het beeld naar het koude adres, woord voor woord.
3:  cbz x2, 4f
    ldr x9, [x1], #8
    str x9, [x0], #8
    sub x2, x2, #8
    b 3b
4:  dsb sy
    ic iallu
    dsb sy
    isb
    mov x0, x4
    mov x1, xzr
    mov x2, xzr
    movz x3, #{f0}
    movk x3, #{f1}, lsl #16
    movk x3, #{f2}, lsl #32
    movk x3, #{f3}, lsl #48
    br x16

    // De kale dumper: ESR, ELR en FAR in het record, de magic als laatste,
    // naar DRAM (een watchdog-reset spoelt geen cache), en stil.
hopos_chain_dump:
    mrs x9, esr_el2
    mrs x10, elr_el2
    mrs x11, far_el2
    adr x12, hopos_chain_trap
    stp x9, x10, [x12, #8]
    str x11, [x12, #24]
    dsb sy
    movz x13, #{t0}
    movk x13, #{t1}, lsl #16
    movk x13, #{t2}, lsl #32
    movk x13, #{t3}, lsl #48
    str x13, [x12]
    dc civac, x12
    add x12, x12, #16
    dc civac, x12
    dsb sy
7:  wfi
    b 7b
    .balign 64
    .global hopos_chain_trap
hopos_chain_trap:
    .quad 0, 0, 0, 0
    // De tabel: elke ingang naar de dumper.
    .balign 2048
hopos_chain_vec:
    .rept 16
    .balign 128
    b hopos_chain_dump
    .endr
    .global hopos_chain_tramp_end
hopos_chain_tramp_end:

    .popsection
"#,
        f0 = const FLIP_ENTRY & 0xffff,
        f1 = const (FLIP_ENTRY >> 16) & 0xffff,
        f2 = const (FLIP_ENTRY >> 32) & 0xffff,
        f3 = const (FLIP_ENTRY >> 48) & 0xffff,
        t0 = const TRAP_MAGIC & 0xffff,
        t1 = const (TRAP_MAGIC >> 16) & 0xffff,
        t2 = const (TRAP_MAGIC >> 32) & 0xffff,
        t3 = const (TRAP_MAGIC >> 48) & 0xffff,
    );

    unsafe extern "C" {
        safe static hopos_chain_tramp: u8;
        safe static hopos_chain_trap: u8;
        safe static hopos_chain_tramp_end: u8;
    }

    /// Waar het record van de kale vector in de trampoline ligt.
    pub(super) fn trap_off() -> Option<u64> {
        let off = (&raw const hopos_chain_trap as usize)
            .wrapping_sub(&raw const hopos_chain_tramp as usize);
        trampoline()
            .is_some_and(|t| off + 32 <= t.len())
            .then_some(off as u64)
    }

    pub(super) fn trampoline() -> Option<&'static [u8]> {
        let start = &raw const hopos_chain_tramp;
        let len = (&raw const hopos_chain_tramp_end as usize).wrapping_sub(start as usize);
        if len == 0 || len > 4096 {
            return None;
        }
        // SAFETY: twee labels in dezelfde `global_asm!` hierboven, in één
        // sectie, met het einde erachter (net getoetst): de bytes ertussen
        // zijn code in de eigen `.text`, leesbaar, `'static` en nooit
        // beschreven.
        Some(unsafe { core::slice::from_raw_parts(start, len) })
    }

    /// # Safety
    ///
    /// Het contract van [`super::chain`].
    pub(super) unsafe fn jump(j: &Jump) -> ! {
        // SAFETY: interrupts gaan hier dicht (een timer-IRQ tussen "MMU uit"
        // en de sprong zou in code landen die er straks niet meer is); de
        // I-cache wordt ongeldig gemaakt zodat de net gekopieerde
        // trampoline vers gehaald wordt; de sprong gaat naar code die
        // `chain` op `tramp` legde en naar DRAM veegde. Er komt niets terug.
        unsafe {
            core::arch::asm!(
                "msr daifset, #0xf",
                "dsb sy",
                "ic iallu",
                "dsb sy",
                "isb",
                "br {t}",
                t = in(reg) j.tramp.0,
                in("x0") j.dst.0,
                in("x1") j.src.0,
                in("x2") j.len,
                in("x3") j.entry,
                in("x4") j.x0,
                in("x5") j.sweep[0].0.0,
                in("x6") j.sweep[0].1.0,
                in("x7") j.sweep[1].0.0,
                in("x8") j.sweep[1].1.0,
                options(noreturn, nostack)
            )
        }
    }
}

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
mod arch {
    use super::Jump;

    // Het cache-onderhoud van de C906 (T-Head, feature `thead`): de hele
    // D-cache naar DRAM en ongeldig, dan de I-cache ongeldig; zonder (QEMU,
    // coherent) alleen de `fence.i` erachter.
    #[cfg(feature = "thead")]
    macro_rules! flush {
        () => {
            r#"
    .4byte 0x0030000b
    .4byte 0x01b0000b
    .4byte 0x0100000b
    .4byte 0x01b0000b
"#
        };
    }
    #[cfg(not(feature = "thead"))]
    macro_rules! flush {
        () => {
            ""
        };
    }

    // De trampoline. Positie-onafhankelijk (alleen registers en relatieve
    // sprongen), want hij draait op een adres dat hij niet kent.
    //
    // In: a0 = dst, a1 = src, a2 = len (8-voud), a3 = entry, a4 = de a1 van
    // de firmware. Eerst de D-cache leeg (geen vuile regel van de oude kern
    // die later over de nieuwe heen valt), dan de kopie, dan alles naar
    // DRAM en de I-cache leeg (het venster was net nog code van de oude
    // kern, op dezelfde adressen).
    core::arch::global_asm!(
        r#"
    .pushsection .text.hopos_chain, "ax"
    .balign 64
    .global hopos_chain_tramp
hopos_chain_tramp:
    csrw mie, zero
    csrci mstatus, 8
    mv t6, a3
"#,
        flush!(),
        r#"
1:  beqz a2, 2f
    ld t0, 0(a1)
    sd t0, 0(a0)
    addi a1, a1, 8
    addi a0, a0, 8
    addi a2, a2, -8
    j 1b
2:  fence
"#,
        flush!(),
        r#"
    fence.i
    li a0, 0
    mv a1, a4
    jr t6
    .global hopos_chain_tramp_end
hopos_chain_tramp_end:
    .popsection
"#
    );

    unsafe extern "C" {
        safe static hopos_chain_tramp: u8;
        safe static hopos_chain_tramp_end: u8;
    }

    pub(super) fn trampoline() -> Option<&'static [u8]> {
        let start = &raw const hopos_chain_tramp;
        let len = (&raw const hopos_chain_tramp_end as usize).wrapping_sub(start as usize);
        if len == 0 || len > 4096 {
            return None;
        }
        // SAFETY: twee labels in dezelfde `global_asm!` hierboven, in één
        // sectie, met het einde erachter (net getoetst): code in de eigen
        // `.text`, leesbaar, `'static` en nooit beschreven.
        Some(unsafe { core::slice::from_raw_parts(start, len) })
    }

    /// De kale vector van arm64 ook niet: de trampoline draait in machine
    /// mode met de interrupts dicht, en een trap gaat naar de mtvec van de
    /// oude kern.
    pub(super) fn trap_off() -> Option<u64> {
        None
    }

    /// # Safety
    ///
    /// Het contract van [`super::chain`].
    pub(super) unsafe fn jump(j: &Jump) -> ! {
        // SAFETY: de interrupts gaan dicht (een timer tussen de kopie en de
        // sprong zou in code landen die er straks niet meer is); de I-cache
        // wordt ongeldig zodat de net gekopieerde trampoline vers gehaald
        // wordt; de sprong gaat naar code die `chain` op `tramp` legde en
        // naar DRAM veegde. Er komt niets terug.
        unsafe {
            core::arch::asm!(
                "csrw mie, zero",
                "csrci mstatus, 8",
                flush!(),
                "fence.i",
                "jr {t}",
                t = in(reg) j.tramp.0,
                in("a0") j.dst.0,
                in("a1") j.src.0,
                in("a2") j.len,
                in("a3") j.entry,
                in("a4") j.x0,
                options(noreturn, nostack)
            )
        }
    }
}

#[cfg(not(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "riscv64")
)))]
mod arch {
    //! Host-kant: geen trampoline en geen sprong; de tests bewijzen de
    //! relocatie en de indeling.
    use super::{ChainError, Jump};

    pub(super) fn trampoline() -> Option<&'static [u8]> {
        None
    }

    pub(super) fn trap_off() -> Option<u64> {
        None
    }

    /// # Safety
    ///
    /// Op de host is er niets om in te springen.
    pub(super) unsafe fn jump(_j: &Jump) -> ChainError {
        ChainError::NoTrampoline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn relocate_adds_the_delta_to_every_listed_word() {
        let mut img = vec![0x4020_0000u64, 7, 0x4020_1234, 0x6000_0000];
        let base = Pa(img.as_mut_ptr() as u64);
        let delta = 0x4020_0000u64.wrapping_sub(0x6020_0000);
        // Het beeld is gelinkt op 0x6020_0000 en gaat naar 0x4020_0000.
        img[0] = 0x6020_0000;
        img[2] = 0x6020_1234;
        let n = relocate(base, 32, [0u32, 16].into_iter(), delta).unwrap();
        assert_eq!(n, 2);
        assert_eq!(img, [0x4020_0000, 7, 0x4020_1234, 0x6000_0000]);
    }

    #[test]
    fn a_trap_record_is_read_once() {
        let mut rec = vec![TRAP_MAGIC, 0x9600_0045, 0x4020_1234, 0x4800_0000];
        let at = Pa(rec.as_mut_ptr() as u64);
        assert_eq!(
            take_trap_at(at),
            Some((0x9600_0045, 0x4020_1234, 0x4800_0000))
        );
        assert_eq!(take_trap_at(at), None, "read twice");
        rec[0] = 0x464C_4950;
        assert_eq!(take_trap_at(Pa(rec.as_mut_ptr() as u64)), None, "no magic");
        assert_eq!(take_trap(at), None, "the host carries no vector");
    }

    #[test]
    fn a_bad_table_touches_nothing() {
        let mut img = vec![1u64, 2, 3];
        let base = Pa(img.as_mut_ptr() as u64);
        for bad in [[0u32, 4], [0, 24], [0, u32::MAX]] {
            assert!(relocate(base, 24, bad.into_iter(), 100).is_err());
        }
        assert_eq!(img, [1, 2, 3], "patched before the table was checked");
    }

    #[test]
    fn the_host_has_no_jump() {
        let j = Jump {
            dst: Pa(0x4020_0000),
            src: Pa(0xB020_0000),
            len: 0x1000,
            entry: 0x4020_0000,
            x0: 0,
            sweep: [(Pa(0x4000_0000), Pa(0x4f00_0000)), (Pa(0), Pa(0))],
            tramp: Pa(0xB000_2000),
        };
        assert_eq!(j.check(), Err(ChainError::NoTrampoline));
        assert!(trampoline().is_none());
    }
}
