//! De Intel igb-familie: de I210 (8086:1533) van de Ampere Altra Dev Kit, en
//! QEMU's 82576-model (8086:10c9, `-device igb`) waarmee dit pad end-to-end
//! getest wordt.
//!
//! Geschreven naar het I210-datasheet met de Linux igb-driver als referentie
//! (`OLD/metal/driver/nic/igb`, sinds 13-07 het Altra-recept): één RX- en
//! één TX-queue met advanced descriptors (het enige type dat Linux gebruikt
//! én dat QEMU's model emuleert), getypeerde registers op BAR0, en de ringen
//! in een DMA-regio die het board uitdeelt. De firmware (UEFI) heeft de BAR
//! al toegewezen; het board leest hem uit en zet bus-mastering aan, de
//! driver kent geen PCI.
//!
//! Geschreven vóór het eerste Altra-contact (juli): de registerlaag is tegen
//! QEMU's igb bewezen (gedeelde 82575+-familie); de I210-aannames (interne
//! PHY-autoneg na reset, NVM-autoload van het MAC) staan gemarkeerd.
//!
//! De driver is een actor-onderdeel: wie `&mut self` heeft, is de enige die
//! de ringen aanraakt. Doorbells (RDT, TDT) vallen in
//! [`flush`](netdev::Device::flush), één keer per burst.
//!
//! De interrupt is optioneel en alleen MSI-X: het board zet de MSI-X-tabel
//! (vector 0) en de capability aan, en daarna zet [`Igb::set_irq`] de NIC
//! in de MSI-X-modus van Linux `igb_configure_msix`, met één vector voor
//! RX- en TX-queue 0, het queuepaar van één `q_vector` (zie daar): een
//! ontvangen frame en een TX-writeback (RS staat op elke descriptor) trekken
//! dezelfde lijn. Zonder `set_irq` blijft alles dicht.

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
use core::mem::{offset_of, size_of};
use dev::{Pa, Reg};
use driver_mdio::{self as mdio, Link, Mdio};
use netdev::{Mac, TxError};
use sync::Signal;

/// De registers van de 82575+-familie (Linux `e1000_regs.h`): 82576 in
/// QEMU, I210/I211 op de Altra. De queue-registers zijn de queue-0-aliassen
/// op 0x2800 en 0x3800.
#[repr(C)]
struct Regs {
    ctrl: Reg<u32>,
    _r0: u32,
    status: Reg<u32>,
    _r1: [u32; 5],
    /// MDI control: PHY-registertoegang.
    mdic: Reg<u32>,
    _r2: [u32; 55],
    rctl: Reg<u32>,
    _r3: [u32; 191],
    tctl: Reg<u32>,
    _r4: [u32; 1087],
    /// Interrupt cause: lezen wist.
    icr: Reg<u32>,
    _ics: Reg<u32>,
    _ims: Reg<u32>,
    /// Interrupt mask clear.
    imc: Reg<u32>,
    _iam: u32,
    /// General purpose interrupt enable: de MSI-X-modus.
    gpie: Reg<u32>,
    _r5: [u32; 2],
    /// Extended interrupt cause set: een vector met de hand afvuren.
    eics: Reg<u32>,
    /// Extended interrupt mask set.
    eims: Reg<u32>,
    /// Extended interrupt mask clear.
    eimc: Reg<u32>,
    /// Extended interrupt auto clear: EICR-bits wissen bij het bericht.
    eiac: Reg<u32>,
    /// Extended interrupt auto mask: EIMS-bits wissen bij het bericht.
    eiam: Reg<u32>,
    _r5b: [u32; 19],
    /// Extended interrupt cause read (lezen wist met GPIE.NSICR).
    eicr: Reg<u32>,
    _r5d: [u32; 95],
    /// IVAR0: welke vector RX- en TX-queue 0 en 1 krijgen.
    ivar0: Reg<u32>,
    _r5c: [u32; 1087],
    rdbal: Reg<u32>,
    rdbah: Reg<u32>,
    rdlen: Reg<u32>,
    srrctl: Reg<u32>,
    rdh: Reg<u32>,
    _r6: u32,
    rdt: Reg<u32>,
    _r7: [u32; 3],
    rxdctl: Reg<u32>,
    _r8: [u32; 1013],
    tdbal: Reg<u32>,
    tdbah: Reg<u32>,
    tdlen: Reg<u32>,
    _r9: u32,
    tdh: Reg<u32>,
    _r10: u32,
    tdt: Reg<u32>,
    _r11: [u32; 3],
    txdctl: Reg<u32>,
    _r12: [u32; 1781],
    /// Het MAC-adres (NVM-autoload na reset).
    ral0: Reg<u32>,
    rah0: Reg<u32>,
}

const _: () = {
    assert!(offset_of!(Regs, ctrl) == 0x0000);
    assert!(offset_of!(Regs, status) == 0x0008);
    assert!(offset_of!(Regs, mdic) == 0x0020);
    assert!(offset_of!(Regs, rctl) == 0x0100);
    assert!(offset_of!(Regs, tctl) == 0x0400);
    assert!(offset_of!(Regs, icr) == 0x1500);
    assert!(offset_of!(Regs, imc) == 0x150c);
    assert!(offset_of!(Regs, gpie) == 0x1514);
    assert!(offset_of!(Regs, eics) == 0x1520);
    assert!(offset_of!(Regs, eims) == 0x1524);
    assert!(offset_of!(Regs, eimc) == 0x1528);
    assert!(offset_of!(Regs, eiac) == 0x152c);
    assert!(offset_of!(Regs, eiam) == 0x1530);
    assert!(offset_of!(Regs, eicr) == 0x1580);
    assert!(offset_of!(Regs, ivar0) == 0x1700);
    assert!(offset_of!(Regs, rdbal) == 0x2800);
    assert!(offset_of!(Regs, rdbah) == 0x2804);
    assert!(offset_of!(Regs, rdlen) == 0x2808);
    assert!(offset_of!(Regs, srrctl) == 0x280c);
    assert!(offset_of!(Regs, rdh) == 0x2810);
    assert!(offset_of!(Regs, rdt) == 0x2818);
    assert!(offset_of!(Regs, rxdctl) == 0x2828);
    assert!(offset_of!(Regs, tdbal) == 0x3800);
    assert!(offset_of!(Regs, tdbah) == 0x3804);
    assert!(offset_of!(Regs, tdlen) == 0x3808);
    assert!(offset_of!(Regs, tdh) == 0x3810);
    assert!(offset_of!(Regs, tdt) == 0x3818);
    assert!(offset_of!(Regs, txdctl) == 0x3828);
    assert!(offset_of!(Regs, ral0) == 0x5400);
    assert!(offset_of!(Regs, rah0) == 0x5404);
};

/// Hoeveel van BAR0 het board moet mappen: het registerblok hierboven.
pub const MMIO_LEN: u64 = size_of::<Regs>() as u64;

/// Eén advanced descriptor, als vier woorden. RX-lees: `addr` is het
/// pakketadres, `w2`/`w3` (hdr_addr) nul. RX-writeback: `w2` is de status,
/// `w3[15:0]` de lengte. TX: `w2` is cmd_type_len, `w3` olinfo (writeback:
/// DD in bit 0).
#[repr(C)]
struct Desc {
    addr_lo: u32,
    addr_hi: u32,
    w2: u32,
    w3: u32,
}

const _: () = {
    assert!(size_of::<Desc>() == 16);
    assert!(offset_of!(Desc, addr_lo) == 0);
    assert!(offset_of!(Desc, addr_hi) == 4);
    assert!(offset_of!(Desc, w2) == 8);
    assert!(offset_of!(Desc, w3) == 12);
};
const DESC: u64 = size_of::<Desc>() as u64;
const W2: u64 = offset_of!(Desc, w2) as u64;
const W3: u64 = offset_of!(Desc, w3) as u64;

// CTRL.
/// Set link up: verplicht vóór de MAC een link meldt.
const CTRL_SLU: u32 = 1 << 6;
/// Device-reset (zelfwissend).
const CTRL_RST: u32 = 1 << 26;
// STATUS.
const STATUS_FD: u32 = 1 << 0;
const STATUS_LU: u32 = 1 << 1;
const STATUS_SPEED_SHIFT: u32 = 6;
// MDIC: DATA [15:0], REGADD [20:16], PHYADD [25:21], OP [27:26] (01 write,
// 10 read), READY bit 28, ERROR bit 30.
const MDIC_OP_WRITE: u32 = 0b01 << 26;
const MDIC_OP_READ: u32 = 0b10 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
/// De interne PHY van de igb-familie antwoordt op adres 1 (Linux
/// `hw->phy.addr`).
pub const PHY_ADDR: u8 = 1;
// RCTL: enable, broadcast accepteren (DHCP), FCS strippen.
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
// TCTL: enable, korte pakketten padden, en de datasheet-defaults voor de
// collision-velden (CT=15, COLD=0x3f), die Linux ook schrijft.
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x0f << 4;
const TCTL_COLD: u32 = 0x3f << 12;
// SRRCTL: buffergrootte in KB [6:0], descriptortype [27:25] = 001 (advanced,
// één buffer).
const SRRCTL_BSIZE_2K: u32 = 2;
const SRRCTL_DESC_ADV1: u32 = 0b001 << 25;
// RXDCTL/TXDCTL.
const Q_ENABLE: u32 = 1 << 25;
// RX-writeback.
const RX_DD: u32 = 1 << 0;
const RX_EOP: u32 = 1 << 1;
// TX: lengte [17:0], DTYP data (0011 << 20), DCMD EOP, IFCS (FCS door de
// MAC), RS (writeback DD), DEXT (advanced); olinfo: payload << 14.
const TX_DTYP_DATA: u32 = 0b0011 << 20;
const TX_EOP: u32 = 1 << 24;
const TX_IFCS: u32 = 1 << 25;
const TX_RS: u32 = 1 << 27;
const TX_DEXT: u32 = 1 << 29;
const TX_PAY_SHIFT: u32 = 14;
const TX_DD: u32 = 1 << 0;
// GPIE (Linux `e1000_defines.h`): de vier bits die `igb_configure_msix`
// zet. NSICR: ICR lezen wist alles; MSIX_MODE: de causes gaan via IVAR naar
// EICR; EIAME: EIAM maskeert bij het bericht; PBA: de pending-bits in de
// MSI-X-PBA.
const GPIE_NSICR: u32 = 1 << 0;
const GPIE_MSIX_MODE: u32 = 1 << 4;
const GPIE_EIAME: u32 = 1 << 30;
const GPIE_PBA: u32 = 1 << 31;
/// Wat `set_irq` in GPIE schrijft.
pub const GPIE_MSIX: u32 = GPIE_MSIX_MODE | GPIE_PBA | GPIE_EIAME | GPIE_NSICR;
/// `E1000_IVAR_VALID`: het allocatiebit naast het vectornummer.
const IVAR_VALID: u32 = 0x80;
/// De MSI-X-vector van queue 0 (RX en TX), en de enige: entry 0 van de MSI-X-tabel
/// (die het board zet), dus EventID 0 bij de ITS.
pub const Q_VECTOR: u32 = 0;
/// Het bit van [`Q_VECTOR`] in EICR/EIMS/EIMC/EIAC/EIAM/EICS.
pub const Q_EIMS: u32 = 1 << Q_VECTOR;

/// Eén RX- of TX-buffer: de SRRCTL-eenheid, ruim boven 1522.
pub const BUF_SIZE: usize = 2048;
/// RX-descriptors. 256, niet 64: gepold om de 300 µs komen op 1 Gbit tot
/// ~80 frames per ronde binnen, en een ring van 64 liep over (inbound 5
/// MB/s op de Altra tegen 37 gepold op de O6N met zijn ring van 256; 19-09,
/// L83). RDLEN blijft een veelvoud van 128.
pub const N_RX: u16 = 256;
/// TX-descriptors.
pub const N_TX: u16 = 64;
/// Na hoeveel ontvangen frames de driver zelf de RX-doorbell luidt: een
/// wrapper die door een burst heen lust zonder naar de pomp terug te keren,
/// liet de NIC anders zonder descriptors (bundel 83: 36 naar 1 MB/s toen
/// alleen de pomp flushte).
pub const RX_SELF_FLUSH: u16 = 32;
/// De frame-buffers beginnen in een eigen blok van 2 MB, los van de ringen:
/// het coherent/streaming-onderscheid van Linux, zodat een board het
/// bufferblok cacheable mag mappen terwijl de gepolde descriptors device
/// blijven.
pub const BUF_OFF: u64 = 2 << 20;
/// Wat de driver van de DMA-regio vraagt.
pub const DMA_NEED: u64 = BUF_OFF + (N_RX as u64 + N_TX as u64) * BUF_SIZE as u64;
const RX_RING_OFF: u64 = 0;
const TX_RING_OFF: u64 = N_RX as u64 * DESC;

const _: () = {
    assert!((N_RX as u64 * DESC).is_multiple_of(128));
    assert!((N_TX as u64 * DESC).is_multiple_of(128));
    assert!(TX_RING_OFF + N_TX as u64 * DESC <= BUF_OFF);
    assert!(BUF_SIZE >= netdev::MAX_FRAME);
};

/// De pauze na CTRL.RST vóór de eerste blik op het register (Go: 10 ms,
/// zoals Linux' `igb_reset_hw_82575` met zijn `msleep(10)`): wie RAL0/RAH0
/// direct leest als het bit zakt, kan de NVM-autoload voor zijn.
const RESET_PAUSE_NS: u64 = 10_000_000;
/// Hoe lang een device-reset daarna nog mag duren (Go: 100 × 1 ms).
const RESET_NS: u64 = 100_000_000;
/// Hoe lang een MDIC-transactie mag duren.
const MDIC_NS: u64 = 10_000_000;
/// Hoe lang de queue-enable mag duren.
const QUEUE_NS: u64 = 10_000_000;

/// De igb-familieleden die deze driver aankan (vendor 0x8086).
pub const DEVICE_IDS: [u16; 6] = [
    0x10c9, // 82576 (QEMU)
    0x1533, // I210 koper
    0x1536, // I210 fiber
    0x1537, // I210 serdes
    0x1538, // I210 sgmii
    0x1539, // I211
];
/// Intel.
pub const VENDOR: u16 = 0x8086;

/// Drijft deze driver `vendor:device`?
#[must_use]
pub fn supported(vendor: u16, device: u16) -> bool {
    vendor == VENDOR && DEVICE_IDS.contains(&device)
}

/// Waarom de driver weigert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Het registerblok leest alles-enen: het device is niet op de bus.
    OffBus,
    /// De DMA-regio is te klein.
    DmaTooSmall {
        /// Wat nodig was.
        need: u64,
        /// Wat er was.
        have: u64,
    },
    /// De DMA-basis is niet op 2 MB gealigneerd.
    DmaAlign(u64),
    /// CTRL.RST wist zichzelf niet.
    ResetStuck {
        /// CTRL na de grens.
        ctrl: u32,
    },
    /// RAL0/RAH0 zijn leeg: geen NVM, of een lege.
    NoMac,
    /// Een queue kwam niet aan.
    QueueStuck {
        /// 0 = RX, 1 = TX.
        tx: bool,
    },
    /// De PHY-laag faalde.
    Phy(mdio::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::OffBus => f.write_str("igb: registers read all-ones (device off the bus?)"),
            Self::DmaTooSmall { need, have } => {
                write!(f, "igb: DMA region too small ({need} > {have} bytes)")
            }
            Self::DmaAlign(b) => write!(f, "igb: DMA base {b:#x} not 2 MB aligned"),
            Self::ResetStuck { ctrl } => write!(f, "igb: reset stuck (CTRL={ctrl:#x})"),
            Self::NoMac => f.write_str("igb: no MAC in RAL0/RAH0 (empty NVM?)"),
            Self::QueueStuck { tx } => {
                write!(
                    f,
                    "igb: {} queue did not enable",
                    if tx { "TX" } else { "RX" }
                )
            }
            Self::Phy(e) => write!(f, "igb: {e}"),
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// IVAR0 met RX- en TX-queue 0 op `vector`: Linux `igb_write_ivar(hw,
/// vector, 0, 0)` en `(hw, vector, 0, 8)`, gelijk op de I210 (rij-gewijs)
/// en de 82576 (kolom-gewijs), want voor queue 0 is de index bij beide 0
/// en de offset 0 (RX) en 8 (TX). De rest (de queues van 1) blijft staan.
#[must_use]
pub const fn ivar_q0(ivar: u32, vector: u32) -> u32 {
    let v = (vector & 0x1f) | IVAR_VALID;
    (ivar & !0xffff) | v | (v << 8)
}

/// Het interrupt-pad: alleen EIMC. `Copy`, zodat het board hem naast de
/// driver houdt voor de dispatch.
#[derive(Clone, Copy)]
pub struct IrqAck {
    base: Pa,
}

impl IrqAck {
    fn regs(&self) -> &'static Regs {
        // SAFETY: `base` kwam uit `Igb::new`, dat een gemapt blok eiste;
        // EIMC deelt niets met de ringen.
        unsafe { dev::regs(self.base) }
    }
}

impl netdev::IrqAck for IrqAck {
    /// Masker van de vector dicht, uit de dispatch vóór de EOI. EIAM
    /// deed dat bij het bericht al (`GPIE.EIAME`); dit is dezelfde stand
    /// zonder op dat bit te leunen, één geposte schrijf. Pas de pomp
    /// heropent hem, bij een lege ring (`receive`), het ritme van NAPI
    /// (`igb_msix_ring`, dan `igb_poll`, dan `igb_ring_irq_enable`).
    fn ack(&self) {
        self.regs().eimc.write(Q_EIMS);
    }
}

/// De MSI-X-stand van de NIC na een vector die niet aankwam. EIMS 0 en
/// EICR 0 na EICS: EIAM en EIAC deden hun werk, de NIC zond het bericht.
/// EIMS 1 en EICR 1: hij zond niet (GPIE niet blijven staan, functie
/// gemaskeerd).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqRegs {
    /// GPIE.
    pub gpie: u32,
    /// IVAR0.
    pub ivar0: u32,
    /// EIMS (het masker).
    pub eims: u32,
    /// EICR.
    pub eicr: u32,
}

impl fmt::Display for IrqRegs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GPIE {:#x} IVAR0 {:#x} EIMS {:#x} EICR {:#x}",
            self.gpie, self.ivar0, self.eims, self.eicr
        )
    }
}

/// Eén igb.
pub struct Igb {
    base: Pa,
    mac: Mac,
    now: fn() -> u64,
    rx_ring: Pa,
    tx_ring: Pa,
    rx_bufs: Pa,
    tx_bufs: Pa,
    rx_head: u16,
    /// De laatst herwapende RX-descriptor die nog niet in RDT staat.
    rx_tail: Option<u16>,
    rx_since: u16,
    tx_head: u16,
    /// TX-descriptors klaargezet sinds de laatste doorbell.
    tx_pending: u16,
    /// De meetlat; `doorbells` telt RDT en TDT samen.
    stats: netdev::Stats,
    /// De bel van de MSI-X-vector, als het board er een bedraadde.
    irq: Option<&'static Signal>,
}

impl Igb {
    /// Reset het device, leest het MAC, en zet de ringen klaar in `dma`
    /// (RX en TX aan, interrupts dicht). `now` geeft monotone
    /// nanoseconden. De link start daarna met [`start_link`](Self::start_link)
    /// en komt in de taak ([`netdev::Device::link`]).
    ///
    /// # Safety
    ///
    /// `base` is BAR0 van een igb, gemapt als Device voor minstens
    /// [`MMIO_LEN`] bytes en voor altijd; memory-decode en bus-mastering
    /// staan aan. `[dma, dma+dma_size)` is gemapt geheugen dat alleen deze
    /// driver en het device gebruiken, nu en zolang het programma draait, op
    /// een adres dat het device ziet zoals de CPU (geen IOMMU-vertaling).
    pub unsafe fn new(base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        if dma_size < DMA_NEED {
            return Err(Error::DmaTooSmall {
                need: DMA_NEED,
                have: dma_size,
            });
        }
        if !dma.is_aligned(BUF_OFF) {
            return Err(Error::DmaAlign(dma.0));
        }
        let mut n = Self::at(base, dma, now);
        n.reset()?;
        n.init()?;
        Ok(n)
    }

    /// De staat zonder één registertoegang (ook voor de tests).
    fn at(base: Pa, dma: Pa, now: fn() -> u64) -> Self {
        let bufs = dma.add(BUF_OFF);
        Self {
            base,
            mac: Mac::default(),
            now,
            rx_ring: dma.add(RX_RING_OFF),
            tx_ring: dma.add(TX_RING_OFF),
            rx_bufs: bufs,
            tx_bufs: bufs.add(u64::from(N_RX) * BUF_SIZE as u64),
            rx_head: 0,
            rx_tail: None,
            rx_since: 0,
            tx_head: 0,
            tx_pending: 0,
            stats: netdev::Stats::default(),
            irq: None,
        }
    }

    /// Het interrupt-pad, voor het board.
    #[must_use]
    pub fn irq_ack(&self) -> IrqAck {
        IrqAck { base: self.base }
    }

    /// Hangt de bel aan de driver en zet de NIC in de MSI-X-modus met één
    /// vector, `igb_configure_msix` plus `igb_irq_enable` voor één queue:
    /// GPIE, IVAR0 voor RX- en TX-queue 0 op [`Q_VECTOR`], dan EIAC, EIAM en EIMS
    /// voor die vector. Geen "other"-vector: de link-wissel en de rest van
    /// ICR blijven dicht (IMS 0, IVAR_MISC ongeldig), zoals gepold. Geen
    /// EITR: de reset-stand 0 is geen demping, de pomp is de demping (het
    /// masker blijft dicht tot de ring leeg is).
    ///
    /// Aanroepen nadat het board de MSI-X-capability aanzette en entry 0
    /// van de tabel schreef: "Turn on MSI-X capability first, or our
    /// settings won't stick" (Linux).
    pub fn set_irq(&mut self, bell: &'static Signal) {
        let r = self.regs();
        r.gpie.write(GPIE_MSIX);
        r.ivar0.update(|v| ivar_q0(v, Q_VECTOR));
        r.eiac.update(|v| v | Q_EIMS);
        r.eiam.update(|v| v | Q_EIMS);
        self.irq = Some(bell);
        self.rearm();
    }

    /// Terug naar pollen: de vector dicht en de bel weg (het board na een
    /// zelftest die niet aankwam).
    pub fn clear_irq(&mut self) {
        self.irq = None;
        self.regs().eimc.write(u32::MAX);
        let _ = self.regs().status.read(); // commit
    }

    /// Vuurt de vector met de hand (EICS), de zelftest van het board:
    /// zo deed `igb_watchdog_task` het ("Cause software interrupt to ensure
    /// Rx ring is cleaned"). Komt hij niet aan, dan klopt de route
    /// (tabel, DeviceID, ITS) niet en kan het board terug naar pollen.
    pub fn fire_irq(&self) {
        self.regs().eics.write(Q_EIMS);
        let _ = self.regs().status.read(); // commit
    }

    /// De MSI-X-registers voor een diagnoseregel. EICR lezen wist hem
    /// (GPIE.NSICR), dus alleen als de lijn toch al niet werkt.
    #[must_use]
    pub fn irq_regs(&self) -> IrqRegs {
        let r = self.regs();
        IrqRegs {
            gpie: r.gpie.read(),
            ivar0: r.ivar0.read(),
            eims: r.eims.read(),
            eicr: r.eicr.read(),
        }
    }

    /// Het masker van de vector open (`igb_ring_irq_enable`).
    fn rearm(&self) {
        self.regs().eims.write(Q_EIMS);
    }

    fn regs(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van `new`.
        unsafe { dev::regs(self.base) }
    }

    /// De `igb_reset_hw`-kern: interrupts dicht, RX/TX uit, CTRL.RST,
    /// wachten tot hij zichzelf wist, interrupts opnieuw dicht, en het MAC
    /// uit RAL0/RAH0. Daar laadt de NVM het na een reset (I210-aanname,
    /// QEMU-bewezen); een leeg register is een meting, geen driverfout.
    fn reset(&mut self) -> Result {
        let r = self.regs();
        if r.status.read() == u32::MAX {
            return Err(Error::OffBus);
        }
        // Ook de MSI-X-vectoren: na een warme flip staat de vorige
        // generatie nog in de MSI-X-modus (`igb_irq_disable`).
        r.eimc.write(u32::MAX);
        r.imc.write(u32::MAX);
        r.rctl.write(0);
        r.tctl.write(TCTL_PSP);
        dev::mb();
        r.ctrl.update(|v| v | CTRL_RST);
        dev::delay(self.now, RESET_PAUSE_NS);
        if !dev::poll_until(self.now, RESET_NS, || r.ctrl.read() & CTRL_RST == 0) {
            return Err(Error::ResetStuck {
                ctrl: r.ctrl.read(),
            });
        }
        r.eimc.write(u32::MAX);
        r.imc.write(u32::MAX);
        let _ = r.icr.read(); // restjes wissen
        let (ral, rah) = (r.ral0.read(), r.rah0.read());
        if ral == 0 && rah & 0xffff == 0 {
            return Err(Error::NoMac);
        }
        let [a, b, c, d] = ral.to_le_bytes();
        let [e, f, _, _] = rah.to_le_bytes();
        self.mac = Mac([a, b, c, d, e, f]);
        Ok(())
    }

    /// Zet de ringen klaar en RX/TX aan, in de `igb_configure_rx_ring`-
    /// volgorde: basis en lengte, buffers, enable, wachten tot de queue
    /// ENABLE terugleest, dán RCTL en RDT (Go `Init`). Een RDT die de
    /// queue mist terwijl hij nog uit staat, laat RDT == RDH: een lege ring
    /// en een stil dove RX. Zo ook TX: TXDCTL, wachten, dan TCTL.
    fn init(&mut self) -> Result {
        for i in 0..N_RX {
            self.arm_rx(i);
        }
        dev::clear(self.tx_ring, (u64::from(N_TX) * DESC) as usize);
        dev::mb();
        let r = self.regs();
        let (lo, hi) = split(self.rx_ring);
        r.rdbal.write(lo);
        r.rdbah.write(hi);
        r.rdlen.write(u32::from(N_RX) * DESC as u32);
        r.srrctl.write(SRRCTL_BSIZE_2K | SRRCTL_DESC_ADV1);
        r.rdh.write(0);
        r.rdt.write(0);
        r.rxdctl.write(Q_ENABLE);
        self.wait_queue(false)?;
        r.rctl.write(RCTL_EN | RCTL_BAM | RCTL_SECRC);
        // Alles op één na aan de hardware: RDT == RDH is "leeg".
        r.rdt.write(u32::from(N_RX - 1));
        self.rx_tail = None;

        let (lo, hi) = split(self.tx_ring);
        r.tdbal.write(lo);
        r.tdbah.write(hi);
        r.tdlen.write(u32::from(N_TX) * DESC as u32);
        r.tdh.write(0);
        r.tdt.write(0);
        r.txdctl.write(Q_ENABLE);
        self.wait_queue(true)?;
        r.tctl.write(TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);
        dev::mb();
        Ok(())
    }

    fn wait_queue(&self, tx: bool) -> Result {
        let r = self.regs();
        let q = if tx { &r.txdctl } else { &r.rxdctl };
        if dev::poll_until(self.now, QUEUE_NS, || q.read() & Q_ENABLE != 0) {
            Ok(())
        } else {
            Err(Error::QueueStuck { tx })
        }
    }

    /// Zet CTRL.SLU en herstart de PHY-autonegotiatie: het igb-recept na
    /// een reset, de MAC volgt de PHY (geen force-bits). Zonder deze
    /// herstart meldt (onder andere het QEMU-model) de link zich niet.
    pub fn start_link(&mut self) -> Result {
        self.regs().ctrl.update(|v| v | CTRL_SLU);
        self.write(
            PHY_ADDR,
            mdio::reg::BMCR,
            mdio::BMCR_AN_ENABLE | mdio::BMCR_AN_RESTART,
        )
        .map_err(Error::Phy)
    }

    fn mdic_wait(&self, phy: u8, reg: u8) -> mdio::Result<u32> {
        let mdic = &self.regs().mdic;
        let mut v = 0;
        let _ = dev::poll_until(self.now, MDIC_NS, || {
            v = mdic.read();
            v & (MDIC_READY | MDIC_ERROR) != 0
        });
        if v & MDIC_ERROR != 0 || v & MDIC_READY == 0 {
            return Err(mdio::Error::Bus { phy, reg });
        }
        Ok(v)
    }

    /// Geeft RX-descriptor `i` terug in lees-formaat met zijn eigen buffer.
    fn arm_rx(&self, i: u16) {
        let d = self.rx_ring.add(u64::from(i) * DESC);
        let (lo, hi) = split(self.rx_buf(i));
        dev::write32(d, lo);
        dev::write32(d.add(4), hi);
        dev::write32(d.add(W2), 0);
        dev::write32(d.add(W3), 0);
    }

    fn rx_buf(&self, i: u16) -> Pa {
        self.rx_bufs.add(u64::from(i) * BUF_SIZE as u64)
    }

    fn tx_buf(&self, i: u16) -> Pa {
        self.tx_bufs.add(u64::from(i) * BUF_SIZE as u64)
    }

    fn flush_rx(&mut self) {
        self.rx_since = 0;
        if let Some(t) = self.rx_tail.take() {
            self.regs().rdt.write(u32::from(t));
            self.stats.doorbells += 1;
        }
    }

    /// Haalt één frame op, `None` = de ring is leeg. Een descriptor met een
    /// lengte die niet in zijn buffer past, of zonder EOP (een frame groter
    /// dan de buffer, kan niet bij 2 KB tegen MTU 1522), wordt herwapend
    /// zonder kopie: een device-lengte mag nooit bytes van de buurbuffer
    /// blootgeven.
    fn receive_one(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let i = self.rx_head;
            let d = self.rx_ring.add(u64::from(i) * DESC);
            let status = dev::read32(d.add(W2));
            if status & RX_DD == 0 {
                return None;
            }
            dev::mb();
            let len = (dev::read32(d.add(W3)) & 0xffff) as usize;
            let good = status & RX_EOP != 0 && len > 0 && len <= BUF_SIZE;
            let n = if good { len.min(buf.len()) } else { 0 };
            if n > 0 {
                let src = self.rx_buf(i);
                // Oude regels weg vóór de lees: de NIC schreef buiten de
                // caches om. Op een Device-mapping onschadelijk.
                dev::pull(src, n);
                if let Some(dst) = buf.get_mut(..n) {
                    dev::copy_out(dst, src);
                }
            } else {
                self.stats.rx_bad += 1;
            }
            self.arm_rx(i);
            dev::mb();
            self.rx_tail = Some(i);
            self.rx_head = (i + 1) % N_RX;
            self.rx_since += 1;
            if self.rx_since >= RX_SELF_FLUSH {
                self.flush_rx();
            }
            if n > 0 {
                return Some(n);
            }
        }
    }

    fn flush_tx(&mut self) {
        if self.tx_pending > 0 {
            dev::mb();
            self.regs().tdt.write(u32::from(self.tx_head));
            self.tx_pending = 0;
            self.stats.doorbells += 1;
        }
    }
}

impl Mdio for Igb {
    fn read(&mut self, phy: u8, reg: u8) -> mdio::Result<u16> {
        let cmd = (u32::from(reg & 0x1f) << 16) | (u32::from(phy & 0x1f) << 21) | MDIC_OP_READ;
        self.regs().mdic.write(cmd);
        self.mdic_wait(phy, reg).map(|v| (v & 0xffff) as u16)
    }

    fn write(&mut self, phy: u8, reg: u8, val: u16) -> mdio::Result {
        let cmd = u32::from(val)
            | (u32::from(reg & 0x1f) << 16)
            | (u32::from(phy & 0x1f) << 21)
            | MDIC_OP_WRITE;
        self.regs().mdic.write(cmd);
        self.mdic_wait(phy, reg).map(|_| ())
    }
}

/// STATUS naar een link: LU, FD, en de snelheid in [7:6] (00 = 10, 01 =
/// 100, 1x = 1000).
fn decode_status(s: u32) -> Option<Link> {
    if s & STATUS_LU == 0 {
        return None;
    }
    let mbps = match (s >> STATUS_SPEED_SHIFT) & 0b11 {
        0 => 10,
        1 => 100,
        _ => 1000,
    };
    Some(Link {
        mbps,
        full_duplex: s & STATUS_FD != 0,
    })
}

fn split(pa: Pa) -> (u32, u32) {
    ((pa.0 & 0xffff_ffff) as u32, (pa.0 >> 32) as u32)
}

impl netdev::Device for Igb {
    /// Zet één frame op de TX-ring; de doorbell (TDT) volgt in `flush`.
    ///
    /// TDT mag nooit op TDH uitkomen: TDT == TDH is voor de 82575/I210 een
    /// lége ring, en een burst van `N_TX` posts zonder rem doet precies dat
    /// (waarna de hardware niets meer fetcht). Daarom nooit meer dan
    /// `N_TX - 2` klaarzetten zonder doorbell, en de DD-writeback in het
    /// DMA-geheugen (geen TDH-lees over PCIe per frame) bewaakt het
    /// hergebruik: een descriptor die gebruikt is (`w2 != 0`) maar nog geen DD
    /// heeft, is van de hardware, en dan is de ring vol.
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), TxError> {
        if frame.is_empty() || frame.len() > BUF_SIZE {
            return Err(TxError::Size(frame.len()));
        }
        if self.tx_pending >= N_TX - 2 {
            self.flush_tx();
        }
        let d = self.tx_ring.add(u64::from(self.tx_head) * DESC);
        if dev::read32(d.add(W2)) != 0 && dev::read32(d.add(W3)) & TX_DD == 0 {
            self.stats.tx_full += 1;
            return Err(TxError::Full);
        }
        let buf = self.tx_buf(self.tx_head);
        dev::copy_in(buf, frame);
        // Vuile regels naar het geheugen vóór de NIC de buffer haalt; op een
        // Device-mapping onschadelijk.
        dev::push(buf, frame.len());
        let len = frame.len() as u32;
        let (lo, hi) = split(buf);
        dev::write32(d, lo);
        dev::write32(d.add(4), hi);
        dev::write32(d.add(W3), len << TX_PAY_SHIFT);
        dev::mb();
        dev::write32(
            d.add(W2),
            len | TX_DTYP_DATA | TX_EOP | TX_IFCS | TX_RS | TX_DEXT,
        );
        self.tx_head = (self.tx_head + 1) % N_TX;
        self.tx_pending += 1;
        Ok(())
    }

    /// Haalt één frame op (zie `receive_one`).
    ///
    /// Leeg met een bedrade lijn: dan gaat het masker weer open (EIAM en de
    /// ack sloten het bij het bericht), en daarna kijkt de driver nog één
    /// keer, zodat een frame dat tussen de lees en het openen binnenkwam
    /// niet tot de vangrail blijft liggen (het patroon van de dwmac). Altijd
    /// schrijven, nooit EIMS lezen: een geposte schrijf kost minder dan een
    /// lees over PCIe, en een open masker nog eens openen doet niets.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        if let Some(n) = self.receive_one(buf) {
            return Some(n);
        }
        self.irq?;
        self.rearm();
        self.receive_one(buf)
    }

    /// Eén doorbell per ring per burst.
    fn flush(&mut self) {
        self.flush_tx();
        self.flush_rx();
    }

    fn mac(&self) -> Mac {
        self.mac
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    /// De link zoals de MAC hem ziet (STATUS). De MAC volgt de PHY zelf:
    /// niets te zetten.
    fn link(&mut self) -> Option<u32> {
        decode_status(self.regs().status.read()).map(|l| l.mbps)
    }

    fn stats(&self) -> netdev::Stats {
        self.stats
    }
}

#[cfg(test)]
mod tests;
