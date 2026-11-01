//! De GICv3-interruptcontroller: distributor, redistributor, claim en EOI,
//! als [`cpu::irq::Controller`].
//!
//! Eén driver voor elk ARM-board met zo'n GIC: QEMU virt, de Radxa
//! (GIC-600), de Altra, de Orion O6N (GIC-700). De Pi's hebben een GIC-400
//! (v2) en krijgen hun eigen driver; de distributor delen ze ([`dist`]).
//!
//! Group 1, niet Group 0 (09-09): op élk bord met TF-A op EL3 staat
//! GICD_CTLR.DS=0 en is Group 0 het secure domein; voor ons (non-secure
//! EL2) zijn de Group-0-registers RAZ/WI en meldt ICC_IAR0 nooit iets.
//! Non-secure Group 1 is wat de firmware voor het OS overlaat, en het is
//! ook op een GIC zonder security (QEMU: DS=1) gewoon beschikbaar. Dus:
//! IGROUPR-bit gezet, EnableGrp1NS in GICD_CTLR, ICC_IGRPEN1 en IAR1/EOIR1.
//!
//! Wat deze driver bewust NIET doet: alle interrupts van de distributor
//! uitzetten bij `init` (tamago's gebruik). Op een bord met firmware-eigen
//! lijnen (SCP, EC) is dat niet ons register; wij raken alleen de lijnen
//! aan die wij aanzetten. En de route van een SPI gaat expliciet naar déze
//! core, met IRM=0: een app-core mag nooit een doel zijn. Dat is de
//! isolatieregel, niet "meestal".
//!
//! De CPU-interface (de ICC-systeemregisters) is een instructie, geen
//! MMIO, en assembly hoort in `cpu` en `board` (handboek §5): de driver
//! krijgt hem via de trait [`Icc`], met [`SysRegIcc`] (de instructies van
//! `cpu::gicv3`) op ijzer. Zo test de driver op de host met een
//! nep-interface.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use core::fmt;
use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use cpu::irq::{Controller, Line};
use dev::{Pa, Reg};
use dist::PRIORITY;
pub use dist::{FIRST_SPECIAL, FIRST_SPI, SGI_COUNT};

pub mod dist;
pub mod its;

/// De SGI's die de niet-beveiligde wereld heeft: 0..=7. ARM raadt aan
/// 8..=15 voor de secure wereld te houden, en TF-A doet dat
/// (`ARM_IRQ_SEC_SGI_0` = 8 tot en met `ARM_IRQ_SEC_SGI_7` = 15, als Group 0
/// of Secure Group 1; op Rockchip `RK_IRQ_SEC_SGI_*`). Een niet-beveiligde
/// schrijf naar ICC_SGI1R_EL1 voor zo'n SGI stuurt niets, en zijn bits in
/// GICR_IGROUPR0, ISENABLER0 en ISPENDR0 zijn voor ons RAZ/WI: de kick
/// verdwijnt stil. Linux gebruikt om dezelfde reden alleen 0..=7 voor zijn
/// IPI's. De Go-generatie wist het al (`KickSGI = 1`, 09-09); de Rust-kern
/// had tot 30-09 SGI 8 op de UEFI-boards, en de O6N viel op de kick van de
/// zelftest terug op de timer.
pub const NS_SGI_COUNT: u32 = 8;

/// Is SGI `id` er een die de niet-beveiligde wereld mag sturen en
/// ontvangen (zie [`NS_SGI_COUNT`])?
#[must_use]
pub const fn is_ns_sgi(id: u32) -> bool {
    id < NS_SGI_COUNT
}

/// De redistributor-frames: RD_base (64 KB) + SGI_base (64 KB) = 128 KB;
/// bij VLPIS (GICv4) nog VLPI_base en een gereserveerd frame erbij.
const FRAME: u64 = 0x1_0000;

/// De affiniteitsbits van MPIDR voor IROUTER: aff0-2 en aff3.
const AFF_MASK: u64 = (0xff << 32) | 0xff_ffff;

/// Het distributor-blok (ARM IHI 0069, tabel 12-25): het gedeelde deel
/// ([`dist::Gicd`]) en IROUTER.
#[repr(C)]
struct Gicd {
    d: dist::Gicd,
    _r: [u32; 5312],
    /// IROUTER, geïndexeerd op INTID (de eerste 32 zijn gereserveerd): de
    /// INTID-basis ligt op 0x6000, IROUTER[32] op 0x6100.
    irouter: [Reg<u64>; 1020],
}

const _: () = assert!(offset_of!(Gicd, irouter) == 0x6000);

/// Eén redistributor: het RD_base-frame en het SGI_base-frame erachter
/// (ARM IHI 0069, tabellen 12-27 en 12-29).
#[repr(C)]
struct Gicr {
    ctlr: Reg<u32>,
    iidr: Reg<u32>,
    /// 64 bit: bit 4 Last, bit 1 VLPIS, [63:32] de affiniteit van de PE.
    typer: Reg<u64>,
    _r0: u32,
    waker: Reg<u32>,
    _r1: [u32; 22],
    /// De LPI-configuratietabel (1 byte per LPI): adres, cache, IDbits.
    propbaser: Reg<u64>,
    /// De LPI-pending-tabel (1 bit per INTID), 64 KB-gealigneerd.
    pendbaser: Reg<u64>,
    _r2: [u32; 16352],
    // SGI_base, op +0x10000.
    _s0: [u32; 32],
    igroupr0: Reg<u32>,
    _s1: [u32; 31],
    isenabler0: Reg<u32>,
    _s2: [u32; 31],
    icenabler0: Reg<u32>,
    _s3: [u32; 31],
    ispendr0: Reg<u32>,
    _s4: [u32; 31],
    icpendr0: Reg<u32>,
    _s5: [u32; 95],
    ipriorityr: [Reg<u8>; 32],
    _s6: [u32; 504],
    icfgr: [Reg<u32>; 2],
}

const _: () = {
    assert!(offset_of!(Gicr, ctlr) == 0x0000);
    assert!(offset_of!(Gicr, iidr) == 0x0004);
    assert!(offset_of!(Gicr, typer) == 0x0008);
    assert!(offset_of!(Gicr, waker) == 0x0014);
    assert!(offset_of!(Gicr, propbaser) == 0x0070);
    assert!(offset_of!(Gicr, pendbaser) == 0x0078);
    assert!(offset_of!(Gicr, igroupr0) == 0x1_0080);
    assert!(offset_of!(Gicr, isenabler0) == 0x1_0100);
    assert!(offset_of!(Gicr, icenabler0) == 0x1_0180);
    assert!(offset_of!(Gicr, ispendr0) == 0x1_0200);
    assert!(offset_of!(Gicr, icpendr0) == 0x1_0280);
    assert!(offset_of!(Gicr, ipriorityr) == 0x1_0400);
    assert!(offset_of!(Gicr, icfgr) == 0x1_0c00);
};

/// GICD_CTLR: affinity routing (NS-view ARE_NS; DS=1: ARE).
const CTLR_ARE_NS: u32 = 1 << 4;
/// GICD_CTLR: Group 1 aan (NS-view EnableGrp1A; DS=1: EnableGrp1).
const CTLR_ENABLE_GRP1_NS: u32 = 1 << 1;
/// GICD_TYPER: de distributor kent LPI's.
const TYPER_LPIS: u32 = 1 << 17;
/// GICR_TYPER: deze redistributor kan LPI's (PLPIS).
const RTYPER_PLPIS: u64 = 1;
/// GICR_CTLR: LPI's aan. Eenmaal gezet is uitzetten IMPLEMENTATION
/// DEFINED; daarom de toets bij [`Gic::enable_lpis`].
const RCTLR_ENABLE_LPIS: u32 = 1;
/// GICR_PENDBASER: de pending-tabel is nul (PTZ): de GIC hoeft hem niet te
/// lezen.
const PENDBASER_PTZ: u64 = 1 << 62;
/// PROPBASER/PENDBASER InnerCache [9:7] = 0b001: Normal non-cacheable, want
/// de tabellen staan in de NC-gemapte DMA-regio van het board. Zo zien de
/// GIC en wij hetzelfde zonder cache-onderhoud, ook op een GIC die niet
/// coherent aan de caches hangt.
const BASER_INNER_NC: u64 = 1 << 7;
/// De adresbits [51:12] van PROPBASER.
const PROP_ADDR: u64 = 0x000f_ffff_ffff_f000;
/// De eerste LPI (INTID 8192).
pub const FIRST_LPI: u32 = 8192;
/// De INTID-breedte die wij de LPI's geven: 14 bits, dus INTID 8192 tot en
/// met 16383. Het minimum dat LPI's toelaat, en een configuratietabel van
/// 8 KB en een pending-tabel van 2 KB; HopOS bedient een handvol devices.
pub const LPI_ID_BITS: u32 = 14;
/// De maat van de LPI-configuratietabel bij [`LPI_ID_BITS`].
pub const LPI_PROP_LEN: u64 = (1 << LPI_ID_BITS) - FIRST_LPI as u64;
/// De maat van de pending-tabel bij [`LPI_ID_BITS`] (een bit per INTID,
/// vanaf 0).
pub const LPI_PEND_LEN: u64 = (1 << LPI_ID_BITS) / 8;
/// GICR_WAKER: de core slaapt.
const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
/// GICR_WAKER: de redistributor slaapt nog.
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// GICR_TYPER: het laatste frame van de reeks.
const TYPER_LAST: u64 = 1 << 4;
/// GICR_TYPER: GICv4-frames (vier in plaats van twee).
const TYPER_VLPIS: u64 = 1 << 1;
/// De poll-grens op GICR_WAKER. Op DS=0 is WAKER secure-only en RAZ/WI:
/// dan is hij al wakker, want de firmware bracht deze core op.
const WAKER_POLLS: u32 = 1 << 16;

/// De CPU-interface: de ICC-systeemregisters van de Group-1-set.
pub trait Icc {
    /// Zet de systeemregister-interface aan (ICC_SRE_EL2 en _EL1: SRE, en
    /// op EL2 ook Enable).
    fn enable_sre(&self);
    /// Zet het prioriteitsmasker (ICC_PMR_EL1).
    fn set_pmr(&self, pmr: u8);
    /// Zet Group 1 aan of uit (ICC_IGRPEN1_EL1).
    fn set_grp1(&self, on: bool);
    /// Leest ICC_IAR1_EL1: de claim.
    fn iar1(&self) -> u32;
    /// Schrijft ICC_EOIR1_EL1: priority drop én deactivate (EOImode 0).
    fn eoir1(&self, intid: u32);
}

/// De CPU-interface als systeemregisters ([`cpu::gicv3`]): de [`Icc`] van
/// elk board met een GICv3.
pub struct SysRegIcc;

impl Icc for SysRegIcc {
    fn enable_sre(&self) {
        cpu::gicv3::enable_sre();
    }
    fn set_pmr(&self, pmr: u8) {
        cpu::gicv3::set_pmr(pmr);
    }
    fn set_grp1(&self, on: bool) {
        cpu::gicv3::set_grp1(on);
    }
    fn iar1(&self) -> u32 {
        cpu::gicv3::iar1()
    }
    fn eoir1(&self, intid: u32) {
        cpu::gicv3::eoir1(intid);
    }
}

/// Waarom de GIC iets weigert.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Geen geldige INTID voor deze operatie.
    BadIntId(u32),
    /// De redistributor bleef slapen (GICR_WAKER).
    Asleep,
    /// Distributor of redistributor kent geen LPI's (GICD_TYPER.LPIS,
    /// GICR_TYPER.PLPIS).
    NoLpis,
    /// De distributor kan minder INTID-bits dan [`LPI_ID_BITS`].
    LpiBits {
        /// Wat GICD_TYPER.IDbits zegt.
        have: u32,
    },
    /// LPI's stonden al aan met een tabel die niet de onze is. Terugzetten
    /// kan niet (EnableLPIs is eenmaal gezet vaak blijvend), dus we laten
    /// hem staan en pollen.
    LpisTaken {
        /// Het adres in GICR_PROPBASER.
        at: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadIntId(id) => write!(f, "gicv3: invalid interrupt {id}"),
            Self::Asleep => f.write_str("gicv3: redistributor stays asleep (WAKER)"),
            Self::NoLpis => f.write_str("gicv3: no LPI support (GICD_TYPER.LPIS/GICR_TYPER.PLPIS)"),
            Self::LpiBits { have } => write!(
                f,
                "gicv3: distributor supports {have} INTID bits, LPIs need {LPI_ID_BITS}"
            ),
            Self::LpisTaken { at } => write!(
                f,
                "gicv3: LPIs already enabled with a property table at {at:#x} that is not ours"
            ),
        }
    }
}

/// Eén GICv3, voor de core die hem opzet.
///
/// Het redistributor-frame is dat van de core van de kern, en die kan bij
/// boot verhuizen (de OS-core, PORT.md beslissing 2): daarom een atomic die
/// [`Gic::set_redistributor`] één keer bijstelt, vóór [`Gic::init`].
pub struct Gic<I: Icc> {
    gicd: Pa,
    gicr: AtomicU64,
    icc: I,
}

impl<I: Icc> Gic<I> {
    /// Een GIC met de distributor op `gicd` en het redistributor-frame
    /// (RD_base) van DEZE core op `gicr` (zie [`find_redistributor`]).
    ///
    /// # Safety
    ///
    /// `gicd` en `gicr` zijn gemapte GICv3-blokken (64 KB distributor,
    /// 128 KB redistributor) die zolang het programma draait blijven.
    #[must_use]
    pub const unsafe fn new(gicd: Pa, gicr: Pa, icc: I) -> Self {
        Self {
            gicd,
            gicr: AtomicU64::new(gicr.0),
            icc,
        }
    }

    /// Zet het redistributor-frame op dat van de core die de GIC bedient
    /// (zie [`find_redistributor`]), vóór [`Gic::init`].
    ///
    /// # Safety
    ///
    /// `gicr` is een gemapt GICv3-redistributorframe van 128 KB dat zolang
    /// het programma draait blijft, zoals bij [`Gic::new`].
    pub unsafe fn set_redistributor(&self, gicr: Pa) {
        self.gicr.store(gicr.0, Relaxed);
    }

    fn gicr(&self) -> Pa {
        Pa(self.gicr.load(Relaxed))
    }

    fn d(&self) -> &'static Gicd {
        // SAFETY: de voorwaarde van `new`.
        unsafe { dev::regs(self.gicd) }
    }

    fn r(&self) -> &'static Gicr {
        // SAFETY: de voorwaarde van `new` en `set_redistributor`.
        unsafe { dev::regs(self.gicr()) }
    }

    /// Zet de GIC op voor deze core: de redistributor wakker, de
    /// CPU-interface aan (SRE, PMR 0xff, Group 1), en in de distributor
    /// affinity routing en Group 1 aan zonder de andere bits (Group 0 is
    /// van de firmware) aan te raken. Idempotent.
    pub fn init(&self) -> Result<(), Error> {
        let r = self.r();
        r.waker.update(|w| w & !WAKER_PROCESSOR_SLEEP);
        let mut polls = 0;
        while r.waker.read() & WAKER_CHILDREN_ASLEEP != 0 {
            polls += 1;
            if polls > WAKER_POLLS {
                return Err(Error::Asleep);
            }
        }
        self.icc.enable_sre();
        self.icc.set_pmr(0xff);
        self.icc.set_grp1(true);
        self.d()
            .d
            .ctlr
            .update(|c| c | CTLR_ARE_NS | CTLR_ENABLE_GRP1_NS);
        dev::mb();
        Ok(())
    }

    /// Zet lijn `id` aan: route (SPI) naar de core met `mpidr`, Group 1,
    /// onze prioriteit, en dan pas enable. Route en groep staan vóór de
    /// enable, anders kan de lijn één keer verkeerd afgaan. De
    /// [`Controller::enable`] van de dispatcher is dit met de MPIDR van de
    /// aanroepende core.
    pub fn enable(&self, id: u32, mpidr: u64) -> Result<(), Error> {
        if id >= FIRST_SPECIAL {
            return Err(Error::BadIntId(id));
        }
        let bit = 1u32 << (id % 32);
        if id < FIRST_SPI {
            let r = self.r();
            r.igroupr0.update(|g| g | bit);
            r.ipriorityr[id as usize].write(PRIORITY);
            dev::mb();
            r.isenabler0.write(bit);
            dev::mb();
        } else {
            let d = self.d();
            d.irouter[id as usize].write(mpidr & AFF_MASK);
            d.d.igroupr[(id / 32) as usize].update(|g| g | bit);
            d.d.enable(id);
        }
        Ok(())
    }

    /// Zet de LPI's aan op de redistributor van deze core: de
    /// configuratietabel op `prop` ([`LPI_PROP_LEN`] bytes, 4 KB-gealigneerd)
    /// en de pending-tabel op `pend` ([`LPI_PEND_LEN`] bytes,
    /// 64 KB-gealigneerd), beide nul en Normal-NC. Geeft `true` als de LPI's
    /// al aan stonden met precies deze tabellen (een kern na een flip op
    /// hetzelfde venster): dan hergebruiken we ze.
    ///
    /// EnableLPIs weer uitzetten is IMPLEMENTATION DEFINED, en PROPBASER
    /// wijzigen terwijl hij aan staat UNPREDICTABLE (GICv3 §12.11.2). Daarom
    /// geen poging: andermans tabel is [`Error::LpisTaken`] en de devices
    /// pollen.
    pub fn enable_lpis(&self, prop: Pa, pend: Pa) -> Result<bool, Error> {
        let r = self.r();
        let dtyper = self.d().d.typer.read();
        if dtyper & TYPER_LPIS == 0 || r.typer.read() & RTYPER_PLPIS == 0 {
            return Err(Error::NoLpis);
        }
        let bits = ((dtyper >> 19) & 0x1f) + 1;
        if bits < LPI_ID_BITS {
            return Err(Error::LpiBits { have: bits });
        }
        if r.ctlr.read() & RCTLR_ENABLE_LPIS != 0 {
            let at = r.propbaser.read() & PROP_ADDR;
            return if at == prop.0 {
                Ok(true)
            } else {
                Err(Error::LpisTaken { at })
            };
        }
        r.propbaser
            .write((prop.0 & PROP_ADDR) | BASER_INNER_NC | u64::from(LPI_ID_BITS - 1));
        r.pendbaser
            .write((pend.0 & 0x000f_ffff_ffff_0000) | BASER_INNER_NC | PENDBASER_PTZ);
        dev::mb();
        r.ctlr.update(|c| c | RCTLR_ENABLE_LPIS);
        dev::mb();
        Ok(false)
    }

    /// Staan de LPI's op deze redistributor al aan (een vorige kern, of
    /// firmware)?
    #[must_use]
    pub fn lpis_enabled(&self) -> bool {
        self.r().ctlr.read() & RCTLR_ENABLE_LPIS != 0
    }

    /// Het fysieke adres van het RD_base-frame van deze core (voor een
    /// ITS met PTA = 1) en zijn GICR_TYPER (het processornummer voor een
    /// ITS met PTA = 0).
    #[must_use]
    pub fn redistributor(&self) -> (Pa, u64) {
        (self.gicr(), self.r().typer.read())
    }

    /// Wat de redistributor van deze core over SGI of PPI `id` zegt: de
    /// ruwe GICR_IGROUPR0, ISENABLER0 en ISPENDR0. `None` voor een SPI of
    /// hoger, die niet in de redistributor wonen.
    ///
    /// Waarom de ruwe woorden en niet drie bits: een niet-beveiligde lezer
    /// ziet de bits van een secure lijn als 0 (RAZ). Staat de groep van
    /// een lijn die wij net in Group 1 zetten op 0, dan is hij niet van ons
    /// (O6N 30-09, SGI 8), en het hele woord laat zien welke het wel zijn.
    #[must_use]
    pub fn local(&self, id: u32) -> Option<Local> {
        if id >= FIRST_SPI {
            return None;
        }
        let r = self.r();
        Some(Local {
            id,
            igroupr0: r.igroupr0.read(),
            isenabler0: r.isenabler0.read(),
            ispendr0: r.ispendr0.read(),
        })
    }

    /// De IIDR's en CTLR: de meting dat we met de goede blokken praten
    /// (GIC-700: GICD_IIDR 0x0402143b).
    #[must_use]
    pub fn describe(&self) -> Describe {
        Describe {
            gicd: self.gicd,
            gicd_iidr: self.d().d.iidr.read(),
            gicd_ctlr: self.d().d.ctlr.read(),
            gicr: self.gicr(),
            gicr_iidr: self.r().iidr.read(),
            gicr_typer: self.r().typer.read(),
        }
    }
}

/// De GICv3 als controller van `cpu::irq`. De soort van een lijn
/// (`set_trigger`) blijft zoals de firmware hem zette: geen v3-board
/// registreert een flank, en een LPI is er altijd een.
impl<I: Icc> Controller for Gic<I> {
    /// [`Gic::enable`] naar de aanroepende core.
    fn enable(&self, l: Line) -> Result<(), cpu::irq::Error> {
        Gic::enable(self, l.0, cpu::mpidr()).map_err(|_| cpu::irq::Error::Rejected { line: l.0 })
    }

    /// Zet lijn `l` uit: een SGI of PPI in de redistributor, een SPI in
    /// de distributor. Een LPI gaat uit in de tabel van zijn ITS, niet
    /// hier.
    fn disable(&self, l: Line) {
        let id = l.0;
        if id >= FIRST_SPECIAL {
            return;
        }
        if id < FIRST_SPI {
            self.r().icenabler0.write(1 << (id % 32));
            dev::mb();
        } else {
            self.d().d.disable(id);
        }
    }

    /// Claimt de hoogste wachtende interrupt (ICC_IAR1); `None` bij een
    /// speciale INTID (1020 tot en met 1023: niets te doen). Een LPI
    /// (8192 en hoger) is een gewone claim: tot 29-09 stond hier `id <
    /// 1020`, en dat las elke MSI als "niets".
    fn claim(&self) -> Option<Line> {
        let id = self.icc.iar1() & 0xff_ffff;
        (!(FIRST_SPECIAL..FIRST_SPECIAL + 4).contains(&id)).then_some(Line(id))
    }

    /// Sluit een geclaimde interrupt af (ICC_EOIR1: priority drop én
    /// deactivate).
    fn complete(&self, l: Line) {
        self.icc.eoir1(l.0);
    }
}

/// De redistributor-kant van één SGI of PPI ([`Gic::local`]): de drie
/// woorden van het SGI_base-frame waar zijn bit in staat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
    /// De INTID (0..=31).
    pub id: u32,
    /// GICR_IGROUPR0: 1 = Group 1 (niet-beveiligd), 0 = Group 0 of secure.
    pub igroupr0: u32,
    /// GICR_ISENABLER0: 1 = aan.
    pub isenabler0: u32,
    /// GICR_ISPENDR0: 1 = pending.
    pub ispendr0: u32,
}

impl Local {
    fn bit(&self) -> u32 {
        1 << (self.id % 32)
    }

    /// Is de lijn van ons: staat hij aan? Een secure lijn is RAZ/WI, dus de
    /// enable van [`Gic::enable`] blijft dan 0. De groepsbit telt bewust
    /// niet mee: op de Cix-firmware van de O6N leest GICR_IGROUPR0 als nul
    /// terwijl SGI 1 wel aankomt (de zelftest zag `kick=(Ipi, 0 us)`,
    /// 30-09); de zelftest is de rechter, dit is de voorspelling.
    #[must_use]
    pub fn is_ours(&self) -> bool {
        self.isenabler0 & self.bit() != 0
    }

    /// Staat de lijn pending?
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.ispendr0 & self.bit() != 0
    }
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GICR_IGROUPR0 {:#010x} ISENABLER0 {:#010x} ISPENDR0 {:#010x}",
            self.igroupr0, self.isenabler0, self.ispendr0
        )
    }
}

/// Eén regel voor de bootlog.
#[derive(Debug, Clone, Copy)]
pub struct Describe {
    gicd: Pa,
    gicd_iidr: u32,
    gicd_ctlr: u32,
    gicr: Pa,
    gicr_iidr: u32,
    gicr_typer: u64,
}

impl fmt::Display for Describe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GICD {:#x} (IIDR {:#x}, CTLR {:#x}), GICR {:#x} (IIDR {:#x}, TYPER {:#x})",
            self.gicd.0,
            self.gicd_iidr,
            self.gicd_ctlr,
            self.gicr.0,
            self.gicr_iidr,
            self.gicr_typer
        )
    }
}

/// De ICC_SGI1R_EL1-waarde die SGI `intid` naar precies de core met
/// affiniteit `mpidr` stuurt: affinity routing met IRM = 0, TargetList-bit
/// aff0 % 16, RS = aff0 / 16, en Aff1..Aff3 op hun plek (ARM IHI 0069,
/// ICC_SGI1R_EL1).
///
/// Een waarde en geen schrijfactie: de zender is de EL2-switcher van een
/// app-core (de kick naar de OS-core, PORT.md beslissing 2), die het woord
/// uit het sched-blok leest en zelf schrijft. Zo raakt een app nooit een
/// GIC-register: op EL1 trapt ICC_SGI1R toch naar EL2 zodra FMO of IMO
/// staat.
#[must_use]
pub const fn sgi1r(mpidr: u64, intid: u32) -> u64 {
    let aff0 = mpidr & 0xff;
    let aff1 = (mpidr >> 8) & 0xff;
    let aff2 = (mpidr >> 16) & 0xff;
    let aff3 = (mpidr >> 32) & 0xff;
    (1 << (aff0 % 16))
        | (aff1 << 16)
        | (((intid % SGI_COUNT) as u64) << 24)
        | (aff2 << 32)
        | ((aff0 / 16) << 44)
        | (aff3 << 48)
}

/// Zoekt in de GICR-reeks `[base, base+len)` het RD_base-frame van de core
/// met dit MPIDR: elk frame draagt in GICR_TYPER[63:32] de affiniteit van
/// zijn PE, en is 128 KB (v3) of 256 KB (v4, VLPIS) groot. Stopt op
/// TYPER.Last of het eind van de reeks.
///
/// GICR_TYPER staat op +0x8; +0 is CTLR|IIDR en matchte op QEMU alleen per
/// ongeluk (core 0 = affiniteit 0, IIDR hoog woord 0). O6N 09-09: "EE".
///
/// # Safety
///
/// `[base, base+len)` is een gemapte GICR-reeks.
#[must_use]
pub unsafe fn find_redistributor(base: Pa, len: u64, mpidr: u64) -> Option<Pa> {
    let want = ((mpidr & 0xff_ffff) | ((mpidr >> 32 & 0xff) << 24)) as u32;
    let mut off = 0u64;
    while off + FRAME <= len {
        let typer = dev::read64(base.add(off + 8));
        if (typer >> 32) as u32 == want {
            return Some(base.add(off));
        }
        if typer & TYPER_LAST != 0 {
            break;
        }
        // v3: RD + SGI = 2 frames; v4 (VLPIS): RD + SGI + VLPI + reserved =
        // 4 frames (GICv3/4 §12.10). Tot 17-09 stond hier 3 frames voor v4:
        // op de GIC-700 van de O6N liep de zoeker dan langs de frames heen
        // ("no redistributor for MPIDR 0xa00") en bleef de NIC gepold.
        off += if typer & TYPER_VLPIS != 0 {
            4 * FRAME
        } else {
            2 * FRAME
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct FakeIcc {
        sre: Cell<bool>,
        pmr: Cell<u8>,
        grp1: Cell<bool>,
        pending: RefCell<Vec<u32>>,
        eoi: RefCell<Vec<u32>>,
    }

    impl Icc for &FakeIcc {
        fn enable_sre(&self) {
            self.sre.set(true);
        }
        fn set_pmr(&self, pmr: u8) {
            self.pmr.set(pmr);
        }
        fn set_grp1(&self, on: bool) {
            self.grp1.set(on);
        }
        fn iar1(&self) -> u32 {
            self.pending.borrow_mut().pop().unwrap_or(1023)
        }
        fn eoir1(&self, intid: u32) {
            self.eoi.borrow_mut().push(intid);
        }
    }

    /// Nep-geheugen voor een distributor (0x8000) en een redistributor
    /// (0x20000), als u64 zodat 8-byte-registers gealigneerd zijn.
    fn mem() -> (Vec<u64>, Vec<u64>) {
        (vec![0; 0x8000 / 8], vec![0; 0x2_0000 / 8])
    }

    fn pa(v: &mut [u64]) -> Pa {
        Pa(v.as_mut_ptr() as usize as u64)
    }

    #[test]
    fn spi_routes_its_own_intid_and_joins_group_1() {
        let (mut d, mut r) = mem();
        let icc = FakeIcc::default();
        // SAFETY: de vectoren leven de hele test en zijn groot genoeg.
        let gic = unsafe { Gic::new(pa(&mut d), pa(&mut r), &icc) };
        for id in [32, 48, 1019] {
            gic.enable(id, 0x12_8000_0003).unwrap();
            assert_eq!(
                dev::read64(pa(&mut d).add(0x6000 + 8 * u64::from(id))),
                0x12_0000_0003,
                "id {id}"
            );
        }
        // Een ongerelateerde route (SPI 80) blijft onaangeroerd.
        assert_eq!(dev::read64(pa(&mut d).add(0x6100 + 48 * 8)), 0);
        // INTID 48 = register 1, bit 16: Group 1 en enabled.
        assert_ne!(dev::read32(pa(&mut d).add(0x80 + 4)) & (1 << 16), 0);
        assert_ne!(dev::read32(pa(&mut d).add(0x100 + 4)) & (1 << 16), 0);
        assert_eq!(dev::read8(pa(&mut d).add(0x400 + 48)), PRIORITY);
        assert_eq!(gic.enable(1020, 0), Err(Error::BadIntId(1020)));
    }

    #[test]
    fn ppi_lives_in_the_redistributor_sgi_frame() {
        let (mut d, mut r) = mem();
        let icc = FakeIcc::default();
        // SAFETY: zie hierboven.
        let gic = unsafe { Gic::new(pa(&mut d), pa(&mut r), &icc) };
        gic.enable(30, 0).unwrap();
        assert_eq!(dev::read32(pa(&mut r).add(0x1_0080)), 1 << 30);
        assert_eq!(dev::read32(pa(&mut r).add(0x1_0100)), 1 << 30);
        assert_eq!(dev::read8(pa(&mut r).add(0x1_0400 + 30)), PRIORITY);
        gic.disable(Line(30));
        assert_eq!(dev::read32(pa(&mut r).add(0x1_0180)), 1 << 30);
        // De distributor is niet aangeraakt.
        assert!(d.iter().all(|&w| w == 0));
    }

    #[test]
    fn init_wakes_and_enables_and_claim_skips_specials() {
        let (mut d, mut r) = mem();
        dev::write32(pa(&mut r).add(0x14), WAKER_PROCESSOR_SLEEP);
        let icc = FakeIcc::default();
        // SAFETY: zie hierboven.
        let gic = unsafe { Gic::new(pa(&mut d), pa(&mut r), &icc) };
        gic.init().unwrap();
        assert_eq!(dev::read32(pa(&mut r).add(0x14)), 0);
        assert_eq!(dev::read32(pa(&mut d)), CTLR_ARE_NS | CTLR_ENABLE_GRP1_NS);
        assert!(icc.sre.get() && icc.grp1.get());
        assert_eq!(icc.pmr.get(), 0xff);
        icc.pending.borrow_mut().extend([1023, 30]);
        assert_eq!(gic.claim(), Some(Line(30)));
        gic.complete(Line(30));
        assert_eq!(gic.claim(), None);
        assert_eq!(*icc.eoi.borrow(), vec![30]);
    }

    /// Een nagebootste GICv3 achter de dispatcher van `cpu::irq`: de vorm
    /// van qemuvirt, rk3566 en uefi. Alles gelekt, want de dispatcher wil
    /// een `&'static dyn Controller`.
    fn dispatcher() -> (
        &'static cpu::irq::Dispatcher,
        &'static FakeIcc,
        &'static mut [u64],
        &'static mut [u64],
    ) {
        let d: &'static mut [u64] = Vec::leak(vec![0; 0x8000 / 8]);
        let r: &'static mut [u64] = Vec::leak(vec![0; 0x2_0000 / 8]);
        let icc: &'static FakeIcc = Box::leak(Box::default());
        // SAFETY: de gelekte vectoren leven de hele test en zijn groot genoeg.
        let gic: &'static Gic<&'static FakeIcc> =
            Box::leak(Box::new(unsafe { Gic::new(pa(d), pa(r), icc) }));
        let disp: &'static cpu::irq::Dispatcher = Box::leak(Box::default());
        disp.use_controller(gic);
        (disp, icc, d, r)
    }

    // De ronde van een v3-board: de timer-PPI (ack zet hem uit), de kick
    // (ack telt), de NIC-SPI (ack en bel) en een lijn die niemand kent
    // (uit in de distributor). Elke claim krijgt zijn EOI, en de ronde telt
    // per lijn wat het board in `Dispatched` zet.
    #[test]
    fn the_dispatcher_claims_acks_rings_and_eois_on_a_gicv3() {
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        static TIMER: AtomicU32 = AtomicU32::new(0);
        static KICK: AtomicU32 = AtomicU32::new(0);
        static NIC: sync::Signal = sync::Signal::new();
        let (disp, icc, d, r) = dispatcher();
        let timer: fn() = || {
            TIMER.fetch_add(1, SeqCst);
        };
        let kick: fn() = || {
            KICK.fetch_add(1, SeqCst);
        };
        disp.enable(Line(30), Some(timer), None).unwrap();
        disp.enable(Line(1), Some(kick), None).unwrap();
        disp.enable(Line(64), Some(|| {}), Some(&NIC)).unwrap();
        // PPI en SGI in de redistributor (de groep; ISENABLER0 is
        // write-1-to-set en het nep-blok bewaart alleen de laatste), de SPI
        // in de distributor met zijn route (de host-MPIDR is 0), groep en
        // enable.
        assert_eq!(dev::read32(pa(r).add(0x1_0080)), (1 << 30) | (1 << 1));
        assert_eq!(dev::read32(pa(r).add(0x1_0100)), 1 << 1);
        assert_eq!(dev::read64(pa(d).add(0x6000 + 8 * 64)), 0);
        assert_eq!(dev::read32(pa(d).add(0x80 + 4 * 2)), 1);
        assert_eq!(dev::read32(pa(d).add(0x100 + 4 * 2)), 1);
        assert_eq!(dev::read8(pa(d).add(0x400 + 64)), PRIORITY);
        // De FakeIcc claimt van achter naar voren.
        icc.pending.borrow_mut().extend([99, 64, 1, 30, 64]);
        let pass = disp.dispatch();
        assert_eq!(pass.claimed, 5);
        assert_eq!(
            (
                pass.claims(Line(30)),
                pass.claims(Line(1)),
                pass.claims(Line(64)),
                pass.unknown
            ),
            (1, 1, 2, 1)
        );
        assert_eq!((TIMER.load(SeqCst), KICK.load(SeqCst)), (1, 1));
        assert!(NIC.take());
        assert_eq!(*icc.eoi.borrow(), vec![64, 30, 1, 64, 99]);
        // SPI 99 is uit: ICENABLER[3] bit 3, en alleen die.
        assert_eq!(pass.disabled, Some(Line(99)));
        assert_eq!(dev::read32(pa(d).add(0x180 + 4 * 3)), 1 << 3);
        assert_eq!(dev::read32(pa(d).add(0x180 + 4 * 2)), 0);
        assert!(icc.pending.borrow().is_empty());
    }

    /// De console van de toetsen: alles wat `cpu::println!` schrijft.
    static CONSOLE: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

    /// Hoeveel regels van de console `needle` bevatten.
    fn console_lines(needle: &str) -> usize {
        cpu::console::set_sink(|b| CONSOLE.lock().unwrap().extend_from_slice(b));
        let out = CONSOLE.lock().unwrap();
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }

    // Een NIC onder last (een ack per claim) breekt de ronde af op
    // STRAY_LIMIT en blijft aan; een level-SPI zonder ack die blijft
    // terugkomen gaat uit in de distributor. Een LPI die niemand kent krijgt
    // zijn EOI, en de distributor blijft ongemoeid (een LPI woont in de
    // tabel van zijn ITS). De console krijgt per soort één regel, geen
    // regel per ronde.
    #[test]
    fn a_storm_on_a_gicv3_ends_the_pass_and_a_stuck_spi_goes_off() {
        use cpu::irq::STRAY_LIMIT;
        console_lines("");
        let (disp, icc, d, _r) = dispatcher();
        disp.enable(Line(64), Some(|| {}), None).unwrap();
        let n = STRAY_LIMIT as usize + 10;
        icc.pending.borrow_mut().extend(std::iter::repeat_n(64, n));
        let pass = disp.dispatch();
        assert_eq!(pass.stray, Some(Line(64)));
        assert_eq!(pass.stuck, None);
        assert_eq!(pass.claimed, STRAY_LIMIT + 1);
        assert_eq!(icc.eoi.borrow().len(), STRAY_LIMIT as usize + 1);
        assert_eq!(icc.pending.borrow().len(), 9);
        assert_eq!(dev::read32(pa(d).add(0x180 + 4 * 2)), 0);
        // De tweede storm op dezelfde lijn telt, maar zegt niets meer.
        icc.pending.borrow_mut().extend(std::iter::repeat_n(64, n));
        assert_eq!(disp.dispatch().stray, Some(Line(64)));
        assert_eq!(disp.stats.stray_passes.load(Relaxed), 2);
        let storm = format!("INTID 64 came back {STRAY_LIMIT} times in one pass, pass ended");
        assert_eq!(console_lines(&storm), 1);
        assert_eq!(console_lines("line stays enabled HOPOS_IRQ_STORM"), 1);

        let (disp, icc, d, _r) = dispatcher();
        disp.enable(Line(65), None, None).unwrap();
        icc.pending.borrow_mut().extend(std::iter::repeat_n(65, n));
        let pass = disp.dispatch();
        assert_eq!(pass.stuck, Some(Line(65)));
        assert_eq!(dev::read32(pa(d).add(0x180 + 4 * 2)), 1 << 1);
        assert_eq!(console_lines("INTID 65 came back"), 1);
        assert_eq!(console_lines("line disabled HOPOS_IRQ_STUCK"), 1);
        // Een storm die de lijn aan laat, ná een lijn die uitging: ook die
        // eerste komt op de console.
        disp.enable(Line(66), Some(|| {}), None).unwrap();
        icc.pending.borrow_mut().extend(std::iter::repeat_n(66, n));
        assert_eq!(disp.dispatch().stuck, None);
        assert_eq!(console_lines("INTID 66 came back"), 1);

        let (disp, icc, d, _r) = dispatcher();
        icc.pending.borrow_mut().push(FIRST_LPI + 8);
        let pass = disp.dispatch();
        assert_eq!((pass.claimed, pass.unknown), (1, 1));
        assert_eq!(*icc.eoi.borrow(), vec![FIRST_LPI + 8]);
        assert!(d.iter().all(|&w| w == 0));
        // Alleen de eerste verdwaalde lijn komt op de console.
        icc.pending.borrow_mut().push(FIRST_LPI + 9);
        assert_eq!(disp.dispatch().unknown, 1);
        assert_eq!(disp.stats.unknown.load(Relaxed), 2);
        assert_eq!(console_lines("INTID 8200 fired but nobody serves it"), 1);
        assert_eq!(console_lines("INTID 8201"), 0);
    }

    #[test]
    fn a_redistributor_that_never_wakes_is_an_error() {
        let (mut d, mut r) = mem();
        dev::write32(pa(&mut r).add(0x14), WAKER_CHILDREN_ASLEEP);
        let icc = FakeIcc::default();
        // SAFETY: zie hierboven.
        let gic = unsafe { Gic::new(pa(&mut d), pa(&mut r), &icc) };
        assert_eq!(gic.init(), Err(Error::Asleep));
    }

    #[test]
    fn sgi1r_aims_at_exactly_one_core() {
        // Core 0 van cluster 0, SGI 8: alleen TargetList-bit 0.
        assert_eq!(sgi1r(0, 8), (8 << 24) | 1);
        // QEMU virt core 1 (aff0 = 1).
        assert_eq!(sgi1r(1, 8), (8 << 24) | 2);
        // Aff1 = 0xa (de O6N-vorm 0xa00), aff0 = 0.
        assert_eq!(sgi1r(0x8000_0a00, 3), (3 << 24) | (0xa << 16) | 1);
        // Aff0 = 17: RS = 1, bit 1.
        assert_eq!(sgi1r(17, 0), (1 << 44) | 2);
        // Aff2 en Aff3.
        assert_eq!(
            sgi1r(0x12_0003_0000, 1),
            (1 << 24) | (3 << 32) | (0x12 << 48) | 1
        );
    }

    #[test]
    fn sgi1r_reaches_every_o6n_core() {
        // De Cix P1 van de O6N: twaalf cores, elk een eigen Aff1 met Aff0 =
        // 0, bit 31 (RES1) en MT (bit 24) gezet: MPIDR 0x8100_0000 | core
        // << 8 (de Go-regel van 17-09 zag "MPIDR 0xa00" voor core 10). Het
        // woord is dan TargetList-bit 0, Aff1 = de core, geen RS en geen
        // IRM, en de kick in bits 27:24; MT en bit 31 lekken nergens in.
        for core in 0..12u64 {
            let mpidr = 0x8100_0000 | (core << 8);
            let w = sgi1r(mpidr, 1);
            assert_eq!(w, (1 << 24) | (core << 16) | 1, "core {core}");
            assert_eq!(w >> 40 & 1, 0, "IRM staat uit, core {core}");
            assert_eq!(w >> 44 & 0xf, 0, "RS, core {core}");
        }
    }

    #[test]
    fn the_ns_sgis_are_the_low_eight() {
        assert!((0..8).all(is_ns_sgi));
        assert!(!(8..SGI_COUNT).any(is_ns_sgi));
    }

    #[test]
    fn local_reads_the_sgi_frame_and_sees_a_secure_line() {
        let (mut d, mut r) = mem();
        let icc = FakeIcc::default();
        // SAFETY: zie hierboven.
        let gic = unsafe { Gic::new(pa(&mut d), pa(&mut r), &icc) };
        gic.enable(1, 0).unwrap();
        let l = gic.local(1).unwrap();
        assert!(l.is_ours() && !l.is_pending());
        dev::write32(pa(&mut r).add(0x1_0200), 1 << 1);
        assert!(gic.local(1).unwrap().is_pending());
        // Een secure lijn: de NS-schrijf van `enable` valt weg (RAZ/WI),
        // hier nagebootst door de bits na de enable te wissen.
        gic.enable(8, 0).unwrap();
        dev::write32(pa(&mut r).add(0x1_0080), 1 << 1);
        dev::write32(pa(&mut r).add(0x1_0100), 1 << 1);
        let l = gic.local(8).unwrap();
        assert!(!l.is_ours());
        assert_eq!(
            l.to_string(),
            "GICR_IGROUPR0 0x00000002 ISENABLER0 0x00000002 ISPENDR0 0x00000002"
        );
        assert_eq!(gic.local(32), None);
    }

    #[test]
    fn find_redistributor_walks_v3_and_v4_frames() {
        // Vier v3-frames (128 KB elk): affiniteit 0, 1, 2, 3; de laatste
        // draagt Last.
        let mut v3 = vec![0u64; 4 * 0x2_0000 / 8];
        let base = pa(&mut v3);
        for i in 0..4u64 {
            let mut typer = i << 32;
            if i == 3 {
                typer |= TYPER_LAST;
            }
            dev::write64(base.add(i * 0x2_0000 + 8), typer);
        }
        let len = 4 * 0x2_0000;
        // SAFETY: de vector is de reeks.
        unsafe {
            assert_eq!(
                find_redistributor(base, len, 2),
                Some(base.add(2 * 0x2_0000))
            );
            assert_eq!(find_redistributor(base, len, 7), None);
        }
        // v4: 256 KB per frame; MPIDR 0xa00 (aff1 = 0xa) op het tweede.
        let mut v4 = vec![0u64; 2 * 0x4_0000 / 8];
        let base = pa(&mut v4);
        dev::write64(base.add(8), TYPER_VLPIS);
        dev::write64(
            base.add(0x4_0000 + 8),
            (0xa00 << 32) | TYPER_VLPIS | TYPER_LAST,
        );
        // SAFETY: zie hierboven.
        unsafe {
            assert_eq!(
                find_redistributor(base, 2 * 0x4_0000, 0x8000_0a00),
                Some(base.add(0x4_0000))
            );
        }
    }
}
