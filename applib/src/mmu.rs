//! De stage-1 van elke app op arm64: één identiteitsmap (VA = IPA), gebouwd
//! en aangezet in `_start` vóór de eerste regel Rust die geheugen van de app
//! aanraakt, en de vectortabel van EL1 die een exception van de app zelf
//! vangt en meldt.
//!
//! Dit module bezit de tabellen in de 64 KB onder het image (de ruimte die
//! de ABI ervoor openhoudt, `abi::layout::LINK_TEXT_OFF`) en de vectortabel.
//! Niet van hier: de stage-2 (van de kern), en wat in het glas staat
//! (`crate::fb`).
//!
//! # Waarom elke app een MMU heeft (30-09, de eerste Pi 5-boot)
//!
//! Tot 30-09 draaide een app op EL1 met de MMU uit. Dan is al het geheugen
//! voor stage 1 Device-nGnRnE, en op Device is elke ongealigneerde load of
//! store een alignment-fault. Rust (en leannet, JSON, `from_le_bytes` op
//! een slice, `memcpy` met een staart) doet ze bij de vleet. QEMU-TCG
//! toetst het niet zodra stage 2 aan staat (`aprofile_require_alignment`
//! in target/arm/tcg/hflags.c geeft `false` bij HCR_EL2.VM), een
//! Cortex-A76 wel: Hop viel bij zijn eerste beurt op de Pi 5 en sprong naar
//! zijn lege VBAR_EL1 (`esr=0x82000005 far=0x200`, een instructie-abort op
//! `VBAR + 0x200`). TamaGo zette zijn MMU aan in `hwinit0`, "as soon as
//! possible", om precies deze reden; hier gebeurt het in `_start`.
//!
//! # De map
//!
//! | Gebied | Attribuut | Waarom |
//! | --- | --- | --- |
//! | de 64 KB tabellen onderin de RAM-declaratie | Normal-NC, XN | de walker leest Non-cacheable (TCR IRGN0/ORGN0 0), dus een latere schrijf van [`map_glass`] staat meteen in het geheugen, zonder cache-onderhoud |
//! | de rest van de RAM-declaratie (image, heap, stack) | Normal write-back, uitvoerbaar op EL1 | de kern schrijft het image en de `.bss` via zijn eigen Normal-WB-map; de cores zijn onderling coherent, dus WB tegen WB is coherent zonder onderhoud |
//! | de control-page (eerste 4 KB van de staart) | Normal-NC, XN | de EL2-switcher schrijft er met de MMU uit het fault-rapport in (`CTRL_FAULT_*`) en leest er de deurbel (`CTRL_RX_DOOR`); dezelfde cacheline draagt `CTRL_IDLE`, die de app vaak schrijft. Met WB zou de vuile regel van de app bij de volgende `dc civac` van de kern over het rapport van de switcher heen schrijven. NC is precies de semantiek die de page met de MMU uit had, zonder de alignment-val |
//! | de rest van de staart (outbox, frame-ringen) | Normal write-back, XN | het is RAM; de kern en zijn switch lezen en schrijven het via Normal-WB, geen DMA. De switcher peekt alleen de RX-kop (lezen; "mag achterlopen", switch.rs) |
//! | het glas van de display-app | Normal-NC, XN | de scanout leest DRAM; [`map_glass`] zet het er later bij, in dezelfde tabellen |
//!
//! SCTLR krijgt M, C en I. In de vorige versie (fb.rs, alleen de
//! display-app) bleef C uit, omdat de kern en de switcher app-RAM "met de
//! MMU uit" zouden lezen. Dat klopt niet (meer): de kern raakt app-geheugen
//! alleen via zijn Normal-WB-map van de pool, de switcher schrijft alleen de
//! ctx-blokken buiten de partitie plus de control-page, en die blijft hier
//! NC. De prijs van C uit was dat elke data-toegang van de app
//! Non-cacheable was.
//!
//! # De volgorde in `_start` en het cache-gevaar
//!
//! Wat de app met de MMU uit schrijft, gaat Non-cacheable naar het geheugen;
//! een core die dezelfde regel gecached heeft (de kern na zijn scrub van de
//! partitie, of een vorige huurder) houdt dan een oude kopie, en een
//! cacheable lees na de MMU-aan kan die krijgen. Daarom:
//!
//! 1. de tabellen: `dc civac` over de 64 KB vóór het schrijven (er ligt dan
//!    nergens een kopie) en erna (weg met wat een buurcore intussen
//!    speculatief las); de walker leest ze NC;
//! 2. de stack: [`__applib_stage1_build`] keert terug vóór de MMU aangaat,
//!    en `_start` zet daarna de stack opnieuw bovenin. Niets dat met de MMU
//!    uit op de stack kwam, wordt met de MMU aan nog gelezen;
//! 3. `.bss` wordt pas ná de MMU-aan geveegd, cacheable;
//! 4. de patch-woorden (`RamStart`, `RamSize`) worden met de MMU uit alleen
//!    gelezen; de kern publiceerde ze (`push`).
//!
//! En de bouwer zelf draait met de MMU uit, dus op Device: hij doet alleen
//! gealigneerde 64-bit-toegang (`dev::read64`/`write64`), geen slices,
//! geen `memcpy`.
//!
//! # Een secundaire core
//!
//! Een SMP-app deelt zijn tabellen: de primaire geeft TTBR0, TCR, MAIR en
//! VBAR mee in de handoff ([`regime`], `crate::smp`), en de stub van de
//! secundaire zet ze plus SCTLR M/C/I vóór zijn eerste geheugentoegang.
//! Zijn stack komt uit de heap van de primaire (WB): coherent.
//!
//! RISC-V heeft de PMP-kooi en geen alignment-val; daar is dit module een
//! stub.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Een pagina van de stage-1.
pub const PAGE: u64 = 0x1000;

/// Waarom er geen stage-1 is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmuError {
    /// De RAM-declaratie begint niet op het linkvenster: de 64 KB eronder
    /// zijn dan niet van de tabellen.
    NotAtLinkBase {
        /// `RamStart`.
        start: u64,
    },
    /// De RAM-declaratie is kleiner dan de tabelruimte, staat niet op een
    /// pagina, of de staart loopt om.
    Shape {
        /// `RamStart`.
        start: u64,
        /// `RamSize`.
        size: u64,
    },
    /// De tabellen passen niet in de 64 KB.
    Tables,
    /// Niet gebouwd: een ander target (de host, RISC-V).
    Off,
}

impl fmt::Display for MmuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NotAtLinkBase { start } => write!(
                f,
                "no stage-1 map: RamStart {start:#x} is not the link base {:#x}",
                abi::layout::LINK_BASE
            ),
            Self::Shape { start, size } => {
                write!(
                    f,
                    "no stage-1 map: RAM {start:#x}+{size:#x} has no room or shape for it"
                )
            }
            Self::Tables => write!(
                f,
                "no stage-1 map: the tables exceed the 64 KB under the image"
            ),
            Self::Off => write!(f, "no stage-1 map on this target"),
        }
    }
}

/// Het boot-woord van `_start`: 0 = niet gebouwd, 1..=[`TABLE_PAGES`] = aan
/// met zoveel tabelpagina's, bit 63 plus een code = geweigerd.
const REFUSED: u64 = 1 << 63;

/// Een [`MmuError`] als boot-woord (met de getallen van de weigering kwijt;
/// die meldt [`report`] opnieuw uit de patch-woorden).
const fn refusal_word(e: MmuError) -> u64 {
    REFUSED
        | match e {
            MmuError::NotAtLinkBase { .. } => 1,
            MmuError::Shape { .. } => 2,
            MmuError::Tables => 3,
            MmuError::Off => 4,
        }
}

/// Het boot-woord terug als uitkomst.
fn decode(word: u64, start: u64, size: u64) -> Result<usize, MmuError> {
    match word {
        0 => Err(MmuError::Off),
        w if w & REFUSED != 0 => Err(match w & 0xff {
            1 => MmuError::NotAtLinkBase { start },
            2 => MmuError::Shape { start, size },
            3 => MmuError::Tables,
            _ => MmuError::Off,
        }),
        w => usize::try_from(w).map_err(|_| MmuError::Tables),
    }
}

/// Wat de stage-1 draagt, allemaal identiteit en op 4 KB: `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// De tabelruimte: de 64 KB onderin de RAM-declaratie.
    pub(crate) tables: (u64, u64),
    /// De rest van de RAM-declaratie: image, heap, stack.
    pub(crate) ram: (u64, u64),
    /// De control-page.
    pub(crate) ctrl: (u64, u64),
    /// De rest van de staart: outbox en frame-ringen.
    pub(crate) rings: (u64, u64),
}

impl Plan {
    /// Het plan voor een RAM-declaratie van `size` bytes op `start`.
    pub(crate) const fn new(start: u64, size: u64) -> Result<Plan, MmuError> {
        use abi::layout::{ABI_TAIL, CTRL_STRIDE, LINK_BASE, LINK_TEXT_OFF};
        if start != LINK_BASE {
            return Err(MmuError::NotAtLinkBase { start });
        }
        let shape = MmuError::Shape { start, size };
        let Some(tail) = start.checked_add(size) else {
            return Err(shape);
        };
        let Some(end) = tail.checked_add(ABI_TAIL) else {
            return Err(shape);
        };
        if size <= LINK_TEXT_OFF || !size.is_multiple_of(PAGE) {
            return Err(shape);
        }
        let text = start + LINK_TEXT_OFF;
        Ok(Plan {
            tables: (start, text),
            ram: (text, tail),
            ctrl: (tail, tail + CTRL_STRIDE),
            rings: (tail + CTRL_STRIDE, end),
        })
    }
}

/// Hoeveel pagina's tabel er in de 64 KB onder het image passen.
#[cfg(any(test, all(target_os = "none", target_arch = "aarch64")))]
pub(crate) const TABLE_PAGES: usize = (abi::layout::LINK_TEXT_OFF / PAGE) as usize;

/// De tabellen: alleen waar ze gebouwd worden (het aarch64-target) en in de
/// tests.
#[cfg(any(test, all(target_os = "none", target_arch = "aarch64")))]
pub(crate) mod tables {
    use super::{MmuError, PAGE, Plan, TABLE_PAGES};

    /// Een blok van de tweede laag.
    pub(crate) const BLOCK: u64 = 0x20_0000;
    /// Een blok van de eerste laag.
    pub(crate) const GIB: u64 = 0x4000_0000;

    // De descriptorbits van een stage-1-entry (4 KB-korrel, ARM ARM D8.3).
    pub(crate) const DESC_BLOCK: u64 = 0b01;
    pub(crate) const DESC_TABLE: u64 = 0b11;
    pub(crate) const DESC_PAGE: u64 = 0b11;
    pub(crate) const ATTR_SHIFT: u64 = 2;
    /// Inner shareable.
    pub(crate) const SH_INNER: u64 = 0b11 << 8;
    /// Access flag: zonder hem faultt de eerste toegang.
    pub(crate) const AF: u64 = 1 << 10;
    /// Niet uitvoerbaar op EL1.
    pub(crate) const PXN: u64 = 1 << 53;
    /// Niet uitvoerbaar op EL0.
    pub(crate) const UXN: u64 = 1 << 54;
    /// Het adresveld van een entry.
    pub(crate) const OA: u64 = 0x0000_FFFF_FFFF_F000;

    /// De MAIR-indexen: Device-nGnRnE, Normal-NC, Normal write-back
    /// (MAIR_EL1 in `hw`). Device staat erin voor de volledigheid: de map
    /// gebruikt hem niet meer.
    pub(crate) const IDX_DEVICE: u64 = 0;
    pub(crate) const IDX_NC: u64 = 1;
    pub(crate) const IDX_WB: u64 = 2;

    /// RAM: write-back, uitvoerbaar op EL1.
    pub(crate) const ATTR_RAM: u64 = IDX_WB << ATTR_SHIFT | SH_INNER | AF | UXN;
    /// De ringen: write-back, niet uitvoerbaar.
    pub(crate) const ATTR_RINGS: u64 = IDX_WB << ATTR_SHIFT | SH_INNER | AF | PXN | UXN;
    /// Tabellen, control-page en glas: Normal-NC, niet uitvoerbaar.
    pub(crate) const ATTR_NC: u64 = IDX_NC << ATTR_SHIFT | SH_INNER | AF | PXN | UXN;

    /// Het geheugen waar de tabellen in staan: entry `idx` van tabelpagina
    /// `page`. Op het target de 64 KB onder het image, in de tests een
    /// vector.
    pub(crate) trait TableMem {
        /// Leest een entry.
        fn get(&self, page: usize, idx: usize) -> u64;
        /// Schrijft een entry.
        fn set(&mut self, page: usize, idx: usize, v: u64);
    }

    /// De tabellen: pagina 0 is de eerste laag, de rest wordt uitgedeeld
    /// zoals de map ze vraagt, tot de [`TABLE_PAGES`] op zijn.
    ///
    /// # Invariants
    ///
    /// `1 <= used <= TABLE_PAGES`; de pagina's `0..used` zijn tabellen van
    /// deze map, de rest is vrij.
    pub(crate) struct Tables<M: TableMem> {
        pub(crate) mem: M,
        /// Uitgedeeld.
        pub(crate) used: usize,
        /// Het adres van pagina 0.
        pub(crate) base: u64,
    }

    impl<M: TableMem> Tables<M> {
        /// Lege tabellen: alleen een geveegde eerste laag.
        pub(crate) fn new(mem: M, base: u64) -> Tables<M> {
            // INVARIANT: pagina 0 is de eerste laag.
            let mut t = Tables { mem, used: 1, base };
            t.clear(0);
            t
        }

        /// De tabellen die [`Tables::of`] eerder bouwde, met `used`
        /// pagina's in gebruik: om er een gebied bij te zetten.
        pub(crate) fn resume(mem: M, base: u64, used: usize) -> Result<Tables<M>, MmuError> {
            if used == 0 || used > TABLE_PAGES {
                return Err(MmuError::Tables);
            }
            // INVARIANT: zojuist getoetst.
            Ok(Tables { mem, used, base })
        }

        /// Het adres van pagina `i`.
        pub(crate) fn pa(&self, i: usize) -> u64 {
            self.base.wrapping_add(i as u64 * PAGE)
        }

        fn clear(&mut self, page: usize) {
            for i in 0..512 {
                self.mem.set(page, i, 0);
            }
        }

        /// De tabel onder entry `idx` van pagina `page`: bestaand, of vers.
        fn child(&mut self, page: usize, idx: usize) -> Result<usize, MmuError> {
            let cur = self.mem.get(page, idx);
            if cur & 0b11 == DESC_TABLE {
                let off = (cur & OA).wrapping_sub(self.base) / PAGE;
                return usize::try_from(off)
                    .ok()
                    .filter(|p| *p < self.used)
                    .ok_or(MmuError::Tables);
            }
            if cur != 0 {
                // Een blok waar een tabel moet: de plannen overlappen.
                return Err(MmuError::Tables);
            }
            let new = self.used;
            if new >= TABLE_PAGES {
                return Err(MmuError::Tables);
            }
            // INVARIANT: `new < TABLE_PAGES`, dus `used` blijft binnen.
            self.used += 1;
            self.clear(new);
            self.mem.set(page, idx, self.pa(new) | DESC_TABLE);
            Ok(new)
        }

        /// Mapt `[lo, hi)` (4 KB-gealigneerd) op zichzelf met `attr`: hele
        /// 1 GB- en 2 MB-blokken als blok, de randen als pagina's. Zo past
        /// ook een grote codec-partitie in de 64 KB tabelruimte.
        pub(crate) fn map(&mut self, lo: u64, hi: u64, attr: u64) -> Result<(), MmuError> {
            if !lo.is_multiple_of(PAGE) || !hi.is_multiple_of(PAGE) {
                return Err(MmuError::Tables);
            }
            let mut a = lo;
            while a < hi {
                let l1 = usize::try_from(a / GIB)
                    .ok()
                    .filter(|i| *i < 512)
                    .ok_or(MmuError::Tables)?;
                if a.is_multiple_of(GIB) && hi - a >= GIB && self.mem.get(0, l1) == 0 {
                    self.mem.set(0, l1, a | attr | DESC_BLOCK);
                    a += GIB;
                    continue;
                }
                let l2page = self.child(0, l1)?;
                let l2 = ((a % GIB) / BLOCK) as usize;
                if a.is_multiple_of(BLOCK) && hi - a >= BLOCK {
                    self.mem.set(l2page, l2, a | attr | DESC_BLOCK);
                    a += BLOCK;
                    continue;
                }
                let l3page = self.child(l2page, l2)?;
                let l3 = ((a % BLOCK) / PAGE) as usize;
                self.mem.set(l3page, l3, a | attr | DESC_PAGE);
                a += PAGE;
            }
            Ok(())
        }

        /// De tabellen van `p` in `mem`, met pagina 0 op het begin van de
        /// tabelruimte.
        pub(crate) fn of(mem: M, p: &Plan) -> Result<Tables<M>, MmuError> {
            let mut t = Tables::new(mem, p.tables.0);
            t.map(p.tables.0, p.tables.1, ATTR_NC)?;
            t.map(p.ram.0, p.ram.1, ATTR_RAM)?;
            t.map(p.ctrl.0, p.ctrl.1, ATTR_NC)?;
            t.map(p.rings.0, p.rings.1, ATTR_RINGS)?;
            Ok(t)
        }
    }
}

/// Het boot-woord zoals `_start` het na de MMU-aan en de veeg van `.bss`
/// overdroeg ([`__applib_stage1_adopt`]), en daarna de tabelpagina's in
/// gebruik (bijgewerkt door [`map_glass`]). Eén schrijver per moment: de
/// primaire core, eerst in `_start`, daarna in de taak van de display-app.
static STATE: AtomicU64 = AtomicU64::new(0);

/// De stage-1 zoals `_start` hem achterliet: het aantal tabelpagina's, of
/// waarom er geen is.
pub fn state() -> Result<usize, MmuError> {
    let (start, size) = crate::rt::ram_declaration();
    decode(STATE.load(Relaxed), start, size)
}

/// Is al het geheugen van deze app Normal (de stage-1 staat aan)? Dan mag
/// een toegang ongealigneerd zijn ([`crate::mem`]). Vóór de overdracht in
/// `_start` en na een weigering niet: dan is alles Device. Eén load, want
/// elke `memcpy` vraagt het.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[inline(always)]
pub(crate) fn normal() -> bool {
    let w = STATE.load(Relaxed);
    w != 0 && w & REFUSED == 0
}

/// Hoe deze app zijn frame-ringen mapt ([`abi::ring::Coherence`]):
/// [`Hardware`](abi::ring::Coherence::Hardware) als de stage-1 aan staat
/// (de rij "de rest van de staart" in de tabel bovenaan: `ATTR_RINGS`, WB
/// en inner shareable; de stage-2 van de kern geeft RAM ook WB inner
/// shareable, cpu/src/el2/stage2.rs `BLOCK_RW`). Zonder stage-1 is alles
/// Device en moet de kopie langs het geheugen.
///
/// Dit is alleen de belofte van deze kant; de ring zelf kopieert pas
/// zonder onderhoud als de kern hetzelfde belooft in de ringkop (elke
/// v3-kern op arm64; op Apple en de Radxa mapt hij daarvoor de staart
/// Normal, en weigert die remap, dan belooft hij niets).
///
/// RISC-V: geen ARM-stage-1 (`hw::PRESENT` is onwaar) en de C906 is niet
/// coherent met het andere hart; daar onderhoud, behalve als de app op het
/// hart van de kern zelf draait (`on_kern_hart`, de control-page): dan is
/// er één cache en belooft hij [`Hardware`](abi::ring::Coherence::Hardware),
/// zoals de kern dan ook doet.
#[must_use]
pub fn ring_coherence(on_kern_hart: bool) -> abi::ring::Coherence {
    if (hw::PRESENT && state().is_ok()) || (cfg!(target_arch = "riscv64") && on_kern_hart) {
        abi::ring::Coherence::Hardware
    } else {
        abi::ring::Coherence::Maintained
    }
}

/// De overdracht van `_start` na de MMU-aan: het boot-woord van
/// [`__applib_stage1_build`]. Na de veeg van `.bss`, dus cacheable en
/// blijvend.
#[unsafe(no_mangle)]
pub extern "C" fn __applib_stage1_adopt(word: u64) {
    STATE.store(word, Relaxed);
}

/// Eén regel over de stage-1 naar de outbox, vanuit de main-schil. Op een
/// target zonder ARM-stage-1 niets: daar is geen MMU uit te leggen.
pub(crate) fn report() {
    if !hw::PRESENT {
        return;
    }
    match state() {
        Ok(pages) => crate::log!(
            "applib: stage-1 on: RAM write-back, control page Normal-NC, rings write-back, {} KB of tables HOPOS_APP_MMU",
            pages as u64 * PAGE / 1024
        ),
        Err(e) => crate::log!(
            "applib: {e}, running with the MMU off: every unaligned access faults on hardware HOPOS_APP_NO_MMU"
        ),
    }
}

/// Het EL1-regime van deze core voor de handoff van een secundaire:
/// (TTBR0, TCR, MAIR, VBAR), gelezen van de levende registers. Nullen op
/// een target zonder ARM-stage-1.
#[must_use]
pub fn regime() -> (u64, u64, u64, u64) {
    hw::regime()
}

/// Zet `[lo, hi)` (4 KB-gealigneerd) er als Normal-NC bij, in dezelfde
/// tabellen: het glas van de display-app. Geeft de bytes aan tabellen die
/// de map daarna gebruikt. Zonder `gui` vraagt niemand het (fb.rs), maar
/// het blijft een stap van de MMU, met zijn tabellen.
#[cfg_attr(
    not(feature = "gui"),
    expect(dead_code, reason = "alleen applib::fb (feature gui) mapt het glas")
)]
pub(crate) fn map_glass(lo: u64, hi: u64) -> Result<u64, MmuError> {
    let used = state()?;
    let used = hw::add_nc(used, lo, hi)?;
    STATE.store(used as u64, Relaxed);
    Ok(used as u64 * PAGE)
}

/// De bouwer, aangeroepen door `_start` met de MMU uit, met `RamStart` en
/// `RamSize` uit de patch-woorden. Geeft het boot-woord: het aantal
/// tabelpagina's (de tabellen staan dan op `start`), of een weigering.
///
/// Draait op Device-geheugen: alleen gealigneerde 64-bit-toegang, zie de
/// moduledoc.
#[unsafe(no_mangle)]
pub extern "C" fn __applib_stage1_build(start: u64, size: u64) -> u64 {
    match Plan::new(start, size).and_then(|p| hw::build(&p)) {
        Ok(used) => used as u64,
        Err(e) => refusal_word(e),
    }
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod hw {
    //! De hardwarekant: de tabellen in het geheugen, de registers, de
    //! vectortabel.

    use super::tables::{IDX_DEVICE, IDX_NC, IDX_WB, TableMem, Tables};
    use super::{ATTR_GLASS, MmuError, PAGE, Plan};
    use abi::hopabi::{
        AppStatus, CTRL_APP_FAULT_ELR, CTRL_APP_FAULT_ESR, CTRL_APP_FAULT_FAR, CTRL_APP_FAULT_VEC,
        CTRL_EXIT_CODE, CTRL_STATUS, EXIT_APP_FAULT,
    };
    use abi::layout::{LINK_BASE, LINK_TEXT_OFF};
    use core::arch::{asm, global_asm};
    use dev::Pa;

    /// Er is een ARM-stage-1.
    pub(super) const PRESENT: bool = true;

    /// MAIR_EL1: Device-nGnRnE (0x00), Normal-NC (0x44) en Normal
    /// write-back read/write-allocate (0xFF) op hun indexen.
    pub(crate) const MAIR: u64 =
        0x00 << (8 * IDX_DEVICE) | 0x44 << (8 * IDX_NC) | 0xFF << (8 * IDX_WB);

    /// TCR_EL1: 39-bit VA vanaf de eerste laag (T0SZ 25), 4 KB-korrel,
    /// tabelwandeling Non-cacheable (IRGN0/ORGN0 0: de tabellen zijn met de
    /// MMU uit geschreven en een latere toevoeging gaat NC, dus de walker
    /// leest wat er staat zonder cache-onderhoud), inner shareable, geen
    /// TTBR1-wandeling (EPD1, TG1 op 4 KB), 36-bit IPA (IPS 1: alles van een
    /// app ligt onder 4 GB).
    pub(crate) const TCR: u64 = 25 | 0b11 << 12 | 1 << 23 | 0b10 << 30 | 0b001 << 32;

    /// De tabelruimte, rechtstreeks: met de MMU uit Device, erna Normal-NC.
    /// Beide keren staat elke store meteen in het geheugen.
    struct UnderImage(u64);

    impl UnderImage {
        fn at(&self, page: usize, idx: usize) -> Pa {
            Pa(self.0 + page as u64 * PAGE + idx as u64 * 8)
        }
    }

    impl TableMem for UnderImage {
        fn get(&self, page: usize, idx: usize) -> u64 {
            dev::read64(self.at(page, idx))
        }
        fn set(&mut self, page: usize, idx: usize, v: u64) {
            dev::write64(self.at(page, idx), v);
        }
    }

    /// De bouw met de MMU uit: de tabelruimte uit elke cache (vóór en na
    /// het schrijven, zie de moduledoc), dan de tabellen.
    pub(super) fn build(p: &Plan) -> Result<usize, MmuError> {
        let len = usize::try_from(p.tables.1 - p.tables.0).unwrap_or(0);
        dev::pull(Pa(p.tables.0), len);
        let t = Tables::of(UnderImage(p.tables.0), p)?;
        dev::pull(Pa(p.tables.0), len);
        Ok(t.used)
    }

    /// Zet `[lo, hi)` er als Normal-NC bij, met de MMU aan: de entries gaan
    /// NC het geheugen in, dan de TLB's van elke core van deze app weg
    /// (een SMP-app deelt de tabellen).
    pub(super) fn add_nc(used: usize, lo: u64, hi: u64) -> Result<usize, MmuError> {
        let mut t = Tables::resume(UnderImage(LINK_BASE), LINK_BASE, used)?;
        let r = t.map(lo, hi, ATTR_GLASS);
        // SAFETY: alleen barrières en een TLB-invalidatie van de eigen
        // EL1-vertalingen (deze VMID): de tabellen zelf zijn net geschreven
        // en blijven geldig, dus elke core vertaalt hierna hooguit opnieuw.
        unsafe {
            asm!(
                "dsb ishst",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags)
            );
        }
        r.map(|()| t.used)
    }

    /// (TTBR0, TCR, MAIR, VBAR) van deze core.
    pub(super) fn regime() -> (u64, u64, u64, u64) {
        let (ttbr, tcr, mair, vbar): (u64, u64, u64, u64);
        // SAFETY: vier leesacties van systeemregisters zonder bijwerkingen.
        unsafe {
            asm!(
                "mrs {0}, ttbr0_el1",
                "mrs {1}, tcr_el1",
                "mrs {2}, mair_el1",
                "mrs {3}, vbar_el1",
                out(reg) ttbr,
                out(reg) tcr,
                out(reg) mair,
                out(reg) vbar,
                options(nomem, nostack, preserves_flags)
            );
        }
        // Zonder stage-1 (TTBR0 nog 0 of M uit) geeft de primaire geen
        // tabellen mee: de secundaire loopt dan ook zonder MMU.
        let sctlr: u64;
        // SAFETY: een lees van een systeemregister zonder bijwerkingen.
        unsafe {
            asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack, preserves_flags))
        };
        if sctlr & 1 == 0 {
            return (0, 0, 0, vbar);
        }
        (ttbr, tcr, mair, vbar)
    }

    // De vectortabel van EL1. Een app heeft zijn interrupts dicht, dus wat
    // hier landt is een fault van de app zelf: een alignment-fault, een
    // ongedefinieerde instructie, een stage-1-vertaalfout (een stack die
    // onder zijn RAM loopt). Zestien ingangen, elk met zijn index in x9,
    // naar één afhandeling die GEEN stack gebruikt (die kan juist de
    // oorzaak zijn): de control-page uit de patch-woorden (de staart begint
    // op RamStart + RamSize), ESR/ELR/FAR en de index erop, exitcode
    // EXIT_APP_FAULT en status EXITED, dan HVC #0. De kern drukt het
    // rapport (HOPOS_HOP_FAULT, HOPOS_SLOT_FAULT). De registers van de app
    // zijn daarna waardeloos; hij keert niet terug.
    //
    // De stores gaan naar de control-page, die in de map Normal-NC is (en
    // met de MMU uit Device): ze staan meteen in het geheugen, zonder
    // cache-onderhoud. Een fault in deze afhandeling zelf (een kapotte
    // patch) is een stage-2-fault en die meldt EL2 zoals voorheen.
    global_asm!(
        ".pushsection .text.applib_vectors, \"ax\"",
        ".balign 2048",
        ".global __applib_vectors",
        "__applib_vectors:",
        ".irp i, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15",
        ".balign 128",
        "    mov x9, #\\i",
        "    b __applib_fault",
        ".endr",
        "__applib_fault:",
        "    adrp x10, \"runtime/goos.RamStart\"",
        "    ldr x10, [x10, :lo12:\"runtime/goos.RamStart\"]",
        "    adrp x11, \"runtime/goos.RamSize\"",
        "    ldr x11, [x11, :lo12:\"runtime/goos.RamSize\"]",
        "    add x10, x10, x11",
        "    mrs x11, esr_el1",
        "    str x11, [x10, #{esr}]",
        "    mrs x11, elr_el1",
        "    str x11, [x10, #{elr}]",
        "    mrs x11, far_el1",
        "    str x11, [x10, #{far}]",
        "    add x9, x9, #1",
        "    str x9, [x10, #{vec}]",
        "    mov x11, #{code}",
        "    str x11, [x10, #{exit_code}]",
        "    dsb sy",
        "    mov x11, #{exited}",
        "    str x11, [x10, #{status}]",
        "    dsb sy",
        "1:  hvc #0",
        "    b 1b",
        ".popsection",
        esr = const CTRL_APP_FAULT_ESR,
        elr = const CTRL_APP_FAULT_ELR,
        far = const CTRL_APP_FAULT_FAR,
        vec = const CTRL_APP_FAULT_VEC,
        code = const EXIT_APP_FAULT,
        exit_code = const CTRL_EXIT_CODE,
        exited = const AppStatus::Exited as u64,
        status = const CTRL_STATUS,
    );

    // De tabelruimte is precies wat de ABI openhoudt.
    const _: () = assert!(LINK_TEXT_OFF == super::TABLE_PAGES as u64 * PAGE);
    // De offsets passen in het directe veld van `str` (een 8-voud onder
    // 32 KB).
    const _: () = assert!(CTRL_APP_FAULT_VEC < 32760 && CTRL_APP_FAULT_FAR.is_multiple_of(8));
    const _: () = assert!(EXIT_APP_FAULT <= 0xffff);
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod hw {
    //! Elders geen ARM-stage-1: op RISC-V houdt de PMP-kooi de app, op de
    //! host is er niets.

    use super::{MmuError, Plan};

    /// Er is geen ARM-stage-1.
    pub(super) const PRESENT: bool = false;

    pub(super) fn build(_p: &Plan) -> Result<usize, MmuError> {
        Err(MmuError::Off)
    }

    pub(super) fn add_nc(_used: usize, _lo: u64, _hi: u64) -> Result<usize, MmuError> {
        Err(MmuError::Off)
    }

    pub(super) fn regime() -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }
}

/// Het attribuut van het glas: Normal-NC, zoals de tabellen en de
/// control-page.
#[cfg(any(test, all(target_os = "none", target_arch = "aarch64")))]
const ATTR_GLASS: u64 = tables::ATTR_NC;

/// De registerwaarden van de MMU-aan, voor `_start` (rt.rs).
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub(crate) use hw::{MAIR, TCR};

#[cfg(test)]
mod tests;
