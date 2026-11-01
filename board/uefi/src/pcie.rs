//! De PCIe-zoektocht van een UEFI-machine met eigen drivers (de O6N, de
//! Altra): de ECAM-vensters uit de MCFG, de eerste functie met een
//! toegewezen BAR, en de NVMe met zijn lijn. Generiek over een
//! [`Config`]-venster, zodat de zoektocht op de host te toetsen is.
//!
//! De firmware configureerde de hiërarchie en wees de BAR's toe: we lezen
//! alleen (Go: "hem overschrijven zou de controller op PA 0 zetten"; op de
//! Altra een data-abort).

use crate::BLK_DMA;
use crate::irq::{self, At, Wired};
use board::Error;
use bounded::BoundedVec;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use dev::Pa;
use driver_nvme::{Nvme, pci::Pci};
use driver_pcie::{CMD_INTX_DISABLE, Config, Ecam, Function, find};
use sync::Signal;

/// Zoveel ECAM-vensters: de Altra heeft er tot acht, de O6N vijf.
pub const MAX_SEGMENTS: usize = 8;

/// De klassecode van een NVMe-controller: mass storage, NVM, NVMe.
pub const CLASS_NVME: u32 = 0x01_08_02;

/// De ECAM-vensters met hun eerste bus.
pub type Segments = BoundedVec<(Ecam, u8), MAX_SEGMENTS>;

/// De ECAM-vensters uit de MCFG (de eerste [`MAX_SEGMENTS`]).
#[must_use]
pub fn segments() -> Segments {
    let mut v = BoundedVec::new();
    for (e, _seg, start) in crate::ecams() {
        if v.push((e, start)).is_err() {
            break;
        }
    }
    v
}

/// Een gevonden device: de functie, de BAR die de driver wil, het venster
/// en zijn eerste bus (op de O6N heeft elke root-poort zijn eigen venster;
/// de INTx-lijn hangt eraan).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// De functie.
    pub f: Function,
    /// Het adres van de BAR.
    pub bar: u64,
    /// De index van het venster waarin hij gevonden is.
    pub win: usize,
    /// De eerste bus van dat venster.
    pub root_bus: u8,
}

/// Zoekt over `windows` (config, eerste bus) de eerste functie waarop `is`
/// ja zegt, met een toegewezen memory-BAR `bar`. Een functie waarvan de BAR
/// nul is, telt niet: de firmware wees hem niet toe, en hem zelf toewijzen
/// doen we op een UEFI-machine niet.
pub fn first<'a, C: Config + 'a>(
    windows: impl IntoIterator<Item = (&'a C, u8)>,
    bar: u8,
    is: impl Fn(&Function) -> bool,
) -> Option<Found> {
    windows
        .into_iter()
        .enumerate()
        .find_map(|(win, (c, start))| {
            find(c, start, |f| {
                !f.is_bridge() && is(f) && f.bar_addr(c, bar) != 0
            })
            .map(|f| Found {
                f,
                bar: f.bar_addr(c, bar),
                win,
                root_bus: start,
            })
        })
}

/// De eerste functie over `segs` (zie [`first`]).
#[must_use]
pub fn first_in(segs: &Segments, bar: u8, is: impl Fn(&Function) -> bool) -> Option<Found> {
    first(segs.as_slice().iter().map(|(e, s)| (e, *s)), bar, is)
}

/// Vindt en initialiseert de eerste NVMe (het hele device) in [`BLK_DMA`],
/// met zijn lijn ([`wire_line`]; de interrupts staan al). `Ok(None)` = geen
/// NVMe; een lijn die niet komt is [`Error::Irq`]. Eén keer: de aanroeper
/// bewaakt dat.
pub fn probe_nvme() -> Result<Option<Nvme<Pci>>, Error> {
    let segs = segments();
    let Some(hit) = first_in(&segs, 0, |f| f.class == CLASS_NVME) else {
        return Ok(None);
    };
    // Een hoge BAR (boven 1 TB, de Altra) moet eerst in de identity map.
    if !crate::map_device(hit.bar, driver_nvme::pci::MMIO_LEN) {
        return Err(Error::Disk("NVMe BAR0 unreachable"));
    }
    if let Some((e, _)) = segs.get(hit.win) {
        hit.f.enable(e);
        // Stil tot zijn lijn er is ([`wire_line`]): geen INTx, en geen
        // MSI-X van de vorige kern van een flip naar een ITS die deze kern
        // nog niet opzette (Linux `msix_capability_init` zet hem ook eerst
        // uit).
        hit.f.set_command(e, CMD_INTX_DISABLE);
        if let Some(m) = hit.f.msix(e) {
            hit.f.msix_enable(e, &m, false);
        }
    }
    // SAFETY: BAR0 is door de firmware toegewezen en nu Device-gemapt
    // (`map_device`); memory-decode en bus-mastering staan aan. BLK_DMA is
    // van deze driver alleen, Normal-NC gemapt door de stub.
    let mut disk =
        unsafe { Nvme::<Pci>::new(Pa(hit.bar), BLK_DMA.base, BLK_DMA.size, cpu::idle::now) }
            .map_err(|e| {
                cpu::println!("disk: {e} HOPOS_NVME_FAIL");
                Error::Disk("nvme init failed")
            })?;
    let (major, minor) = disk.version();
    cpu::println!(
        "disk: nvme {} at {} bar0 {:#x}, NVMe {major}.{minor}, {} blocks of {} bytes, max transfer {} HOPOS_NVME_UP",
        disk.model(),
        hit.f.bdf,
        hit.bar,
        disk.blocks(),
        disk.block_size(),
        disk.max_transfer()
    );
    let (wired, us, vectors) = wire_line(&hit, &mut disk).map_err(|why| {
        cpu::println!(
            "disk: nvme at {} has no line: {why} HOPOS_NVME_IRQ",
            hit.f.bdf
        );
        Error::Irq("the NVMe line did not come up")
    })?;
    cpu::println!(
        "disk: nvme at {} {wired}, vector 0 of {vectors}, first interrupt after {us} us HOPOS_NVME_IRQ",
        hit.f.bdf
    );
    Ok(Some(disk))
}

/// De bel van de NVMe-lijn: de dispatch luidt hem, de wachter van de
/// opslag (`blkdev::Queue`) slaapt erop.
static NVME_BELL: Signal = Signal::new();

/// De LPI van de schijf (de NVMe, of de virtio-blk van QEMU), 0 = geen: de
/// dispatch telt hem niet als NIC.
pub(crate) static DISK_LPI: AtomicU32 = AtomicU32::new(0);

/// Hoe lang de zelftest op de interrupt wacht (zoals de igb op de Altra).
const IRQ_TEST_NS: u64 = 50_000_000;

/// Zet de NVMe op zijn lijn: MSI-X vector 0 via de ITS van zijn
/// root-complex (`irq::wire_msix`, de weg van de NIC), en dan de zelftest:
/// één admin-opdracht, en de dispatch moet zijn interrupt binnen
/// [`IRQ_TEST_NS`] zien. Pas dan krijgt de driver de bel; anders de reden
/// (een stille route is een schijf zonder lijn). Nooit INTx: dat doodt de
/// Altra (L83). Geeft de lijn, de microseconden tot de eerste interrupt en
/// het aantal vectoren van de functie.
fn wire_line(hit: &Found, disk: &mut Nvme<Pci>) -> Result<(Wired, u64, u16), &'static str> {
    let segs = segments();
    let (e, _) = segs.get(hit.win).ok_or("no config window")?;
    // Het segment van het venster: `segments` en `pcie_segments` lopen
    // allebei de MCFG in volgorde af.
    let seg = crate::pcie_segments().nth(hit.win).map_or(0, |(_, s, _)| s);
    let at = At {
        ecam: e,
        seg,
        root_bus: hit.root_bus,
        f: &hit.f,
    };
    let m = hit.f.msix(e).ok_or("no MSI-X capability")?;
    let wired = irq::wire_msix(&at, &NVME_BELL, irq::no_ack, true)?;
    let Wired::Msix { lpi, dev_id } = wired else {
        return Err("not MSI-X");
    };
    DISK_LPI.store(lpi, Relaxed);
    let _ = NVME_BELL.take();
    disk.fire_irq()
        .map_err(|_| "the self-test command failed")?;
    if let Some(us) = irq::bell_within(&NVME_BELL, IRQ_TEST_NS) {
        disk.set_irq(&NVME_BELL);
        return Ok((wired, us, m.size));
    }
    cpu::println!(
        "disk: nvme MSI-X diag: {} HOPOS_NVME_IRQ_DIAG",
        irq::msix_diag(&at, dev_id)
    );
    hit.f.msix_enable(e, &m, false);
    Err("the self-test interrupt did not arrive within 50 ms")
}
