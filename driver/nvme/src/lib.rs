//! NVMe: één core met een transport eronder, zoals Linux
//! (`drivers/nvme/host/core.c` met `pci.c` en `apple.c`).
//!
//! De core kent de queues, de opdrachten en het blokcontract; het
//! [`Transport`] weet hoe een opdracht op de controller komt en wat er bij
//! de opstart anders is. Twee transports:
//!
//! - [`pci`]: NVMe over PCIe (de O6N, de Altra): BAR0, een SQ-ring met een
//!   tail-deurbel op de stride die de controller meldt.
//! - [`apple`]: de ANS van de Mac mini M4: een lineaire SQ, een TCB per
//!   opdracht voor de NVMMU, een coprocessor die bij elke wachtlus hoort, en
//!   een schijf die van macOS is.
//!
//! Eén admin-queue en één I/O-queue-paar van [`Q_ENTRIES`]. De
//! DMA-regio heeft per queue-paar drie tabellen van 16 KiB (de zijtabel
//! van het transport, de SQ, de CQ), dan een PRP-lijstpagina per ticket, en
//! vanaf [`DATA_OFF`] het datablok in een eigen blok van 2 MB dat het board
//! cacheable mag mappen (op de M4 gemeten 03-09: ongecachete loads haalden
//! ~100 MB/s); de core doet zelf `push` na het vullen en `pull` vóór het
//! lezen, en op ongecachet geheugen zijn die gratis.
//!
//! Het blokcontract ([`blkdev::AsyncBlockDevice`]): [`DEPTH`] tickets
//! tegelijk, elk met zijn eigen pagina's van het datablok (een pool van 512
//! pagina's, de PRP-lijst van het ticket wijst ze aan), en daarnaast
//! hoogstens één read-ahead. Een ticket groter dan de MDTS van de
//! controller wordt meer opdrachten achter één deurbel, elk met een eigen
//! CID (O6N 01-10: MDTS 512 KiB, een lees van 1 MiB werd twee seriële
//! opdrachten). Niets wacht: `start_tag` zet de opdrachten op de controller
//! en keert terug, `reap` haalt de completions op in elke volgorde, en de
//! wachter (`blkdev::Queue`) slaapt op de bel van de lijn als het board er
//! een bedraadde (PCI: MSI-X vector 0, [`pci`]), anders pollt hij. Les van
//! 30-09: de synchrone commit van
//! hopfs hield op QEMU de OS-core tot 7 s stil. GEMETEN 01-10 op de M4:
//! willekeurig 4 KiB lezen 11.888 per seconde met één tegelijk, 175.055 met
//! zestien.
//!
//! Alleen de opstart en het afsluiten wachten ter plekke (admin-opdrachten
//! op CID 0, er loopt dan niets anders), met de ene wachthulp
//! `Nvme::wait`. Een opdracht die niet binnen [`COMMAND_TIMEOUT_NS`]
//! terugkomt, of een completion die nergens bij hoort, maakt de driver
//! dood: de controller kan nog in een buffer schrijven. Dood is luid en
//! blijvend.

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

use blkdev::Op;
use core::fmt;
use core::mem::{offset_of, size_of};
use core::task::Poll;
use dev::{Pa, Reg};
use sync::Signal;

pub mod apple;
pub mod pci;

/// De standaardregisters (NVMe 1.4 §3.1), bij elk transport op de basis
/// van zijn venster. De 64-bit registers in twee helften, laag eerst, zoals
/// Linux (`lo_hi_readq`): de ANS neemt geen 64-bit toegang aan.
#[repr(C)]
struct Regs {
    /// Capabilities: MQES [15:0], TO [31:24] (in 500 ms), DSTRD [35:32],
    /// MPSMIN [51:48].
    cap: [Reg<u32>; 2],
    vs: Reg<u32>,
    _intms: u32,
    _intmc: u32,
    cc: Reg<u32>,
    _r0: u32,
    csts: Reg<u32>,
    _nssr: u32,
    aqa: Reg<u32>,
    asq: [Reg<u32>; 2],
    acq: [Reg<u32>; 2],
}

/// Eén submission-entry (NVMe §4.2), voor de offsets.
#[repr(C)]
struct Sqe {
    /// Opcode [7:0], CID [31:16].
    cdw0: u32,
    nsid: u32,
    _cdw2: u32,
    _cdw3: u32,
    _mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    _cdw13: u32,
    _cdw14: u32,
    _cdw15: u32,
}

/// Eén completion-entry (NVMe §4.6), voor de offsets.
#[repr(C)]
struct Cqe {
    _dw0: u32,
    _dw1: u32,
    _sq_head: u16,
    _sq_id: u16,
    cid: u16,
    /// Phase [0], status [15:1].
    status: u16,
}

const _: () = {
    assert!(offset_of!(Regs, vs) == 0x08);
    assert!(offset_of!(Regs, cc) == 0x14);
    assert!(offset_of!(Regs, csts) == 0x1c);
    assert!(offset_of!(Regs, aqa) == 0x24);
    assert!(offset_of!(Regs, asq) == 0x28);
    assert!(offset_of!(Regs, acq) == 0x30);
    assert!(size_of::<Sqe>() == 64);
    assert!(offset_of!(Sqe, nsid) == 4);
    assert!(offset_of!(Sqe, prp1) == 24);
    assert!(offset_of!(Sqe, prp2) == 32);
    assert!(offset_of!(Sqe, cdw10) == 40);
    assert!(offset_of!(Sqe, cdw11) == 44);
    assert!(offset_of!(Sqe, cdw12) == 48);
    assert!(size_of::<Cqe>() == 16);
    assert!(offset_of!(Cqe, cid) == 12);
    assert!(offset_of!(Cqe, status) == 14);
};
const SQE: u64 = size_of::<Sqe>() as u64;
const CQE: u64 = size_of::<Cqe>() as u64;

/// Het begin van de doorbells: SQ y tail op `DB + (2y) << (2 + DSTRD)`, CQ y
/// head op `DB + (2y + 1) << (2 + DSTRD)`.
const DB: u64 = 0x1000;

const CC_EN: u32 = 1 << 0;
/// 64-byte submission-entries (2^6).
const CC_IOSQES: u32 = 6 << 16;
/// 16-byte completion-entries (2^4).
const CC_IOCQES: u32 = 4 << 20;
const CC_SHN_MASK: u32 = 3 << 14;
/// CREATE_IO_CQ/SQ: fysiek aaneengesloten.
const Q_PC: u32 = 1 << 0;
/// CREATE_IO_CQ: interrupts aan, op de vector in [31:16].
const CQ_IEN: u32 = 1 << 1;
const CSTS_RDY: u32 = 1 << 0;
/// Controller Fatal Status.
const CSTS_CFS: u32 = 1 << 1;

// Opcodes. Oneven schrijft naar het device, even leest ervan (de ANS zet
// de DMA-richting van zijn TCB zo).
const ADM_CREATE_SQ: u8 = 0x01;
const ADM_CREATE_CQ: u8 = 0x05;
const ADM_IDENTIFY: u8 = 0x06;
const ADM_GET_FEATURES: u8 = 0x0a;
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

/// Entries per queue.
pub const Q_ENTRIES: u16 = 64;
/// De namespace die we gebruiken.
const NSID: u32 = 1;
/// De controllerpagina (CC.MPS = 0).
pub const PAGE: u64 = 4096;
/// De grootste transfer van één ticket: één frame van de system-API
/// (`MAX_IO_CHUNK`).
pub const MAX_TRANSFER: u64 = 1 << 20;
/// Zoveel opdrachten mag één ticket hoogstens worden: een ticket groter dan
/// de MDTS gaat als zoveel opdrachten achter één deurbel.
const REQ_CMDS: u64 = 16;
/// Hoe lang één opdracht mag duren, tenzij de controller in CAP.TO langer
/// vraagt. Een gezonde completion is er binnen microseconden.
pub const COMMAND_TIMEOUT_NS: u64 = 5_000_000_000;
/// De sector van het blokcontract.
pub const SECTOR: u64 = 512;

/// Zoveel tickets staan hoogstens tegelijk op de controller, plus de
/// read-ahead ertussen. Tweeëndertig: twee bundels van zestien
/// (`OP_READ_MANY`, een bundel per verbinding van een app) staan zo
/// allebei heel op het device, in plaats van dat de tweede op tickets van
/// de eerste wacht. GEMETEN 04-10 op de Altra (SN770, vitals `rand`, elke
/// run vlak na een flip): vier apps met bundels van zestien 216k lezingen
/// per seconde met 16, 249k met 32; acht apps 225k en 305k; één app met
/// twee bundels 135k en 159k. Op de M4 (ANS) liep 32 diep zonder fout.
/// Linux geeft de I/O-queue van de ANS 62 tags.
pub const DEPTH: usize = 32;
/// CID's van de I/O-queue: één bit per opdracht in de lucht. Hoogstens
/// `Q_ENTRIES - 1`, dan loopt een SQ-ring nooit over (en de lineaire SQ van
/// de ANS heeft per CID één plek).
const CIDS: u64 = (1 << (Q_ENTRIES - 1)) - 1;

// De DMA-regio: per queue-paar drie tabellen van 16 KiB (zijtabel, SQ, CQ;
// admin vanaf 0, I/O vanaf 0xc000), de PRP-lijsten, het datablok.
const TABLE: u64 = 0x4000;
const PRP_OFF: u64 = 6 * TABLE;
/// Het datablok: een eigen blok van 2 MB dat het board cacheable mag
/// mappen; de queues en de PRP-lijsten blijven erbuiten.
pub const DATA_OFF: u64 = 2 << 20;
/// De maat van het datablok.
pub const DATA_SIZE: u64 = 2 << 20;
/// Wat de driver van de DMA-regio vraagt.
pub const DMA_NEED: u64 = DATA_OFF + DATA_SIZE;

/// De pagina's van het datablok. Een ticket krijgt er zoveel als hij nodig
/// heeft, waar ze ook liggen: zo passen één MiB voor de sequentiële lezer
/// en vijftien opdrachten van 4 KiB samen in het blok.
const PAGES: usize = (DATA_SIZE / PAGE) as usize;
/// Het hoogste aantal pagina's van één ticket.
const TAG_PAGES: usize = (MAX_TRANSFER / PAGE) as usize;
/// Zoveel pagina's blijven na een read-ahead vrij voor andere opdrachten
/// (256 KiB: tweeëndertig keer 4 KiB met ruimte).
const AHEAD_RESERVE: usize = 64;

const _: () = {
    // Een SQE per plek, en een TCB van 128 bytes per plek in de zijtabel.
    assert!(Q_ENTRIES as u64 * 128 <= TABLE && Q_ENTRIES as u64 * SQE <= TABLE);
    assert!(PRP_OFF + DEPTH as u64 * PAGE <= DATA_OFF);
    // Eén PRP-lijstpagina draagt 512 adressen; 1 MiB vraagt er 255.
    assert!((TAG_PAGES - 1) * 8 <= PAGE as usize);
    // Een volle opdracht naast een volle read-ahead past, en een read-ahead
    // laat de reserve over.
    assert!(2 * TAG_PAGES <= PAGES && TAG_PAGES + AHEAD_RESERVE <= PAGES);
    assert!(PAGES.is_multiple_of(64) && PAGES <= u16::MAX as usize);
    assert!(DATA_OFF.is_multiple_of(2 << 20) && DATA_SIZE.is_multiple_of(2 << 20));
    assert!(DEPTH <= blkdev::MAX_DEPTH && DEPTH <= u8::MAX as usize);
};

/// Waarom de driver weigert. `E` is de fout van het [`Transport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// De DMA-regio is te klein, niet uitgelijnd, of loopt om.
    Dma {
        /// De basis.
        base: u64,
        /// De maat.
        size: u64,
    },
    /// Het registerblok leest alles-enen: de controller is niet op de bus.
    OffBus,
    /// De controller eist pagina's groter dan 4 KB, te kleine queues, of een
    /// stride die niet in het venster past.
    Unsupported {
        /// CAP.
        cap: u64,
    },
    /// Nog niet gestart (of afgesloten).
    NotStarted,
    /// Het transport zelf liep mis (de coprocessor van de ANS); dood.
    Transport(E),
    /// De NVMe-kant van de firmware van de ANS kwam niet op.
    BootStatus {
        /// BOOT_STATUS na de grens.
        status: u32,
    },
    /// CSTS.RDY werd niet wat gevraagd, of de controller meldt een fatale
    /// fout.
    NotReady {
        /// CSTS na de grens.
        csts: u32,
        /// CC op dat moment.
        cc: u32,
        /// De gevraagde RDY.
        want: bool,
    },
    /// CSTS.SHST werd niet "klaar".
    Shutdown {
        /// CSTS na de grens.
        csts: u32,
    },
    /// Namespace 1 is onbruikbaar (geen blokken, metadata, bescherming, of
    /// een blokmaat buiten 512..4096).
    Namespace {
        /// Aantal blokken.
        blocks: u64,
        /// LBADS.
        lbads: u8,
    },
    /// De controller meldde een fout op een opdracht.
    Status {
        /// De opcode.
        opc: u8,
        /// SCT/SC, zonder de phase.
        status: u16,
    },
    /// Geen completion binnen de grens; de driver is vanaf nu dood.
    Timeout {
        /// De opcode.
        opc: u8,
    },
    /// Een completion voor een CID die niet in de lucht is; dood.
    Cid {
        /// Wat de controller terugzette.
        got: u16,
    },
    /// De NVMMU van de ANS nam de invalidatie van een slot niet aan; dood.
    Nvmmu {
        /// Het slot.
        slot: u16,
        /// TCB_STAT.
        stat: u32,
    },
    /// Een eerder verzoek liep af; niets gaat meer naar de controller.
    Dead,
    /// Alle tickets, CID's of pagina's staan nog op de controller; probeer
    /// het na een completion opnieuw.
    Busy,
    /// Een lengte die geen blokveelvoud is, nul, te groot, of buiten de
    /// schijf.
    Range {
        /// Het eerste blok.
        block: u64,
        /// Het aantal bytes.
        len: usize,
    },
    /// Blok 1 van de ANS is geen GPT-header: de capaciteit is onbekend en de
    /// schijf blijft dicht.
    NoGpt,
    /// De GPT-header noemt onbruikbare grenzen.
    Gpt {
        /// Het blok van de reservekopie.
        backup: u64,
        /// Het eerste bruikbare blok.
        first: u64,
        /// Het laatste bruikbare blok.
        last: u64,
    },
    /// Een schrijf zonder venster: de schijf is van macOS.
    NoWindow {
        /// Het eerste blok.
        block: u64,
        /// Het aantal bytes.
        len: usize,
    },
    /// Een schrijf buiten het venster.
    OutsideWindow {
        /// Het eerste blok.
        block: u64,
        /// Het aantal bytes.
        len: usize,
        /// Het venster: eerste blok.
        first: u64,
        /// Het venster: aantal blokken.
        blocks: u64,
    },
    /// Een venster dat leeg is of buiten het bruikbare deel van de GPT valt.
    BadWindow {
        /// Het gevraagde eerste blok.
        first: u64,
        /// Het gevraagde aantal blokken.
        blocks: u64,
        /// Het eerste bruikbare blok van de GPT.
        usable_first: u64,
        /// Het laatste bruikbare blok van de GPT.
        usable_last: u64,
    },
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("nvme: ")?;
        match self {
            Self::Dma { base, size } => write!(
                f,
                "DMA region {size:#x} at {base:#x} invalid (need {DMA_NEED:#x}, aligned)"
            ),
            Self::OffBus => f.write_str("registers read all-ones (device off the bus?)"),
            Self::Unsupported { cap } => write!(f, "unsupported controller (CAP={cap:#x})"),
            Self::NotStarted => f.write_str("not started"),
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::BootStatus { status } => write!(
                f,
                "boot status {status:#x} (expected {:#x}), firmware not ready",
                apple::BOOT_STATUS_OK
            ),
            Self::NotReady { csts, cc, want } => write!(
                f,
                "CSTS.RDY never became {} (CSTS={csts:#x} CC={cc:#x})",
                u8::from(*want)
            ),
            Self::Shutdown { csts } => write!(f, "shutdown did not complete (CSTS={csts:#x})"),
            Self::Namespace { blocks, lbads } => write!(
                f,
                "namespace unusable (blocks={blocks}, block size 2^{lbads})"
            ),
            Self::Status { opc, status } => write!(f, "command {opc:#x} status {status:#x}"),
            Self::Timeout { opc } => {
                write!(f, "timeout on command {opc:#x}, DMA retained, driver dead")
            }
            Self::Cid { got } => write!(f, "completion for CID {got} not in flight, driver dead"),
            Self::Nvmmu { slot, stat } => write!(
                f,
                "NVMMU invalidation for slot {slot} failed ({stat:#x}), driver dead"
            ),
            Self::Dead => f.write_str("driver dead after an unfinished command"),
            Self::Busy => f.write_str("all tags still in flight"),
            Self::Range { block, len } => write!(f, "{len} bytes at block {block} out of range"),
            Self::NoGpt => f.write_str("no GPT on block 1, capacity unknown, disk stays closed"),
            Self::Gpt {
                backup,
                first,
                last,
            } => write!(f, "GPT unusable (backup {backup}, usable {first}..{last})"),
            Self::NoWindow { block, len } => write!(
                f,
                "WRITE REFUSED: {len} bytes at block {block}, no write window (the disk belongs to macOS)"
            ),
            Self::OutsideWindow {
                block,
                len,
                first,
                blocks,
            } => write!(
                f,
                "WRITE REFUSED: {len} bytes at block {block} outside window {first}+{blocks}"
            ),
            Self::BadWindow {
                first,
                blocks,
                usable_first,
                usable_last,
            } => write!(
                f,
                "window {first}+{blocks} outside the usable GPT range {usable_first}..{usable_last}"
            ),
        }
    }
}

/// De `Result` van deze crate; `E` is de fout van het transport.
pub type Result<V = (), E = core::convert::Infallible> = core::result::Result<V, Error<E>>;

/// Wat per aanlanding verschilt (Linux: `nvme_ctrl_ops` en de `queue_rq`
/// van het transport). De opstart is geen methode: elk transport heeft zijn
/// eigen (`Nvme::<Pci>::new`, `Nvme::<Apple<C>>::start`), met de stukken
/// van de core.
pub trait Transport {
    /// De fout van het transport zelf.
    type Error: fmt::Display + fmt::Debug + Copy;

    /// Lineair (de ANS): de opdracht met CID `c` staat op SQ-plek `c` en de
    /// deurbel noemt die plek ([`submit`](Self::submit)). Anders een ring:
    /// de core zet elke opdracht op de tail en luidt na de laatste van een
    /// verzoek de tail-deurbel (NVMe §3.1.24).
    const LINEAR: bool = false;
    /// De uitlijning van de DMA-regio.
    const DMA_ALIGN: u64 = PAGE;

    /// De SQE op plek `slot` van `q` staat klaar (met een barrière erna):
    /// zet hem op de controller. Een ring hoeft niets.
    fn submit(&mut self, _q: &Queue, _slot: u16, _m: &Cmd) -> Result<(), Self::Error> {
        Ok(())
    }

    /// De completion van `cid` is gelezen, vóór de CQ-head opschuift.
    fn completed(&mut self, _cid: u16) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Hoort in elke wachtlus en zolang er iets loopt (de mailbox van de
    /// ANS). Een fout is een dood transport.
    fn service(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Eén SQ/CQ-paar met zijn zijtabel en de poll-staat.
#[derive(Clone, Copy, Debug)]
pub struct Queue {
    /// De tabel van het transport (de ANS: een TCB per plek).
    side: Pa,
    sq: Pa,
    cq: Pa,
    id: u16,
    /// SQ-tail van een ring: wij produceren.
    tail: u16,
    /// CQ-head: wij consumeren.
    head: u16,
    /// De phase die de volgende geldige CQ-entry draagt.
    phase: bool,
}

impl Queue {
    fn new(dma: Pa, id: u16) -> Self {
        let base = dma.add(3 * TABLE * u64::from(id));
        Self {
            side: base,
            sq: base.add(TABLE),
            cq: base.add(2 * TABLE),
            id,
            tail: 0,
            head: 0,
            phase: true,
        }
    }
}

/// Een opdracht in opbouw: de velden van een SQE die deze driver zet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cmd {
    opc: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

/// Waarvoor een ticket op de controller staat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Een ticket van het blokcontract: een wachter wacht erop.
    Ticket,
    /// Een read-ahead. `stale`: een schrijf raakte zijn blokken, of de
    /// eigenaar las iets anders; de bytes worden nooit gebruikt.
    Ahead {
        /// Niet meer bruikbaar.
        stale: bool,
    },
}

/// Eén ticket op de controller, met de pagina's van `pages[i]`.
#[derive(Clone, Copy, Debug)]
struct Inflight {
    opc: u8,
    /// Het eerste blok en het aantal bytes (0 bij een flush).
    block: u64,
    len: usize,
    /// Hoeveel pagina's van het datablok hij houdt.
    pages: usize,
    /// De LBA van het contract, voor de fout.
    lba: u64,
    t0: u64,
    use_: Use,
    /// Zoveel opdrachten staan nog op de controller.
    left: u8,
    /// De eerste foutstatus (SCT/SC), 0 als alles goed ging.
    status: u16,
}

impl Inflight {
    fn new(opc: u8, block: u64, len: usize, lba: u64, use_: Use) -> Self {
        Self {
            opc,
            block,
            len,
            pages: len.div_ceil(PAGE as usize),
            lba,
            t0: 0,
            use_,
            left: 0,
            status: 0,
        }
    }

    /// `None` zolang er een opdracht loopt, daarna SCT/SC.
    fn done(&self) -> Option<u16> {
        (self.left == 0).then_some(self.status)
    }
}

/// Het deel van de schijf dat het blokcontract ziet, in blokken: bij PCI
/// het hele device, bij de ANS het venster na macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window {
    first: u64,
    blocks: u64,
}

/// Eén NVMe-controller met namespace 1, achter transport `T`.
///
/// # Invariants
///
/// Een schrijf raakt alleen blokken binnen `window`.
pub struct Nvme<T: Transport> {
    x: T,
    base: Pa,
    dma: Pa,
    now: fn() -> u64,
    timeout_ns: u64,
    dstrd: u32,
    admin: Queue,
    io: Queue,
    started: bool,
    dead: bool,
    model: [u8; 40],
    block_size: u64,
    blocks: u64,
    max_transfer: u64,
    window: Option<Window>,
    /// De tickets op de controller.
    inflight: [Option<Inflight>; DEPTH],
    /// De CID's van de I/O-queue in de lucht (bit) en hun ticket.
    cids: u64,
    owner: [u8; Q_ENTRIES as usize],
    /// De vrije pagina's van het datablok (bit = vrij).
    free: [u64; PAGES / 64],
    /// Per ticket zijn pagina's, in volgorde.
    pages: [[u16; TAG_PAGES]; DEPTH],
    /// Het blok waar de laatste lees van de eigenaar eindigde: begint de
    /// volgende daar, dan leest hij sequentieel en komt er een read-ahead.
    seq_end: u64,
    /// Meetlat: lezingen die de read-ahead al (deels) gedaan had.
    ahead_hits: u64,
    /// Meetlat: read-aheads die niemand gebruikte.
    ahead_waste: u64,
    /// Meetlat: afgehandelde opdrachten.
    commands: u64,
    /// Meetlat: het langste ticket in nanoseconden.
    slowest_ns: u64,
    /// De bel van de lijn van de I/O-CQ, als het board hem bedraadde.
    irq: Option<&'static Signal>,
}

impl<T: Transport> Nvme<T> {
    /// De staat zonder één registertoegang. Weigert een DMA-regio die te
    /// klein is, niet uitgelijnd, of omloopt. `base` is het venster met de
    /// standaardregisters, `now` geeft monotone nanoseconden.
    fn at(x: T, base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self, T::Error> {
        if dma.0 == 0
            || !dma.is_aligned(T::DMA_ALIGN)
            || dma_size < DMA_NEED
            || dma.0.checked_add(dma_size).is_none()
        {
            return Err(Error::Dma {
                base: dma.0,
                size: dma_size,
            });
        }
        Ok(Self {
            x,
            base,
            dma,
            now,
            timeout_ns: COMMAND_TIMEOUT_NS,
            dstrd: 0,
            admin: Queue::new(dma, 0),
            io: Queue::new(dma, 1),
            started: false,
            dead: false,
            model: [0; 40],
            block_size: 0,
            blocks: 0,
            max_transfer: MAX_TRANSFER,
            window: None,
            inflight: [None; DEPTH],
            cids: 0,
            owner: [0; Q_ENTRIES as usize],
            free: [u64::MAX; PAGES / 64],
            pages: [[0; TAG_PAGES]; DEPTH],
            seq_end: u64::MAX,
            ahead_hits: 0,
            ahead_waste: 0,
            commands: 0,
            slowest_ns: 0,
            irq: None,
        })
    }

    /// Vanaf nu wekt `bell` de wachter: het board bedraadde vector 0 (de
    /// ITS bij PCI, de AIC bij de ANS).
    pub fn set_irq(&mut self, bell: &'static Signal) {
        self.irq = Some(bell);
    }

    fn regs(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van de constructor van het transport: `base`
        // is het registervenster, gemapt als Device voor altijd.
        unsafe { dev::regs(self.base) }
    }

    fn data(&self) -> Pa {
        self.dma.add(DATA_OFF)
    }

    fn doorbell(&self, id: u16, cq: bool) -> Pa {
        let n = 2 * u64::from(id) + u64::from(cq);
        self.base.add(DB + (n << (2 + self.dstrd)))
    }

    /// Het transport bijhouden; een fout maakt de driver dood.
    fn service(&mut self) -> Result<(), T::Error> {
        self.x.service().inspect_err(|_| self.dead = true)
    }

    /// De ene wachthulp van de driver (de opstart, de admin-opdrachten, het
    /// afsluiten): pollt `done` tot hij waar is of `ns` om is, en houdt
    /// intussen het transport bij. `false` bij de grens.
    fn wait(&mut self, ns: u64, done: impl Fn(&Self) -> bool) -> Result<bool, T::Error> {
        let deadline = (self.now)().saturating_add(ns);
        loop {
            if done(self) {
                return Ok(true);
            }
            if (self.now)() >= deadline {
                return Ok(false);
            }
            self.service()?;
            core::hint::spin_loop();
        }
    }

    /// Wacht tot CSTS.RDY `want` is; een fatale status stopt meteen.
    fn wait_ready(&mut self, want: bool) -> Result<(), T::Error> {
        let fatal = |c: u32| c & CSTS_CFS != 0 || c == u32::MAX;
        let ok = self.wait(self.timeout_ns, |n| {
            let c = n.regs().csts.read();
            fatal(c) || (c & CSTS_RDY != 0) == want
        })?;
        let r = self.regs();
        let csts = r.csts.read();
        if ok && !fatal(csts) {
            return Ok(());
        }
        Err(Error::NotReady {
            csts,
            cc: r.cc.read(),
            want,
        })
    }

    /// Linux `nvme_enable_ctrl`, nadat het transport EN eraf haalde, op
    /// RDY 0 wachtte en zijn eigen registers zette: het queuegeheugen leeg,
    /// het datablok uit de cache, de admin-queue aanmelden, CC = `cc`, RDY;
    /// dan de controller-identify (CNS 1): model en MDTS.
    fn enable(&mut self, cc: u32) -> Result<(), T::Error> {
        dev::clear(self.dma, DATA_OFF as usize);
        dev::push(self.data(), DATA_SIZE as usize);
        self.admin = Queue::new(self.dma, 0);
        self.io = Queue::new(self.dma, 1);
        let r = self.regs();
        let q = u32::from(Q_ENTRIES - 1);
        r.aqa.write((q << 16) | q);
        write_lo_hi(&r.asq, self.admin.sq.0);
        write_lo_hi(&r.acq, self.admin.cq.0);
        dev::mb();
        r.cc.write(cc);
        self.wait_ready(true)?;

        let buf = self.data();
        self.admin(Cmd {
            opc: ADM_IDENTIFY,
            prp1: buf.0,
            cdw10: 1,
            ..Cmd::default()
        })?;
        dev::pull(buf, PAGE as usize);
        dev::copy_out(&mut self.model, buf.add(24));
        // MDTS = 0 is "geen limiet"; anders 2^MDTS controllerpagina's. Onze
        // eigen buffer blijft de harde bovengrens.
        let mdts = dev::read8(buf.add(77));
        if mdts != 0 && mdts < 52 {
            self.max_transfer = MAX_TRANSFER.min(PAGE << mdts);
        }
        Ok(())
    }

    /// Namespace 1 (CNS 0): grootte en blokmaat uit de actieve
    /// LBA-indeling; het blokcontract ziet de hele namespace.
    fn identify_namespace(&mut self) -> Result<(), T::Error> {
        let buf = self.data();
        self.admin(Cmd {
            opc: ADM_IDENTIFY,
            nsid: NSID,
            prp1: buf.0,
            ..Cmd::default()
        })?;
        dev::pull(buf, PAGE as usize);
        let blocks = dev::read64(buf);
        let flbas = u64::from(dev::read8(buf.add(26)) & 0xf);
        let lbaf = dev::read32(buf.add(128 + flbas * 4));
        let lbads = ((lbaf >> 16) & 0xff) as u8;
        let metadata = lbaf & 0xffff;
        let protection = dev::read8(buf.add(29)) & 7;
        if blocks == 0 || !(9..=12).contains(&lbads) || metadata != 0 || protection != 0 {
            return Err(Error::Namespace { blocks, lbads });
        }
        self.blocks = blocks;
        self.block_size = 1 << lbads;
        self.window = Some(Window { first: 0, blocks });
        Ok(())
    }

    /// Meldt het I/O-queue-paar aan: CQ eerst, fysiek aaneengesloten, met
    /// interrupts op vector 0 (Linux `adapter_alloc_cq`): elke schijf heeft
    /// een lijn (docs/sequence-hopos.md §14.1 O3).
    fn create_io_queues(&mut self) -> Result<(), T::Error> {
        let q = u32::from(Q_ENTRIES - 1) << 16;
        let id = u32::from(self.io.id);
        self.admin(Cmd {
            opc: ADM_CREATE_CQ,
            prp1: self.io.cq.0,
            cdw10: q | id,
            cdw11: Q_PC | CQ_IEN, // IV 0
            ..Cmd::default()
        })?;
        self.admin(Cmd {
            opc: ADM_CREATE_SQ,
            prp1: self.io.sq.0,
            cdw10: q | id,
            cdw11: (id << 16) | Q_PC, // CQID
            ..Cmd::default()
        })
    }

    /// Zet `m` met CID `cid` op de SQ van de admin- of de I/O-queue: op de
    /// plek `cid` (lineair) of op de tail, en geeft hem aan het transport.
    fn put(&mut self, admin: bool, cid: u16, m: &Cmd) -> Result<(), T::Error> {
        let mut q = if admin { self.admin } else { self.io };
        let slot = if T::LINEAR { cid } else { q.tail };
        write_sqe(q.sq.add(u64::from(slot) * SQE), cid, m);
        q.tail = (q.tail + 1) % Q_ENTRIES;
        self.store(admin, q);
        self.x.submit(&q, slot, m).inspect_err(|_| self.dead = true)
    }

    /// Na de laatste [`put`](Self::put) van een verzoek: de tail-deurbel van
    /// een ring (lineair luidde het transport al per opdracht).
    fn ring(&self, admin: bool) {
        if !T::LINEAR {
            let q = if admin { self.admin } else { self.io };
            dev::write32(self.doorbell(q.id, false), u32::from(q.tail));
        }
    }

    fn store(&mut self, admin: bool, q: Queue) {
        if admin {
            self.admin = q;
        } else {
            self.io = q;
        }
    }

    /// Draagt de CQ-entry op de head van `q` de phase van een nieuwe?
    fn arrived(q: &Queue) -> bool {
        let st = dev::read16(q.cq.add(u64::from(q.head) * CQE + offset_of!(Cqe, status) as u64));
        (st & 1 != 0) == q.phase
    }

    /// Haalt de volgende completion van een queue op als die er is: CID en
    /// SCT/SC. Het transport hoort het, de head schuift op; welk ticket
    /// erbij hoort, toetst de aanroeper.
    fn reap_one(&mut self, admin: bool) -> Result<Option<(u16, u16)>, T::Error> {
        let mut q = if admin { self.admin } else { self.io };
        if !Self::arrived(&q) {
            return Ok(None);
        }
        // De phase vóór de inhoud: pas na de barrière is de rest van de entry
        // van de controller.
        dev::mb();
        let cqe = q.cq.add(u64::from(q.head) * CQE);
        let st = dev::read16(cqe.add(offset_of!(Cqe, status) as u64));
        let cid = dev::read16(cqe.add(offset_of!(Cqe, cid) as u64));
        self.x.completed(cid).inspect_err(|_| self.dead = true)?;
        q.head = (q.head + 1) % Q_ENTRIES;
        if q.head == 0 {
            q.phase = !q.phase;
        }
        self.store(admin, q);
        self.commands += 1;
        Ok(Some((cid, st >> 1)))
    }

    /// Na de laatste [`reap_one`](Self::reap_one) van een ronde: de
    /// head-deurbel van de CQ, één keer voor alles wat er lag (Linux
    /// `nvme_poll_cq`). De CQ loopt niet over: er staan nooit meer opdrachten
    /// uit dan hij plekken heeft.
    fn ring_cq(&self, admin: bool) {
        let q = if admin { self.admin } else { self.io };
        dev::write32(self.doorbell(q.id, true), u32::from(q.head));
    }

    /// Eén admin-opdracht op CID 0 die ter plekke wacht: alleen de opstart en
    /// het afsluiten, als er niets anders loopt. Het blokcontract wacht nooit
    /// zo.
    fn admin(&mut self, m: Cmd) -> Result<(), T::Error> {
        if self.dead {
            return Err(Error::Dead);
        }
        self.put(true, 0, &m)?;
        self.ring(true);
        if !self.wait(self.timeout_ns, |n| Self::arrived(&n.admin))? {
            self.dead = true;
            return Err(Error::Timeout { opc: m.opc });
        }
        let got = self.reap_one(true)?;
        self.ring_cq(true);
        match got {
            Some((0, 0)) => Ok(()),
            Some((0, status)) => Err(Error::Status { opc: m.opc, status }),
            Some((got, _)) => {
                self.dead = true;
                Err(Error::Cid { got })
            }
            None => Err(Error::Timeout { opc: m.opc }),
        }
    }

    fn ready(&self) -> Result<(), T::Error> {
        match (self.started, self.dead) {
            (_, true) => Err(Error::Dead),
            (false, _) => Err(Error::NotStarted),
            _ => Ok(()),
        }
    }

    /// De hapgrootte van één ticket: [`REQ_CMDS`] opdrachten van de
    /// grootste transfer, binnen [`MAX_TRANSFER`].
    fn step(&self) -> usize {
        (self.max_transfer * REQ_CMDS).min(MAX_TRANSFER) as usize
    }

    /// De blokmaat van de schijf in bytes: de eenheid van een blok.
    #[must_use]
    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// De grootte van de schijf in blokken.
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// De grootste transfer van één opdracht in bytes (de MDTS).
    #[must_use]
    pub fn max_transfer(&self) -> u64 {
        self.max_transfer
    }

    /// Wat het blokcontract ziet, in sectoren van [`SECTOR`] bytes: de hele
    /// namespace, of het venster. Nul zonder venster.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.window
            .map_or(0, |w| w.blocks.saturating_mul(self.block_size / SECTOR))
    }

    /// De modelnaam uit de identify, zonder de opvulling.
    #[must_use]
    pub fn model(&self) -> &str {
        let end = self
            .model
            .iter()
            .rposition(|&b| b != 0 && b != b' ')
            .map_or(0, |i| i + 1);
        self.model
            .get(..end)
            .and_then(|m| core::str::from_utf8(m).ok())
            .unwrap_or("?")
    }

    /// De NVMe-versie (VS: major 31:16, minor 15:8).
    #[must_use]
    pub fn version(&self) -> (u16, u8) {
        let vs = self.regs().vs.read();
        ((vs >> 16) as u16, (vs >> 8) as u8)
    }

    /// Leest `buf.len()` bytes (een veelvoud van de blokmaat) vanaf blok
    /// `block`, ook buiten het venster, en wacht erop: voor het board bij de
    /// probe (de GPT van de ANS), vóór de executor.
    pub fn read_at(&mut self, block: u64, buf: &mut [u8]) -> Result<(), T::Error> {
        self.ready()?;
        self.read_blocks(block, buf)
    }

    fn read_blocks(&mut self, block: u64, buf: &mut [u8]) -> Result<(), T::Error> {
        if buf.is_empty() {
            return Err(Error::Range { block, len: 0 });
        }
        let mut b = block;
        for chunk in buf.chunks_mut(self.step()) {
            let i = self.start_read(b, chunk.len(), 0)?;
            self.wait_ticket(i, chunk)?;
            b += chunk.len() as u64 / self.block_size;
        }
        Ok(())
    }

    /// Wacht ter plekke op ticket `i` (vóór de executor, met `block_on`):
    /// ophalen en afronden tot hij klaar is of de driver dood.
    fn wait_ticket(&mut self, i: usize, into: &mut [u8]) -> Result<(), T::Error> {
        blkdev::block_on(core::future::poll_fn(|_| match self.reap_io() {
            Err(e) => Poll::Ready(Err(e)),
            Ok(_) => self.poll_ticket(i, into),
        }))
    }

    /// Een blok-LBA van het contract (512 bytes, 0 = begin van het venster)
    /// als blok van de schijf, met de lengte getoetst.
    fn contract_block(&self, lba: u64, len: usize) -> blkdev::Result<u64> {
        let bad = blkdev::Error::OutOfRange { lba, len };
        let w = self.window.ok_or(bad)?;
        let (per, n) = (self.block_size / SECTOR, len as u64);
        if per == 0 || len == 0 || !lba.is_multiple_of(per) || !n.is_multiple_of(self.block_size) {
            return Err(bad);
        }
        let first = lba / per;
        match first.checked_add(n / self.block_size) {
            Some(end) if end <= w.blocks => Ok(w.first + first),
            _ => Err(bad),
        }
    }

    /// De grenzen van één ticket: hele blokken, niet leeg, hoogstens een
    /// hap, binnen de schijf. NLB is 0-based: een lege transfer werd 0xffff
    /// blokken, ver buiten de buffer.
    fn check(&self, block: u64, len: usize) -> Result<(), T::Error> {
        let (bs, n) = (self.block_size, len as u64);
        let ok = len != 0
            && bs != 0
            && n.is_multiple_of(bs)
            && len <= self.step()
            && block
                .checked_add(n / bs)
                .is_some_and(|end| end <= self.blocks);
        if ok {
            Ok(())
        } else {
            Err(Error::Range { block, len })
        }
    }

    /// De grens van schrijven: het hele verzoek binnen het venster.
    fn check_window(&self, block: u64, len: usize) -> Result<(), T::Error> {
        let Some(w) = self.window else {
            return Err(Error::NoWindow { block, len });
        };
        let n = (len as u64).div_ceil(self.block_size.max(1));
        let inside = len != 0
            && block >= w.first
            && block
                .checked_add(n)
                .is_some_and(|end| end <= w.first + w.blocks);
        if !inside {
            return Err(Error::OutsideWindow {
                block,
                len,
                first: w.first,
                blocks: w.blocks,
            });
        }
        Ok(())
    }
}

/// De pagina's van het datablok: wie welke heeft.
impl<T: Transport> Nvme<T> {
    /// Pagina `p` van het datablok.
    fn page(&self, p: u16) -> Pa {
        self.data().add(u64::from(p) * PAGE)
    }

    /// Het aantal vrije pagina's.
    fn free_pages(&self) -> usize {
        self.free.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Geeft ticket `i` zijn `n` pagina's: de laagste vrije, waar ze ook
    /// liggen. `false` (en niets genomen) als er te weinig zijn.
    fn take_pages(&mut self, i: usize, n: usize) -> bool {
        if n > TAG_PAGES || self.free_pages() < n {
            return false;
        }
        let Some(list) = self.pages.get_mut(i) else {
            return false;
        };
        let mut k = 0;
        for (w, word) in self.free.iter_mut().enumerate() {
            while k < n && *word != 0 {
                let b = word.trailing_zeros() as usize;
                *word &= *word - 1;
                if let Some(s) = list.get_mut(k) {
                    *s = (w * 64 + b) as u16;
                }
                k += 1;
            }
        }
        true
    }

    /// Geeft de `n` pagina's van ticket `i` terug aan het blok.
    fn put_pages(&mut self, i: usize, n: usize) {
        let Some(list) = self.pages.get(i) else {
            return;
        };
        for &p in list.iter().take(n) {
            if let Some(w) = self.free.get_mut(usize::from(p) / 64) {
                *w |= 1 << (p % 64);
            }
        }
    }

    /// Het adres van pagina `k` van ticket `i`.
    fn page_of(&self, i: usize, k: usize) -> u64 {
        self.pages
            .get(i)
            .and_then(|l| l.get(k))
            .map_or(0, |&p| self.page(p).0)
    }

    /// De PRP-lijst van ticket `i`: op plek `k` het adres van zijn pagina
    /// `k + 1`, voor de eerste `n` bytes.
    fn prp_list(&self, i: usize, n: u64) -> Pa {
        let list = self.dma.add(PRP_OFF + i as u64 * PAGE);
        for k in 1..n.div_ceil(PAGE) as usize {
            dev::write64(list.add((k as u64 - 1) * 8), self.page_of(i, k));
        }
        list
    }

    /// De paginawijzers voor `n` bytes vanaf byte `off` (op een pagina) van
    /// ticket `i`: PRP1 naar de eerste pagina; PRP2 voor twee pagina's naar
    /// de tweede, daarboven naar de plek in de lijst waar de tweede staat.
    /// Zo deelt elke opdracht van een ticket dezelfde lijstpagina (een
    /// lijstwijzer binnen een pagina mag, zolang hij op 8 staat; Linux'
    /// kleine PRP-pool doet hetzelfde).
    fn prps(&self, i: usize, list: Pa, off: u64, n: u64) -> (u64, u64) {
        let k = (off / PAGE) as usize;
        match n.div_ceil(PAGE) {
            0 => (0, 0),
            1 => (self.page_of(i, k), 0),
            2 => (self.page_of(i, k), self.page_of(i, k + 1)),
            _ => (self.page_of(i, k), list.0 + k as u64 * 8),
        }
    }

    /// De pagina's van ticket `i` voor `len` bytes als aaneengesloten
    /// stukken: (adres, bytes). Per stuk één cache-onderhoud met zijn
    /// barrières en één kopie: per pagina kostte dat (GEMETEN 01-10, M27)
    /// een derde van het leespad, 512 `dsb` per MiB. Met de laagste vrije
    /// pagina's eerst is een ticket bijna altijd één stuk.
    fn runs(&self, i: usize, len: usize) -> impl Iterator<Item = (Pa, usize)> + '_ {
        let list = self.pages.get(i).map_or(&[][..], |l| &l[..]);
        let n = len.div_ceil(PAGE as usize).min(list.len());
        let mut k = 0;
        core::iter::from_fn(move || {
            if k >= n {
                return None;
            }
            let first = *list.get(k)?;
            let mut m = 1;
            while k + m < n && list.get(k + m) == Some(&(first + m as u16)) {
                m += 1;
            }
            let at = k * PAGE as usize;
            k += m;
            Some((self.page(first), (m * PAGE as usize).min(len - at)))
        })
    }

    /// Kopieert `data` in de pagina's van ticket `i` en duwt ze naar het
    /// geheugen (de controller leest buiten de cache). Het datablok is RAM
    /// dat elk board Normal mapt, en de controller raakt het pas na de
    /// opdracht: een gewone `memcpy`.
    fn fill(&self, i: usize, data: &[u8]) {
        let mut at = 0;
        for (pa, n) in self.runs(i, data.len()) {
            if let Some(c) = data.get(at..at + n) {
                dev::copy_in_normal(pa, c);
                dev::push(pa, n);
            }
            at += n;
        }
    }

    /// Kopieert de eerste `into.len()` bytes uit de pagina's van ticket `i`.
    fn drain(&self, i: usize, into: &mut [u8]) {
        let mut at = 0;
        for (pa, n) in self.runs(i, into.len()) {
            if let Some(c) = into.get_mut(at..at + n) {
                dev::pull(pa, n);
                dev::copy_out_normal(c, pa);
            }
            at += n;
        }
    }
}

/// Het I/O-pad van het blokcontract: tot [`DEPTH`] tickets tegelijk, elk
/// met zijn eigen pagina's ([`Use::Ticket`]), plus hoogstens één read-ahead
/// ([`Use::Ahead`]). Niets hier wacht: `issue` zet een ticket op de
/// controller, `reap_io` haalt op wat terug is, `poll_ticket` rondt één
/// ticket af.
impl<T: Transport> Nvme<T> {
    /// Zet ticket `i` (zijn pagina's genomen) op de I/O-queue: één opdracht
    /// per [`max_transfer`](Self::max_transfer) bytes, elk met een vrije CID
    /// en zijn stuk van de PRP-lijst van het ticket, achter één deurbel. Te
    /// weinig CID's: [`Error::Busy`], en de pagina's terug.
    fn issue(&mut self, i: usize, f: Inflight) -> Result<(), T::Error> {
        let (bs, mt, n) = (self.block_size.max(1), self.max_transfer, f.len as u64);
        let count = n.div_ceil(mt).max(1);
        if self.dead || u64::from((!self.cids & CIDS).count_ones()) < count {
            self.put_pages(i, f.pages);
            return Err(if self.dead { Error::Dead } else { Error::Busy });
        }
        let list = self.prp_list(i, n);
        let mut off = 0;
        for _ in 0..count {
            let chunk = (n - off).min(mt);
            let at = f.block + off / bs;
            let (prp1, prp2) = self.prps(i, list, off, chunk);
            let m = Cmd {
                opc: f.opc,
                nsid: NSID,
                prp1,
                prp2,
                cdw10: (at & 0xffff_ffff) as u32,
                cdw11: (at >> 32) as u32,
                cdw12: (chunk / bs).saturating_sub(1) as u32,
            };
            let cid = (!self.cids & CIDS).trailing_zeros() as u16;
            self.cids |= 1 << cid;
            if let Some(o) = self.owner.get_mut(usize::from(cid)) {
                *o = i as u8;
            }
            self.put(false, cid, &m)?;
            off += chunk;
        }
        self.ring(false);
        if let Some(s) = self.inflight.get_mut(i) {
            *s = Some(Inflight {
                t0: (self.now)(),
                left: count as u8,
                ..f
            });
        }
        Ok(())
    }

    /// Haalt elke completion van de I/O-queue op die er is, en toetst de
    /// time-out van wat nog loopt. Geeft de tickets die klaar kwamen (bit
    /// = ticket). Een completion voor een CID die niet in de lucht is, of een
    /// opdracht over zijn grens, maakt de driver dood: de controller kan dan
    /// nog in een buffer schrijven.
    fn reap_io(&mut self) -> Result<u64, T::Error> {
        if self.dead {
            return Err(Error::Dead);
        }
        let mut ready = 0u64;
        let mut ahead_back = false;
        let mut any = false;
        while let Some((cid, st)) = self.reap_one(false)? {
            any = true;
            let bit = 1u64.checked_shl(u32::from(cid)).unwrap_or(0) & CIDS;
            let i = self
                .owner
                .get(usize::from(cid))
                .map_or(DEPTH, |&o| usize::from(o));
            let Some(f) = self
                .inflight
                .get_mut(i)
                .and_then(Option::as_mut)
                .filter(|f| self.cids & bit != 0 && f.left > 0)
            else {
                self.dead = true;
                return Err(Error::Cid { got: cid });
            };
            self.cids &= !bit;
            f.left -= 1;
            if f.status == 0 {
                f.status = st;
            }
            let f = *f;
            if f.left > 0 {
                continue;
            }
            self.slowest_ns = self.slowest_ns.max((self.now)().saturating_sub(f.t0));
            match f.use_ {
                Use::Ticket => ready |= 1 << i,
                // Een read-ahead die niemand meer wil (geteld bij het
                // onbruikbaar maken) of die faalde: weg.
                Use::Ahead { stale } if stale || f.status != 0 => {
                    self.release(i);
                    self.ahead_waste += u64::from(!stale);
                    ahead_back = true;
                }
                Use::Ahead { .. } => ahead_back = true,
            }
        }
        if any {
            self.ring_cq(false);
        }
        if ahead_back {
            // Een flush wacht op de read-ahead (zie `poll_ticket`): kijk hem
            // opnieuw na.
            for (i, f) in self.inflight.iter().enumerate() {
                if f.is_some_and(|f| f.opc == IO_FLUSH && f.done().is_some()) {
                    ready |= 1 << i;
                }
            }
        }
        // Wat nog loopt, vraagt om het transport (een wachtende coprocessor
        // doet geen DMA). Eerst de CQ: een completion die er al is, wacht
        // daar niet op.
        if self.inflight.iter().flatten().any(|f| f.left > 0) {
            self.service()?;
        }
        let now = (self.now)();
        let late = self
            .inflight
            .iter()
            .flatten()
            .find(|f| f.left > 0 && now >= f.t0.saturating_add(self.timeout_ns));
        if let Some(f) = late {
            let opc = f.opc;
            self.dead = true;
            return Err(Error::Timeout { opc });
        }
        Ok(ready)
    }

    /// Ticket `i` en zijn pagina's weer vrij.
    fn release(&mut self, i: usize) {
        if let Some(f) = self.inflight.get_mut(i).and_then(Option::take) {
            self.put_pages(i, f.pages);
        }
    }

    /// Een vrij ticket met `n` pagina's. Is er geen plaats, dan gaan eerst
    /// de read-aheads weg die terug zijn (een lopende wordt onbruikbaar en
    /// komt bij zijn completion vrij); daarna is het [`Error::Busy`] tot
    /// een ander ticket terug is.
    fn room(&mut self, n: usize) -> Result<usize, T::Error> {
        for again in [false, true] {
            if again {
                self.drop_aheads(None);
            }
            if let Some(i) = self.inflight.iter().position(Option::is_none)
                && self.take_pages(i, n)
            {
                return Ok(i);
            }
        }
        Err(Error::Busy)
    }

    /// Maakt de read-aheads onbruikbaar die `range` (eerste blok, aantal)
    /// raken, of allemaal bij `None`. Wat al terug is, gaat meteen weg; wat
    /// nog loopt, wordt weggegooid zodra het terugkomt.
    fn drop_aheads(&mut self, range: Option<(u64, u64)>) {
        let bs = self.block_size.max(1);
        for i in 0..DEPTH {
            let Some(f) = self.inflight.get(i).copied().flatten() else {
                continue;
            };
            let hit = range.is_none_or(|(b, n)| {
                b < f.block + f.len as u64 / bs && f.block < b.saturating_add(n)
            });
            if f.use_ != (Use::Ahead { stale: false }) || !hit {
                continue;
            }
            self.ahead_waste += 1;
            if f.done().is_some() {
                self.release(i);
            } else if let Some(Some(g)) = self.inflight.get_mut(i) {
                g.use_ = Use::Ahead { stale: true };
            }
        }
    }

    /// Een lees: van de read-ahead als die precies deze blokken leest (of
    /// las, zonder fout), anders een nieuw ticket. Geeft het ticket.
    fn start_read(&mut self, block: u64, len: usize, lba: u64) -> Result<usize, T::Error> {
        self.check(block, len)?;
        let hit = self.inflight.iter().position(|f| {
            f.is_some_and(|f| {
                f.use_ == (Use::Ahead { stale: false })
                    && f.block == block
                    && f.len == len
                    && f.done().is_none_or(|s| s == 0)
            })
        });
        if let Some(i) = hit
            && let Some(Some(f)) = self.inflight.get_mut(i)
        {
            f.use_ = Use::Ticket;
            f.lba = lba;
            self.ahead_hits += 1;
            if len == self.step() {
                self.read_ahead(block + len as u64 / self.block_size.max(1), len, true);
            }
            return Ok(i);
        }
        let f = Inflight::new(IO_READ, block, len, lba, Use::Ticket);
        let i = self.room(f.pages)?;
        self.issue(i, f)?;
        Ok(i)
    }

    /// Een schrijf: binnen het venster, de bytes nu in de pagina's van een
    /// vrij ticket (daarna zijn ze van de aanroeper terug), en een
    /// read-ahead over dezelfde blokken telt niet meer.
    fn start_write(&mut self, block: u64, data: &[u8], lba: u64) -> Result<usize, T::Error> {
        self.check_window(block, data.len())?;
        self.check(block, data.len())?;
        self.drop_aheads(Some((block, data.len() as u64 / self.block_size)));
        let f = Inflight::new(IO_WRITE, block, data.len(), lba, Use::Ticket);
        let i = self.room(f.pages)?;
        self.fill(i, data);
        self.issue(i, f)?;
        Ok(i)
    }

    /// Maakt alles wat de controller al bevestigde duurzaam (NVMe Flush).
    /// Linux stuurt hem ook op de ANS (`drivers/nvme/host/apple.c`).
    fn start_flush(&mut self) -> Result<usize, T::Error> {
        let i = self.room(0)?;
        self.issue(i, Inflight::new(IO_FLUSH, 0, 0, 0, Use::Ticket))?;
        Ok(i)
    }

    /// Na een lees van een volle hap die precies verder ging waar de vorige
    /// eindigde: de volgende hap alvast, op een vrij ticket, binnen het
    /// venster. Zo leest de schijf de volgende MiB terwijl de kern deze naar
    /// de app brengt (GEMETEN 01-10 op de M4: schijf 0,59 en transport 0,53
    /// ms per MiB, tot M21 na elkaar). Eén tegelijk, en alleen als er daarna
    /// nog [`AHEAD_RESERVE`] pagina's vrij zijn: de read-ahead is een gok,
    /// de opdrachten van anderen niet. Een vorige die terug is maar die
    /// niemand las (de hap voorbij het eind van een bestand), maakt plaats:
    /// anders hield hij zijn pagina's en kwam er nooit meer een (M29). Een
    /// fout hier is een dode driver en dat zegt de volgende opdracht.
    ///
    /// `early`: de lezer pakte net de vorige read-ahead (die misschien nog
    /// loopt), en dan gaat de volgende meteen de lucht in in plaats van na
    /// de drain: twee happen tegelijk op de controller, zoals Linux zijn
    /// readahead-venster voor de lezer uit houdt. Dat neemt de laatste
    /// pagina's van het datablok, dus alleen als de lezer alleen is (geen
    /// ander ticket in de lucht); wie daarna komt, wacht hoogstens tot de
    /// hap van de lezer terug is. GEMETEN 04-10 op de Altra (SN770):
    /// sequentieel 1 MiB lezen 2456 naar 3379 MB/s (schrijven 2656), door
    /// hopfs 2274 naar 3049.
    fn read_ahead(&mut self, block: u64, len: usize, early: bool) {
        let Some(w) = self.window else { return };
        let end = block.saturating_add(len as u64 / self.block_size.max(1));
        let ahead = |f: &Inflight| matches!(f.use_, Use::Ahead { .. });
        let mine = |f: &Inflight| f.use_ == (Use::Ahead { stale: false }) && f.block == block;
        let flight = self.inflight.iter().flatten();
        let (mut running, mut tickets, mut have) = (false, 0, false);
        for f in flight {
            running |= ahead(f) && f.done().is_none();
            tickets += usize::from(f.use_ == Use::Ticket);
            have |= mine(f);
        }
        if running || have || end > w.first + w.blocks || (early && tickets > 1) {
            return;
        }
        let reserve = if early { 0 } else { AHEAD_RESERVE };
        if !early {
            self.drop_aheads(None);
        }
        let lba = (block - w.first) * (self.block_size / SECTOR);
        let f = Inflight::new(IO_READ, block, len, lba, Use::Ahead { stale: false });
        if self.free_pages() < f.pages + reserve {
            return;
        }
        if let Ok(i) = self.room(f.pages) {
            let _ = self.issue(i, f);
        }
    }

    /// Rondt ticket `i` af als zijn completion er is (die haalde
    /// [`Self::reap_io`] op): bij een lees de bytes naar `into`, het ticket
    /// en zijn pagina's vrij, en zo nodig de read-ahead op de controller.
    fn poll_ticket(&mut self, i: usize, into: &mut [u8]) -> Poll<Result<(), T::Error>> {
        let Some(f) = self.inflight.get(i).copied().flatten() else {
            return Poll::Ready(Err(Error::NotStarted));
        };
        if f.use_ != Use::Ticket {
            return Poll::Ready(Err(Error::NotStarted));
        }
        if self.dead {
            // Dood blijft dood: het ticket komt niet meer vrij (de
            // controller kan nog in zijn pagina's schrijven).
            return Poll::Ready(Err(Error::Dead));
        }
        let Some(st) = f.done() else {
            return Poll::Pending;
        };
        // Een flush is pas klaar als er geen read-ahead meer loopt: na de
        // laatste flush van een bevriezing (de kern-flip) is de rij leeg,
        // want de opdrachten van de eigenaar wachtte de actor al af.
        let ahead = self
            .inflight
            .iter()
            .flatten()
            .any(|g| matches!(g.use_, Use::Ahead { .. }) && g.done().is_none());
        if f.opc == IO_FLUSH && ahead {
            return Poll::Pending;
        }
        let read = st == 0 && f.opc == IO_READ;
        if read && let Some(d) = into.get_mut(..f.len.min(into.len())) {
            self.drain(i, d);
        }
        self.release(i);
        if read {
            let seq = f.block == self.seq_end;
            self.seq_end = f.block + f.len as u64 / self.block_size;
            if seq && f.len == self.step() {
                self.read_ahead(self.seq_end, f.len, false);
            }
        }
        Poll::Ready(match st {
            0 => Ok(()),
            s => Err(Error::Status {
                opc: f.opc,
                status: s,
            }),
        })
    }

    /// Het ticket van het blokcontract voor `op`.
    fn start_op(&mut self, op: Op<'_>) -> blkdev::Result<usize> {
        let (lba, len) = match op {
            Op::Read { lba, len } => (lba, len),
            Op::Write { lba, data } => (lba, data.len()),
            Op::Flush => (0, 0),
        };
        self.ready().map_err(|e| blk_err(&e, lba, len))?;
        let r = match op {
            Op::Read { lba, len } => {
                let b = self.contract_block(lba, len)?;
                self.start_read(b, len, lba)
            }
            Op::Write { lba, data } => {
                let b = self.contract_block(lba, data.len())?;
                self.start_write(b, data, lba)
            }
            Op::Flush => self.start_flush(),
        };
        r.map_err(|e| blk_err(&e, lba, len))
    }

    /// De LBA van het contract van ticket `i`, voor de fout.
    fn lba_of(&self, i: usize) -> u64 {
        self.inflight.get(i).copied().flatten().map_or(0, |f| f.lba)
    }
}

/// Het blokcontract van `blkdev` over het venster: LBA's van 512 bytes,
/// LBA 0 is het eerste blok van het venster (bij PCI: van de namespace).
/// Bij blokken van 4 KB moet een verzoek op een blok beginnen en eindigen;
/// hopfs schrijft in blokken van 4 KB vanaf LBA 0, dus dat doet hij altijd.
/// Een verzoek dat dat niet doet, of buiten het venster valt, is
/// `OutOfRange`.
///
/// Tickets: [`DEPTH`] tegelijk, elk met zijn eigen pagina's van het
/// datablok; `reap` haalt alle completions op, `poll_tag` rondt er één af.
/// Synchroon blijft synchroon: een schrijf of flush is pas klaar als zijn
/// eigen completion terug is. De volgorde tussen opdrachten is van de
/// aanroeper (hopfs: één call per app tegelijk, een flush pas na de
/// schrijfs die hij moet dekken); de controller mag ze in elke volgorde
/// afronden.
impl<T: Transport> blkdev::Disk for Nvme<T> {
    fn sectors(&self) -> u64 {
        Nvme::sectors(self)
    }
    fn model(&self) -> &str {
        Nvme::model(self)
    }
}

impl<T: Transport> blkdev::AsyncBlockDevice for Nvme<T> {
    fn max_transfer(&self) -> usize {
        self.step()
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    fn depth(&self) -> usize {
        DEPTH
    }

    fn start_tag(&mut self, op: Op<'_>) -> blkdev::Result<usize> {
        self.start_op(op)
    }

    fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> Poll<blkdev::Result> {
        let lba = self.lba_of(t);
        self.poll_ticket(t, into).map_err(|e| blk_err(&e, lba, 0))
    }

    fn reap(&mut self) -> blkdev::Result<u64> {
        self.reap_io().map_err(|e| blk_err(&e, 0, 0))
    }

    fn stats(&self, out: &mut dyn fmt::Write) -> core::result::Result<u64, fmt::Error> {
        write!(
            out,
            "commands={} ahead_hits={} ahead_waste={} slowest_us={} free_pages={}",
            self.commands,
            self.ahead_hits,
            self.ahead_waste,
            self.slowest_ns / 1000,
            self.free_pages()
        )?;
        Ok(self.commands)
    }
}

fn blk_err<E>(e: &Error<E>, lba: u64, len: usize) -> blkdev::Error {
    match e {
        Error::Range { .. } | Error::NoWindow { .. } | Error::OutsideWindow { .. } => {
            blkdev::Error::OutOfRange { lba, len }
        }
        Error::Dead
        | Error::Timeout { .. }
        | Error::Cid { .. }
        | Error::Nvmmu { .. }
        | Error::Transport(_) => blkdev::Error::Dead,
        Error::Busy => blkdev::Error::Busy,
        _ => blkdev::Error::Io { lba },
    }
}

/// Zet de SQE `m` met CID `cid` op `sqe`, met een barrière erna.
fn write_sqe(sqe: Pa, cid: u16, m: &Cmd) {
    dev::clear(sqe, SQE as usize);
    let at = |off: usize| sqe.add(off as u64);
    dev::write32(
        at(offset_of!(Sqe, cdw0)),
        u32::from(m.opc) | (u32::from(cid) << 16),
    );
    dev::write32(at(offset_of!(Sqe, nsid)), m.nsid);
    dev::write64(at(offset_of!(Sqe, prp1)), m.prp1);
    dev::write64(at(offset_of!(Sqe, prp2)), m.prp2);
    dev::write32(at(offset_of!(Sqe, cdw10)), m.cdw10);
    dev::write32(at(offset_of!(Sqe, cdw11)), m.cdw11);
    dev::write32(at(offset_of!(Sqe, cdw12)), m.cdw12);
    dev::mb();
}

/// Schrijft een 64-bit registerpaar in twee helften, laag eerst (Linux
/// `lo_hi_writeq`).
fn write_lo_hi(r: &[Reg<u32>; 2], v: u64) {
    let [lo, hi] = r;
    lo.write((v & 0xffff_ffff) as u32);
    hi.write((v >> 32) as u32);
}

#[cfg(test)]
mod tests;
