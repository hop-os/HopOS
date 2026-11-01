//! De Apple Interrupt Controller (AIC2/AIC3: de M2 en later, hier de t8132
//! van de Mac mini M4) achter het contract van `cpu::irq`.
//!
//! Registers en indeling zoals m1n1 (`src/aic.c`) ze uit de ADT afleidt: één
//! config-woord per IRQ vanaf `extint-baseaddress`, daarna SW_SET, SW_CLR,
//! MASK_SET, MASK_CLR en HW_STATE, elk `max_irq / 32` woorden; de
//! event-lees (tegelijk de ack) op `aic-iack-offset`. Wat dit blok bezit: de
//! offsets en het 4-bit doel. Wat niet: welke lijn van wie is (dat is de
//! dispatcher in `cpu::irq`), en het meten van het doel (dat doet het
//! board met [`Aic::soft_raise`] en ISR_EL1).
//!
//! Twee eigenschappen maken het verschil met een GIC:
//!
//! - de AIC maskeert een hardware-IRQ zelf zodra hij uit het
//!   event-register gelezen is; [`complete`](cpu::irq::Controller::complete)
//!   zet het masker weer open (MASK_CLR);
//! - het doel is een 4-bit "target" in het config-woord van de IRQ, en de
//!   firmware laat dat op 0 (niemand). Het board kiest het
//!   ([`Aic::set_target`]) na een meting met een software-IRQ, niet uit een
//!   aanname (Go `board/apple/hop/irq.go`, 19-09).
//!
//! De IPI's lopen op dit silicium niet over de AIC maar over de fast IPI
//! (`cpu::el2::kick` en `cpu::el2::apple_ipi_ack`): een systeemregister per
//! core, dat als FIQ aankomt. Dat is
//! m1n1's eigen wek-recept (`smp.c`: deep_wfi, IPI_RR_GLOBAL, IPI_SR-ack) en
//! Linux' AIC-driver bevestigt de vorm.

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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use cpu::irq::{Controller, Error as IrqError, Line};
use dev::Pa;

/// Het event-type van een hardware-IRQ (m1n1 `AIC_EVENT_TYPE_HW`).
pub const EVENT_TYPE_HW: u32 = 1;
/// Zo veel IRQ's per die hoogstens: `max_irq` is een veelvoud van 32 en past
/// in 16 bits (AIC3 op de M4 meldt er minder dan 4096).
pub const MAX_IRQS: u32 = 4096;
/// De bit in AIC_GLB_CFG die de controller aanzet (Linux
/// `AIC2_CONFIG_ENABLE`); iBoot laat hem uit.
pub const GLB_ENABLE: u32 = 1 << 0;
/// Het doelveld in het config-woord van een IRQ.
pub const CFG_TARGET: u32 = 0xf;

const _: () = {
    assert!(MAX_IRQS.is_multiple_of(32));
    assert!(CFG_TARGET == 0b1111);
};

/// De offsets die de ADT-node `/arm-io/aic` meegeeft (m1n1 leest dezelfde).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Props {
    /// `cap0-offset`: het aantal IRQ's in de onderste 16 bits.
    pub cap0: u64,
    /// `maxnumirq-offset`: het maximum (de maat van de tabellen).
    pub max_num_irq: u64,
    /// `extint-baseaddress`: het eerste config-woord.
    pub extint_base: u64,
    /// `aic-iack-offset`: het event-register.
    pub iack: u64,
    /// `aicglbcfg-offset`: de globale config (0 = niet zetten).
    pub glb_cfg: u64,
}

/// Waarom de AIC niet op kwam.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// De ADT beschreef het blok onvolledig.
    Incomplete {
        /// De basis.
        base: u64,
        /// `extint-baseaddress`.
        extint: u64,
        /// `aic-iack-offset`.
        iack: u64,
    },
    /// De maten uit het silicium zijn onmogelijk.
    Sizes {
        /// CAP0 zoals gelezen.
        cap0: u32,
        /// MAXNUMIRQ zoals gelezen.
        max: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Incomplete { base, extint, iack } => write!(
                f,
                "aic: incomplete description (base {base:#x} extint {extint:#x} iack {iack:#x})"
            ),
            Self::Sizes { cap0, max } => {
                write!(
                    f,
                    "aic: implausible sizes (cap0 {cap0:#x} maxnumirq {max:#x})"
                )
            }
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// De AIC. Leeg te maken als `static` ([`Aic::empty`]) en één keer te vullen
/// bij boot ([`Aic::init`]); daarna alleen gelezen, zodat een
/// `&'static Aic` de controller van `cpu::irq` kan zijn zonder heap.
///
/// # Invariants
///
/// Staat `event` op niet-nul, dan zijn alle adressen gezet door
/// [`Aic::init`] uit een gemapt registerblok, en `max_irq` is een
/// veelvoud van 32 met `nr_irq <= max_irq`.
pub struct Aic {
    base: AtomicU64,
    /// Het eerste config-woord; de bitmap-tabellen liggen erachter
    /// ([`Table`]).
    cfg: AtomicU64,
    event: AtomicU64,
    nr_irq: AtomicU32,
    max_irq: AtomicU32,
    target: AtomicU32,
    version: AtomicU32,
}

/// De bitmap-tabellen na de `max_irq` config-woorden, in deze volgorde en
/// elk `max_irq / 32` woorden (m1n1 `aic.c`; HW_STATE volgt als vijfde).
#[derive(Copy, Clone)]
enum Table {
    SwSet = 0,
    SwClr = 1,
    MaskSet = 2,
    MaskClr = 3,
}

impl Default for Aic {
    fn default() -> Self {
        Self::empty()
    }
}

impl Aic {
    /// Een lege AIC: elke toegang is een no-op tot [`init`](Self::init).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            base: AtomicU64::new(0),
            cfg: AtomicU64::new(0),
            event: AtomicU64::new(0),
            nr_irq: AtomicU32::new(0),
            max_irq: AtomicU32::new(0),
            target: AtomicU32::new(0),
            version: AtomicU32::new(0),
        }
    }

    /// Leest de maten uit het silicium, legt de registerblokken vast en zet
    /// de controller aan (AIC_GLB_CFG). Eén keer, bij boot, vóór hij als
    /// controller geregistreerd wordt.
    ///
    /// # Safety
    ///
    /// `base` is het registerblok van de AIC (ADT `/arm-io/aic` reg[0]),
    /// Device-gemapt, groot genoeg voor de offsets in `p` en de tabellen
    /// erachter, en van deze driver alleen.
    pub unsafe fn init(&self, base: Pa, p: Props) -> Result {
        if base.0 == 0 || p.extint_base == 0 || p.iack == 0 {
            return Err(Error::Incomplete {
                base: base.0,
                extint: p.extint_base,
                iack: p.iack,
            });
        }
        let cap0 = dev::read32(base.add(p.cap0));
        let maxn = dev::read32(base.add(p.max_num_irq));
        let (nr, max) = (cap0 & 0xffff, maxn & 0xffff);
        if max == 0 || !max.is_multiple_of(32) || nr > max || max > MAX_IRQS {
            return Err(Error::Sizes { cap0, max: maxn });
        }
        self.cfg.store(base.0 + p.extint_base, Relaxed);
        self.nr_irq.store(nr, Relaxed);
        self.max_irq.store(max, Relaxed);
        self.version.store(dev::read32(base), Relaxed);
        self.base.store(base.0, Relaxed);
        if p.glb_cfg != 0 {
            let g = base.add(p.glb_cfg);
            dev::write32(g, dev::read32(g) | GLB_ENABLE);
            dev::mb();
        }
        // INVARIANT: alle adressen hierboven gezet; `event` als laatste.
        self.event.store(base.0 + p.iack, Relaxed);
        Ok(())
    }

    /// Het aantal hardware-IRQ's dat dit silicium meldt.
    #[must_use]
    pub fn nr_irq(&self) -> u32 {
        self.nr_irq.load(Relaxed)
    }

    /// Kiest het 4-bit doel dat [`enable`](Controller::enable) in het
    /// config-woord schrijft.
    pub fn set_target(&self, t: u32) {
        self.target.store(t & CFG_TARGET, Relaxed);
    }

    /// Het woord en de bit van IRQ `id` in een bitmap-tabel; `None` buiten
    /// het bereik.
    fn bit(&self, id: u32) -> Option<(u64, u32)> {
        (id < self.nr_irq()).then(|| (u64::from(id / 32) * 4, 1 << (id % 32)))
    }

    /// Het adres van tabel `t`: na de `max_irq` config-woorden, elke tabel
    /// `max_irq / 8` bytes. Nul vóór [`init`](Self::init).
    fn table(&self, t: Table) -> u64 {
        let c = self.cfg.load(Relaxed);
        let max = u64::from(self.max_irq.load(Relaxed));
        if c == 0 {
            0
        } else {
            c + 4 * max + t as u64 * (max / 8)
        }
    }

    fn write_bit(&self, table: Table, id: u32) {
        let Some((w, b)) = self.bit(id) else { return };
        let t = self.table(table);
        if t != 0 {
            dev::write32(Pa(t + w), b);
            dev::mb();
        }
    }

    /// Laat IRQ `id` vanuit software vuren (SW_SET): de meetlat waarmee het
    /// board het doel vindt zonder een device nodig te hebben.
    pub fn soft_raise(&self, id: u32) {
        self.write_bit(Table::SwSet, id);
    }

    /// Haalt de software-IRQ weer weg (SW_CLR).
    pub fn soft_clear(&self, id: u32) {
        self.write_bit(Table::SwClr, id);
    }

    /// Eén regel voor de bootlog.
    #[must_use]
    pub fn describe(&self) -> Describe {
        Describe {
            version: self.version.load(Relaxed),
            base: self.base.load(Relaxed),
            nr: self.nr_irq(),
            max: self.max_irq.load(Relaxed),
            event: self.event.load(Relaxed),
            target: self.target.load(Relaxed),
        }
    }
}

/// De bootregel van de AIC, zonder heap.
#[derive(Copy, Clone, Debug)]
pub struct Describe {
    version: u32,
    base: u64,
    nr: u32,
    max: u32,
    event: u64,
    target: u32,
}

impl fmt::Display for Describe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AIC v{:#x} at {:#x}: {} of {} irqs, event {:#x}, target {}",
            self.version, self.base, self.nr, self.max, self.event, self.target
        )
    }
}

/// Een event-woord uitgepakt: die << 24 | type << 16 | nummer.
#[must_use]
pub const fn decode(ev: u32, max_irq: u32) -> Option<(u32, u32)> {
    let ty = (ev >> 16) & 0xff;
    let line = ((ev >> 24) & 0xff) * max_irq + (ev & 0xffff);
    if ty == 0 { None } else { Some((ty, line)) }
}

impl Controller for Aic {
    /// Doel in het config-woord, dan het masker open.
    fn enable(&self, l: Line) -> core::result::Result<(), IrqError> {
        let c = self.cfg.load(Relaxed);
        if c == 0 {
            return Err(IrqError::NoController);
        }
        if l.0 >= self.nr_irq() {
            return Err(IrqError::Rejected { line: l.0 });
        }
        let a = Pa(c + 4 * u64::from(l.0));
        dev::write32(
            a,
            (dev::read32(a) & !CFG_TARGET) | self.target.load(Relaxed),
        );
        self.write_bit(Table::MaskClr, l.0);
        Ok(())
    }

    fn disable(&self, l: Line) {
        self.write_bit(Table::MaskSet, l.0);
    }

    /// Leest het event-register. De lees is tegelijk de ack, en de AIC heeft
    /// een HW-IRQ dan al gemaskeerd; `complete` maakt hem weer scherp. Een
    /// event van een ander type (op dit silicium horen die niet te komen:
    /// IPI's zijn fast) wordt overgeslagen.
    fn claim(&self) -> Option<Line> {
        let ev = self.event.load(Relaxed);
        if ev == 0 {
            return None;
        }
        let max = self.max_irq.load(Relaxed);
        // Begrensd: een register dat alleen maar vreemde events geeft, mag
        // de dispatcher niet vasthouden.
        for _ in 0..16 {
            match decode(dev::read32(Pa(ev)), max) {
                None => return None,
                Some((EVENT_TYPE_HW, line)) => return Some(Line(line)),
                Some(_) => {}
            }
        }
        None
    }

    fn complete(&self, l: Line) {
        self.write_bit(Table::MaskClr, l.0);
    }

    /// Niets: de claim maskeerde de lijn al, en `complete` zou hem weer
    /// openen. `enable` zet hem open.
    fn hold(&self, _l: Line) {}
}

#[cfg(test)]
mod tests;
