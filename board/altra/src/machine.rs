//! De Altra als [`Platform`] op het UEFI-board: de igb, de NVMe en de
//! SMpro. Het board-contract zelf, en alles wat niet anders is dan op elke
//! UEFI-machine, is van [`On`].

use crate::{LINK_TIMEOUT_NS, is_nic};
use board::{ClockKnob, CoreClass, Error, Thermal};
use board_uefi::irq::{At, Wired, bell_within};
use board_uefi::{NET_BUF, NET_DMA, On, Platform, Uefi, pcie};
use core::cell::RefCell;
use dev::Pa;
use driver_igb::{Igb, IrqAck};
use driver_nvme::{Nvme, pci::Pci};
use driver_smpro::{HWMON_CHANNEL, Smpro};
use executor::Executor;
use netdev::AckSlot;
use sync::{Local, Signal};

// Het cacheable bufferblok van de stub (`net-wb`) is precies dat van de
// igb; zijn ringen liggen ervoor en blijven NC.
const _: () = assert!(
    NET_BUF.base.0 == NET_DMA.base.0 + driver_igb::BUF_OFF
        && driver_igb::DMA_NEED <= NET_BUF.end().0 - NET_DMA.base.0
);

/// De bel van de NIC-lijn.
static NIC_BELL: Signal = Signal::new();

/// De ack van de igb (EIMC), voor de dispatch op de kern-core; gezet vóór
/// de lijn scherp gaat.
static NIC_ACK: AckSlot<IrqAck> = AckSlot::new();

/// Hoe lang de zelftest op de afgevuurde vector wacht (zoals de tg3 op de
/// M4).
const NIC_TEST_NS: u64 = 50_000_000;

/// Wat een `INT` vanuit de ITS deed (de proef zonder device).
enum IntTest {
    /// De LPI kwam, na zoveel microseconden.
    Arrived(u64),
    /// Geen LPI binnen [`NIC_TEST_NS`].
    Silent,
    /// Het commando liep niet.
    Refused(&'static str),
}

impl core::fmt::Display for IntTest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Arrived(us) => write!(
                f,
                "arrived after {us} us (the ITS side works; the igb write does not reach it)"
            ),
            Self::Silent => f.write_str(
                "silent within 50 ms (the ITS, redistributor or collection side is broken)",
            ),
            Self::Refused(why) => write!(f, "not run: {why}"),
        }
    }
}

/// De ack van de NIC-lijn, uit de dispatch vóór de EOI.
fn igb_ack() {
    NIC_ACK.ack();
}

/// De SMpro, zodra [`open_hwmon`] het PCC-kanaal opende. Alleen de
/// executor van core 0 raakt hem aan.
static HWMON: Local<RefCell<Option<Smpro>>> = Local::new(RefCell::new(None));

/// De Altra op het UEFI-board.
pub type Altra = On<Ampere>;

/// Wat de Altra boven het UEFI-board heeft.
pub struct Ampere;

/// De SMpro via PCC-kanaal 14.
impl Thermal for Ampere {
    /// De SMpro met de PCCT van de firmware (één proeflees, één regel).
    fn open_thermal(&self) {
        match Uefi::new().acpi_table(b"PCCT") {
            Some(pcct) => open_hwmon(pcct),
            None => cpu::println!("hwmon: no PCCT - temperature telemetry off"),
        }
    }

    /// De SoC-temperatuur in milligraden, 0 = geen meting.
    fn temp_milli_c(&self) -> i32 {
        HWMON
            .get()
            .borrow_mut()
            .as_mut()
            .and_then(Smpro::soc_temp_milli_c)
            .unwrap_or(0)
    }
}

/// De klok is op servers firmware-domein: geen knop, geen beleid.
impl ClockKnob for Ampere {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;

    fn no_knob(&self, _exec: &'static Executor) {
        cpu::println!("dvfs: server clocks are firmware domain on this board HOPOS_CLOCK_NONE");
    }
}

/// Opent de SMpro op subkanaal [`HWMON_CHANNEL`] van de PCCT-tabel `pcct`
/// (de bytes van de tabel) en doet één proeflees. Geen PCCT of geen
/// antwoord: één regel en verder zonder; telemetrie is nooit een
/// boot-blokker.
fn open_hwmon(pcct: &[u8]) {
    let Some(p) = fw::acpi::pcct_subspace(pcct, HWMON_CHANNEL) else {
        cpu::println!("hwmon: no PCC hwmon channel in the PCCT - temperature telemetry off");
        return;
    };
    // Shmem ligt in gereserveerd DRAM, de doorbell kan in hoge SoC-MMIO
    // wonen: zelfde luik als de hoge BAR's.
    if !board_uefi::map_device(p.shmem, p.shmem_len) || !board_uefi::map_device(p.doorbell, 8) {
        cpu::println!("hwmon: PCC channel unreachable - temperature telemetry off");
        return;
    }
    // SAFETY: shmem en doorbell komen uit de PCCT en zijn nu Device-
    // gemapt; alleen deze driver en de SMpro gebruiken kanaal 14.
    let mut d = unsafe { Smpro::new(HWMON_CHANNEL, p, cpu::idle::now) };
    match d.soc_temp_milli_c() {
        Some(m) => cpu::println!(
            "hwmon: SoC {}.{}C (SMpro, PCC channel {HWMON_CHANNEL})",
            m / 1000,
            (m % 1000) / 100
        ),
        None => {
            cpu::println!(
                "hwmon: SMpro not answering on PCC channel {HWMON_CHANNEL} - temperature telemetry off"
            );
            return;
        }
    }
    *HWMON.get().borrow_mut() = Some(d);
}

/// De lijn van de igb: MSI-X vector 0 via de ITS (`board_uefi::irq`),
/// en dan een zelftest: de driver vuurt de vector met de hand (EICS) en
/// de dispatch moet hem binnen [`NIC_TEST_NS`] zien. Een route die niet
/// klopt (de tabel, de DeviceID uit de IORT, de ITS) is stil; zonder
/// zelftest hoorde de pomp nooit iets. Nooit INTx: dat doodt deze SoC
/// (L83, 19-09). Geeft de lijn en de microseconden tot de eerste
/// aflevering, of de reden: een NIC zonder lijn is een fout
/// (docs/sequence-hopos.md §14.1 O3).
fn wire_nic(
    segs: &pcie::Segments,
    hit: &pcie::Found,
    nic: &mut Igb,
) -> Result<(Wired, u64), &'static str> {
    let (e, _) = segs.get(hit.win).ok_or("no config window for the NIC")?;
    // Het segment van het venster: `segments` en `pcie_segments` lopen
    // allebei de MCFG in volgorde af (op de Altra beginnen ze allemaal
    // op bus 0, dus de bus zegt het niet).
    let seg = board_uefi::pcie_segments()
        .nth(hit.win)
        .map_or(0, |(_, s, _)| s);
    NIC_ACK.set(nic.irq_ack());
    let at = At {
        ecam: e,
        seg,
        root_bus: hit.root_bus,
        f: &hit.f,
    };
    let wired = board_uefi::irq::wire_msix(&at, &NIC_BELL, igb_ack)?;
    let _ = NIC_BELL.take();
    nic.set_irq(&NIC_BELL);
    nic.fire_irq();
    if let Some(us) = bell_within(&NIC_BELL, NIC_TEST_NS) {
        // De ack sloot de vector; de eerste lege ring van de pomp
        // heropent hem.
        return Ok((wired, us));
    }
    if let Wired::Msix { dev_id, .. } = wired {
        diagnose(&at, dev_id, nic);
    }
    nic.clear_irq();
    Err("the forced interrupt (EICS) did not arrive within 50 ms")
}

/// Eén regel na een zelftest die niet aankwam, om te kiezen tussen de
/// verdachten: eerst wat de functie en de NIC zeggen (de PBA en EICR
/// vóór iets ze verandert), de IORT-weg met de SMMU en de ITS, en dan
/// een `INT` vanuit de ITS zelf. Komt die wel, dan is de ITS-kant goed
/// en zit de fout tussen de igb en de ITS (doorbell, DeviceID, SMMU).
fn diagnose(at: &At<'_>, dev_id: u32, nic: &Igb) {
    let d = board_uefi::irq::msix_diag(at, dev_id);
    let regs = nic.irq_regs();
    let _ = NIC_BELL.take();
    let int = match board_uefi::irq::its_fire(dev_id) {
        Ok(()) => match bell_within(&NIC_BELL, NIC_TEST_NS) {
            Some(us) => IntTest::Arrived(us),
            None => IntTest::Silent,
        },
        Err(why) => IntTest::Refused(why),
    };
    cpu::println!("net: igb MSI-X diag: {d}; igb {regs}; ITS INT {int} HOPOS_NIC_IRQ_DIAG");
}

impl Platform for Ampere {
    type Nic = Igb;
    /// De eerste NVMe (het hele device) in de schijf-helft van de
    /// DMA-regio.
    type Disk = Nvme<Pci>;

    const NAME: &'static str = "altra";
    const FIRMWARE: &'static str =
        "boot: Ampere Altra over UEFI: PE stub, ACPI discovery, 48-bit identity map";
    const NEW: Self = Ampere;

    fn core_class(&self, _uefi: &Uefi, core: usize) -> CoreClass {
        crate::core_class(core)
    }

    fn probe_disk(&self) -> Result<Option<Nvme<Pci>>, Error> {
        pcie::probe_nvme()
    }

    /// De eerste igb: BAR0, reset en MAC, ringen, dan de link, en dan de
    /// lijn: MSI-X via de ITS met een zelftest ([`wire_nic`]); zonder lijn
    /// een fout.
    fn probe_nic(&self, _uefi: &Uefi) -> Result<Option<Igb>, Error> {
        let segs = pcie::segments();
        let Some(hit) = pcie::first_in(&segs, 0, is_nic) else {
            return Ok(None);
        };
        if !board_uefi::map_device(hit.bar, driver_igb::MMIO_LEN) {
            return Err(Error::Nic("igb BAR0 unreachable"));
        }
        if let Some((e, _)) = segs.get(hit.win) {
            hit.f.enable(e);
        }
        // SAFETY: BAR0 is door de firmware toegewezen en nu Device-gemapt;
        // memory-decode en bus-mastering staan aan. NET_DMA is van deze
        // driver alleen, 2 MB-gealigneerd en Normal-NC gemapt, met het
        // bufferblok Normal-WB (`NET_BUF`): dat veegt de driver zelf.
        let mut nic = unsafe { Igb::new(Pa(hit.bar), NET_DMA.base, NET_DMA.size, cpu::idle::now) }
            .map_err(|e| {
                cpu::println!("net: {e} at {} bar0 {:#x}", hit.f.bdf, hit.bar);
                Error::Nic("igb init failed")
            })?;
        let link = nic.link_up(LINK_TIMEOUT_NS).map_err(|e| {
            cpu::println!("net: {e}");
            Error::Nic("igb has no link")
        })?;
        let (vendor, device, bdf, bar) = (hit.f.vendor, hit.f.device, hit.f.bdf, hit.bar);
        let (wired, us) = wire_nic(&segs, &hit, &mut nic).map_err(|why| {
            cpu::println!(
                "net: igb {vendor:04x}:{device:04x} at {bdf} bar0 {bar:#x} link {link}, no line: {why} HOPOS_NIC_IRQ"
            );
            Error::Irq("the igb line did not come up")
        })?;
        cpu::println!(
            "net: igb {vendor:04x}:{device:04x} at {bdf} bar0 {bar:#x} link {link}, {wired}, first interrupt after {us} us HOPOS_NIC_IRQ"
        );
        Ok(Some(nic))
    }
}
