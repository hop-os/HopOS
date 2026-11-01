//! Interrupts over PCI op een UEFI-machine: MSI-X via de GICv3-ITS
//! ([`wire_msix`]), of INTx uit de `_PRT` ([`wire_intx`]).
//!
//! Dit bezit de ITS'en (de eerste uit de MADT met zijn tabellen in
//! [`crate::ITS_DMA`], en zo nodig de ITS die de IORT voor het root-complex
//! van een device noemt, in [`crate::ITS_MORE_DMA`]). Het resultaat is
//! altijd een lijn in [`crate::Uefi::enable_line`] (een LPI of een SPI) plus
//! een bel; de dispatch ziet het verschil niet.
//!
//! Welke van de twee, kiest het board, één weg per device en geen knop
//! (Derek, 01-11; `hopos.nicirq` is weg). Een weg die niet opkomt, is een
//! device zonder lijn, en dat is een fout (docs/sequence-hopos.md §14.1
//! O3), geen terugval op de andere:
//!
//! - MSI-X: de virtio-functies van QEMU, de NVMe overal, en de igb van de
//!   Altra. Een LPI is een flank die de ITS naar precies één redistributor
//!   stuurt, zonder level-lijn die blijft hangen als een ack mist. Op de
//!   Altra is het de enige weg: de `_PRT`-INTx aanzetten doodde daar de SoC
//!   (L83, 19-09, UART-bewijs).
//! - INTx: de Realtek van de O6N, de weg die daar op ijzer bewezen is (L80,
//!   INTID 477 uit de `_PRT`, host-getoetst tegen de echte DSDT). Een
//!   level-lijn met de ack van de driver (masker dicht, status schoon) en de
//!   STRAY-grens van de dispatch; de freeze van 17/18-09 ("INTID 477 keeps
//!   asserting after 256 acks in one pass") was een ack die het masker
//!   openliet.

use crate::facts;
use board::Error;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use dev::Pa;
use driver_gicv3::its::Its;
use driver_gicv3::{FIRST_LPI, Gic, Icc};
use driver_pcie::{Ecam, Function, MsixTable};
use sync::{Local, Signal};

/// Hoeveel ITS'en we opzetten: de eerste uit de MADT plus wat in
/// [`crate::ITS_MORE_DMA`] past.
const ITS_MAX: usize = 1 + crate::ITS_MORE;

/// De ITS'en: plek 0 is de eerste uit de MADT, zodra [`start_its`] hem
/// opzette; de rest komt erbij als een device een andere ITS heeft
/// ([`its_for`]). Alleen de executor van de kern-core raakt ze aan (boot,
/// en het bedraden van een device), en de lening loopt nooit over een
/// `.await`.
static ITS: Local<RefCell<[Option<Its>; ITS_MAX]>> =
    Local::new(RefCell::new([const { None }; ITS_MAX]));

/// De redistributor van de kern-core (RD_base en GICR_TYPER), voor de
/// collectie van een ITS die later opkomt.
static RD: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Stonden de LPI's al aan toen [`start_its`] liep (een warme flip): dan
/// houdt de redistributor de tabellen van de vorige kern (op hetzelfde
/// adres) en wist deze kern alleen de ITS-kant. Voor de diagnoseregel.
static LPIS_REUSED: AtomicBool = AtomicBool::new(false);

/// Zet de ITS op als de MADT er een noemt: LPI's aan op de redistributor
/// van de kern-core, de tabellen, collectie 0 naar deze core. Eén regel op
/// de console, wat er ook gebeurt; zonder ITS pollen of INTx'en de devices.
pub(crate) fn start_its<I: Icc>(gic: &Gic<I>) {
    let base = facts::ITS.load(Relaxed);
    if base == 0 {
        cpu::println!("irq: no ITS in the MADT, PCI devices use INTx or poll HOPOS_ITS_NONE");
        return;
    }
    // SAFETY: het ITS-frame komt uit de MADT en de identity map mapt het
    // als Device (`boot::build_map`, ook boven 1 TB); ITS_DMA is een eigen
    // stuk van de NC-gemapte DMA-regio, 64 KB-gealigneerd, van niemand
    // anders (de const-toets in lib.rs).
    let mut its = unsafe { Its::new(Pa(base), crate::ITS_DMA.base, cpu::idle::now) };
    let reused = gic.lpis_enabled();
    LPIS_REUSED.store(reused, Relaxed);
    if !reused {
        its.clear_pending();
    }
    its.clear();
    match gic.enable_lpis(its.prop_table(), its.pend_table()) {
        Ok(_) => {}
        Err(e) => {
            cpu::println!("irq: {e}, PCI devices use INTx or poll HOPOS_ITS_FAIL");
            return;
        }
    }
    let (rd, typer) = gic.redistributor();
    RD[0].store(rd.0, Relaxed);
    RD[1].store(typer, Relaxed);
    match its.init(rd, typer) {
        Ok(d) => {
            cpu::println!(
                "irq: {d}, LPIs {} on the redistributor at {:#x} HOPOS_ITS_UP",
                if reused { "reused" } else { "enabled" },
                rd.0
            );
            if let Some(s) = ITS.get().borrow_mut().first_mut() {
                *s = Some(its);
            }
        }
        Err(e) => cpu::println!("irq: {e}, PCI devices use INTx or poll HOPOS_ITS_FAIL"),
    }
}

/// Zet een LPI aan (de lijn van [`crate::Uefi::enable_line`]).
pub(crate) fn enable_lpi(lpi: u32) -> Result<(), Error> {
    let mut list = ITS.get().borrow_mut();
    let its = list
        .iter_mut()
        .flatten()
        .find(|i| i.owns(lpi))
        .ok_or(Error::Irq("LPI not routed by an ITS"))?;
    match its.enable(lpi, true) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Irq("LPI not routed by this ITS")),
        Err(_) => Err(Error::Irq("ITS refused the LPI")),
    }
}

/// Het frame van de ITS die de IORT voor het root-complex van `at` noemt
/// (de GIC ITS ID van de ITS-groep, opgezocht in de MADT); anders de eerste
/// uit de MADT. De Altra heeft een ITS per root-complex, en een MSI naar de
/// doorbell van een andere komt nooit aan (A7g, 03-10: de igb op segment 5
/// hoort bij ITS 7, de schrijf naar ITS 0 verdween; een `INT` vanuit ITS 0
/// kwam wel).
fn its_base_for(at: &At<'_>) -> u64 {
    let named = facts::tables(*b"IORT")
        .next()
        .and_then(|t| fw::acpi::iort_route(t, at.seg, rid(at.f)))
        .and_then(|r| r.its.1);
    let madt = facts::tables(*b"APIC")
        .next()
        .and_then(|t| fw::acpi::Madt::new(t).ok());
    pick_its(
        facts::ITS.load(Relaxed),
        named,
        madt.iter().flat_map(fw::acpi::Madt::its_ids),
    )
}

/// De keuze van [`its_base_for`]: de basis van de ITS met GIC ITS ID
/// `named` uit `madt`, anders `first`.
fn pick_its(first: u64, named: Option<u32>, mut madt: impl Iterator<Item = (u32, u64)>) -> u64 {
    named
        .and_then(|id| madt.find(|&(i, _)| i == id))
        .map_or(first, |(_, base)| base)
}

/// De plek in [`ITS`] van de ITS op `base`, die zo nodig nu opkomt: eigen
/// tabellen en commandorij in [`crate::ITS_MORE_DMA`], de
/// LPI-configuratie van de redistributor (die van de eerste ITS), een eigen
/// stuk LPI-nummers, en collectie 0 op dezelfde redistributor (Linux doet
/// zo elke ITS-knoop op in `its_probe_one`). Eén regel op de console.
fn its_for(base: u64, bdf: driver_pcie::Bdf) -> Result<usize, &'static str> {
    let mut list = ITS.get().borrow_mut();
    if let Some(i) = list
        .iter()
        .position(|s| s.as_ref().is_some_and(|i| i.base().0 == base))
    {
        return Ok(i);
    }
    let prop = list
        .first()
        .and_then(Option::as_ref)
        .map(Its::prop_table)
        .ok_or("no ITS")?;
    let slot = list
        .iter()
        .position(Option::is_none)
        .ok_or("no room for another ITS")?;
    if !crate::map_device(base, 0x2_0000) {
        return Err("the device's ITS is unreachable");
    }
    let mem = crate::ITS_MORE_DMA
        .base
        .add((slot as u64 - 1) * driver_gicv3::its::MEM_LEN);
    // SAFETY: het frame komt uit de MADT (de ITS die de IORT noemt) en is nu
    // Device-gemapt; `mem` is plek `slot - 1` van ITS_MORE_DMA, 64 KB-
    // gealigneerd, Normal-NC, en alleen van deze ITS (elke plek één keer).
    let mut its = unsafe { Its::new(Pa(base), mem, cpu::idle::now) }.beside(
        prop,
        FIRST_LPI + (slot * driver_gicv3::its::MAX_LPIS) as u32,
    );
    its.clear();
    let rd = Pa(RD[0].load(Relaxed));
    match its.init(rd, RD[1].load(Relaxed)) {
        Ok(d) => cpu::println!(
            "irq: {d}, for {bdf} (the IORT's ITS for its root complex), collection on the redistributor at {:#x} HOPOS_ITS_MORE",
            rd.0
        ),
        Err(e) => {
            cpu::println!("irq: ITS at {base:#x} for {bdf}: {e} HOPOS_ITS_FAIL");
            return Err("the device's ITS did not come up");
        }
    }
    if let Some(s) = list.get_mut(slot) {
        *s = Some(its);
    }
    Ok(slot)
}

/// Is `id` een LPI (en geen SPI, PPI of SGI)?
pub(crate) const fn is_lpi(id: u32) -> bool {
    id >= FIRST_LPI
}

/// Hoe een device aan zijn lijn kwam.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Wired {
    /// MSI-X vector 0 via de ITS, op deze LPI.
    Msix {
        /// De LPI.
        lpi: u32,
        /// De DeviceID die de ITS ziet.
        dev_id: u32,
    },
    /// INTx op deze SPI (uit de `_PRT`).
    Intx {
        /// De INTID.
        intid: u32,
    },
}

impl core::fmt::Display for Wired {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Msix { lpi, dev_id } => {
                write!(f, "MSI-X via the ITS, LPI {lpi} (DeviceID {dev_id:#x})")
            }
            Self::Intx { intid } => write!(f, "INTx on INTID {intid} (SPI {})", intid - 32),
        }
    }
}

/// Een device op een PCIe-segment: waar het zit, zodat IORT en `_PRT` het
/// kunnen vinden.
#[derive(Copy, Clone, Debug)]
pub struct At<'a> {
    /// Het config-venster.
    pub ecam: &'a Ecam,
    /// Het PCI-segment (`_SEG`, en het segment van de IORT).
    pub seg: u16,
    /// De eerste bus van het venster (de `_BBN` van de host-bridge).
    pub root_bus: u8,
    /// De functie.
    pub f: &'a Function,
}

/// De requester-id van een functie: bus, device, functie.
fn rid(f: &Function) -> u16 {
    (u16::from(f.bdf.bus) << 8) | (u16::from(f.bdf.dev) << 3) | u16::from(f.bdf.func)
}

/// De DeviceID van `at` voor de ITS: via de IORT, anders de requester-id
/// (QEMU's afbeelding, en die van elke SoC met één root-complex). Het
/// tweede is een gok, en dat staat in de regel van de aanroeper.
fn device_id(at: &At<'_>) -> (u32, bool) {
    let rid = rid(at.f);
    match facts::tables(*b"IORT")
        .next()
        .and_then(|t| fw::acpi::iort_device_id(t, at.seg, rid))
    {
        Some(id) => (id, true),
        None => (u32::from(rid), false),
    }
}

/// MSI-X vector 0 van `at` naar een eigen LPI: de ITS kent hem toe, de
/// MSI-X-tabel krijgt de doorbell en de EventID, MSI-X gaat aan (en INTx
/// uit), en de LPI wordt een lijn met `bell` en `ack`.
///
/// Zonder IORT-weg is de DeviceID een gok (de requester-id, QEMU's
/// afbeelding), en een gok die mis is, is stil: de ITS gooit de schrijf weg.
/// Daarom staat de gok op de console, en de NVMe en de igb hebben een
/// zelftest die hem vangt.
pub fn wire_msix(at: &At<'_>, bell: &'static Signal, ack: fn()) -> Result<Wired, &'static str> {
    let e = at.ecam;
    let m = at.f.msix(e).ok_or("no MSI-X capability")?;
    let (dev_id, from_iort) = device_id(at);
    let table = at.f.msix_table_addr(e, &m).ok_or("MSI-X BAR unassigned")?;
    if !crate::map_device(table, 16 * u64::from(m.size)) {
        return Err("MSI-X table unreachable");
    }
    let k = its_for(its_base_for(at), at.f.bdf)?;
    let (lpi, doorbell) = {
        let mut list = ITS.get().borrow_mut();
        let its = list.get_mut(k).and_then(Option::as_mut).ok_or("no ITS")?;
        let lpi = its.route(dev_id, 0).map_err(|e| {
            cpu::println!("irq: {} DeviceID {dev_id:#x}: {e}", at.f.bdf);
            "ITS refused the device"
        })?;
        (lpi, its.doorbell())
    };
    // SAFETY: de tabel ligt in een BAR van deze functie die de firmware
    // toewees, nu Device-gemapt (`map_device`), met memory-decode aan (de
    // driver zette hem aan vóór deze bedrading).
    let t = unsafe { MsixTable::new(Pa(table), m.size) };
    t.set(0, doorbell.0, 0);
    at.f.msix_enable(e, &m, true);
    crate::Uefi::new()
        .enable_line(lpi, bell, ack)
        .map_err(|_| "no free line slot")?;
    if !from_iort {
        cpu::println!(
            "irq: no IORT mapping for segment {} rid {:#x}, DeviceID = rid (the QEMU mapping)",
            at.seg,
            rid(at.f)
        );
    }
    Ok(Wired::Msix { lpi, dev_id })
}

/// De INTx-lijn van `at` uit de `_PRT` van zijn host-bridge (DSDT, dan de
/// SSDT's), na de swizzle door de bridges ertussen.
pub fn intx_line(at: &At<'_>) -> Result<u32, &'static str> {
    let (dev, pin) = driver_pcie::intx_at_root(at.ecam, at.root_bus, at.f).ok_or("no INTx pin")?;
    let mut last = "no _PRT for this host bridge";
    for t in facts::tables(*b"DSDT").chain(facts::tables(*b"SSDT")) {
        match fw::aml::prt(t, at.seg, at.root_bus) {
            Ok(p) => return p.lookup(dev, pin).ok_or("_PRT has no entry for this pin"),
            Err(fw::aml::Error::NotFound { .. }) => {}
            Err(e) => {
                cpu::println!("irq: {} INTx: {e}", at.f.bdf);
                last = "_PRT is not static data";
            }
        }
    }
    Err(last)
}

/// De INTx-lijn van `at` uit de `_PRT`, aan met `bell` en `ack`; `ack`
/// draait in de dispatch vóór de EOI en laat de level-lijn vallen.
pub fn wire_intx(at: &At<'_>, bell: &'static Signal, ack: fn()) -> Result<Wired, &'static str> {
    let id = intx_line(at)?;
    crate::Uefi::new()
        .enable_line(id, bell, ack)
        .map_err(|_| "INTx line refused")?;
    Ok(Wired::Intx { intid: id })
}

/// Vuurt event 0 van `dev_id` vanuit de ITS zelf (`INT`): de LPI zonder
/// device. Komt hij hierop wel en op de MSI van het device niet, dan zit
/// de fout tussen device en ITS (doorbell, DeviceID, SMMU).
pub fn its_fire(dev_id: u32) -> Result<(), &'static str> {
    let mut list = ITS.get().borrow_mut();
    let its = list
        .iter_mut()
        .flatten()
        .find(|i| i.routed(dev_id, 0).is_some())
        .ok_or("not routed in this kernel")?;
    match its.fire(dev_id, 0) {
        Ok(true) => Ok(()),
        Ok(false) => Err("not routed in this kernel"),
        Err(_) => Err("the ITS refused INT"),
    }
}

/// Wat er te zien is als de MSI-X van `at` (DeviceID `dev_id`, vector 0)
/// niet aankomt: de weg door de IORT met de SMMU, welke ITS de IORT noemt
/// tegen de onze, of de ITS-mapping in dit kernleven liep, en de
/// MSI-X-stand van de functie zoals hij hem teruggeeft. Alleen lezen.
pub fn msix_diag(at: &At<'_>, dev_id: u32) -> MsixDiag {
    let e = at.ecam;
    let rid = rid(at.f);
    let route = facts::tables(*b"IORT")
        .next()
        .and_then(|t| fw::acpi::iort_route(t, at.seg, rid));
    let smmu = match route.and_then(|r| r.smmu) {
        Some((4, base)) if base != 0 && crate::map_device(base, 0x1000) => {
            // SMMU_CR0 (0x20) en SMMU_GBPA (0x44), pagina 0, niet-secure.
            Some((dev::read32(Pa(base + 0x20)), dev::read32(Pa(base + 0x44))))
        }
        _ => None,
    };
    // De ITS die de device-route heeft, anders de eerste.
    let list = ITS.get().borrow();
    let mine = list
        .iter()
        .flatten()
        .find(|i| i.routed(dev_id, 0).is_some())
        .or_else(|| list.first().and_then(Option::as_ref));
    let ours = mine.map_or(facts::ITS.load(Relaxed), |i| i.base().0);
    let (routed, state) = mine.map_or((None, (0, 0, 0)), |i| (i.routed(dev_id, 0), i.state()));
    drop(list);
    let mut its_n = 0u32;
    let (mut ours_id, mut named_base) = (None, None);
    if let Some(m) = facts::tables(*b"APIC")
        .next()
        .and_then(|t| fw::acpi::Madt::new(t).ok())
    {
        for (id, base) in m.its_ids() {
            its_n += 1;
            if base == ours {
                ours_id = Some(id);
            }
            if route.and_then(|r| r.its.1) == Some(id) {
                named_base = Some(base);
            }
        }
    }
    let m = at.f.msix(e);
    let entry = m.and_then(|m| {
        let t = at.f.msix_table_addr(e, &m)?;
        // SAFETY: zoals in `wire_msix`: de tabel in een BAR van deze
        // functie, Device-gemapt door de bedrading, memory-decode aan.
        unsafe { MsixTable::new(Pa(t), m.size) }.get(0)
    });
    let pba = m
        .and_then(|m| at.f.msix_pba_addr(e, &m))
        .filter(|&p| crate::map_device(p, 8))
        .map(|p| dev::read32(Pa(p)));
    MsixDiag {
        dev_id,
        seg: at.seg,
        rid,
        route,
        smmu,
        its_n,
        ours: (ours_id, ours),
        named_base,
        reused: LPIS_REUSED.load(Relaxed),
        routed,
        state,
        control: m.map(|m| at.f.msix_control(e, &m)),
        command: at.f.command(e),
        entry,
        pba,
    }
}

/// Zie [`msix_diag`].
#[derive(Copy, Clone, Debug)]
pub struct MsixDiag {
    dev_id: u32,
    seg: u16,
    rid: u16,
    route: Option<fw::acpi::IortRoute>,
    smmu: Option<(u32, u32)>,
    its_n: u32,
    ours: (Option<u32>, u64),
    named_base: Option<u64>,
    reused: bool,
    routed: Option<u32>,
    state: (u32, u64, u64),
    control: Option<u16>,
    command: u16,
    entry: Option<[u32; 4]>,
    pba: Option<u32>,
}

impl core::fmt::Display for MsixDiag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "DeviceID {:#x} (seg {} rid {:#x}, ",
            self.dev_id, self.seg, self.rid
        )?;
        match self.route {
            None => f.write_str("no IORT route, guessed")?,
            Some(r) => {
                match r.smmu {
                    None => f.write_str("IORT: no SMMU")?,
                    Some((4, base)) => match self.smmu {
                        Some((cr0, gbpa)) => write!(
                            f,
                            "IORT: SMMUv3 at {base:#x} CR0 {cr0:#x} (SMMUEN {}) GBPA {gbpa:#x} ({})",
                            cr0 & 1,
                            if gbpa & (1 << 20) != 0 {
                                "abort"
                            } else {
                                "bypass"
                            }
                        )?,
                        None => write!(f, "IORT: SMMUv3 at {base:#x}, not readable")?,
                    },
                    Some((t, base)) => {
                        write!(f, "IORT: SMMUv1/2 (node type {t}) at {base:#x}, not read")?;
                    }
                }
                match (r.its.1, self.named_base) {
                    (Some(id), Some(b)) => {
                        write!(f, ", group of {} names ITS id {id} at {b:#x}", r.its.0)?;
                    }
                    (Some(id), None) => {
                        write!(
                            f,
                            ", group of {} names ITS id {id}, not in the MADT",
                            r.its.0
                        )?;
                    }
                    (None, _) => f.write_str(", empty ITS group")?,
                }
            }
        }
        match self.ours.0 {
            Some(id) => write!(f, "); ours ITS id {id} at {:#x}", self.ours.1)?,
            None => write!(f, "); ours ITS at {:#x}", self.ours.1)?,
        }
        write!(
            f,
            " of {} in the MADT, LPIs {}; ",
            self.its_n,
            if self.reused { "reused" } else { "fresh" }
        )?;
        let (ctlr, creadr, cwriter) = self.state;
        match self.routed {
            Some(lpi) => write!(f, "MAPD/MAPTI/INV/SYNC ran in this kernel (LPI {lpi})")?,
            None => f.write_str("NOT routed in this kernel")?,
        }
        write!(
            f,
            ", GITS_CTLR {ctlr:#x} CREADR {creadr:#x} CWRITER {cwriter:#x}; "
        )?;
        match self.control {
            Some(c) => write!(
                f,
                "MSI-X control {c:#x} (enable {} function mask {})",
                (c >> 15) & 1,
                (c >> 14) & 1
            )?,
            None => f.write_str("no MSI-X capability")?,
        }
        write!(
            f,
            ", command {:#x} (memory {} bus master {})",
            self.command,
            (self.command >> 1) & 1,
            (self.command >> 2) & 1
        )?;
        match self.entry {
            Some([lo, hi, data, ctl]) => write!(
                f,
                ", entry 0 address {:#x} data {data:#x} control {ctl:#x}",
                (u64::from(hi) << 32) | u64::from(lo)
            )?,
            None => f.write_str(", entry 0 unreadable")?,
        }
        match self.pba {
            Some(p) => write!(f, ", PBA {p:#x}"),
            None => f.write_str(", PBA unreadable"),
        }
    }
}

/// Een ack die niets doet: MSI-X is een flank, er is geen lijn te laten
/// vallen.
pub fn no_ack() {}

/// Draait de dispatch tot `bell` gaat (de microseconden tot dan) of `ns`
/// verstreken is: de zelftest van een lijn, vóór de executor.
pub fn bell_within(bell: &Signal, ns: u64) -> Option<u64> {
    let t0 = cpu::idle::now();
    let rang = dev::poll_until(cpu::idle::now, ns, || {
        let _ = board::Board::dispatch_interrupts(&crate::Uefi::new());
        bell.take()
    });
    rang.then(|| cpu::idle::now().saturating_sub(t0) / 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_iorts_its_wins_over_the_first() {
        let madt = [(0, 0x1001_0004_0000), (7, 0x1001_0012_0000)];
        // De Altra: de igb op segment 5 hoort bij ITS 7.
        assert_eq!(
            pick_its(0x1001_0004_0000, Some(7), madt.into_iter()),
            0x1001_0012_0000
        );
        // QEMU en de O6N: één ITS, de IORT noemt hem.
        assert_eq!(
            pick_its(0x808_0000, Some(0), [(0, 0x808_0000)].into_iter()),
            0x808_0000
        );
        // Geen IORT-weg, of een ID die de MADT niet kent: de eerste.
        assert_eq!(
            pick_its(0x1001_0004_0000, None, madt.into_iter()),
            0x1001_0004_0000
        );
        assert_eq!(
            pick_its(0x1001_0004_0000, Some(3), madt.into_iter()),
            0x1001_0004_0000
        );
    }

    #[test]
    fn lpis_start_at_8192() {
        assert!(is_lpi(8192) && !is_lpi(477));
    }
}
