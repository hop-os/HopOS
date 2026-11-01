//! Het geheugen-attribuut van een venster in de EIGEN stage-1-map van de
//! kern (TTBR0_EL2, onder E2H = 0 of 1): van Device-nGnRnE naar Normal-WB
//! ([`normal_wb`]) of Normal-NC ([`normal_nc`]), altijd execute-never.
//!
//! Dit bezit: de rekenkunde over de vertaaltabellen (venster, 2 MB-blokken,
//! het splitsen van een 1 GB-blok, de weigeringen) en de TLB- en
//! MAIR-instructies in `arch`. Niet van hier: wélk venster welk attribuut
//! krijgt. Dat is van de kern (de ABI-staart van een slot) en de drivers
//! (een DMA-regio, een framebuffer). De map is boot-staat, daarna alleen
//! gelezen (PORT §3, de rij `memattr.mu`): er is geen slot, want alleen de
//! executor van core 0 roept dit, bij boot en bij een slot-start.
//!
//! # Waarom
//!
//! De boot-map declareert alles buiten de eigen RAM als Device-nGnRnE. Dat
//! is juist voor registers, maar een framebuffer, een DMA-regio of een
//! ringstaart is geen register: het is DRAM. Onder nGnRnE mag het
//! interconnect stores níet samenvoegen (nG), níet herordenen (nR) en niet
//! vroeg bevestigen (nE). Een 1920x1080x4-frame verlaat de core dan als ~1
//! miljoen losse, geordende transacties in plaats van ~130.000 gatherbare
//! bursts van 64 byte (Pi 5, 04-08).
//!
//! De metingen die het hard maakten:
//!
//! - 03-09, M4: de switch kopieerde elk frame per 8 bytes met een store van
//!   ~290 ns: 27 MB/s. Normal-NC (de tussenstap) gaf 116 MB/s; gecached
//!   (Normal-WB) is het memcpy.
//! - 20-09, Radxa RK3566: de NIC-DMA-regio op Normal-NC, en inkomend ging
//!   van 15,5 naar 56,6 MB/s (de lees-kant: élke load op Device is een
//!   aparte, strikt geordende transactie).
//!
//! Normal-NC is wat Linux een framebuffer geeft (write-combine): geen cache,
//! dus geen onderhoud en de scanout of het device leest gewoon DRAM, maar
//! het fabric mag gatheren. Normal-WB is gewoon geheugen, alleen voor een
//! venster dat beide CPU-kanten delen (de ABI-staart): een device snoopt
//! geen cache, dus DMA-regio's blijven NC. Wie woorden deelt met een lezer
//! zonder cache (de EL2-switcher met de MMU uit) leunt op `dev::push` en
//! `dev::pull`, die op het target altijd echt vegen.
//!
//! # Wat niet meeging
//!
//! Go's `dev.MarkNormal`/`MarkCached` (het register van Normal-gemapte
//! bereiken dat `dev.Copy` memmove liet doen) bestaat in de Rust-`dev` nog
//! niet; PORT §3 maakt er een gepubliceerde tabel van. Tot die er is, blijft
//! `dev` woordgewijs kopiëren, wat op Normal-geheugen correct is, alleen
//! niet zo snel als het kan.

use crate::boot::{ATTR_DEVICE, ATTR_NORMAL, ATTR_NORMAL_NC};
use bounded::BoundedVec;
use core::fmt;
use dev::Pa;

/// De blokmaat op niveau 2: fijner dan dit gaat deze module niet.
pub const BLOCK_2M: u64 = 2 << 20;
/// Eén niveau-1-entry.
const GB: u64 = 1 << 30;

/// Een blok-descriptor (niveau 1 en 2).
const DESC_BLOCK: u64 = 0b01;
/// Een tabel-descriptor.
const DESC_TABLE: u64 = 0b11;
/// Het uitvoer-adresveld van een descriptor (bits 47:12). Alles daarbuiten
/// is attribuut: laag type, AttrIndx, AP, SH en AF, hoog XN.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Over hoeveel gigabytes één venster mag lopen. Een codec-arena is al
/// gauw honderden megabytes en landt zelden binnen één GB; zestien is ruim
/// boven alles wat de Go-kern ooit mapte.
pub const MAX_SPAN_GB: usize = 16;

/// Het attribuut dat een venster krijgt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Attr {
    /// Normal, write-back, inner shareable: gewoon geheugen.
    NormalWb,
    /// Normal, non-cacheable: write-combine.
    NormalNc,
    /// Device-nGnRnE: wat de boot-map buiten de eigen RAM geeft. Voor het
    /// kooi-geheugen uit de pool (`kern::cageheap`): de app-cores lezen het
    /// op EL2 met de MMU uit, dus de kern mag het niet gecached zien.
    Device,
}

impl Attr {
    /// De 2 MB-blokdescriptor voor `pa` in een map onder HCR_EL2.E2H =
    /// `e2h`: geldig, AF, inner shareable, de MAIR-index uit
    /// [`crate::boot`], en [`crate::boot::xn`]: een datavenster is nooit
    /// code, ook niet voor de kern zelf.
    #[must_use]
    pub const fn block(self, pa: u64, e2h: bool) -> u64 {
        let idx = match self {
            Self::NormalWb => ATTR_NORMAL,
            Self::NormalNc => ATTR_NORMAL_NC,
            Self::Device => ATTR_DEVICE,
        };
        crate::boot::block_e2h(pa, idx, e2h) | crate::boot::xn(e2h)
    }
}

/// De vorm van de eigen map, gelezen uit TTBR0_EL2, TCR_EL2 en HCR_EL2.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Regime {
    /// De wortel van de vertaling.
    pub ttbr0: u64,
    /// TCR.T0SZ: 64 min het aantal VA-bits.
    pub t0sz: u64,
    /// HCR_EL2.E2H: bepaalt welke bits execute-never zijn.
    pub e2h: bool,
}

/// Waarom een venster zijn attribuut niet kreeg. Elke variant draagt de
/// getallen: "misaligned" zonder adres is niets waard op een headless node.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Het venster ligt niet op 2 MB-grenzen: afronden zou de buren
    /// meenemen.
    Misaligned {
        /// Het gevraagde begin.
        va: u64,
        /// De gevraagde maat.
        size: u64,
        /// Het naar beneden afgeronde begin.
        lo: u64,
        /// Het naar boven afgeronde einde.
        hi: u64,
    },
    /// Het venster loopt over het einde van de adresruimte.
    Overflow {
        /// Het gevraagde begin.
        va: u64,
        /// De gevraagde maat.
        size: u64,
    },
    /// Deze TCR-vorm kent deze module niet (T0SZ onder 16 of boven 33).
    Regime {
        /// De T0SZ die er stond.
        t0sz: u64,
    },
    /// Het GB valt buiten wat de map met deze T0SZ dekt.
    OutsideMap {
        /// Het GB-nummer.
        gb: u64,
        /// Het aantal GB dat de map dekt.
        limit: u64,
    },
    /// De niveau-0-entry boven dit GB is geen tabel.
    NoL0Table {
        /// De L0-index.
        index: u64,
        /// De entry die er stond.
        entry: u64,
    },
    /// Het GB is niet gemapt (geen blok, geen tabel).
    Unmapped {
        /// Het GB-nummer.
        gb: u64,
        /// De entry die er stond.
        entry: u64,
    },
    /// Het venster loopt over meer dan [`MAX_SPAN_GB`] gigabytes.
    TooWide {
        /// Het eerste GB.
        first: u64,
        /// Het laatste GB.
        last: u64,
    },
    /// Er kwam geen geheugen voor een nieuwe niveau-2-tabel.
    OutOfTables {
        /// Het GB dat gesplitst moest worden.
        gb: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Misaligned { va, size, lo, hi } => write!(
                f,
                "memattr: window {va:#x}+{size:#x} is not on 2MB boundaries \
                 ({lo:#x}..{hi:#x}); the neighbours are not ours"
            ),
            Self::Overflow { va, size } => {
                write!(
                    f,
                    "memattr: window {va:#x}+{size:#x} wraps the address space"
                )
            }
            Self::Regime { t0sz } => write!(f, "memattr: unsupported TCR.T0SZ {t0sz}"),
            Self::OutsideMap { gb, limit } => {
                write!(f, "memattr: GB {gb} lies outside the {limit}GB map")
            }
            Self::NoL0Table { index, entry } => {
                write!(f, "memattr: L0 entry {index} is not a table ({entry:#x})")
            }
            Self::Unmapped { gb, entry } => {
                write!(f, "memattr: GB {gb} is not mapped ({entry:#x})")
            }
            Self::TooWide { first, last } => write!(
                f,
                "memattr: window spans GB {first}..={last}, more than {MAX_SPAN_GB}"
            ),
            Self::OutOfTables { gb } => {
                write!(f, "memattr: no memory for the L2 table splitting GB {gb}")
            }
        }
    }
}

/// Het resultaat van deze module.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Zet `[va, va+size)` in de eigen map op Normal-WB: gecached en inner
/// shareable, gewoon geheugen met memcpy-snelheid.
///
/// Alleen voor een venster dat beide CPU-kanten delen (de ABI-staart van een
/// slot); dezelfde grenzen en weigeringen als [`normal_nc`].
pub fn normal_wb(va: u64, size: u64) -> Result {
    arch::apply(va, size, Attr::NormalWb)
}

/// Zet `[va, va+size)` in de eigen map op Normal-NC (write-combine).
///
/// Het venster moet op 2 MB-grenzen liggen: de map werkt niet fijner dan
/// de bloklaag, en afronden zou de buren meenemen. Idempotent: een tweede
/// aanroep schrijft dezelfde entries. Het venster blijft zo tot de reboot.
pub fn normal_nc(va: u64, size: u64) -> Result {
    arch::apply(va, size, Attr::NormalNc)
}

/// Zet `[va, va+size)` in de eigen map op Device-nGnRnE: de terugweg van
/// [`normal_wb`], voor een stuk van de pool dat de kooi als kooi-geheugen
/// gebruikt. De aanroeper veegt het venster eerst uit de caches (een vuile
/// regel van een vorige huurder klobbert anders later de verse bytes).
pub fn device(va: u64, size: u64) -> Result {
    arch::apply(va, size, Attr::Device)
}

/// Het venster, getoetst: `(lo, hi)` in 2 MB-blokken, of de weigering.
fn window(va: u64, size: u64) -> Result<(u64, u64)> {
    let end = va.checked_add(size).ok_or(Error::Overflow { va, size })?;
    let lo = va & !(BLOCK_2M - 1);
    let hi = end
        .checked_add(BLOCK_2M - 1)
        .ok_or(Error::Overflow { va, size })?
        & !(BLOCK_2M - 1);
    // AFRONDING MAG NIET BUITEN HET VENSTER VALLEN. Dat is geen theorie
    // gebleven: de framebuffer die iBoot achterlaat op de Mac mini begint op
    // 0x105e5304000, ruim een megabyte ná een 2 MB-grens. Hem doorlaten
    // betekende ruim een megabyte FIRMWARE-geheugen van Device naar
    // Normal-NC zetten terwijl die firmware het gebruikte, en de machine
    // herstartte daarna om de paar minuten, zonder paniek en zonder spoor
    // (gemeten 30-08). Een attribuutwissel op geheugen van iemand anders is
    // geen optimalisatie maar een bug; liever een tragere buffer.
    if lo != va || hi != end {
        return Err(Error::Misaligned { va, size, lo, hi });
    }
    Ok((lo, hi))
}

/// Zet het venster op `attr` in de map van `regime`; `new_table` levert een
/// gewiste, 4 KB-gealigneerde tabelpagina die voor altijd blijft bestaan.
///
/// Pure rekenkunde over `dev`: op de host test dit over een buffer. De
/// TLB-invalidatie is van de aanroeper (`arch`).
pub fn remap_in(
    regime: &Regime,
    va: u64,
    size: u64,
    attr: Attr,
    new_table: &mut dyn FnMut() -> Option<Pa>,
) -> Result {
    if size == 0 {
        return Ok(());
    }
    let (lo, hi) = window(va, size)?;
    // Een venster mag over GB-grenzen lopen. Wél eerst ALLE tabellen
    // ophalen en dan pas schrijven: mislukt het ophalen halverwege, dan
    // staat anders de ene helft van het venster op een ander attribuut dan
    // de andere, en dat is een fout die zich pas veel later meldt. Het
    // ophalen zelf mag een 1 GB-blok splitsen: dat verandert niets aan de
    // vertaling (zie `l2_for_gb`).
    let (first, last) = (lo / GB, (hi - 1) / GB);
    if last - first >= MAX_SPAN_GB as u64 {
        return Err(Error::TooWide { first, last });
    }
    let mut tables: BoundedVec<Pa, MAX_SPAN_GB> = BoundedVec::new();
    for gb in first..=last {
        let l2 = l2_for_gb(regime, gb, new_table)?;
        tables
            .push(l2)
            .map_err(|_| Error::TooWide { first, last })?;
    }
    let mut a = lo;
    while a < hi {
        let gb = a / GB;
        let idx = (a % GB) / BLOCK_2M;
        let l2 = tables
            .as_slice()
            .get((gb - first) as usize)
            .copied()
            .ok_or(Error::TooWide { first, last })?;
        dev::write64(l2.add(idx * 8), attr.block(a, regime.e2h));
        a += BLOCK_2M;
    }
    Ok(())
}

/// De niveau-1-tabel die dit GB beschrijft; en dat is niet altijd DE
/// niveau-1-tabel.
///
/// Een 39-bit-map (T0SZ 25, de boot-map) begint op niveau 1 en dekt 512 GB.
/// Op silicium met RAM boven die grens (Apple: DRAM op 1 TiB) begint de map
/// op niveau 0, en wijst entry `gb >> 9` naar de L1 die we zoeken. Hier
/// stond in Go eerst geen bereikcontrole, en dat is niet theoretisch
/// gebleven: een framebuffer op 0x105e5304000 geeft GB 1047, las 535
/// entries voorbij de tabel en nam de node mee (gemeten 30-08 op de M4).
fn l1_for(regime: &Regime, gb: u64) -> Result<Pa> {
    let t0sz = regime.t0sz;
    if !(16..=33).contains(&t0sz) {
        return Err(Error::Regime { t0sz });
    }
    // 2^(64 - T0SZ) bytes aan VA, dus 2^(34 - T0SZ) GB.
    let limit = 1u64 << (34 - t0sz);
    if gb >= limit {
        return Err(Error::OutsideMap { gb, limit });
    }
    let root = Pa(regime.ttbr0 & ADDR_MASK);
    if t0sz >= 25 {
        return Ok(root);
    }
    let index = gb >> 9;
    let entry = dev::read64(root.add(index * 8));
    if entry & 0b11 != DESC_TABLE {
        return Err(Error::NoL0Table { index, entry });
    }
    Ok(Pa(entry & ADDR_MASK))
}

/// De niveau-2-tabel van dit GB. Wijst de L1-entry al naar een tabel, dan
/// die. Staat er een 1 GB-blok, dan splitsen we hem: een verse L2 met 512
/// 2 MB-blokken die exact dezelfde attributen dragen als het blok dat we
/// vervangen, zodat het splitsen zélf niets aan de vertaling verandert.
///
/// Geen break-before-make op de L1-entry, net als in Go: het blok dat we
/// splitsen kan onze eigen code en stack dragen, en die tijdelijk
/// ongeldig maken is zelfmoord. De vertaling blijft dezelfde; de oude
/// TLB-regel gaat weg bij de invalidatie na afloop.
fn l2_for_gb(regime: &Regime, gb: u64, new_table: &mut dyn FnMut() -> Option<Pa>) -> Result<Pa> {
    let l1 = l1_for(regime, gb)?;
    let slot = l1.add((gb & 0x1FF) * 8);
    let cur = dev::read64(slot);
    match cur & 0b11 {
        DESC_TABLE => Ok(Pa(cur & ADDR_MASK)),
        DESC_BLOCK => {
            let tbl = new_table().ok_or(Error::OutOfTables { gb })?;
            let attrs = cur & !ADDR_MASK;
            for i in 0..512u64 {
                dev::write64(tbl.add(i * 8), (gb * GB + i * BLOCK_2M) | attrs);
            }
            // De walker leest cacheable (TCR IRGN0/ORGN0 = WB) en wij
            // schreven cacheable, dus de DSB van de flush volstaat.
            dev::write64(slot, tbl.0 | DESC_TABLE);
            Ok(tbl)
        }
        _ => Err(Error::Unmapped { gb, entry: cur }),
    }
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    //! De registers van de eigen map en de TLB-invalidatie. De kern draait
    //! op EL2 (boot.rs), dus de map is die van TTBR0_EL2; onder E2H = 1
    //! dezelfde registers in de EL2&0-vorm, en TLBI ALLE2IS dekt dat regime.
    extern crate alloc;

    use super::{Attr, Regime, Result, remap_in};
    use crate::boot::ATTR_NORMAL_NC;
    use alloc::vec::Vec;
    use core::arch::asm;
    use dev::Pa;

    /// De MAIR-byte van Normal-NC: Inner en Outer Non-cacheable.
    const MAIR_NC: u64 = 0x44;

    /// Eén tabelpagina.
    #[repr(C, align(4096))]
    struct Table([u64; 512]);

    /// Een gewiste tabelpagina van de heap, voor altijd vastgehouden: de
    /// MMU leest hem zolang de node draait. Identity-map, dus het virtuele
    /// adres ís het fysieke dat in de L1-entry komt.
    ///
    /// `try_reserve_exact` eerst: dan kan de `push` erna niet meer
    /// alloceren, en een volle heap is een `None` in plaats van een abort
    /// (handboek §6).
    fn heap_table() -> Option<Pa> {
        let mut v: Vec<Table> = Vec::new();
        v.try_reserve_exact(1).ok()?;
        v.push(Table([0; 512]));
        let t: &'static mut [Table] = v.leak();
        let p = t.as_ptr() as usize as u64;
        Some(Pa(p))
    }

    pub(super) fn apply(va: u64, size: u64, attr: Attr) -> Result {
        if size == 0 {
            return Ok(());
        }
        ensure_mair();
        let regime = Regime {
            ttbr0: read_ttbr0(),
            t0sz: read_tcr() & 0x3F,
            e2h: read_hcr() & (1 << 34) != 0,
        };
        let r = remap_in(&regime, va, size, attr, &mut heap_table);
        // Ook na een fout: een gesplitst 1 GB-blok is dan al een tabel, en
        // de oude TLB-regel hoort weg.
        flush_tlb();
        r
    }

    /// Eerst MAIR, dán de entries: een entry die naar index 2 wijst terwijl
    /// die index nog 0x00 (Device) is, zou het venster stil device laten.
    /// De boot-stub zet hem al (boot.rs `MAIR`); dit is de toets.
    fn ensure_mair() {
        let shift = 8 * ATTR_NORMAL_NC;
        let m = read_mair();
        if (m >> shift) & 0xff != MAIR_NC {
            write_mair((m & !(0xff << shift)) | (MAIR_NC << shift));
        }
    }

    fn read_mair() -> u64 {
        let v: u64;
        // SAFETY: MAIR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, mair_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    fn write_mair(v: u64) {
        // SAFETY: alleen index 2 verandert, en daar wijst nog geen entry
        // naar (dat is waarom dit vóór de entries gebeurt). De ISB is er
        // niet voor de sier: zonder context-synchronisatie mag de core een
        // volgende toegang nog met de oude attribuut-tabel vertalen.
        unsafe { asm!("msr mair_el2, {}", "isb", in(reg) v, options(nostack)) };
    }

    fn read_tcr() -> u64 {
        let v: u64;
        // SAFETY: TCR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, tcr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    fn read_hcr() -> u64 {
        let v: u64;
        // SAFETY: HCR_EL2 lezen heeft geen neveneffect; dit draait op EL2.
        unsafe { asm!("mrs {}, hcr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    fn read_ttbr0() -> u64 {
        let v: u64;
        // SAFETY: TTBR0_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, ttbr0_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// Publiceert de tabelwijzigingen en gooit de oude vertalingen weg: DSB
    /// ISH (de tabel-writes zichtbaar voor de walker), TLBI ALLE2IS (alle
    /// EL2-vertalingen weg op élke core van het inner-shareable domein: de
    /// kern-map is gedeeld met de node-cores van [`crate::smp`]), DSB ISH
    /// (de invalidatie afgerond), ISB (geen speculatieve toegang met een
    /// oude vertaling meer). Deze volgorde is de architectuur-eis, niet
    /// voorzichtigheid.
    fn flush_tlb() {
        // SAFETY: barrières en een TLB-invalidatie; de tabellen zelf
        // beschrijven na afloop dezelfde of de bedoelde vertaling. Geen
        // `nomem`: de tabel-writes moeten ervóór blijven staan.
        unsafe {
            asm!(
                "dsb ish",
                "tlbi alle2is",
                "dsb ish",
                "isb",
                options(nostack)
            )
        };
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Buiten arm64-ijzer is dit een no-op, en dat is geen gat maar een
    //! gevolg (normalnc_other.go).
    //!
    //! RISC-V: de slot-PTE's dragen expliciet de T-Head-attributen
    //! Cacheable én Bufferable (zónder die twee faultte zelfs een atomic op
    //! de eigen stack, gemeten 31-07). Er is daar dus geen venster dat per
    //! ongeluk device-semantiek krijgt. Op de host is er geen map. De
    //! rekenkunde testen de tests via [`super::remap_in`].
    use super::{Attr, Result};

    pub(super) fn apply(_va: u64, _size: u64, _attr: Attr) -> Result {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[repr(C, align(4096))]
    struct Table([u64; 512]);

    /// Een nep-map over host-geheugen: een wortel en een pool van
    /// tabelpagina's.
    struct Map {
        pool: Vec<Table>,
        next: usize,
    }

    impl Map {
        fn new(pages: usize) -> Map {
            let mut pool = Vec::new();
            for _ in 0..pages {
                pool.push(Table([0; 512]));
            }
            Map { pool, next: 1 }
        }
        fn page(&mut self, i: usize) -> Pa {
            Pa(self.pool[i].0.as_mut_ptr() as usize as u64)
        }
        fn root(&mut self) -> Pa {
            self.page(0)
        }
        fn regime39(&mut self) -> Regime {
            Regime {
                ttbr0: self.root().0,
                t0sz: 25,
                e2h: false,
            }
        }
        fn alloc(&mut self) -> Option<Pa> {
            if self.next >= self.pool.len() {
                return None;
            }
            let p = self.page(self.next);
            self.next += 1;
            Some(p)
        }
        /// Zet GB `gb` in de L1 op een 1 GB-blok met `attr`-index.
        fn gb_block(&mut self, l1: Pa, gb: u64, idx: u64) {
            dev::write64(l1.add(gb * 8), crate::boot::block(gb * GB, idx));
        }
    }

    /// Loopt de map zoals de MMU: de descriptor die `va` vertaalt.
    fn walk(r: &Regime, va: u64) -> Option<u64> {
        let l1 = l1_for(r, va / GB).ok()?;
        let e1 = dev::read64(l1.add(((va / GB) & 0x1FF) * 8));
        match e1 & 0b11 {
            DESC_BLOCK => Some(e1),
            DESC_TABLE => {
                let e2 = dev::read64(Pa(e1 & ADDR_MASK).add(((va % GB) / BLOCK_2M) * 8));
                (e2 & 0b11 == DESC_BLOCK).then_some(e2)
            }
            _ => None,
        }
    }

    fn attr_index(desc: u64) -> u64 {
        (desc >> 2) & 0x7
    }

    fn remap(m: &mut Map, r: &Regime, va: u64, size: u64, attr: Attr) -> Result {
        remap_in(r, va, size, attr, &mut || m.alloc())
    }

    #[test]
    fn a_gb_block_splits_without_changing_the_rest() {
        let mut m = Map::new(4);
        let r = m.regime39();
        let root = m.root();
        m.gb_block(root, 3, crate::boot::ATTR_DEVICE);
        let before = walk(&r, 3 * GB + 100 * MB).unwrap();

        let va = 3 * GB + 64 * MB;
        remap(&mut m, &r, va, 8 * MB, Attr::NormalNc).unwrap();

        // Het venster: vier NC-blokken, XN, op hun eigen adres.
        for a in (va..va + 8 * MB).step_by(BLOCK_2M as usize) {
            let d = walk(&r, a).unwrap();
            assert_eq!(d, Attr::NormalNc.block(a, false));
            assert_eq!(attr_index(d), ATTR_NORMAL_NC);
            assert_ne!(d & crate::boot::xn(false), 0);
        }
        // De buren: dezelfde attributen als het oude 1 GB-blok.
        let after = walk(&r, 3 * GB + 100 * MB).unwrap();
        assert_eq!(after & !ADDR_MASK, before & !ADDR_MASK);
        assert_eq!(after & ADDR_MASK, 3 * GB + 100 * MB);
        assert_eq!(
            walk(&r, va - BLOCK_2M).unwrap() & !ADDR_MASK,
            before & !ADDR_MASK
        );
        assert_eq!(
            walk(&r, va + 8 * MB).unwrap() & !ADDR_MASK,
            before & !ADDR_MASK
        );
        assert_eq!(m.next, 2, "one L2 table taken");
    }

    #[test]
    fn an_existing_table_is_reused_and_repeats_are_idempotent() {
        let mut m = Map::new(4);
        let r = m.regime39();
        let root = m.root();
        // GB0 wijst al naar een tabel (de nullpointer-val in tamago).
        let l2 = m.alloc().unwrap();
        dev::write64(root, l2.0 | DESC_TABLE);
        let taken = m.next;
        remap(&mut m, &r, 0x0020_0000, 2 * MB, Attr::NormalWb).unwrap();
        remap(&mut m, &r, 0x0020_0000, 2 * MB, Attr::NormalWb).unwrap();
        assert_eq!(m.next, taken);
        let d = walk(&r, 0x0020_0000).unwrap();
        assert_eq!(d, Attr::NormalWb.block(0x0020_0000, false));
        assert_eq!(attr_index(d), ATTR_NORMAL);
        assert_ne!(d & crate::boot::xn(false), 0, "a data window is never code");
    }

    #[test]
    fn a_window_may_cross_a_gb_boundary() {
        let mut m = Map::new(4);
        let r = m.regime39();
        let root = m.root();
        m.gb_block(root, 1, crate::boot::ATTR_DEVICE);
        m.gb_block(root, 2, crate::boot::ATTR_DEVICE);
        let va = 2 * GB - 4 * MB;
        remap(&mut m, &r, va, 8 * MB, Attr::NormalNc).unwrap();
        for a in (va..va + 8 * MB).step_by(BLOCK_2M as usize) {
            assert_eq!(walk(&r, a).unwrap(), Attr::NormalNc.block(a, false));
        }
        assert_eq!(m.next, 3);
    }

    #[test]
    fn misaligned_windows_are_refused_without_a_write() {
        let mut m = Map::new(4);
        let r = m.regime39();
        let root = m.root();
        m.gb_block(root, 1, crate::boot::ATTR_DEVICE);
        let e = remap(&mut m, &r, GB + MB, 2 * MB, Attr::NormalNc);
        assert_eq!(
            e,
            Err(Error::Misaligned {
                va: GB + MB,
                size: 2 * MB,
                lo: GB,
                hi: GB + 4 * MB
            })
        );
        let e = remap(&mut m, &r, GB, 3 * MB, Attr::NormalNc);
        assert!(matches!(e, Err(Error::Misaligned { .. })));
        assert_eq!(m.next, 1, "nothing split");
        assert_eq!(walk(&r, GB).unwrap() & 0b11, DESC_BLOCK);
        assert_eq!(remap(&mut m, &r, GB, 0, Attr::NormalNc), Ok(()));
        let e = remap(&mut m, &r, u64::MAX - MB, 4 * MB, Attr::NormalNc);
        assert!(matches!(e, Err(Error::Overflow { .. })));
    }

    #[test]
    fn the_m4_framebuffer_is_outside_a_flat_map() {
        // 0x105e5304000 ligt in GB 1047; de vlakke 39-bit-map dekt er 512.
        let mut m = Map::new(2);
        let r = m.regime39();
        let e = remap(&mut m, &r, 1047 * GB, 2 * MB, Attr::NormalNc);
        assert_eq!(
            e,
            Err(Error::OutsideMap {
                gb: 1047,
                limit: 512
            })
        );
    }

    #[test]
    fn a_48_bit_map_goes_through_l0() {
        let mut m = Map::new(4);
        let l0 = m.root();
        let r = Regime {
            ttbr0: l0.0,
            t0sz: 16,
            e2h: true,
        };
        // GB 1047 hangt onder L0-entry 2.
        let e = remap(&mut m, &r, 1047 * GB, 2 * MB, Attr::NormalNc);
        assert_eq!(e, Err(Error::NoL0Table { index: 2, entry: 0 }));
        let l1 = m.alloc().unwrap();
        dev::write64(l0.add(2 * 8), l1.0 | DESC_TABLE);
        m.gb_block(l1, 1047 & 0x1FF, crate::boot::ATTR_DEVICE);
        remap(&mut m, &r, 1047 * GB, 2 * MB, Attr::NormalNc).unwrap();
        assert_eq!(
            walk(&r, 1047 * GB).unwrap(),
            Attr::NormalNc.block(1047 * GB, true)
        );
    }

    #[test]
    fn fetch_all_tables_before_writing_any_entry() {
        let mut m = Map::new(4);
        let r = m.regime39();
        let root = m.root();
        m.gb_block(root, 1, crate::boot::ATTR_DEVICE);
        // GB 2 is niet gemapt: het venster over GB 1 en 2 moet in zijn
        // geheel weigeren, en GB 1 blijft Device (gesplitst, maar gelijk).
        let e = remap(&mut m, &r, 2 * GB - 2 * MB, 4 * MB, Attr::NormalNc);
        assert_eq!(e, Err(Error::Unmapped { gb: 2, entry: 0 }));
        let d = walk(&r, 2 * GB - 2 * MB).unwrap();
        assert_eq!(attr_index(d), crate::boot::ATTR_DEVICE);
    }

    #[test]
    fn out_of_tables_and_too_wide() {
        let mut m = Map::new(1);
        let r = m.regime39();
        let root = m.root();
        m.gb_block(root, 1, crate::boot::ATTR_DEVICE);
        let e = remap(&mut m, &r, GB, 2 * MB, Attr::NormalNc);
        assert_eq!(e, Err(Error::OutOfTables { gb: 1 }));
        let e = remap(&mut m, &r, GB, 17 * GB, Attr::NormalNc);
        assert_eq!(e, Err(Error::TooWide { first: 1, last: 17 }));
        let e = remap(&mut m, &r, GB, 2 * MB, Attr::NormalNc);
        assert!(e.is_err());
        let bad = Regime {
            ttbr0: 0,
            t0sz: 40,
            e2h: false,
        };
        assert_eq!(
            remap(&mut m, &bad, GB, 2 * MB, Attr::NormalNc),
            Err(Error::Regime { t0sz: 40 })
        );
    }

    #[test]
    fn execute_never_holds_for_the_kern_in_both_regimes() {
        const UXN_OR_XN: u64 = 1 << 54;
        const PXN: u64 = 1 << 53;
        for attr in [Attr::NormalWb, Attr::NormalNc] {
            // E2H = 0: bit 54 is XN voor EL2, bit 53 is RES0.
            let d = attr.block(GB, false);
            assert_eq!(d & (UXN_OR_XN | PXN), UXN_OR_XN);
            // E2H = 1 (de O6N, Apple): bit 54 is alleen UXN, de kern
            // houdt pas PXN tegen.
            let d = attr.block(GB, true);
            assert_eq!(d & (UXN_OR_XN | PXN), UXN_OR_XN | PXN);
        }
        // De rest van de descriptor hangt niet af van het regime.
        assert_eq!(
            Attr::NormalNc.block(GB, true) & !PXN,
            Attr::NormalNc.block(GB, false)
        );
        // De boot-map: Device en NC krijgen hetzelfde, de kern-RAM blijft
        // in beide regimes uitvoerbaar.
        assert_eq!(
            crate::boot::block_e2h(0, crate::boot::ATTR_DEVICE, true) & PXN,
            PXN
        );
        assert_eq!(
            crate::boot::block_e2h(GB, ATTR_NORMAL, true) & (UXN_OR_XN | PXN),
            0
        );
    }

    #[test]
    fn host_apply_is_a_no_op() {
        assert_eq!(normal_nc(GB + MB, MB), Ok(()));
        assert_eq!(normal_wb(0, 2 * MB), Ok(()));
    }
}
