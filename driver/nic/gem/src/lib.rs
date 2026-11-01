//! De Cadence GEM gigabit-MAC: op de Raspberry Pi 5 het ethernet-blok in de
//! RP1-southbridge, achter de PCIe-link die `driver-brcmpcie` traint. Dezelfde
//! IP-core zit op veel andere ARM-SoC's.
//!
//! Geschreven naar het RP1-peripherals-datasheet en de macb-registerlayout
//! (Linux en U-Boot als referentie); één RX- en één TX-queue, 64-bit
//! descriptors. Boardvast bewezen in de Go-kern (probe6, 10-07; P2 sinds
//! 11-07). DMA-adressen: busadres = fysiek adres + `bus_off`: de
//! RP1-masters sturen hun adressen 1:1 als PCIe-upstream door, en het
//! inbound-window van de BCM2712-root-complex legt PCIe 0x10_0000_0000 op
//! DRAM 0 (Linux' conventie; het board zet dat window).
//!
//! Eigendom: de driver is van de RX-pomp (`&mut self`); de DMA-regio is van
//! hem alleen.
//!
//! CACHE-COHERENTIE: de RP1 snoopt niet. De descriptors liggen onderin de
//! regio, Normal-NC, zonder onderhoud. De framebuffers liggen in hun eigen
//! twee blokken van 2 MB ([`BUF_OFF`], [`BUF_BLOCK`]) die het board
//! Normal-WB en XN mapt, en de driver veegt zelf: `dev::push` na elke
//! TX-kopie, `dev::pull` vóór elke RX-kopie, en één `pull` over alle
//! buffers in [`Gem::init`] (een vorige kern kan er regels achtergelaten
//! hebben); Linux' `dma_sync_single_for_device`/`_for_cpu`, en de net-wb
//! van de O6N, de Altra en de dwmac4 op de Radxa. De kopieën zijn `memcpy`.
//! GEMETEN 04-10 op de Pi 5 (een app die 20000 keer 1514 bytes kopieert,
//! 1500 MHz): lezen uit NC met vluchtige woorden van 8 bytes (zoals
//! `dev::copy_out`) 8,4 µs per frame, uit NC met `memcpy` 1,0, uit WB na
//! `dc civac` 0,68; schrijven naar NC met woorden 0,77, met `memcpy` 0,16,
//! naar WB met `dc cvac` 0,38. Bij 111 MB/s in (77k frames per seconde)
//! kostte het lezen uit NC 0,64 s per seconde van de OS-core. Het zenden
//! (55 MB/s) zat niet in de kopie maar in het wachten per frame
//! (`Gem::tx_restart`).
//!
//! De interrupt: [`ack_irq`] (masker dicht, latch gewist) is de device-ack
//! van de dispatcher; de RX-pomp heropent hem als hij de ring leeg vindt
//! (het rearm-ritme van `hopnet.rxLoop` in Go). Op de lijn: RCOMP, en de
//! TX-kant zoals Linux' `MACB_TX_INT_FLAGS`: TCOMP (een frame ging de draad
//! op, er is ring vrij) en TXUBR (de zender las een vrije descriptor en
//! stopte; de herstart van [`Gem::tx_restart`] volgt in de pomp-ronde).

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
use dev::{Pa, Reg};
use driver_mdio::{Link, Mdio};
use netdev::{Mac, TxError};
use sync::Signal;

#[cfg(test)]
mod tests;

/// Het GEM-registerblok (macb/u-boot-conventie).
#[repr(C)]
struct Regs {
    nwctrl: Reg<u32>,
    nwcfg: Reg<u32>,
    nwstatus: Reg<u32>,
    _r0: u32,
    dmacfg: Reg<u32>,
    txstatus: Reg<u32>,
    rxqbase: Reg<u32>,
    txqbase: Reg<u32>,
    rxstatus: Reg<u32>,
    /// Interrupt status: W1C met het bit zelf.
    isr: Reg<u32>,
    ier: Reg<u32>,
    idr: Reg<u32>,
    /// Interrupt mask (1 = uit).
    imr: Reg<u32>,
    /// PHY maintenance (MDIO).
    man: Reg<u32>,
    _r1: [u32; 3],
    /// RX partial store-and-forward (uit = 0).
    pbuf_rx_cut: Reg<u32>,
    _r2: [u32; 3],
    /// AXI Max Pipeline: AR2R [7:0], AW2W [15:8], AW2B_FILL 16.
    amp: Reg<u32>,
    _r3: [u32; 12],
    spaddr1b: Reg<u32>,
    spaddr1t: Reg<u32>,
    _r4: [u32; 124],
    /// Design config 1: DBWDEF [27:25] = de AXI-busbreedte.
    dcfg1: Reg<u32>,
    _r5: [u32; 145],
    /// TX queue base, hoge 32 bits.
    tbqph: Reg<u32>,
    _r6: [u32; 2],
    /// RX queue base, hoge 32 bits.
    rbqph: Reg<u32>,
}

const _: () = {
    assert!(offset_of!(Regs, dmacfg) == 0x010);
    assert!(offset_of!(Regs, rxqbase) == 0x018);
    assert!(offset_of!(Regs, isr) == 0x024);
    assert!(offset_of!(Regs, idr) == 0x02c);
    assert!(offset_of!(Regs, man) == 0x034);
    assert!(offset_of!(Regs, pbuf_rx_cut) == 0x044);
    assert!(offset_of!(Regs, amp) == 0x054);
    assert!(offset_of!(Regs, spaddr1b) == 0x088);
    assert!(offset_of!(Regs, dcfg1) == 0x280);
    assert!(offset_of!(Regs, tbqph) == 0x4c8);
    assert!(offset_of!(Regs, rbqph) == 0x4d4);
};

/// De maat van het registerblok dat deze driver aanraakt.
pub const MMIO_SIZE: u64 = 0x4d8;

const CTRL_RX_EN: u32 = 1 << 2;
const CTRL_TX_EN: u32 = 1 << 3;
const CTRL_MGMT_EN: u32 = 1 << 4;
const CTRL_CLR_STAT: u32 = 1 << 5;
const CTRL_TX_START: u32 = 1 << 9;

const CFG_SPEED100: u32 = 1 << 0;
const CFG_FD: u32 = 1 << 1;
const CFG_GIGABIT: u32 = 1 << 10;
/// Pause-frames sturen bij een volle RX-FIFO (macb MACB_PAE): de druk niet
/// ongedempt op de GEM-DMA/AXI laten staan (freeze-jacht 13-07).
const CFG_PAE: u32 = 1 << 13;
/// FCS strippen van RX-frames.
const CFG_RX_FCS: u32 = 1 << 17;
/// MDC-divisor pclk/48.
const CFG_MDC_DIV48: u32 = 0b100 << 18;
/// De databusbreedte [22:21]: MOET de gesynthetiseerde AXI-breedte
/// (DCFG1.DBWDEF) volgen. GEMETEN 10-07 (probe6 run 4): met DBW=0 op de
/// RP1-GEM is álle DMA dood (TGO blijft hangen, nul RX) terwijl MDIO gewoon
/// werkt.
const CFG_DBW_SHIFT: u32 = 21;

const STATUS_MDIO_IDLE: u32 = 1 << 2;

const DMA_BURST_INCR16: u32 = 0x10;
const DMA_RX_SIZE_SHIFT: u32 = 16;
const DMA_TX_PBUF_FULL: u32 = 1 << 10;
const DMA_RX_PBUF_FULL: u32 = 0b11 << 8;
const DMA_ADDR64: u32 = 1 << 30;

/// RX woord 0: software mag hem lezen (DMA klaar).
const RX_OWNED: u32 = 1 << 0;
const RX_WRAP: u32 = 1 << 1;
const RX_LEN_MASK: u32 = 0x1fff;
const RX_SOF: u32 = 1 << 14;
const RX_EOF: u32 = 1 << 15;

const TX_USED: u32 = 1 << 31;
const TX_WRAP: u32 = 1 << 30;
const TX_LAST: u32 = 1 << 15;
/// TXSTATUS: de zender is bezig (TGO).
const TX_GO: u32 = 1 << 3;
/// NWCTRL met TSTART: RX, TX en MDIO blijven aan.
const TX_KICK: u32 = CTRL_MGMT_EN | CTRL_TX_EN | CTRL_RX_EN | CTRL_TX_START;

const MAN_CLAUSE22: u32 = 1 << 30;
const MAN_READ: u32 = 0b10 << 28;
const MAN_WRITE: u32 = 0b01 << 28;
const MAN_MUST_BE_10: u32 = 0b10 << 16;

/// RP1 (rp1.dtsi): cdns,ar2r-max-pipe = 8, aw2w-max-pipe = 8, use-aw2b-fill.
const AMP_RP1: u32 = 8 | (8 << 8) | (1 << 16);

/// Receive complete (MACB_RCOMP).
const INT_RCOMP: u32 = 1 << 1;
/// TX used bit read (MACB_TXUBR): de zender stopte op een vrije descriptor.
const INT_TXUBR: u32 = 1 << 3;
/// Transmit complete (MACB_TCOMP).
const INT_TCOMP: u32 = 1 << 7;
/// Wat de pomp wekt: ontvangen, verzonden, en de zender die stilviel.
const INT_LINE: u32 = INT_RCOMP | INT_TXUBR | INT_TCOMP;

/// Buffergrootte per descriptor (64-voud).
pub const BUF: usize = 1536;
/// 256 RX-descriptors, niet 64: bij een gepolde ontvanger is de ring de
/// buffer tussen twee pomprondes, en 64 x 1,5 KB is 0,8 ms op 1 Gbit (de
/// igb ging van 5 naar 36 MB/s met 64 naar 256, 19-09).
pub const N_RX: usize = 256;
/// TX-descriptors.
pub const N_TX: usize = 64;
/// De RX-buffers: een eigen 2 MB-blok.
const OFF_RX_BUFS: u64 = 0x20_0000;
/// De TX-buffers: nog een 2 MB-blok.
const OFF_TX_BUFS: u64 = 0x40_0000;
/// Wat deze driver aan DMA-geheugen vraagt.
pub const DMA_NEED: u64 = OFF_TX_BUFS + 0x20_0000;
/// Waar de framebuffers beginnen: vanaf hier mapt het board de regio
/// Normal-WB (de driver veegt), eronder liggen de descriptors NC.
pub const BUF_OFF: u64 = OFF_RX_BUFS;
/// De maat van het bufferstuk: de twee blokken van RX en TX.
pub const BUF_BLOCK: u64 = DMA_NEED - BUF_OFF;

const _: () = {
    assert!((N_RX + N_TX) * 16 <= OFF_RX_BUFS as usize);
    assert!(N_RX * BUF <= (OFF_TX_BUFS - OFF_RX_BUFS) as usize);
    assert!(N_TX * BUF <= (DMA_NEED - OFF_TX_BUFS) as usize);
    // Elke buffer op eigen cachelijnen: een veeg van de ene raakt de
    // andere niet.
    assert!((BUF as u64).is_multiple_of(dev::LINE));
};

/// De MDIO-pollgrens: enkele µs typisch, ruim begrensd.
const MDIO_POLLS: u32 = 100_000;

/// Waarom de GEM iets weigert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// De DMA-regio is te klein.
    Dma {
        /// Wat er is.
        have: u64,
        /// Wat er nodig is.
        need: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Dma { have, need } => write!(f, "gem: DMA region {have:#x} < {need:#x}"),
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Het registerblok op `base`.
///
/// # Safety
///
/// `base` is het gemapte GEM-blok, voor altijd (de voorwaarde van
/// [`Gem::new`]).
unsafe fn regs(base: Pa) -> &'static Regs {
    // SAFETY: de voorwaarde van deze functie.
    unsafe { dev::regs(base) }
}

/// De eerste vier RX-descriptors (w0, w1) zoals de CPU ze nu leest, voor de
/// diagnose: staat OWNED (bit 0 van w0) waar de hardware al vulde? `ring`
/// is de RX-ring van een levende `Gem` (`rx_ring` uit `new`).
#[must_use]
pub fn ring_words(ring: Pa) -> [u32; 8] {
    let mut out = [0u32; 8];
    for i in 0..4 {
        out[2 * i] = dev::read32(ring.add((i * 16) as u64));
        out[2 * i + 1] = dev::read32(ring.add((i * 16 + 4) as u64));
    }
    out
}

/// Registerdump voor de diagnose (de flip-jacht van 30-09 op de Pi 5):
/// NWCTRL, NWCFG, NWSTATUS, DMACFG, TXSTATUS, RXQBASE, RXSTATUS, ISR, IMR.
///
/// # Safety
///
/// `base` is het gemapte GEM-blok (de voorwaarde van `Gem::new`).
#[must_use]
pub unsafe fn diag(base: Pa) -> [u32; 9] {
    // SAFETY: de voorwaarde van deze functie.
    let r = unsafe { regs(base) };
    [
        r.nwctrl.read(),
        r.nwcfg.read(),
        r.nwstatus.read(),
        r.dmacfg.read(),
        r.txstatus.read(),
        r.rxqbase.read(),
        r.rxstatus.read(),
        r.isr.read(),
        r.imr.read(),
    ]
}

/// Waar de RX-queue-pointer (RXQBASE gelezen: de descriptor die de DMA nu
/// heeft) in de ring staat waarvan de eerste descriptor op busadres
/// `ring_bus` ligt: de index, of `None` als hij erbuiten wijst (de ring van
/// een vorige eigenaar, of een DMA die zijn nieuwe basis nooit oppakte).
#[must_use]
pub fn rx_index(rxqbase: u32, ring_bus: u64) -> Option<usize> {
    let off = rxqbase.wrapping_sub(ring_bus as u32) as usize;
    (off.is_multiple_of(16) && off / 16 < N_RX).then_some(off / 16)
}

/// Zet de GEM stil: RX en TX uit, de tellers en alle statusbits gewist,
/// elke interrupt dicht en de latch leeg (Linux `macb_reset_hw`, ook de
/// eerste stap van [`Gem::init`]). Ook voor een GEM die een vorige kern liet
/// draaien: zijn DMA hoort stil te staan vóór iemand de PCIe-link eronder
/// reset, zoals `macb_shutdown` op Linux' kexec-weg (03-10).
///
/// # Safety
///
/// `base` is het gemapte GEM-blok (de voorwaarde van [`Gem::new`]).
pub unsafe fn stop(base: Pa) {
    // SAFETY: de voorwaarde van deze functie.
    let r = unsafe { regs(base) };
    r.nwctrl.write(0);
    r.nwctrl.write(CTRL_CLR_STAT);
    r.idr.write(u32::MAX);
    // De ISR van de RP1-GEM wist bij schrijven (`ack_irq`); Linux schrijft
    // dan ook alle bits (MACB_CAPS_ISR_CLEAR_ON_WRITE).
    r.isr.write(u32::MAX);
    r.txstatus.write(u32::MAX);
    r.rxstatus.write(u32::MAX);
    r.pbuf_rx_cut.write(0);
}

/// De device-ack van de dispatcher: masker dicht (IDR) en de latch gewist.
///
/// Alleen wissen is niet genoeg: zolang de ring werk heeft, zet de GEM RCOMP
/// meteen weer. En altijd de expliciete bits schrijven, nooit de
/// teruggelezen waarde: de ISR van deze GEM is "clear on write" (RP1,
/// DCFG1.IRQCOR=0); een gemaskeerd bit leest als 0 maar blijft gelatcht, en
/// bij IER kwam de lijn meteen terug: 134k interrupts/s op een stille Pi 5
/// (bundels 1-5 en 11, 20-09).
///
/// # Safety
///
/// `base` is het gemapte GEM-blok van een [`Gem`] die leeft.
pub unsafe fn ack_irq(base: Pa) {
    // SAFETY: de voorwaarde van deze functie.
    let r = unsafe { regs(base) };
    r.idr.write(INT_LINE);
    r.isr.write(INT_LINE);
}

/// Eén GEM.
pub struct Gem {
    base: Pa,
    bus_off: u64,
    mac: [u8; 6],
    rx_ring: Pa,
    tx_ring: Pa,
    rx_bufs: Pa,
    tx_bufs: Pa,
    dma_size: u64,
    rx_head: usize,
    tx_head: usize,
    irq: Option<&'static Signal>,
    /// De board-kant van de rearm (RP1: de MSI-X IACK).
    rearm: Option<fn()>,
}

impl Gem {
    /// Een GEM op `base`, met zijn ringen en buffers in `[dma, dma +
    /// dma_size)` en busadres = fysiek adres + `bus_off`. Raakt nog niets
    /// aan.
    ///
    /// # Safety
    ///
    /// `base` is het gemapte GEM-blok; de DMA-regio is van deze driver
    /// alleen en de GEM bereikt hem op fysiek + `bus_off`. De descriptors
    /// (`[dma, dma + BUF_OFF)`) zijn Normal-NC gemapt, het bufferstuk erna
    /// Normal-NC of Normal-WB (de driver veegt het), nooit Device: de
    /// frames gaan er met `memcpy` in en uit. Beide blijven zolang de
    /// driver leeft.
    #[must_use]
    pub const unsafe fn new(base: Pa, bus_off: u64, dma: Pa, dma_size: u64, mac: [u8; 6]) -> Self {
        Self {
            base,
            bus_off,
            mac,
            rx_ring: dma,
            tx_ring: dma.add((N_RX * 16) as u64),
            rx_bufs: dma.add(OFF_RX_BUFS),
            tx_bufs: dma.add(OFF_TX_BUFS),
            dma_size,
            rx_head: 0,
            tx_head: 0,
            irq: None,
            rearm: None,
        }
    }

    fn r(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van `new`.
        unsafe { regs(self.base) }
    }

    /// Alleen de management-poort aan: MDIO-scan zonder verder iets te
    /// initialiseren.
    pub fn mdio_enable(&mut self) {
        self.r().nwcfg.write(CFG_MDC_DIV48);
        self.r().nwctrl.write(CTRL_MGMT_EN);
    }

    fn mdio_wait(&self) -> bool {
        let s = &self.r().nwstatus;
        (0..MDIO_POLLS).any(|_| s.read() & STATUS_MDIO_IDLE != 0)
    }

    /// Zet ringen en MAC-config klaar en zet RX en TX aan, op de snelheid
    /// die de autonegotiatie afsprak; de RP1-CLKGEN volgt de MAC-snelheid
    /// vanzelf. Het volledige `macb_reset_hw`-recept vooraf.
    pub fn init(&mut self, link: Link) -> Result {
        if self.dma_size < DMA_NEED {
            return Err(Error::Dma {
                have: self.dma_size,
                need: DMA_NEED,
            });
        }
        // SAFETY: de voorwaarde van `new`.
        unsafe { stop(self.base) };
        let r = self.r();

        // Eerst geen regel van de buffers meer in de cache (een vorige kern
        // kan er een achtergelaten hebben): niets vuils dat later over een
        // frame van de DMA heen valt. De GEM staat stil.
        dev::pull(self.rx_bufs, N_RX * BUF);
        dev::pull(self.tx_bufs, N_TX * BUF);
        // RX-ring: elke descriptor wijst naar zijn eigen buffer; de DMA is
        // eigenaar.
        for i in 0..N_RX {
            let d = self.rx_ring.add((i * 16) as u64);
            let bus = self.rx_bufs.add((i * BUF) as u64).0 + self.bus_off;
            let mut w0 = (bus as u32) & !0b11;
            if i == N_RX - 1 {
                w0 |= RX_WRAP;
            }
            dev::write32(d, w0);
            dev::write32(d.add(4), 0);
            dev::write32(d.add(8), (bus >> 32) as u32);
            dev::write32(d.add(12), 0);
        }
        // TX-ring: alles aan software (USED).
        for i in 0..N_TX {
            let d = self.tx_ring.add((i * 16) as u64);
            let mut w1 = TX_USED;
            if i == N_TX - 1 {
                w1 |= TX_WRAP;
            }
            dev::write32(d, 0);
            dev::write32(d.add(4), w1);
            dev::write32(d.add(8), 0);
            dev::write32(d.add(12), 0);
        }
        dev::mb();
        self.rx_head = 0;
        self.tx_head = 0;

        let m = self.mac;
        r.spaddr1b
            .write(u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
        r.spaddr1t.write(u32::from_le_bytes([m[4], m[5], 0, 0]));
        r.nwcfg.write(cfg_for(r.dcfg1.read(), link));
        // RP1 (Linux `gem_init_axi`, kernel-breed alléén voor de RP1): de
        // outstanding AXI-reads/writes van de GEM-DMA op 8/8 en writes tellen
        // tot de B-response. De DMA steekt hier een AXI-naar-PCIe-brug over;
        // zonder rem zet sustained RX-DMA de brug vol en valt de hele fabric
        // stil (de stille totale freeze, jacht van 13-07).
        r.amp.update(|v| (v & !0x1_ffff) | AMP_RP1);
        r.dmacfg.write(
            DMA_BURST_INCR16
                | DMA_TX_PBUF_FULL
                | DMA_RX_PBUF_FULL
                | ((BUF / 64) as u32) << DMA_RX_SIZE_SHIFT
                | DMA_ADDR64,
        );
        let rx = self.rx_ring.0 + self.bus_off;
        let tx = self.tx_ring.0 + self.bus_off;
        r.rxqbase.write(rx as u32);
        r.rbqph.write((rx >> 32) as u32);
        r.txqbase.write(tx as u32);
        r.tbqph.write((tx >> 32) as u32);
        r.nwctrl.write(CTRL_MGMT_EN | CTRL_TX_EN | CTRL_RX_EN);
        Ok(())
    }

    /// Hangt de NIC aan zijn lijn: `bell` is het signaal van de dispatcher,
    /// `rearm` de board-kant van het heropenen (RP1: IACK). Opent de
    /// interrupts van de lijn.
    pub fn set_irq(&mut self, bell: &'static Signal, rearm: fn()) {
        self.irq = Some(bell);
        self.rearm = Some(rearm);
        let r = self.r();
        r.isr.write(INT_LINE);
        r.ier.write(INT_LINE);
    }

    /// Latch gewist, de board-ack (met het masker nog dicht, zodat de RP1
    /// een lage lijn bemonstert), dan het masker open.
    fn rearm_irq(&self) {
        let Some(board) = self.rearm else { return };
        let r = self.r();
        r.isr.write(INT_LINE);
        board();
        r.ier.write(INT_LINE);
    }

    /// Linux' `macb_tx_restart`: staat de zender stil (TGO laag) terwijl de
    /// laatst gegeven descriptor nog van de hardware is, dan opnieuw
    /// TSTART. RP1-eigenaardigheid (macb_main.c, "TSTART write might get
    /// dropped"): over de PCIe-backed AXI kan de TSTART-schrijf verloren
    /// gaan terwijl de DMA net stopt. Linux doet dit op de TXUBR-interrupt;
    /// hier bij een volle ring en bij elke lege RX-ronde van de pomp, en
    /// TXUBR zit op de lijn, dus de stilgevallen zender wekt die ronde zelf.
    /// Elke volgende TSTART haalt ook alles mee wat ervoor bleef liggen.
    ///
    /// Tot 04-10 wachtte `transmit` hier per frame tot de descriptor terug
    /// was, in stappen van 10 µs: één frame tegelijk op de draad, 55 MB/s uit
    /// tegen 111 in op de Pi 5.
    fn tx_restart(&self) {
        let last = (self.tx_head + N_TX - 1) % N_TX;
        let d = self.tx_ring.add((last * 16) as u64);
        if dev::read32(d.add(4)) & TX_USED == 0 && self.r().txstatus.read() & TX_GO == 0 {
            self.r().nwctrl.write(TX_KICK);
        }
    }
}

/// NWCFG voor een link, met de busbreedte uit DCFG1.DBWDEF (4 = 128,
/// 2 = 64, anders 32 bits).
fn cfg_for(dcfg1: u32, link: Link) -> u32 {
    let dbw = match (dcfg1 >> 25) & 0x7 {
        d if d >= 4 => 2,
        d if d >= 2 => 1,
        _ => 0,
    };
    let mut cfg = CFG_MDC_DIV48 | CFG_RX_FCS | CFG_PAE | (dbw << CFG_DBW_SHIFT);
    if link.full_duplex {
        cfg |= CFG_FD;
    }
    match link.mbps {
        1000 => cfg |= CFG_GIGABIT,
        100 => cfg |= CFG_SPEED100,
        _ => {}
    }
    cfg
}

impl Mdio for Gem {
    fn read(&mut self, phy: u8, reg: u8) -> driver_mdio::Result<u16> {
        if !self.mdio_wait() {
            return Err(driver_mdio::Error::Bus { phy, reg });
        }
        self.r().man.write(
            MAN_CLAUSE22
                | MAN_READ
                | (u32::from(phy & 0x1f) << 23)
                | (u32::from(reg & 0x1f) << 18)
                | MAN_MUST_BE_10,
        );
        if !self.mdio_wait() {
            return Err(driver_mdio::Error::Bus { phy, reg });
        }
        Ok(self.r().man.read() as u16)
    }

    fn write(&mut self, phy: u8, reg: u8, val: u16) -> driver_mdio::Result {
        if !self.mdio_wait() {
            return Err(driver_mdio::Error::Bus { phy, reg });
        }
        self.r().man.write(
            MAN_CLAUSE22
                | MAN_WRITE
                | (u32::from(phy & 0x1f) << 23)
                | (u32::from(reg & 0x1f) << 18)
                | MAN_MUST_BE_10
                | u32::from(val),
        );
        if !self.mdio_wait() {
            return Err(driver_mdio::Error::Bus { phy, reg });
        }
        Ok(())
    }
}

impl netdev::Device for Gem {
    /// Eén frame op de ring en TSTART, zonder op de draad te wachten. Een
    /// descriptor die de DMA nog heeft is een volle ring, geen wachtlus.
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), TxError> {
        if frame.is_empty() || frame.len() > BUF {
            return Err(TxError::Size(frame.len()));
        }
        let d = self.tx_ring.add((self.tx_head * 16) as u64);
        if dev::read32(d.add(4)) & TX_USED == 0 {
            self.tx_restart();
            return Err(TxError::Full);
        }
        let dst = self.tx_bufs.add((self.tx_head * BUF) as u64);
        dev::copy_in_normal(dst, frame);
        dev::push(dst, frame.len());
        let bus = dst.0 + self.bus_off;
        dev::write32(d, bus as u32);
        dev::write32(d.add(8), (bus >> 32) as u32);
        let mut w1 = frame.len() as u32 | TX_LAST;
        if self.tx_head == N_TX - 1 {
            w1 |= TX_WRAP;
        }
        dev::mb();
        dev::write32(d.add(4), w1);
        dev::mb();
        self.r().nwctrl.write(TX_KICK);
        self.tx_head = (self.tx_head + 1) % N_TX;
        Ok(())
    }

    /// Eén frame, of `None` (en dan de rearm: de ring is leeg, dus de lijn
    /// mag weer). Alleen een compleet frame in één buffer; een ander gaat
    /// terug naar de DMA.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let d = self.rx_ring.add((self.rx_head * 16) as u64);
            let w0 = dev::read32(d);
            if w0 & RX_OWNED == 0 {
                self.tx_restart();
                self.rearm_irq();
                return None;
            }
            dev::mb();
            let status = dev::read32(d.add(4));
            let got = rx_len(status).map(|n| n.min(buf.len()));
            if let Some(n) = got
                && let Some(dst) = buf.get_mut(..n)
            {
                let src = self.rx_bufs.add((self.rx_head * BUF) as u64);
                dev::pull(src, n);
                dev::copy_out_normal(dst, src);
            }
            // Terug aan de DMA: het adres blijft, alleen OWNED eraf.
            dev::write32(d.add(4), 0);
            dev::mb();
            dev::write32(d, w0 & !RX_OWNED);
            dev::mb();
            self.rx_head = (self.rx_head + 1) % N_RX;
            if got.is_some() {
                return got;
            }
        }
    }

    fn mac(&self) -> Mac {
        Mac(self.mac)
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }
}

/// De framelengte uit RX-woord 1; `None` voor een frame dat niet compleet
/// is of niet in één buffer past.
fn rx_len(status: u32) -> Option<usize> {
    let len = (status & RX_LEN_MASK) as usize;
    (status & (RX_SOF | RX_EOF) == RX_SOF | RX_EOF && len > 0 && len <= BUF).then_some(len)
}
