//! Synopsys DesignWare MAC: de kern die beide generaties delen (reset,
//! MDIO, de ringen, het frame-pad en de interrupt), met per generatie een
//! ops-tabel voor wat verschilt.
//!
//! De vorm is die van Linux `drivers/net/ethernet/stmicro/stmmac`: één
//! `stmmac_main.c` en per core een `stmmac_hwif_entry` (`hwif.c`) met de
//! descriptor-, DMA-, MAC- en MII-ops. Hier is dat de trait [`Ops`], met
//! statische dispatch: het board kiest de generatie, er is geen tabel die
//! bij de boot op het VERSION-register zoekt.
//!
//! - `dwmac1000`: de 3.x (DWMAC1000/GMAC), de 100M-poort van de SG2002
//!   (LicheeRV Nano). Normal-format-descriptors op één cacheline elk, een
//!   poll-demand als doorbell, en een gecachete DMA-regio.
//! - `dwmac4`: de 4.x/5.x (DWMAC4/EQOS), het GMAC1 van de RK3566 (Radxa
//!   Zero 3E). Descriptors met een lees- en een schrijfvorm, een MTL-laag,
//!   DMA per kanaal en een tail-pointer als doorbell.
//!
//! Elke generatie is een feature; het board zet alleen de zijne aan.
//!
//! Wat de generaties delen en dus hier staat: de DMA-softreset (bit 0 van
//! DMA_BUS_MODE), de MDIO-machine (BUSY op bit 0, alleen de velden liggen
//! anders: [`Mii`], Linux' `mii_regs`), de indeling van de regio (RX- en
//! TX-descriptors onderin, de buffers vanaf [`Ops::BUF_OFF`]), de lengte-
//! en foutcontrole van een RX-frame, en het ritme van de interrupt: masker
//! dicht bij de claim, open als de pomp de ring leeg las. Op de lijn zitten
//! RX en TX (Linux `DMA_CHAN_INTR_NORMAL`: NIE, RIE en TIE), en elke
//! TX-descriptor vraagt zijn interrupt (IOC): een vrijgekomen descriptor
//! wekt de pomp. Linux zet IOC maar om de zoveel frames en ruimt de rest
//! op met een timer (`tx_coal_frames`, `stmmac_tx_timer`); hier geen timer,
//! dus op elk frame.
//!
//! De SoC-glue (klokken, pinmux, PHY-reset, de RGMII-deler) zit NIET hier
//! maar in het board; de clause-22-PHY-logica komt uit `driver-mdio`, via
//! [`Mdio`] op [`Probe`].
//!
//! De levensloop is een type (handboek §1.2): een [`Probe`] is het blok
//! vóór de ringen (versie, reset, MDIO), [`Probe::start`] maakt er een
//! [`Stmmac`] van, en alleen die draagt frames.
//!
//! Batching: [`transmit`](netdev::Device::transmit) en de RX-teruggave
//! zetten descriptors klaar; de doorbells vallen pas in
//! [`flush`](netdev::Device::flush), één keer per ring per burst.

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

#[cfg(feature = "dwmac1000")]
pub mod dwmac1000;
#[cfg(feature = "dwmac4")]
pub mod dwmac4;
#[cfg(test)]
mod tests;

use core::fmt;
use core::marker::PhantomData;
use dev::{Pa, Reg};
use driver_mdio::Mdio;
use netdev::{Mac, TxError};
use sync::Signal;

/// DMA_BUS_MODE: de softreset, in beide generaties bit 0; klaart vanzelf.
const BUS_SOFT_RESET: u32 = 1 << 0;
/// De MDIO-machine is bezig, in beide generaties bit 0 van het
/// adresregister; zelf gezet om een transactie te starten.
const MII_BUSY: u32 = 1 << 0;
/// De MAC meldt de lengte mét FCS: ACS (de automatische strip) staat in
/// beide generaties uit.
const FCS_LEN: usize = 4;

/// De maskers van de MMC-tellers: alle interrupts dicht, in beide
/// generaties (Linux `stmmac_mmc_setup`, `dwmac_mmc_intr_all_mask`). Een
/// teller op de helft of vol zet anders de MAC-interrupt, langs het
/// enable-register van de DMA heen op dezelfde lijn, en niemand hier wist
/// hem: de lijn blijft hoog en de pomp wordt een interruptstorm. De
/// octettellers zijn 32 bits; de Radxa haalde de helft (2 GiB in) na acht
/// bench-rondes van 256 MiB (03-10).
const MMC_INT_ALL: u32 = 0xFFFF_FFFF;

/// De MDIO-grens: een PHY die niet reageert mag de boot niet ophouden.
const MDIO_TIMEOUT_NS: u64 = 100_000_000;
/// De grens van de DMA-softreset: 1 s, zoals Linux voor beide generaties
/// (`readl_poll_timeout` in `dwmac_dma_reset` en `dwmac4_dma_reset`).
const RESET_TIMEOUT_NS: u64 = 1_000_000_000;

/// Monotone nanoseconden; de klok van het board.
pub type Clock = fn() -> u64;

/// De velden van het MDIO-adresregister (Linux `struct mii_regs` plus de
/// opcodes): per generatie anders geplaatst, verder dezelfde machine.
pub struct Mii {
    /// Het PHY-adres, vijf bits.
    addr_shift: u32,
    /// Het register, vijf bits.
    reg_shift: u32,
    /// De CSR-klokrange, vier bits.
    csr_shift: u32,
    /// De opcode voor lezen.
    read: u32,
    /// De opcode voor schrijven.
    write: u32,
}

/// Het MDIO-commandowoord: adres, register en klokrange op hun plek, de
/// opcode, en BUSY. Te grote adressen worden gemaskeerd, niet doorgeschoven.
fn mdio_cmd(m: &Mii, phy: u8, reg: u8, csr: u32, write: bool) -> u32 {
    let op = if write { m.write } else { m.read };
    ((u32::from(phy) & 0x1F) << m.addr_shift)
        | ((u32::from(reg) & 0x1F) << m.reg_shift)
        | ((csr & 0xF) << m.csr_shift)
        | op
        | MII_BUSY
}

/// Wat per generatie verschilt: de registers, de descriptors en de maten
/// (Linux: `stmmac_desc_ops`, `stmmac_dma_ops`, `stmmac_ops` en `mii_regs`
/// in één). Geïmplementeerd door een leeg type per generatie.
pub trait Ops: Copy + 'static {
    /// Het registerblok vanaf de basis.
    type Regs;

    /// Het aantal RX-descriptors.
    const NUM_RX: u16;
    /// Het aantal TX-descriptors.
    const NUM_TX: u16;
    /// De afstand tussen twee descriptors, en de alignment die de regio
    /// moet hebben.
    const DESC_STRIDE: u64;
    /// Eén framebuffer; een veelvoud van een cacheline, zodat het
    /// onderhoud van de één de ander niet raakt.
    const BUF_SIZE: usize;
    /// Waar de buffers in de regio beginnen, achter alle descriptors.
    const BUF_OFF: u64;
    /// De regio die de driver nodig heeft: descriptors en alle buffers.
    const NEED_BYTES: u64 =
        Self::BUF_OFF + (Self::NUM_RX as u64 + Self::NUM_TX as u64) * Self::BUF_SIZE as u64;
    /// Het grootste frame dat wij versturen.
    const MAX_FRAME: usize;
    /// De grootste lengte (met FCS) die de MAC ons mag melden: wat we hem
    /// als buffergrootte opgaven. Meer is een lengte die bytes uit de
    /// naburige buffer zou blootgeven.
    const RX_LIMIT: usize;
    /// De velden van het MDIO-adresregister.
    const MII: Mii;
    /// De interrupts van de lijn (RX en TX) in het enable-register.
    const INTR: u32;
    /// De statusbits (W1C) die ze wissen.
    const STAT: u32;

    /// Het VERSION-register.
    fn version(r: &Self::Regs) -> &Reg<u32>;
    /// DMA_BUS_MODE, met de softreset op bit 0.
    fn bus_mode(r: &Self::Regs) -> &Reg<u32>;
    /// Het MDIO-adres- en het MDIO-dataregister.
    fn mii(r: &Self::Regs) -> (&Reg<u32>, &Reg<u32>);
    /// Het interrupt-enable- en het statusregister van de DMA.
    fn irq(r: &Self::Regs) -> (&Reg<u32>, &Reg<u32>);

    /// Kan deze MAC de onderhandelde snelheid? Gevraagd vóór de reset.
    fn speed_ok(mbps: u32) -> bool;
    /// Programmeert MAC en DMA en zet ze aan: na de reset, met de ringen al
    /// gelegd (Linux `stmmac_hw_setup`).
    fn program(
        r: &Self::Regs,
        ring: &Rings<Self>,
        mac: Mac,
        mbps: u32,
        full_duplex: bool,
    ) -> Result;
    /// De RX-doorbell; `tail` is de descriptor die de kern als volgende
    /// leest.
    fn rx_doorbell(r: &Self::Regs, tail: Pa);
    /// De TX-doorbell; `tail` is de descriptor die de kern als volgende
    /// vult.
    fn tx_doorbell(r: &Self::Regs, tail: Pa);

    /// Legt RX-descriptor `d` met buffer `buf` bij de start; `last` is de
    /// laatste van de ring.
    fn rx_init(d: Pa, buf: Pa, last: bool) {
        Self::rx_give(d, buf, last);
    }
    /// Geeft RX-descriptor `d` terug aan de DMA, OWN als laatste.
    fn rx_give(d: Pa, buf: Pa, last: bool);
    /// Het statuswoord van RX-descriptor `d`, of `None` als hij nog van de
    /// DMA is. Na `Some` mag de buffer gelezen worden (de barrière zit
    /// hierin).
    fn rx_status(d: Pa) -> Option<u32>;
    /// Is het een goed frame in één descriptor (geen fout, eerste én
    /// laatste)?
    fn rx_whole(sts: u32) -> bool;
    /// De rauwe lengte uit het statuswoord, met FCS.
    fn rx_raw_len(sts: u32) -> usize;
    /// Legt TX-descriptor `d` met buffer `buf` bij de start, van ons.
    fn tx_init(d: Pa, buf: Pa, last: bool);
    /// Is TX-descriptor `d` van ons (verzonden of nooit gebruikt)?
    fn tx_free(d: Pa) -> bool;
    /// Geeft TX-descriptor `d` met `len` bytes in `buf` aan de DMA, OWN als
    /// laatste. `len` ligt in `1..=MAX_FRAME`.
    fn tx_give(d: Pa, buf: Pa, len: usize, last: bool);
}

/// Waarom de driver weigert.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// VERSION leest 0 of alles-één: een blok zonder klok of een dode bus.
    NoMac {
        /// De gelezen waarde.
        version: u32,
    },
    /// De DMA-softreset klaart niet. Op de RK3566 staat dan de AXI-kant nog
    /// in reset (05-08: `bus mode 0x00000001`).
    ResetStuck {
        /// DMA_BUS_MODE na de grens.
        bus_mode: u32,
    },
    /// De DMA-regio is te klein.
    DmaTooSmall {
        /// Wat nodig is.
        need: u64,
        /// Wat er is.
        have: u64,
    },
    /// De DMA-regio ligt niet op [`Ops::DESC_STRIDE`] of niet onder 4 GB.
    DmaPlace {
        /// Het begin.
        base: u64,
    },
    /// Een snelheid die deze MAC niet kan (de DWMAC1000 via RMII: alleen 10
    /// en 100).
    Speed {
        /// De gevraagde snelheid.
        mbps: u32,
    },
    /// HW_FEATURE1 van de DWMAC4 geeft onmogelijke FIFO-maten: een blok
    /// zonder klok.
    Fifo {
        /// De TX-FIFO in bytes.
        tx: u32,
        /// De RX-FIFO in bytes.
        rx: u32,
        /// Het rauwe register.
        hw_feature1: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMac { version } => write!(f, "stmmac: no MAC (version {version:#010x})"),
            Self::ResetStuck { bus_mode } => write!(
                f,
                "stmmac: DMA soft reset does not clear (bus mode {bus_mode:#010x})"
            ),
            Self::DmaTooSmall { need, have } => write!(
                f,
                "stmmac: DMA region {} KB, need at least {} KB",
                have >> 10,
                need >> 10
            ),
            Self::DmaPlace { base } => write!(
                f,
                "stmmac: DMA region at {base:#x} is unaligned or above 4 GB"
            ),
            Self::Speed { mbps } => write!(f, "stmmac: {mbps} Mbps is not a speed of this MAC"),
            Self::Fifo {
                tx,
                rx,
                hw_feature1,
            } => write!(
                f,
                "stmmac: implausible FIFO sizes tx={tx}B rx={rx}B (hw-feature1 {hw_feature1:#010x}), is the block clocked?"
            ),
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// De laagste 32 bits van een DMA-adres (de regio ligt onder 4 GB, getoetst
/// in `start`).
fn lo(pa: Pa) -> u32 {
    (pa.0 & 0xFFFF_FFFF) as u32
}

/// Het MAC-adres in de perfect-filter (Addr0), in beide generaties
/// dezelfde twee woorden.
fn set_mac_addr(hi: &Reg<u32>, lo: &Reg<u32>, mac: Mac) {
    let m = mac.0;
    hi.write((u32::from(m[5]) << 8) | u32::from(m[4]));
    lo.write(u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
}

/// De framelengte uit het statuswoord zonder de vier FCS-bytes, of `None`
/// als de melding niet kan: niet langer dan een FCS, of langer dan wat we
/// de MAC als buffer opgaven. Vergeet je de FCS, dan komt elk frame vier
/// bytes te lang de stack in en faalt elke checksum.
fn rx_len<O: Ops>(sts: u32) -> Option<usize> {
    let raw = O::rx_raw_len(sts);
    if raw <= FCS_LEN || raw > O::RX_LIMIT {
        return None;
    }
    Some(raw - FCS_LEN)
}

/// Het blok vóór de ringen: versie, reset en de MDIO-master. Het board
/// doet hiermee de PHY-scan en de autonegotiatie, en maakt er dan met
/// [`Probe::start`] een draaiende [`Stmmac`] van.
pub struct Probe<O: Ops> {
    base: Pa,
    csr: u32,
    now: Clock,
    _ops: PhantomData<O>,
}

impl<O: Ops> Probe<O> {
    /// Het blok op `base`, met CSR-klokrange `csr` voor de MDC-deler. Raakt
    /// nog niets aan.
    ///
    /// # Safety
    ///
    /// `base` is het gemapte (Device) registerblok van deze generatie, van
    /// minstens `size_of::<O::Regs>()` bytes, dat blijft zolang het
    /// programma draait, en zijn klokken staan open (het board doet dat
    /// vóór deze aanroep; een Rockchip-blok zonder klok houdt de bus vast).
    #[must_use]
    pub unsafe fn new(base: Pa, csr: u32, now: Clock) -> Self {
        Self {
            base,
            csr,
            now,
            _ops: PhantomData,
        }
    }

    fn regs(&self) -> &'static O::Regs {
        // SAFETY: de voorwaarde van `new`.
        unsafe { dev::regs(self.base) }
    }

    /// Het rauwe VERSION-register. Het eerste dat een bring-up hoort te
    /// lezen.
    #[must_use]
    pub fn version(&self) -> u32 {
        O::version(self.regs()).read()
    }

    /// Leeft de MAC? 0 of alles-één is een dode bus.
    pub fn check(&self) -> Result<u32> {
        match self.version() {
            v @ (0 | 0xFFFF_FFFF) => Err(Error::NoMac { version: v }),
            v => Ok(v),
        }
    }

    /// Het rauwe MDIO-adresregister, voor het meetinstrument: blijft BUSY
    /// staan, dan wacht de machine op een klok of een bus, en dat is een
    /// ander probleem dan "er zit geen PHY".
    #[must_use]
    pub fn mdio_state(&self) -> u32 {
        O::mii(self.regs()).0.read()
    }

    /// De DMA-softreset. Apart van `start`: de reset zet ook de
    /// MDIO-machine schoon, en het board mag hem vóór de PHY-scan doen.
    pub fn reset(&mut self) -> Result {
        let bus = O::bus_mode(self.regs());
        bus.update(|v| v | BUS_SOFT_RESET);
        if dev::poll_until(self.now, RESET_TIMEOUT_NS, || {
            bus.read() & BUS_SOFT_RESET == 0
        }) {
            Ok(())
        } else {
            Err(Error::ResetStuck {
                bus_mode: bus.read(),
            })
        }
    }

    /// Wacht, begrensd, tot de MDIO-machine vrij is. Een bool en geen
    /// fout: de aanroepers maken er [`driver_mdio::Error::Bus`] van, wat de
    /// scan als leeg adres leest (Go: 0xffff, "geen PHY").
    fn mdio_wait(&self) -> bool {
        let addr = O::mii(self.regs()).0;
        dev::poll_until(self.now, MDIO_TIMEOUT_NS, || addr.read() & MII_BUSY == 0)
    }

    /// Legt de ringen in de DMA-regio en zet MAC en DMA aan: na de
    /// board-glue en na de autonegotiatie, want `mbps` en `full_duplex`
    /// komen daaruit. Doet zelf de softreset.
    ///
    /// # Safety
    ///
    /// `[dma, dma + dma_size)` ligt onder 4 GB en wordt alleen door deze
    /// driver en het device gebruikt, nu en zolang het programma draait. Het
    /// mag gecachet zijn (de driver doet het onderhoud), maar niet Device:
    /// een frame gaat met `memcpy` de buffers in en uit.
    pub unsafe fn start(
        mut self,
        dma: Pa,
        dma_size: u64,
        mac: Mac,
        mbps: u32,
        full_duplex: bool,
    ) -> Result<Stmmac<O>> {
        if dma_size < O::NEED_BYTES {
            return Err(Error::DmaTooSmall {
                need: O::NEED_BYTES,
                have: dma_size,
            });
        }
        if !dma.is_aligned(O::DESC_STRIDE) || dma.0.saturating_add(O::NEED_BYTES) > 1 << 32 {
            return Err(Error::DmaPlace { base: dma.0 });
        }
        if !O::speed_ok(mbps) {
            return Err(Error::Speed { mbps });
        }
        self.reset()?;
        let mut n = Stmmac::at(self.base, mac, dma);
        n.bring_up(mbps, full_duplex)?;
        Ok(n)
    }
}

impl<O: Ops> Mdio for Probe<O> {
    fn read(&mut self, phy: u8, reg: u8) -> driver_mdio::Result<u16> {
        let bus = driver_mdio::Error::Bus { phy, reg };
        let (addr, data) = O::mii(self.regs());
        if !self.mdio_wait() {
            return Err(bus);
        }
        addr.write(mdio_cmd(&O::MII, phy, reg, self.csr, false));
        if !self.mdio_wait() {
            return Err(bus);
        }
        Ok((data.read() & 0xFFFF) as u16)
    }

    fn write(&mut self, phy: u8, reg: u8, val: u16) -> driver_mdio::Result {
        let bus = driver_mdio::Error::Bus { phy, reg };
        let (addr, data) = O::mii(self.regs());
        if !self.mdio_wait() {
            return Err(bus);
        }
        data.write(u32::from(val));
        addr.write(mdio_cmd(&O::MII, phy, reg, self.csr, true));
        if self.mdio_wait() { Ok(()) } else { Err(bus) }
    }
}

/// Waar de ringen en buffers liggen: RX- en TX-descriptors onderin de
/// regio, [`Ops::DESC_STRIDE`] uit elkaar, en de RX- en TX-buffers op een
/// rij vanaf [`Ops::BUF_OFF`].
pub struct Rings<O: Ops> {
    rx_desc: Pa,
    tx_desc: Pa,
    rx_buf: Pa,
    tx_buf: Pa,
    _ops: PhantomData<O>,
}

impl<O: Ops> Rings<O> {
    const fn at(dma: Pa) -> Self {
        Self {
            rx_desc: dma,
            tx_desc: dma.add(O::NUM_RX as u64 * O::DESC_STRIDE),
            rx_buf: dma.add(O::BUF_OFF),
            tx_buf: dma.add(O::BUF_OFF + O::NUM_RX as u64 * O::BUF_SIZE as u64),
            _ops: PhantomData,
        }
    }

    /// Alle descriptors samen.
    const DESC_BYTES: u64 = (O::NUM_RX as u64 + O::NUM_TX as u64) * O::DESC_STRIDE;
    /// Alle buffers samen.
    const BUF_BYTES: u64 = (O::NUM_RX as u64 + O::NUM_TX as u64) * O::BUF_SIZE as u64;

    fn rx(&self, i: u16) -> Pa {
        self.rx_desc.add(u64::from(i) * O::DESC_STRIDE)
    }

    fn tx(&self, i: u16) -> Pa {
        self.tx_desc.add(u64::from(i) * O::DESC_STRIDE)
    }

    fn rx_buf(&self, i: u16) -> Pa {
        self.rx_buf.add(u64::from(i) * O::BUF_SIZE as u64)
    }

    fn tx_buf(&self, i: u16) -> Pa {
        self.tx_buf.add(u64::from(i) * O::BUF_SIZE as u64)
    }
}

/// De meetlat van de driver.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Afgeleverde frames.
    pub rx_frames: u64,
    /// Afgekeurde descriptors (foutframe of gesplitst frame).
    pub rx_errors: u64,
    /// Onmogelijke lengtes (niet langer dan een FCS of voorbij de gemelde
    /// buffer): geteld en overgeslagen, nooit gekopieerd.
    pub rx_bad_len: u64,
    /// Het rauwe statuswoord van het laatst afgekeurde frame: zonder dat is
    /// "afgekeurd" op ijzer niet te ontleden.
    pub rx_last_err: u32,
    /// Frames die de MAC weggooide omdat er geen vrije descriptor was
    /// (alleen de DWMAC1000 heeft de teller, zie `dwmac1000::Diag`).
    pub rx_missed_ring: u64,
    /// Frames weg door een volle RX-FIFO.
    pub rx_missed_fifo: u64,
    /// Hoe vaak een van de twee hardwaretellers zelf overliep.
    pub rx_missed_ovf: u64,
    /// Verzonden frames (op de ring gezet).
    pub tx_frames: u64,
    /// Keren dat de TX-ring vol zat.
    pub tx_full: u64,
    /// Doorbells: poll-demands of tail-pointers.
    pub doorbells: u64,
    /// Keren dat de lijn weer open ging.
    pub rearms: u64,
}

/// Het interrupt-pad van de NIC: alleen het enable- en het statusregister
/// van de DMA, die niets met de ringen delen. `Copy`, zodat het board hem
/// naast de driver houdt.
#[derive(Clone, Copy)]
pub struct IrqAck<O: Ops> {
    base: Pa,
    _ops: PhantomData<O>,
}

impl<O: Ops> netdev::IrqAck for IrqAck<O> {
    /// Laat de level-lijn los: masker dicht, status gewist (het
    /// rtl8126-ritme). De driver zet het masker weer open als de pomp de
    /// ring leeg las (`receive` die `None` geeft), dus één claim per burst
    /// in plaats van per frame.
    fn ack(&self) {
        // SAFETY: `base` kwam uit een `Probe`, die een gemapt blok eiste;
        // het enable- en statusregister delen niets met de ringen.
        let (ena, status) = O::irq(unsafe { dev::regs(self.base) });
        ena.write(0);
        status.write(O::STAT);
    }
}

/// Eén draaiende DesignWare MAC.
pub struct Stmmac<O: Ops> {
    base: Pa,
    mac: Mac,
    ring: Rings<O>,
    /// De volgende RX-descriptor die wij lezen.
    rx_cur: u16,
    /// De volgende TX-descriptor die wij vullen.
    tx_cur: u16,
    /// Er zijn RX-descriptors teruggegeven sinds de laatste doorbell.
    rx_dirty: bool,
    /// Er zijn TX-descriptors gevuld sinds de laatste doorbell.
    tx_dirty: bool,
    irq: Option<&'static Signal>,
    /// De meetlat.
    pub stats: Stats,
}

impl<O: Ops> Stmmac<O> {
    /// Een driver op registerblok `base` met de ringen in `dma`, nog niet
    /// aangezet.
    fn at(base: Pa, mac: Mac, dma: Pa) -> Self {
        Self {
            base,
            mac,
            ring: Rings::at(dma),
            rx_cur: 0,
            tx_cur: 0,
            rx_dirty: false,
            tx_dirty: false,
            irq: None,
            stats: Stats::default(),
        }
    }

    fn regs(&self) -> &'static O::Regs {
        // SAFETY: de voorwaarde van `Probe::new`, waar `base` vandaan komt.
        unsafe { dev::regs(self.base) }
    }

    /// Legt de ringen en programmeert de generatie; de reset is al gedaan.
    fn bring_up(&mut self, mbps: u32, full_duplex: bool) -> Result {
        // Eerst geen regel van de buffers meer in de cache: een vuile regel
        // van vóór ons (de bootloader, een vorige kern) mag later niet over
        // een frame heen worden teruggeschreven dat de DMA erin zette.
        dev::pull(self.ring.rx_buf, Rings::<O>::BUF_BYTES as usize);
        for i in 0..O::NUM_RX {
            O::rx_init(self.ring.rx(i), self.ring.rx_buf(i), i == O::NUM_RX - 1);
        }
        for i in 0..O::NUM_TX {
            O::tx_init(self.ring.tx(i), self.ring.tx_buf(i), i == O::NUM_TX - 1);
        }
        // De descriptors naar het geheugen; op een ongecachete ring doet
        // alleen de barrière iets.
        dev::push(self.ring.rx_desc, Rings::<O>::DESC_BYTES as usize);
        O::program(self.regs(), &self.ring, self.mac, mbps, full_duplex)
    }

    /// Het interrupt-pad, voor het board.
    #[must_use]
    pub fn irq_ack(&self) -> IrqAck<O> {
        IrqAck {
            base: self.base,
            _ops: PhantomData,
        }
    }

    /// Hangt de bel van de NIC-interrupt aan de driver en zet de
    /// interrupts van de lijn open (RX en TX). De pomp wacht dan op de bel.
    pub fn set_irq(&mut self, bell: &'static Signal) {
        self.irq = Some(bell);
        self.rearm();
    }

    /// Status gewist, masker open.
    fn rearm(&mut self) {
        let (ena, status) = O::irq(self.regs());
        status.write(O::STAT);
        ena.write(O::INTR);
        self.stats.rearms += 1;
    }

    /// Eén descriptor lezen en teruggeven. `None` = nog van de DMA; een
    /// afgekeurde descriptor wordt geteld, teruggegeven en overgeslagen.
    fn receive_one(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let i = self.rx_cur;
            let d = self.ring.rx(i);
            let sts = O::rx_status(d)?;
            let got = self.take(i, sts, buf);
            O::rx_give(d, self.ring.rx_buf(i), i == O::NUM_RX - 1);
            self.rx_cur = (i + 1) % O::NUM_RX;
            self.rx_dirty = true;
            if got.is_some() {
                return got;
            }
        }
    }

    /// Het frame uit descriptor `i` met status `sts` naar `buf`, of `None`
    /// voor een afgekeurde descriptor.
    fn take(&mut self, i: u16, sts: u32, buf: &mut [u8]) -> Option<usize> {
        if !O::rx_whole(sts) {
            self.stats.rx_errors += 1;
            self.stats.rx_last_err = sts;
            return None;
        }
        let Some(len) = rx_len::<O>(sts) else {
            self.stats.rx_bad_len += 1;
            self.stats.rx_last_err = sts;
            return None;
        };
        let n = len.min(buf.len());
        let src = self.ring.rx_buf(i);
        // De buffer vers uit het geheugen vóór de kopie; de regels zijn
        // alleen van deze buffer (BUF_SIZE is een veelvoud van een regel).
        dev::pull(src, n);
        if let Some(dst) = buf.get_mut(..n) {
            dev::copy_out_normal(dst, src);
        }
        self.stats.rx_frames += 1;
        Some(n)
    }

    /// De RX-doorbell, als er descriptors terug zijn.
    fn kick_rx(&mut self) {
        if self.rx_dirty {
            O::rx_doorbell(self.regs(), self.ring.rx(self.rx_cur));
            self.rx_dirty = false;
            self.stats.doorbells += 1;
        }
    }
}

impl<O: Ops> netdev::Device for Stmmac<O> {
    /// Zet één frame op de TX-ring; de doorbell volgt in `flush`. Vol is de
    /// ring als de descriptor van deze plek nog van de DMA is. (Go wachtte
    /// hier tot 1 s; het contract zegt `Full` en laat de pomp kiezen.)
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), TxError> {
        let len = frame.len();
        if len == 0 || len > O::MAX_FRAME {
            return Err(TxError::Size(len));
        }
        let d = self.ring.tx(self.tx_cur);
        if !O::tx_free(d) {
            self.stats.tx_full += 1;
            return Err(TxError::Full);
        }
        // Eerst de buffer naar het geheugen, dan pas de descriptor die
        // ernaar wijst.
        let b = self.ring.tx_buf(self.tx_cur);
        dev::copy_in_normal(b, frame);
        dev::push(b, len);
        O::tx_give(d, b, len, self.tx_cur == O::NUM_TX - 1);
        self.tx_cur = (self.tx_cur + 1) % O::NUM_TX;
        self.tx_dirty = true;
        self.stats.tx_frames += 1;
        Ok(())
    }

    /// Haalt één frame op, of `None` als de RX-ring leeg is. Een foutframe,
    /// een gesplitst frame of een onmogelijke lengte wordt geteld en
    /// teruggegeven zonder kopie; een frame groter dan `buf` wordt
    /// afgekapt.
    ///
    /// Leeg: dan valt de RX-doorbell meteen. De pomp flusht alleen na een
    /// frame, en een RX-DMA die stilstond op een ring vol afgekeurde
    /// descriptors moet het horen (Linux: `stmmac_rx_refill` aan het eind
    /// van elke `stmmac_rx`).
    ///
    /// Leeg met een bedrade lijn: dan gaat het masker weer open (de
    /// dispatch sloot het bij de claim), en daarna kijkt de driver nog één
    /// keer, zodat een frame dat tussen de lees en het openen binnenkwam
    /// niet tot de vangrail blijft liggen.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        if let Some(n) = self.receive_one(buf) {
            return Some(n);
        }
        self.kick_rx();
        if self.irq.is_none() || O::irq(self.regs()).0.read() != 0 {
            return None;
        }
        self.rearm();
        let got = self.receive_one(buf);
        if got.is_none() {
            self.kick_rx();
        }
        got
    }

    /// De doorbells: één schrijf per ring per burst.
    fn flush(&mut self) {
        dev::mb();
        if self.tx_dirty {
            O::tx_doorbell(self.regs(), self.ring.tx(self.tx_cur));
            self.tx_dirty = false;
            self.stats.doorbells += 1;
        }
        self.kick_rx();
    }

    fn mac(&self) -> Mac {
        self.mac
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    fn stats(&self) -> netdev::Stats {
        netdev::Stats {
            rx_bad: self.stats.rx_errors + self.stats.rx_bad_len,
            tx_full: self.stats.tx_full,
            doorbells: self.stats.doorbells,
        }
    }
}
