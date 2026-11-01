//! Het generieke UEFI-board: elke arm64-machine waar UEFI-firmware ons als
//! PE-app start en ACPI de hardware beschrijft. QEMU virt onder EDK2 is de
//! proeftuin; de Orion O6N en de Ampere Altra bouwen erop.
//!
//! Wat dit crate bezit:
//!
//! - de EFI-stub ([`boot`], `entry`): de PE-header, de zelf-relocatie, de
//!   firmware-diensten tot `ExitBootServices`, en de sprong naar dezelfde
//!   `kmain` als op virt;
//! - de feiten die de stub leerde ([`facts`]): de cores met hun MPIDR en
//!   klasse, de GIC (MADT), de ECAM-vensters (MCFG), de console (SPCR), de
//!   timer-PPI (GTDT), de memory map, `hopos.cfg` en het gestagede image;
//! - de identity map van 48 bits ([`mmu`]) en het kernvenster
//!   ([`WINDOW_PA`]): heap, DMA, het kooi-venster en de staging;
//! - de bedrading: de SPCR-UART (PL011 of 16550), de GICv3, PCIe via de
//!   ECAM en virtio-net en virtio-blk over PCI.
//!
//! Wat het niet bezit: de drivers zelf, en board-specifieke kennis (een
//! vroege UART, een NIC die geen virtio is, klokken): dat is van de
//! O6N- en Altra-crates die hierop bouwen.

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

mod arch;
pub mod boot;
mod efi;
// De vorm van het EL2-regime van de kern: nVHE, of VHE met de feature
// `vhe` (de O6N, en de proef op QEMU met neoverse-n1).
mod el2;
mod entry;
pub mod facts;
mod flip; // FLIP: de feitenpagina voor een geflipte kern (het flip-spoor)
pub mod irq;
// Een machine op dit board (de O6N, de Altra): het board-contract één keer,
// met wat de machine eigen heeft ernaast.
mod on;
// De framebuffer van de firmware (GOP), alleen in de gui-smaak; kaal een
// stub voor de twee stappen van de boot (handboek §7, docs/gui.md).
#[cfg(feature = "gui")]
mod gop;
#[cfg(not(feature = "gui"))]
mod gop {
    //! Kaal gebouwd: geen GOP, headless.
    use crate::efi::Efi;
    use crate::mmu::{self, Mmu};

    /// Niets te lezen.
    pub(crate) fn discover(_efi: &Efi) {}
    /// Niets te mappen.
    pub(crate) fn map(_m: &mut Mmu) -> Result<(), mmu::Error> {
        Ok(())
    }
}
mod memmap;
mod mmu;
pub mod pcie;
pub mod slots;
// De xHCI's op PCIe (usb.rs), alleen met de bouwsteen `usb` (de gui- of
// de media-smaak); het DMA-stuk ook voor de O6N.
#[cfg(feature = "usb")]
pub mod usb;
pub mod watchdog;

pub use on::{On, Platform};

use board::heap::Heap;
use board::{Board, CoreClass, Dispatched, Error, Plan, Region};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use cpu::el2::Flavor;
use cpu::irq::{Controller, Line};
use dev::Pa;
use driver_gicv3::{Gic, SysRegIcc};
use driver_ns16550::Ns16550;
use driver_pcie::{Ecam, Function};
use driver_pl011::Pl011;
use driver_virtioblk::VirtioBlk;
use driver_virtionet::VirtioNet;
use driver_virtiopci::pci::{self, Pci};
use sync::Signal;

/// Het kernvenster: één allocatie van de stub op een vast adres, zodat de
/// kern-flip en de slot-lijm er constanten van kunnen maken (zoals op
/// virt). Faalt de claim, dan zegt de stub welke regio's wél vrij zijn
/// (de Go-meting "RAM WINDOW BUSY", 13-07) en gaat hij terug naar de
/// firmware.
///
/// 0x5000_0000: vrij op QEMU virt onder EDK2 met `-m 3G` (gemeten 29-09,
/// zie `tools/qemu-uefi-test.sh`). Een board met DRAM elders kiest een
/// eigen venster met een feature: de O6N `window-8000` (0x8800_0000,
/// bewezen 30-09), de Altra `window-a000` (0xA000_0000, 03-10).
///
/// Eén venster per build, waar Go er zes probeerde (`SLOTS` in
/// uefi-run.sh, B0 eerst omdat 0x9000_0000 op de Altra bezet was, 13-07):
/// niet het linkadres houdt het vast (de kern is een PIE), maar de vaste
/// indeling hieronder, de constanten van `slots` en de kern-flip, die een
/// geflipte kern zonder firmware op hetzelfde venster terugvindt. Is het
/// venster bezet, dan toetst de stub Go's kandidaten tegen de memory map en
/// noemt hij de vrije (`boot.rs`, `HOPOS_UEFI_WINDOW`).
#[cfg(not(any(feature = "window-8000", feature = "window-a000")))]
pub const WINDOW_PA: u64 = 0x5000_0000;
/// Zie de standaardversie hierboven.
#[cfg(all(feature = "window-8000", not(feature = "window-a000")))]
pub const WINDOW_PA: u64 = 0x8800_0000;
/// Zie de standaardversie hierboven: de Altra van 03-10 (B000 bezet).
#[cfg(feature = "window-a000")]
pub const WINDOW_PA: u64 = 0xA000_0000;

/// De maat van het kernvenster: 320 MB.
pub const WINDOW: u64 = 0x1400_0000;

/// De heap van de kern: 220 MB vanaf het begin van het venster.
pub const HEAP: Region = Region {
    base: Pa(WINDOW_PA),
    size: 0x0dc0_0000,
};

/// De tabellen van de identity map: 4 MB (1024 tabellen) na de heap, in
/// Normal-WB-geheugen (de walker leest cacheable).
pub const TABLES: Region = Region {
    base: Pa(WINDOW_PA + 0x0dc0_0000),
    size: 0x0040_0000,
};

/// De kern-RAM: heap en tabellen, Normal WB.
pub const KERN_RAM: Region = Region {
    base: Pa(WINDOW_PA),
    size: 0x0e00_0000,
};

/// De DMA-regio: 16 MB, Normal non-cacheable (een controller leest zonder
/// cache-onderhoud, zoals op virt).
pub const DMA: Region = Region {
    base: Pa(WINDOW_PA + 0x0e00_0000),
    size: 0x0100_0000,
};

/// De NIC-helft van de DMA-regio, min de laatste 3 MB (de ITS'en). Een
/// NIC-driver vraagt hoogstens 2 MB plus 640 KB (rtl8126, igb: `DMA_NEED`).
pub const NET_DMA: Region = Region {
    base: Pa(WINDOW_PA + 0x0e00_0000),
    size: 0x0050_0000,
};

/// Hoeveel ITS'en er naast de eerste op kunnen (`irq`): een device waarvan
/// de IORT een andere ITS noemt dan de eerste uit de MADT (de Altra: een
/// per root-complex), elk met 1 MB tabellen.
pub const ITS_MORE: usize = 2;

/// De tabellen van die ITS'en, vóór [`ITS_DMA`]. Die van de eerste blijft
/// waar hij was: na een warme flip wijst de redistributor nog naar zijn
/// LPI-configuratie.
pub const ITS_MORE_DMA: Region = Region {
    base: Pa(WINDOW_PA + 0x0e50_0000),
    size: ITS_MORE as u64 * driver_gicv3::its::MEM_LEN,
};

/// Het bufferblok van de NIC binnen [`NET_DMA`]: het tweede blok van 2 MB,
/// waar rtl8126 en igb hun frames leggen (hun `BUF_OFF`; board/o6n en
/// board/altra toetsen dat). Met de feature `net-wb` mapt de stub het
/// Normal-WB en niet uitvoerbaar, terwijl de ringen ervoor en de ITS erachter
/// Normal-NC blijven: de Go-kern deed dat op de O6N en de Altra
/// (`uefi/hop/board.go`: "de dure ongecachte 1500B-reads, het gemeten
/// netdoorvoer-dak (17-07), worden cache-snelheid; de driver doet de
/// DC-hygiëne"). Alleen een board waarvan de driver zelf veegt (`dev::push`
/// vóór TX, `dev::pull` na RX) zet de feature aan; virtio-net op het
/// generieke board doet dat niet en houdt NC.
pub const NET_BUF: Region = Region {
    base: Pa(NET_DMA.base.0 + 0x0020_0000),
    size: 0x0020_0000,
};

/// De tabellen van de GICv3-ITS en de LPI's ([`irq`]): 1 MB aan het eind
/// van de NIC-helft, 64 KB-gealigneerd en Normal-NC zoals alle DMA, zodat
/// de GIC en wij hetzelfde zien zonder cache-onderhoud. Een NIC-driver
/// vraagt hoogstens 2 MB plus 640 KB (rtl8126, igb: `DMA_NEED`).
pub const ITS_DMA: Region = Region {
    base: Pa(WINDOW_PA + 0x0e70_0000),
    size: driver_gicv3::its::MEM_LEN,
};

/// De schijf-helft van de DMA-regio.
pub const BLK_DMA: Region = Region {
    base: Pa(WINDOW_PA + 0x0e80_0000),
    size: 0x0080_0000,
};
/// Het datablok van de NVMe binnen [`BLK_DMA`] (driver_nvme::DATA_OFF en
/// DATA_SIZE; board/o6n toetst dat met een const-assert). De stub mapt dit
/// blok Normal-WB terwijl de rest van de DMA-regio Normal-NC blijft: de
/// driver doet zelf `push` en `pull` om elke opdracht, en een ongecachete
/// kopie van 1 MiB kostte op de O6N ~9 ms (lezen 85 MB/s tegen Go's
/// gecachete 2705, 30-09).
pub const BLK_DATA: Region = Region {
    base: Pa(BLK_DMA.base.0 + 0x0020_0000),
    size: 0x0020_0000,
};

/// Het kooi-venster: 16 MB Device (control-pages en kooien; de app-cores
/// lezen het op EL2 met de MMU uit).
pub const ADMIN: Region = Region {
    base: Pa(WINDOW_PA + 0x0f00_0000),
    size: 0x0100_0000,
};

/// De loader-regio: boot-scratch, het staging-woord en het gestagede
/// image; 64 MB Normal.
pub const LOADER: Region = Region {
    base: Pa(WINDOW_PA + 0x1000_0000),
    size: 0x0400_0000,
};

const _: () = {
    assert!(HEAP.end().0 == TABLES.base.0 && TABLES.end().0 == KERN_RAM.end().0);
    assert!(KERN_RAM.end().0 == DMA.base.0 && DMA.end().0 == ADMIN.base.0);
    assert!(NET_DMA.base.0 == DMA.base.0 && NET_DMA.end().0 == ITS_MORE_DMA.base.0);
    assert!(ITS_MORE_DMA.end().0 == ITS_DMA.base.0);
    assert!(ITS_MORE_DMA.base.0.is_multiple_of(0x1_0000));
    assert!(NET_BUF.base.0 > NET_DMA.base.0 && NET_BUF.end().0 <= NET_DMA.end().0);
    assert!(NET_BUF.base.0.is_multiple_of(2 << 20) && NET_BUF.size == 2 << 20);
    assert!(ITS_DMA.end().0 == BLK_DMA.base.0);
    assert!(BLK_DMA.end().0 == DMA.end().0 && ITS_DMA.base.0.is_multiple_of(0x1_0000));
    assert!(ADMIN.end().0 == LOADER.base.0 && LOADER.end().0 == WINDOW_PA + WINDOW);
    assert!(driver_virtioblk::DMA_NEED <= BLK_DMA.size);
    assert!(WINDOW_PA.is_multiple_of(2 << 20));
};

/// De kick van de OS-core: SGI 1, zoals de Go-generatie (`KickSGI`).
///
/// Tot 30-09 stond hier 8, "zoals op virt", met de omgekeerde reden: niet
/// de niet-beveiligde firmware houdt 0..=7, TF-A houdt 8..=15
/// (`driver_gicv3::NS_SGI_COUNT`). Op QEMU (geen EL3, DS=1) merkte niemand
/// het; op de O6N (TF-A op EL3, DS=0) kwam de kick nooit aan en ving de
/// timer hem in de zelftest (`kick=(Timer, 100000 us, try 2)`). De Altra
/// draait ook TF-A en zou in dezelfde val lopen. 0..=7 zijn van de
/// niet-beveiligde wereld, en na ExitBootServices zijn wij daar de enige.
pub const KICK_SGI: u32 = 1;

// De kick moet een SGI zijn die wij mogen sturen; een secure SGI verdwijnt
// stil (zie hierboven).
const _: () = assert!(driver_gicv3::is_ns_sgi(KICK_SGI));

/// Hoe lang [`Uefi::kick_self`] wacht tot zijn SGI pending staat: 1 ms. Een
/// SGI naar de eigen core staat binnen microseconden in de redistributor;
/// wat er na een milliseconde nog niet staat, komt niet.
const KICK_SEEN_NS: u64 = 1_000_000;

/// De poll-grens naast [`KICK_SEEN_NS`], voor een teller die niet loopt.
const KICK_SEEN_POLLS: u32 = 100_000;

/// Zoveel verloren kicks krijgen een regel; daarna telt [`KICKS_LOST`].
const KICK_LOST_LINES: u32 = 2;

/// Meetlat: kicks van [`Uefi::kick_self`] die niet pending kwamen.
pub static KICKS_LOST: AtomicU32 = AtomicU32::new(0);

/// De PCIe-segmenten die we afzoeken, en hoeveel functies per segment we
/// in de bootlog noemen.
const PCI_LOG_MAX: usize = 32;

/// De bel van de NIC-lijn (MSI-X of INTx), zie [`Uefi::probe_nic`].
static NIC_BELL: Signal = Signal::new();
/// De bel van de schijflijn (MSI-X), zie [`Uefi::probe_disk`].
static DISK_BELL: Signal = Signal::new();

/// Leeft er een NIC uit `probe_nic`? Gezet na een gelukte probe (één per
/// boot).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);
/// Is de schijf al geprobed? `probe_disk` mag één keer.
static DISK_CLAIMED: AtomicBool = AtomicBool::new(false);
/// Staat de console als gestokt (`dev::Stall`)? De UART wordt per schrijf
/// opnieuw opgebouwd, dus de stand woont hier.
static CONSOLE_STALLED: AtomicBool = AtomicBool::new(false);

/// De console na de exit: de SPCR-UART, per schrijf opgebouwd uit de
/// feiten (de adressen zijn pas bij boot bekend, dus geen `static` met
/// een vaste UART zoals op virt).
fn console_write(b: &[u8]) {
    let base = facts::CONSOLE_BASE.load(Relaxed);
    if base == 0 {
        return;
    }
    let ty = facts::CONSOLE_TYPE.load(Relaxed);
    let shift = facts::CONSOLE_SHIFT.load(Relaxed);
    if fw::acpi::is_16550(ty) {
        // SAFETY: de SPCR wees dit blok aan; de identity map mapt het als
        // Device (`boot::build_map`).
        let u = unsafe { Ns16550::new(Pa(base), shift) };
        u.tx().set_stalled(CONSOLE_STALLED.load(Relaxed));
        u.write_bytes(b);
        CONSOLE_STALLED.store(u.tx().is_stalled(), Relaxed);
    } else {
        // SAFETY: zie hierboven; een PL011 (of SBSA-subset: dezelfde DR- en
        // FR-offsets).
        let u = unsafe { Pl011::new(Pa(base)) };
        u.tx().set_stalled(CONSOLE_STALLED.load(Relaxed));
        u.write(b);
        CONSOLE_STALLED.store(u.tx().is_stalled(), Relaxed);
    }
}

/// [`console_write`] zonder wachten, voor een PL011 uit de SPCR (zie
/// `Board::console_nowait`; een 16550 kent hier alleen THRE en blijft
/// wachtend).
fn console_nowait(b: &[u8]) -> usize {
    let base = facts::CONSOLE_BASE.load(Relaxed);
    if base == 0 {
        return b.len();
    }
    // SAFETY: zie `console_write`; `Board::console_nowait` geeft deze haak
    // alleen bij een PL011.
    let u = unsafe { Pl011::new(Pa(base)) };
    u.write_nowait(b)
}

/// De controller van `cpu::irq` op een UEFI-machine: de GIC uit de feiten
/// ([`gic`], per aanroep, zoals de adressen pas bij boot bekend zijn), met
/// de LPI's via hun ITS ([`irq`]): Linux' ITS als eigen domein boven de
/// GICv3, in het klein.
struct Irqchip;

/// De ene [`Irqchip`].
static IRQCHIP: Irqchip = Irqchip;

impl Controller for Irqchip {
    /// Een LPI gaat aan in de configuratietabel van zijn ITS, een SGI, PPI
    /// of SPI in de GIC, naar deze core.
    fn enable(&self, l: Line) -> Result<(), cpu::irq::Error> {
        if irq::is_lpi(l.0) {
            return irq::enable_lpi(l.0).map_err(|_| cpu::irq::Error::Rejected { line: l.0 });
        }
        Controller::enable(&gic(), l)
    }

    /// Een LPI blijft zoals hij is: een flank die de ITS één keer stuurt,
    /// en een LPI die niemand kent heeft geen route (`start_its` wist de
    /// ITS). Zo was het vóór de dispatcher ook.
    fn disable(&self, l: Line) {
        if !irq::is_lpi(l.0) {
            gic().disable(l);
        }
    }

    fn claim(&self) -> Option<Line> {
        gic().claim()
    }

    fn complete(&self, l: Line) {
        gic().complete(l);
    }
}

/// De GIC, uit de feiten.
fn gic() -> Gic<SysRegIcc> {
    // SAFETY: GICD en het redistributor-frame van core 0 komen uit de MADT
    // en de identity map mapt ze als Device (onder 1 TB standaard, daarboven
    // expliciet in `boot::build_map`).
    unsafe {
        Gic::new(
            Pa(facts::GICD.load(Relaxed)),
            Pa(facts::GICR_BASE.load(Relaxed)),
            SysRegIcc,
        )
    }
}

/// De ECAM-vensters als config-space.
fn ecams() -> impl Iterator<Item = (Ecam, u16, u8)> {
    facts::ecams().map(|(base, seg, start, end)| {
        // SAFETY: het venster komt uit de MCFG en de identity map mapt het
        // als Device (`boot::build_map`).
        (unsafe { Ecam::new(Pa(base), start, end) }, seg, start)
    })
}

/// De lijn van een virtio-functie: MSI-X vector 0 via de ITS (geen ack:
/// een MSI is een flank), als het transport de vector bij elke queue
/// aannam (`msix_ok`). Zonder MSI-X geen lijn.
fn wire_virtio(
    msix_ok: bool,
    e: &Ecam,
    seg: u16,
    root_bus: u8,
    f: &Function,
    bell: &'static Signal,
) -> Result<irq::Wired, &'static str> {
    if !msix_ok {
        return Err("the device refused the MSI-X vector");
    }
    let at = irq::At {
        ecam: e,
        seg,
        root_bus,
        f,
    };
    irq::wire(&at, irq::Mode::Msix, bell, irq::no_ack, irq::no_ack)
}

/// De eerste virtio-functie van type `ty` op een van de segmenten, met
/// het segment en de eerste bus van zijn venster.
fn find_virtio(ty: u32) -> Option<(Ecam, Function, u16, u8)> {
    ecams().find_map(|(e, seg, start)| {
        driver_pcie::find(&e, start, |f| pci::device_type(f) == Some(ty))
            .map(|f| (e, f, seg, start))
    })
}

/// Het generieke UEFI-board.
pub struct Uefi;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Uefi;

impl Uefi {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// De OS-core die `hopos.cfg` vraagt (`hopos.oscore=<small|mid|big|N>`)
    /// met de klassen `class` van het board erboven: de O6N heeft een eigen
    /// indeling (de MPIDR-tabel), want zijn MADT zegt overal 0. Een reden
    /// als de vraag niet kon: dan de boot-core (0), luid.
    #[must_use]
    pub fn os_core_by(&self, class: impl Fn(usize) -> CoreClass) -> (usize, Option<&'static str>) {
        board::os_core(self.boot_param("hopos.oscore"), self.cores(), class, 0)
    }

    /// Een ACPI-tabel met signature `sig` (de eerste; `DSDT` via de FADT),
    /// na de toets van lengte en checksum. Voor de O6N (`_CPC` in de
    /// DSDT/SSDT) en de Altra (de PCCT). Na `kmain`.
    #[must_use]
    pub fn acpi_table(&self, sig: &[u8; 4]) -> Option<&'static [u8]> {
        facts::tables(*sig).next()
    }

    /// De OEM-ID van de XSDT (zes bytes): de toets van een board-crate dat
    /// dit werkelijk zijn machine is vóór er board-kennis in MMIO gaat.
    #[must_use]
    pub fn oem_id(&self) -> [u8; 6] {
        facts::oem_id()
    }

    /// Alle ACPI-tabellen met signature `sig` (de SSDT's).
    pub fn acpi_tables(&self, sig: &[u8; 4]) -> impl Iterator<Item = &'static [u8]> {
        facts::tables(*sig)
    }

    /// Zet SPI, PPI of LPI `intid` aan, naar deze core, en luidt `bell` als
    /// hij komt; `ack` draait in de dispatch vóór de EOI (de device-kant van
    /// een level-lijn, zodat hij valt). Een LPI komt van de ITS
    /// ([`irq::wire_msix`]) en gaat aan in zijn configuratietabel, niet in
    /// de distributor. De lijn staat in de tabel van `cpu::irq`, en de
    /// dispatch telt hem als NIC-lijn.
    pub fn enable_line(&self, intid: u32, bell: &'static Signal, ack: fn()) -> Result<(), Error> {
        cpu::irq::enable(Line(intid), Some(ack), Some(bell)).map_err(|e| match e {
            cpu::irq::Error::Full { .. } => Error::Irq("no free interrupt line slot"),
            _ => Error::Irq("line refused"),
        })
    }

    /// `hopos.cfg` van de ESP, als tekst (leeg als er geen was).
    fn esp_config() -> &'static str {
        let (pa, len) = (facts::CFG[0].load(Relaxed), facts::CFG[1].load(Relaxed));
        if pa == 0 || len == 0 {
            return "";
        }
        let Ok(len) = usize::try_from(len) else {
            return "";
        };
        // SAFETY: de stub las het bestand in EfiLoaderData die van ons
        // blijft, Normal gemapt, en niemand schrijft er daarna in.
        let b = unsafe { core::slice::from_raw_parts(pa as usize as *const u8, len) };
        core::str::from_utf8(b).unwrap_or("")
    }

    /// De PCIe-functies in de bootlog: de meting dat de ECAM leeft en wat
    /// de firmware toewees.
    fn log_pcie(&self) {
        for (e, seg, start) in ecams() {
            let mut n = 0;
            driver_pcie::walk(&e, start, |f| {
                n += 1;
                if n <= PCI_LOG_MAX {
                    let link = f.link(&e).map(|l| (l.speed, l.width)).unwrap_or((0, 0));
                    cpu::println!(
                        "pcie: {seg:04x}:{f} bar0 {:#x} link gen{} x{}",
                        f.bar_addr(&e, 0),
                        link.0,
                        link.1
                    );
                }
                true
            });
            cpu::println!(
                "pcie: segment {seg} at {:#x} buses {start}-{}: {n} functions",
                e.base().0,
                e.buses().1
            );
        }
    }
}

impl Default for Uefi {
    fn default() -> Self {
        Self::new()
    }
}

/// De SBSA-watchdog uit de GTDT (`watchdog`); QEMU heeft er geen.
impl board::Watchdog for Uefi {
    type Armed = watchdog::Desc;

    fn arm(&self, timeout_ms: u64) -> Result<watchdog::Desc, &'static str> {
        watchdog::arm(timeout_ms)
    }

    fn pet(&self) {
        watchdog::pet();
    }
}

/// Geen thermometer op het generieke board.
impl board::Thermal for Uefi {}

/// Geen klokknop op het generieke board.
impl board::ClockKnob for Uefi {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;
}

impl Board for Uefi {
    type Nic = VirtioNet<Pci>;
    type Sleeper = cpu::idle::ArmSleeper;
    /// De virtio-blk over PCI.
    type Disk = VirtioBlk<Pci>;

    const NAME: &'static str = "uefi";
    /// De kern onder E2H = 1 met de feature `vhe` (`el2`): dan ook de
    /// switcher.
    const FLAVOR: Flavor = if el2::VHE { Flavor::Vhe } else { Flavor::Nvhe };

    #[cfg(feature = "usb")]
    fn usb_hosts(&self) -> board::UsbHosts {
        usb::hosts()
    }

    fn console(&self) -> fn(&[u8]) {
        if !fw::acpi::is_16550(facts::CONSOLE_TYPE.load(Relaxed))
            && facts::CONSOLE_BASE.load(Relaxed) != 0
        {
            // SAFETY: zie `console_write`.
            unsafe { Pl011::new(Pa(facts::CONSOLE_BASE.load(Relaxed))) }.init();
        }
        console_write
    }

    fn console_nowait(&self) -> Option<fn(&[u8]) -> usize> {
        let pl011 = !fw::acpi::is_16550(facts::CONSOLE_TYPE.load(Relaxed))
            && facts::CONSOLE_BASE.load(Relaxed) != 0;
        pl011.then_some(console_nowait as fn(&[u8]) -> usize)
    }

    fn firmware(&self) -> &'static str {
        "boot: UEFI (PE stub, ExitBootServices), ACPI discovery, 48-bit identity map"
    }

    fn init_heap(&self, heap: &Heap) {
        // SAFETY: de heap is het begin van het kernvenster dat de stub claimde
        // (EfiLoaderData), Normal gemapt, en niemand anders gebruikt het.
        unsafe { heap.init(HEAP.base.as_usize(), HEAP.end().as_usize()) };
    }

    /// Op UEFI is er geen DTB: de stub las de ACPI vóór de exit. Hier de
    /// samenvatting op de echte console.
    fn discover(&self, _dtb: u64) {
        let oem = facts::oem_id();
        cpu::println!(
            "uefi: booted at EL{} from image {:#x}+{:#x}, window {WINDOW_PA:#x}+{WINDOW:#x}, {} page tables, OEM {:?}",
            facts::BOOT_EL.load(Relaxed),
            facts::IMAGE[0].load(Relaxed),
            facts::IMAGE[1].load(Relaxed),
            facts::MMU_TABLES.load(Relaxed),
            core::str::from_utf8(&oem)
                .unwrap_or("?")
                .trim_end_matches(['\0', ' ']),
        );
        el2::report();
        // De DRBG van de kern uit de standaardbronnen van de CPU: RNDR op
        // de O6N (Cortex-A720, FEAT_RNG), de SMCCC TRNG van TF-A op de
        // Altra, anders jitter (EDK2 op QEMU), met één luide regel. Tot 30-09
        // zaaide dit board niets en bleef de DRBG stil ongeseed; nu krijgt
        // elk slot zijn zaad (`CTRL_RNG_SEED`) uit hardware waar die er is.
        // Na een flip is het EFI-zaad dat van de vorige kern (`flip.rs`,
        // `carry_seed`): de firmware is weg, de bron dezelfde. Eerst wat
        // de SMCCC-probe zag, op elk UEFI-board: de Altra zei op 03-10
        // alleen "no SMCCC TRNG" en niet waarom.
        cpu::println!("{}", cpu::trng::probe());
        if facts::EFI_SEED_CARRIED.load(Relaxed) {
            cpu::drbg::init(efi_fill, cpu::idle::counter);
            cpu::println!(
                "trng: the kernel DRBG is seeded from 64 bytes the previous kernel drew from its efi-rng DRBG, carried over the flip (the firmware TRNG is gone after the cold boot) HOPOS_RNG_EFI_CARRIED"
            );
        } else if facts::EFI_SEED_LEN.load(Relaxed) > 0 {
            cpu::drbg::init(efi_fill, cpu::idle::counter);
            cpu::println!(
                "trng: EFI_RNG_PROTOCOL online, the kernel DRBG is seeded from the firmware TRNG (64 bytes at boot, feature efi-rng) HOPOS_RNG_EFI_UP"
            );
        } else {
            cpu::println!("{}", cpu::drbg::seed_from_cpu(cpu::idle::counter));
        }
        let esp = Self::esp_config();
        if !esp.is_empty() {
            cpu::println!(
                "cfg: hopos.cfg from the ESP, {} bytes{} HOPOS_CFG",
                esp.len(),
                if board::cfgwin::text().is_empty() {
                    ""
                } else {
                    ", not read: the window in the image wins"
                }
            );
        }
        self.log_pcie();
    }

    fn clock(&self) -> executor::Clock {
        cpu::idle::now
    }

    /// WFI op de timer-PPI, zoals op virt (dezelfde GIC-bedrading, en de
    /// PPI komt uit de GTDT). Onder VHE is de timer van de kern de CNTHP
    /// (PPI 26): `cntp_*_el0` vanaf EL2 met E2H = 1 is die van EL2, en de
    /// dispatch zet hem uit via `hyp_timer_off`.
    fn sleeper(&self) -> Self::Sleeper {
        let s = cpu::idle::ArmSleeper::new(cpu::idle::Mode::Wfi);
        // Onder E2H = 1 wiste de event-stream-write van `new` de
        // EL1-timerbits van CNTHCTL_EL2: terug, anders trapt de teller in
        // elke bewoner. Onder nVHE niets.
        el2::el1_timer_access();
        s
    }

    fn mem_total(&self) -> u64 {
        slots::final_map().ram_bytes()
    }

    fn cores(&self) -> usize {
        facts::CORES.load(Relaxed).max(1)
    }

    /// De klasse uit de efficiëntieklasse van de MADT: gelijk overal = big;
    /// anders de laagste small, de hoogste big, de rest mid (de O6N:
    /// A520, A720, A720-boost).
    fn core_class(&self, core: usize) -> CoreClass {
        let n = self.cores().min(facts::MAX_CORES);
        let class = |c: usize| facts::CORE_CLASS.get(c).map_or(0, |a| a.load(Relaxed));
        let (lo, hi) = (0..n).fold((u8::MAX, 0u8), |(lo, hi), c| {
            (lo.min(class(c)), hi.max(class(c)))
        });
        let me = class(core);
        if lo == hi || me == hi {
            CoreClass::Big
        } else if me == lo {
            CoreClass::Small
        } else {
            CoreClass::Mid
        }
    }

    fn plan(&self) -> Plan {
        Plan {
            kern_ram: KERN_RAM,
            dma: DMA,
            net_dma: NET_DMA,
        }
    }

    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        let mpidr = cpu::mpidr();
        if facts::GICD.load(Relaxed) == 0 {
            return Err(Error::Irq("no GICD in the MADT"));
        }
        if facts::GICR_BASE.load(Relaxed) == 0 {
            let (base, len) = (
                facts::GICR_RANGE[0].load(Relaxed),
                facts::GICR_RANGE[1].load(Relaxed),
            );
            // SAFETY: de reeks komt uit de MADT en is Device-gemapt.
            let rd = unsafe { driver_gicv3::find_redistributor(Pa(base), len, mpidr) }
                .ok_or(Error::Irq("no redistributor frame for core 0"))?;
            facts::GICR_BASE.store(rd.0, Relaxed);
        }
        let gic = gic();
        gic.init()
            .map_err(|_| Error::Irq("redistributor stays asleep"))?;
        cpu::irq::use_controller(&IRQCHIP);
        let timer = Line(facts::TIMER_PPI.load(Relaxed));
        cpu::irq::enable(timer, Some(arch::timer_off), None)
            .map_err(|_| Error::Irq("timer PPI refused"))?;
        // De OS-core: de EL2-timer (de deadline tijdens de beurt van een
        // bewoner) en de kick van de app-cores (die alleen telt), zoals op
        // virt.
        let hyp = Line(facts::HYP_TIMER_PPI.load(Relaxed));
        cpu::irq::enable(hyp, Some(cpu::idle::hyp_timer_off), None)
            .map_err(|_| Error::Irq("hyp timer PPI refused"))?;
        cpu::irq::enable(Line(KICK_SGI), Some(cpu::el2::count_kick), None)
            .map_err(|_| Error::Irq("kick SGI refused"))?;
        // Leest de enable terug als 0, dan is de SGI niet van ons (secure,
        // RAZ/WI) en hoort de OS-core de app-cores alleen op zijn timer.
        // Geen weigering: de boot kan verder, maar luid. De groepsbit is
        // hier geen maat (de O6N leest hem als nul en de kick komt toch).
        if let Some(l) = gic.local(KICK_SGI)
            && !l.is_ours()
        {
            cpu::println!(
                "irq: kick SGI {KICK_SGI} is not ours ({l}), the OS core hears app cores only on its timer HOPOS_KICK_SECURE"
            );
        }
        cpu::println!("irq: {}", gic.describe());
        irq::start_its(&gic);
        cpu::irq::unmask();
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        let pass = cpu::irq::global().dispatch();
        cpu::irq::unmask();
        // De kick telt nergens (hij staat in `os(kicks=)`); elke lijn van
        // `enable_line` behalve de schijf is een NIC-lijn, en de schijf en
        // wat niemand kent zijn `other` (zoals op virt).
        let timer = pass
            .claims(Line(facts::TIMER_PPI.load(Relaxed)))
            .saturating_add(pass.claims(Line(facts::HYP_TIMER_PPI.load(Relaxed))));
        let kick = pass.claims(Line(KICK_SGI));
        let disk = match pcie::DISK_LPI.load(Relaxed) {
            0 => 0,
            l => pass.claims(Line(l)),
        };
        let other = pass.unknown.saturating_add(disk);
        Dispatched {
            timer,
            nic: pass
                .claimed
                .saturating_sub(timer.saturating_add(kick).saturating_add(other)),
            other,
        }
    }

    #[cfg(feature = "gui")]
    fn framebuffer(&self) -> Option<board::fb::Desc> {
        gop::framebuffer()
    }

    /// De eerste virtio-net over PCI, op MSI-X via de ITS (vector 0 voor
    /// beide queues). Zonder MSI-X geen lijn, en dan een fout
    /// (docs/sequence-hopos.md §14.1 O3).
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        let Some((e, f, seg, root_bus)) = find_virtio(1) else {
            return Ok(None);
        };
        // SAFETY: zie `probe_disk`.
        let mut t = unsafe { Pci::new(&e, &f) }.map_err(|_| Error::Nic("virtio-pci transport"))?;
        // De queues krijgen vector 0 bij hun opzet; zonder MSI-X aan op de
        // functie negeert het device hem.
        t.use_msix(0);
        // SAFETY: NET_DMA is van deze driver alleen, Normal-NC gemapt.
        let mut nic =
            unsafe { VirtioNet::with_transport(t, NET_DMA.base, NET_DMA.size, cpu::idle::now) }
                .map_err(|_| Error::Nic("virtio-net init failed"))?;
        // Alleen MSI-X: INTx op virtio-pci vraagt een ack die het
        // ISR-register leest, en QEMU's `_PRT` loopt via link-devices die
        // `fw::aml` luid weigert.
        let wired = wire_virtio(nic.transport().msix_ok(), &e, seg, root_bus, &f, &NIC_BELL)
            .map_err(|_| Error::Irq("virtio-net-pci has no MSI-X line"))?;
        nic.set_irq(&NIC_BELL);
        cpu::println!(
            "net: virtio-net-pci at {}, {wired}, queue {} HOPOS_NIC_IRQ",
            f.bdf,
            nic.queue_size()
        );
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// Vindt en initialiseert de schijf (virtio-blk over PCI) in de
    /// schijf-helft van de DMA-regio, op MSI-X via de ITS zoals de NIC.
    /// `Ok(None)` = geen schijf; één keer.
    fn probe_disk(&self) -> Result<Option<VirtioBlk<Pci>>, Error> {
        if DISK_CLAIMED.swap(true, Relaxed) {
            return Err(Error::Twice("probe_disk"));
        }
        let Some((e, f, seg, root_bus)) = find_virtio(2) else {
            return Ok(None);
        };
        // SAFETY: de BAR's van een door de firmware geconfigureerde functie
        // liggen onder 1 TB of in een expliciet gemapt venster (Device).
        let mut t = unsafe { Pci::new(&e, &f) }.map_err(|_| Error::Disk("virtio-pci transport"))?;
        t.use_msix(0);
        // SAFETY: BLK_DMA is van deze driver alleen, Normal-NC gemapt.
        let mut disk =
            unsafe { VirtioBlk::with_transport(t, BLK_DMA.base, BLK_DMA.size, cpu::idle::now) }
                .map_err(|_| Error::Disk("virtio-blk init failed"))?;
        let wired = wire_virtio(
            disk.transport().msix_ok(),
            &e,
            seg,
            root_bus,
            &f,
            &DISK_BELL,
        )
        .map_err(|_| Error::Irq("virtio-blk-pci has no MSI-X line"))?;
        if let irq::Wired::Msix { lpi, .. } = wired {
            pcie::DISK_LPI.store(lpi, Relaxed);
        }
        disk.set_irq(&DISK_BELL);
        cpu::println!(
            "disk: virtio-blk-pci at {}, {wired}, {} sectors, flush {}",
            f.bdf,
            disk.sectors(),
            if disk.can_flush() { "yes" } else { "no" }
        );
        Ok(Some(disk))
    }

    /// De fysieke index van de core waar dit draait.
    fn this_core(&self) -> usize {
        slots::core_of(cpu::mpidr())
    }

    /// De kick van de OS-core voor de rotatie van `cpu::el2`: de SGI naar
    /// deze core, en de peek waarmee de kern na een terugkeer ziet of het de
    /// kick was. Zoals op virt (dezelfde GICv3-systeemregisters).
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell {
            sgi1r: driver_gicv3::sgi1r(cpu::mpidr(), KICK_SGI),
            sgir: 0,
            intid: KICK_SGI,
            pending: cpu::gicv3::hppir1,
        }
    }

    /// Stuurt de kick naar deze core zelf: de zelftest van het IPI-pad.
    /// Komt hij niet pending (GICR_ISPENDR0, of ICC_HPPIR1 zegt hem), dan
    /// één luide regel met het woord, het doel en wat de redistributor zei:
    /// de zelftest ziet daarna alleen nog `Timer` en weet niet waarom.
    fn kick_self(&self) {
        let mpidr = cpu::mpidr();
        let word = driver_gicv3::sgi1r(mpidr, KICK_SGI);
        cpu::gicv3::sgi1r(word);
        let gic = gic();
        let hz = cpu::idle::freq();
        let limit = cpu::idle::counter().wrapping_add(cpu::idle::ns_to_ticks(KICK_SEEN_NS, hz));
        for _ in 0..KICK_SEEN_POLLS {
            if gic.local(KICK_SGI).is_some_and(|l| l.is_pending())
                || cpu::gicv3::hppir1() == KICK_SGI
            {
                return;
            }
            if cpu::idle::counter().wrapping_sub(limit) as i64 >= 0 {
                break;
            }
        }
        if KICKS_LOST.fetch_add(1, Relaxed) < KICK_LOST_LINES {
            let seen = gic.local(KICK_SGI);
            cpu::println!(
                "irq: kick SGI {KICK_SGI} did not arrive: ICC_SGI1R {word:#x} to MPIDR {mpidr:#x}, {}, ICC_HPPIR1 {} HOPOS_KICK_LOST",
                Seen(seen),
                cpu::gicv3::hppir1()
            );
        }
    }

    /// De OS-core die `hopos.cfg` vraagt (`hopos.oscore=<small|mid|big|N>`),
    /// met een reden als de vraag niet kon: dan de boot-core (0), luid. Een
    /// klasse is de eerste core van die klasse (MADT-efficiëntieklasse).
    fn os_core(&self) -> (usize, Option<&'static str>) {
        self.os_core_by(|c| self.core_class(c))
    }

    /// De config van de node: het venster in het image
    /// (`board::cfgwin`) als het gevuld is, anders `hopos.cfg` van de ESP
    /// ("" als er geen van beide is).
    fn config(&self) -> &'static str {
        board::cfgwin::or(Self::esp_config())
    }

    /// 64 bytes van de DRBG voor de nieuwe kern (`flip`): het
    /// EFI_RNG_PROTOCOL is na de koude boot weg.
    fn flip_carry_seed(&self) -> bool {
        flip::carry_seed()
    }
}

/// De redistributor-woorden van de kick voor een logregel, of "no
/// redistributor view" als er geen waren.
struct Seen(Option<driver_gicv3::Local>);

impl core::fmt::Display for Seen {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(l) => write!(f, "{l}"),
            None => f.write_str("no redistributor view"),
        }
    }
}

/// Een config-space-functie als tekst, voor wie een driver op een eigen
/// segment zoekt (de O6N- en Altra-crates).
pub fn pcie_segments() -> impl Iterator<Item = (Ecam, u16, u8)> {
    ecams()
}

/// Mapt `[pa, pa + size)` als Device in de identity map, voor een BAR
/// boven de standaard-span van 1 TB (Altra: BAR's en ECAM's boven de
/// 512 GB). Onder 1 TB is alles al Device en is dit een no-op.
pub fn map_device(pa: u64, size: u64) -> bool {
    slots::map_device(pa, size)
}

/// De naam van het EFI-zaad als bron, in de boot-log en in
/// `cpu::drbg::source` (ook na een flip, `flip::carry_seed`).
const EFI_RNG: &str = "efi-rng";

/// De bron "efi-rng" voor de DRBG: het zaad dat de stub uit het
/// EFI_RNG_PROTOCOL haalde, één keer (de firmware is na ExitBootServices
/// weg). De DRBG vraagt bij `init` 48 bytes en bij een herzaai 24; na de
/// 64 is de bron op en houdt de DRBG zijn staat (drbg.rs `reseed`).
fn efi_fill(dst: &mut [u8]) -> cpu::trng::Result<cpu::trng::Kind> {
    let used = EFI_USED.load(Relaxed);
    let len = facts::EFI_SEED_LEN.load(Relaxed);
    if used + dst.len() > len {
        return Err(cpu::trng::Error::Exhausted(cpu::trng::Kind::Soc(EFI_RNG)));
    }
    for (i, b) in dst.iter_mut().enumerate() {
        let at = used + i;
        let w = facts::EFI_SEED.get(at / 8).map_or(0, |w| w.load(Relaxed));
        *b = w.to_le_bytes()[at % 8];
    }
    EFI_USED.store(used + dst.len(), Relaxed);
    Ok(cpu::trng::Kind::Soc(EFI_RNG))
}

/// Hoeveel bytes van het EFI-zaad al gebruikt zijn.
static EFI_USED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kick_of_the_o6n_os_core_is_a_non_secure_sgi() {
        // Core 0 van de O6N (MPIDR 0x8100_0000) en core 8, de eerste big
        // core (Aff1 = 8): TargetList-bit 0, Aff1 op 23:16, SGI 1 op 27:24.
        assert_eq!(driver_gicv3::sgi1r(0x8100_0000, KICK_SGI), (1 << 24) | 1);
        assert_eq!(
            driver_gicv3::sgi1r(0x8100_0800, KICK_SGI),
            (1 << 24) | (8 << 16) | 1
        );
        assert!(driver_gicv3::is_ns_sgi(KICK_SGI));
    }
}
