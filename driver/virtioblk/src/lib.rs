//! virtio-blk over virtio-mmio of virtio-pci: het blokapparaat onder hopfs.
//!
//! De vorm is die van de NVMe-driver uit de Go-kern
//! (`OLD/metal/driver/nvme`): één verzoek tegelijk, één DMA-buffer, achter
//! [`blkdev::AsyncBlockDevice`] (submit plus completion, op ticket 0). Dat
//! is het enige pad: de hopfs-actor wacht erop met `.await`, en wat vóór de executor
//! draait (de mount, de meetbank) met `blkdev::block_on` over dezelfde
//! futures (waarom: de crate-doc van `blkdev`).
//! Eén in-flight verzoek is geen beperking maar de vorm: de eigenaar
//! (de hopfs-actor, `&mut self`) doet toch één ding tegelijk, en zo hoeft
//! de driver geen tags, geen rij en geen herordening te kennen.
//!
//! Het transport is modern virtio (VERSION_1) achter
//! [`driver_virtiopci::Transport`], zoals bij `driver-virtionet`:
//! virtio-mmio versie 2 op QEMU virt ([`VirtioBlk::new`], virtio 1.2
//! §4.2.2) of virtio-pci onder EDK2 ([`VirtioBlk::with_transport`] met een
//! [`driver_virtiopci::Pci`]). Daarboven één split-virtqueue
//! (descriptortabel, avail- en used-ring) en een DMA-regio die het board
//! uitdeelt, buiten de kern-RAM en niet gecached gemapt. Een verzoek is een
//! keten van drie descriptors: de kop ([`ReqHdr`], het device leest), de
//! data (het device leest of schrijft) en de statusbyte (het device
//! schrijft).
//!
//! Wachten: na de doorbell gaat de executor door, en de wachter van
//! `blkdev::Queue` kijkt weer bij de bel van de IRQ-lijn
//! ([`VirtioBlk::set_irq`], het board bedraadt hem) of, zonder lijn,
//! pollend. Les van 30-09: de synchrone commit van hopfs hield de OS-core tot 7 s stil op een trage
//! schijf (een FLUSH is op macOS een F_FULLFSYNC van het image). Een
//! verzoek dat na [`REQUEST_TIMEOUT_NS`] niet klaar is, maakt de driver
//! dood: het device kan nog in de buffer schrijven, dus een volgend verzoek
//! zou andermans bytes zien. Dood is luid en blijvend (elke volgende call
//! faalt), nooit stil.

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
use dev::Pa;
pub use driver_virtiopci::IrqAck;
use driver_virtiopci::{Mmio, Transport, mmio};
use sync::Signal;

/// De config van virtio-blk (virtio 1.2 §5.2.4), alleen voor de offsets:
/// de driver leest hem via het transport. `capacity` in sectoren van 512
/// bytes, als twee woorden met de generatie-lus: een 64-bit lees op
/// device-geheugen is hier gealigneerd, maar de spec noemt de config per
/// veld en QEMU antwoordt per 32 bits.
#[repr(C)]
struct BlkConfig {
    capacity: u64,
    size_max: u32,
    seg_max: u32,
    geometry: u32,
    blk_size: u32,
}

const _: () = {
    assert!(offset_of!(BlkConfig, capacity) == 0x00);
    assert!(offset_of!(BlkConfig, size_max) == 0x08);
    assert!(offset_of!(BlkConfig, seg_max) == 0x0c);
    assert!(offset_of!(BlkConfig, geometry) == 0x10);
    assert!(offset_of!(BlkConfig, blk_size) == 0x14);
};

/// Eén descriptor van de split-virtqueue (virtio 1.2 §2.7.5).
#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// De kop van een blk-verzoek (`virtio_blk_req`, §5.2.6).
#[repr(C)]
struct ReqHdr {
    kind: u32,
    _reserved: u32,
    sector: u64,
}

/// Eén element van de used-ring.
#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

const _: () = {
    assert!(size_of::<Desc>() == 16);
    assert!(offset_of!(Desc, addr) == 0);
    assert!(offset_of!(Desc, len) == 8);
    assert!(offset_of!(Desc, flags) == 12);
    assert!(offset_of!(Desc, next) == 14);
    assert!(size_of::<ReqHdr>() == 16);
    assert!(offset_of!(ReqHdr, kind) == 0);
    assert!(offset_of!(ReqHdr, sector) == 8);
    assert!(size_of::<UsedElem>() == 8);
    assert!(offset_of!(UsedElem, len) == 4);
};

/// DeviceID van een blokapparaat.
const DEVICE_BLK: u32 = 2;
/// VIRTIO_BLK_F_RO: het device is alleen-lezen.
const FEAT_RO: u32 = 1 << 5;
/// VIRTIO_BLK_F_FLUSH: het device kent `VIRTIO_BLK_T_FLUSH` (een
/// write-back-cache; QEMU biedt hem standaard).
const FEAT_FLUSH: u32 = 1 << 9;

/// Descriptor-flag: de keten gaat verder op `next`.
const DESC_NEXT: u16 = 1;
/// Descriptor-flag: het device schrijft in deze buffer.
const DESC_WRITE: u16 = 2;

/// Verzoeksoorten.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
/// De statusbyte bij succes; wat de driver er vooraf in zet, is geen
/// geldige status (`VIRTIO_BLK_S_OK`, `IOERR`, `UNSUPP` zijn 0, 1, 2).
const S_OK: u8 = 0;
const S_PENDING: u8 = 0xff;

/// De sectormaat van het protocol: `sector` telt altijd in 512 bytes.
pub const SECTOR: u64 = 512;
/// De queuegrootte: een verzoek is drie descriptors, en er is er één
/// tegelijk; vier is de kleinste macht van twee die dat draagt.
pub const QSIZE: u16 = 4;
/// De grootste transfer van één verzoek: de DMA-databuffer. 1 MiB, net als
/// de I/O-brok van de system-API (`MAX_IO_CHUNK`), zodat een app-write van
/// 1 MiB één verzoek is; hopfs bundelt aaneengesloten blokken tot deze maat.
pub const MAX_TRANSFER: usize = 1 << 20;
/// Hoe lang één verzoek mag duren. QEMU op een SSD doet 1 MiB in ruim onder
/// de 10 ms; vijf seconden is een device dat weg is, niet een trage schijf.
pub const REQUEST_TIMEOUT_NS: u64 = 5_000_000_000;

// De indeling van de DMA-regio. De ringen hebben elk hun eigen regel; de
// databuffer begint op een pagina.
const DESC_OFF: u64 = 0;
const AVAIL_OFF: u64 = 64;
const USED_OFF: u64 = 128;
const HDR_OFF: u64 = 256;
const STATUS_OFF: u64 = 272;
const DATA_OFF: u64 = 4096;
/// Wat de driver van de DMA-regio vraagt.
pub const DMA_NEED: u64 = DATA_OFF + MAX_TRANSFER as u64;

const _: () = {
    let q = QSIZE as u64;
    assert!(q.is_power_of_two() && q >= 3);
    assert!(DESC_OFF + q * size_of::<Desc>() as u64 <= AVAIL_OFF);
    // avail: flags, idx, ring[q], used_event.
    assert!(AVAIL_OFF + 6 + 2 * q <= USED_OFF);
    // used: flags, idx, ring[q], avail_event; 4-bytes gealigneerd.
    assert!(USED_OFF.is_multiple_of(4));
    assert!(USED_OFF + 6 + q * size_of::<UsedElem>() as u64 <= HDR_OFF);
    assert!(HDR_OFF.is_multiple_of(16) && HDR_OFF + size_of::<ReqHdr>() as u64 <= STATUS_OFF);
    assert!(STATUS_OFF < DATA_OFF && DATA_OFF.is_multiple_of(4096));
    assert!((MAX_TRANSFER as u64).is_multiple_of(SECTOR));
};

/// Waarom de driver weigert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Het opzetten liep vast in het transport of de handdruk (ook: geen
    /// blokapparaat, een queue kleiner dan [`QSIZE`], te weinig DMA).
    Setup(driver_virtiopci::Error),
    /// De capaciteit bleef veranderen terwijl de driver hem las.
    ConfigUnstable,
    /// Een lengte die geen veelvoud van de sector is, of buiten de schijf.
    Range {
        /// De eerste sector.
        sector: u64,
        /// Het aantal bytes.
        len: usize,
    },
    /// Het device is alleen-lezen.
    ReadOnly,
    /// Het device antwoordde niet binnen [`REQUEST_TIMEOUT_NS`]; de driver
    /// is vanaf nu dood.
    Timeout {
        /// De sector van het verzoek.
        sector: u64,
    },
    /// Het device meldde een fout (`IOERR` 1, `UNSUPP` 2).
    Status {
        /// De sector van het verzoek.
        sector: u64,
        /// De statusbyte.
        status: u8,
    },
    /// Een eerder verzoek liep af; niets gaat meer naar het device.
    Dead,
    /// Er loopt nog een verzoek (de wachter ging weg vóór de completion);
    /// de DMA-buffer is nog van het device.
    Busy,
    /// Er is geen verzoek om op te wachten.
    Idle,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Setup(e) => write!(f, "virtioblk: {e}"),
            Self::ConfigUnstable => f.write_str("virtioblk: capacity kept changing while read"),
            Self::Range { sector, len } => {
                write!(f, "virtioblk: {len} bytes at sector {sector} out of range")
            }
            Self::ReadOnly => f.write_str("virtioblk: device is read-only"),
            Self::Timeout { sector } => write!(
                f,
                "virtioblk: request at sector {sector} not done after {} s, driver dead",
                REQUEST_TIMEOUT_NS / 1_000_000_000
            ),
            Self::Status { sector, status } => {
                write!(f, "virtioblk: status {status} at sector {sector}")
            }
            Self::Dead => f.write_str("virtioblk: driver dead after a timeout"),
            Self::Busy => f.write_str("virtioblk: a request is still in flight"),
            Self::Idle => f.write_str("virtioblk: no request in flight"),
        }
    }
}

impl From<driver_virtiopci::Error> for Error {
    fn from(e: driver_virtiopci::Error) -> Self {
        Self::Setup(e)
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Is er op `base` een modern virtio-blokapparaat?
///
/// # Safety
///
/// `base` is een gemapt virtio-mmio-slot (minstens 0x200 bytes).
#[must_use]
pub unsafe fn is_modern_blk(base: Pa) -> bool {
    // SAFETY: de voorwaarde van deze functie.
    unsafe { mmio::is_modern(base, DEVICE_BLK) }
}

/// Het verzoek dat op het device staat.
#[derive(Clone, Copy, Debug)]
struct Pending {
    kind: u32,
    sector: u64,
    len: usize,
    t0: u64,
    /// Een FLUSH zonder FLUSH-feature: er ging niets naar het device, en
    /// hij is meteen klaar.
    noop: bool,
}

/// Eén virtio-blk met zijn queue en DMA-buffer, over een virtio-transport:
/// virtio-mmio op QEMU virt, virtio-pci onder EDK2.
pub struct VirtioBlk<T: Transport = Mmio> {
    t: T,
    dma: Pa,
    sectors: u64,
    flush: bool,
    read_only: bool,
    now: fn() -> u64,
    avail_idx: u16,
    last_used: u16,
    dead: bool,
    pending: Option<Pending>,
    irq: Option<&'static Signal>,
    /// Meetlat: afgehandelde verzoeken.
    requests: u64,
    /// Meetlat: het langste verzoek in nanoseconden.
    slowest_ns: u64,
}

impl VirtioBlk<Mmio> {
    /// Zet het device op het virtio-mmio-slot `base` op: reset, VERSION_1
    /// (plus FLUSH als het device hem biedt) onderhandelen, queue 0 in
    /// `dma`, DRIVER_OK. `now` geeft monotone nanoseconden, voor de
    /// time-out van een verzoek.
    ///
    /// # Safety
    ///
    /// `base` is een gemapt virtio-mmio-blok dat voor altijd blijft, en
    /// `[dma, dma+dma_size)` is gemapt, niet gecached geheugen dat alleen
    /// deze driver en het device gebruiken, nu en zolang het programma
    /// draait.
    pub unsafe fn new(base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        // SAFETY: de eerste helft van de voorwaarde van deze functie.
        let t = unsafe { Mmio::new(base) };
        t.check()?;
        // SAFETY: de tweede helft van de voorwaarde van deze functie.
        unsafe { Self::with_transport(t, dma, dma_size, now) }
    }

    /// Het interrupt-pad, voor de dispatch van het board.
    #[must_use]
    pub fn irq_ack(&self) -> IrqAck {
        self.t.irq_ack()
    }
}

impl<T: Transport> VirtioBlk<T> {
    /// Zet het device achter transport `t` op: reset, VERSION_1 (plus
    /// FLUSH als het device hem biedt) onderhandelen, queue 0 in `dma`,
    /// DRIVER_OK. `now` geeft monotone nanoseconden, voor de time-out van
    /// een verzoek.
    ///
    /// # Safety
    ///
    /// `[dma, dma+dma_size)` is gemapt, niet gecached geheugen dat alleen
    /// deze driver en het device gebruiken, nu en zolang het programma
    /// draait.
    pub unsafe fn with_transport(t: T, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        if dma_size < DMA_NEED {
            return Err(driver_virtiopci::Error::DmaTooSmall {
                need: DMA_NEED,
                have: dma_size,
            }
            .into());
        }
        let id = t.device_id();
        if id != DEVICE_BLK {
            return Err(driver_virtiopci::Error::WrongDevice {
                want: DEVICE_BLK,
                got: id,
            }
            .into());
        }
        let mut d = Self {
            t,
            dma,
            sectors: 0,
            flush: false,
            read_only: false,
            now,
            avail_idx: 0,
            last_used: 0,
            dead: false,
            pending: None,
            irq: None,
            requests: 0,
            slowest_ns: 0,
        };
        // VERSION_1, plus wat van FLUSH en RO geboden wordt.
        let lo =
            d.t.negotiate(now, |t| t.device_features(0) & (FEAT_FLUSH | FEAT_RO))?;
        d.flush = lo & FEAT_FLUSH != 0;
        d.read_only = lo & FEAT_RO != 0;

        // De capaciteit, consistent gelezen: de config-generatie mag tussen
        // de twee helften niet wisselen.
        d.sectors =
            d.t.config_read64(offset_of!(BlkConfig, capacity) as u32)
                .ok_or(Error::ConfigUnstable)?;

        d.t.select_queue(0);
        let max = d.t.queue_num_max();
        if max < QSIZE {
            return Err(driver_virtiopci::Error::NoQueue {
                queue: 0,
                offered: max,
            }
            .into());
        }
        d.t.set_queue_num(QSIZE);
        dev::clear(dma, DATA_OFF as usize);
        d.t.set_queue_addrs(dma.add(DESC_OFF), dma.add(AVAIL_OFF), dma.add(USED_OFF));
        d.t.enable_queue();
        d.t.driver_ok();
        Ok(d)
    }

    /// De capaciteit in sectoren van 512 bytes.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    /// Kent het device FLUSH (een write-back-cache)?
    #[must_use]
    pub fn can_flush(&self) -> bool {
        self.flush
    }

    /// De naam voor de bootregel.
    #[must_use]
    pub fn model(&self) -> &'static str {
        "virtio-blk"
    }

    /// De lijn-kant voor een board dat de interrupt wil bevestigen: ook
    /// een driver die pollt, laat een scherpe lijn los.
    pub fn ack(&self) -> u32 {
        self.t.ack_interrupt()
    }

    /// Hangt de bel van de IRQ-lijn aan de driver: de wachter wacht erop
    /// (met de termijn `blkdev::TERM`). Het board luidt hem vanuit zijn
    /// dispatch, na [`IrqAck::ack`].
    pub fn set_irq(&mut self, bell: &'static Signal) {
        self.irq = Some(bell);
    }

    /// Het transport, voor het board (nam het de MSI-X-vector aan?).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.t
    }

    fn set_desc(&self, i: u16, addr: Pa, len: u32, flags: u16, next: u16) {
        let d = self
            .dma
            .add(DESC_OFF + u64::from(i) * size_of::<Desc>() as u64);
        dev::write64(d.add(offset_of!(Desc, addr) as u64), addr.0);
        dev::write32(d.add(offset_of!(Desc, len) as u64), len);
        dev::write16(d.add(offset_of!(Desc, flags) as u64), flags);
        dev::write16(d.add(offset_of!(Desc, next) as u64), next);
    }

    /// Zet een verzoek klaar en luidt de doorbell: kop, (data,) status als
    /// één keten vanaf descriptor 0. De vorige keten is dan al klaar (één
    /// tegelijk), dus de descriptors zijn vrij.
    fn submit(&mut self, kind: u32, sector: u64, len: u32) {
        let hdr = self.dma.add(HDR_OFF);
        dev::write32(hdr.add(offset_of!(ReqHdr, kind) as u64), kind);
        dev::write32(hdr.add(4), 0);
        dev::write64(hdr.add(offset_of!(ReqHdr, sector) as u64), sector);
        let status = self.dma.add(STATUS_OFF);
        dev::write8(status, S_PENDING);
        if kind == T_FLUSH {
            self.set_desc(0, hdr, size_of::<ReqHdr>() as u32, DESC_NEXT, 2);
        } else {
            self.set_desc(0, hdr, size_of::<ReqHdr>() as u32, DESC_NEXT, 1);
            let w = if kind == T_IN { DESC_WRITE } else { 0 };
            self.set_desc(1, self.dma.add(DATA_OFF), len, DESC_NEXT | w, 2);
        }
        self.set_desc(2, status, 1, DESC_WRITE, 0);
        let avail = self.dma.add(AVAIL_OFF);
        dev::write16(avail.add(4 + u64::from(self.avail_idx % QSIZE) * 2), 0);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        driver_virtiopci::publish(avail, self.avail_idx);
        self.t.notify(0);
    }

    /// Zet een verzoek op het device, of weigert. Een verzoek waarvan de
    /// wachter wegging, wordt eerst opgehaald als het klaar is; loopt het
    /// nog, dan [`Error::Busy`]: de DMA-buffer is nog van het device.
    fn begin(&mut self, kind: u32, sector: u64, len: usize) -> Result {
        if self.dead {
            return Err(Error::Dead);
        }
        if self.pending.is_some() && self.reap().is_pending() {
            return Err(Error::Busy);
        }
        let noop = kind == T_FLUSH && !self.flush;
        let t0 = (self.now)();
        if !noop {
            self.submit(kind, sector, len as u32);
        }
        self.pending = Some(Pending {
            kind,
            sector,
            len,
            t0,
            noop,
        });
        Ok(())
    }

    /// Kijkt of het device het verzoek terugzette in de used-ring. Na de
    /// time-out is de driver dood.
    fn reap(&mut self) -> Poll<Result> {
        let Some(p) = self.pending else {
            return Poll::Ready(Err(Error::Idle));
        };
        if p.noop {
            self.pending = None;
            return Poll::Ready(Ok(()));
        }
        let used = self.dma.add(USED_OFF);
        if dev::read16(used.add(2)) == self.last_used {
            if (self.now)() >= p.t0.saturating_add(REQUEST_TIMEOUT_NS) {
                self.dead = true;
                self.pending = None;
                return Poll::Ready(Err(Error::Timeout { sector: p.sector }));
            }
            return Poll::Pending;
        }
        // De index vóór de inhoud: pas na de barrière zijn status en data
        // van het device.
        dev::mb();
        self.pending = None;
        self.last_used = self.last_used.wrapping_add(1);
        self.ack();
        let dt = (self.now)().saturating_sub(p.t0);
        self.slowest_ns = self.slowest_ns.max(dt);
        self.requests += 1;
        Poll::Ready(match dev::read8(self.dma.add(STATUS_OFF)) {
            S_OK => Ok(()),
            status => Err(Error::Status {
                sector: p.sector,
                status,
            }),
        })
    }

    /// Toetst `len` bytes vanaf `sector` tegen de schijf.
    fn check(&self, sector: u64, len: usize) -> Result {
        let n = len as u64;
        let end = n
            .is_multiple_of(SECTOR)
            .then(|| sector.checked_add(n / SECTOR))
            .flatten();
        match end {
            Some(e) if e <= self.sectors => Ok(()),
            _ => Err(Error::Range { sector, len }),
        }
    }

    /// Zet één opdracht van hoogstens [`MAX_TRANSFER`] bytes op het device.
    /// Een write gaat nu de DMA-buffer in, dus pas als er niets meer loopt;
    /// een FLUSH zonder FLUSH-feature is niets (er is dan geen cache).
    fn start_op(&mut self, op: Op<'_>) -> Result {
        match op {
            Op::Read { lba, len } => {
                if len > MAX_TRANSFER {
                    return Err(Error::Range { sector: lba, len });
                }
                self.check(lba, len)?;
                self.begin(T_IN, lba, len)
            }
            Op::Write { lba, data } => {
                if self.read_only {
                    return Err(Error::ReadOnly);
                }
                if data.len() > MAX_TRANSFER {
                    return Err(Error::Range {
                        sector: lba,
                        len: data.len(),
                    });
                }
                self.check(lba, data.len())?;
                if self.dead {
                    return Err(Error::Dead);
                }
                if self.pending.is_some() && self.reap().is_pending() {
                    return Err(Error::Busy);
                }
                dev::copy_in(self.dma.add(DATA_OFF), data);
                self.begin(T_OUT, lba, data.len())
            }
            Op::Flush => self.begin(T_FLUSH, 0, 0),
        }
    }

    /// De completion van [`start_op`](Self::start_op): bij een lees de
    /// bytes uit de DMA-buffer naar `into`.
    fn poll_op(&mut self, into: &mut [u8]) -> Poll<Result> {
        let p = self.pending;
        let r = self.reap();
        if let (Poll::Ready(Ok(())), Some(p)) = (&r, p)
            && p.kind == T_IN
        {
            let n = p.len.min(into.len());
            if let Some(d) = into.get_mut(..n) {
                dev::copy_out(d, self.dma.add(DATA_OFF));
            }
        }
        r
    }
}

/// De blkdev-fout van een driverfout op `lba`.
fn blk_err(e: Error, lba: u64, len: usize) -> blkdev::Error {
    match e {
        Error::Range { .. } => blkdev::Error::OutOfRange { lba, len },
        Error::Dead | Error::Timeout { .. } => blkdev::Error::Dead,
        Error::Busy => blkdev::Error::Busy,
        _ => blkdev::Error::Io { lba },
    }
}

impl<T: Transport> blkdev::Disk for VirtioBlk<T> {
    fn sectors(&self) -> u64 {
        VirtioBlk::sectors(self)
    }
    fn model(&self) -> &str {
        VirtioBlk::model(self)
    }
}

impl<T: Transport> blkdev::AsyncBlockDevice for VirtioBlk<T> {
    fn max_transfer(&self) -> usize {
        MAX_TRANSFER
    }

    /// Eén opdracht tegelijk: ticket 0.
    fn start_tag(&mut self, op: Op<'_>) -> blkdev::Result<usize> {
        let (lba, len) = match op {
            Op::Read { lba, len } => (lba, len),
            Op::Write { lba, data } => (lba, data.len()),
            Op::Flush => (0, 0),
        };
        self.start_op(op)
            .map(|()| 0)
            .map_err(|e| blk_err(e, lba, len))
    }

    fn poll_tag(&mut self, _t: usize, into: &mut [u8]) -> Poll<blkdev::Result> {
        let lba = self.pending.map_or(0, |p| p.sector);
        let len = self.pending.map_or(0, |p| p.len);
        self.poll_op(into).map_err(|e| blk_err(e, lba, len))
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    /// Dezelfde namen als de NVMe-kern (`commands`, `slowest_us`).
    fn stats(&self, out: &mut dyn fmt::Write) -> core::result::Result<u64, fmt::Error> {
        write!(
            out,
            "commands={} slowest_us={}",
            self.requests,
            self.slowest_ns / 1000
        )?;
        Ok(self.requests)
    }
}

#[cfg(test)]
mod tests;
