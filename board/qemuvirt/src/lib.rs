//! QEMU virt (aarch64): PL011, GICv3, virtio-net, virtio-blk, FDT; het eerste
//! board van v3.
//!
//! De machine is `-M virt,gic-version=3,virtualization=on`: HopOS eist
//! EL2, QEMU levert PSCI via SMC, tot 12 cores, een GICv3. Dezelfde
//! bouwstenen als de Orion O6N (daar via TF-A), en daarom het
//! referentiedoel: wat hier werkt, werkt daar op de adressen na.
//!
//! In tegenstelling tot QEMU's imx8mp-evk (directe `-kernel`-boot, geen
//! TF-A) levert virt gegarandeerd PSCI, tot `-smp 12`, en een GICv3.
//!
//! Dit crate bezit de adressen van het board (`hw/arm/virt.c`, stabiel
//! gedocumenteerd), de identity map, de klok, de timer, de slaap en de
//! bedrading van de drivers. De drivers zelf kennen geen adres.

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

mod mmu;
// `-device ramfb` over fw_cfg (ramfb.rs), alleen in de gui-smaak
// (docs/gui.md).
#[cfg(feature = "gui")]
mod ramfb;
pub mod slots;
// De PCIe-host (pcie.rs), met de qemu-xhci (usb.rs, alleen met de
// bouwsteen `usb`) en de i6300esb (wdt.rs) erop.
mod pcie;
#[cfg(feature = "usb")]
mod usb;
pub mod wdt;

use board::{Board, CoreClass, Dispatched, Error, Plan, Region};
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use cpu::irq::Line;
use dev::Pa;
use driver_gicv3::{Gic, SysRegIcc};
use driver_pl011::Pl011;
use driver_virtioblk::{IrqAck as BlkAck, VirtioBlk};
use driver_virtionet::{IrqAck, VirtioNet};
use sync::{Local, Signal};

#[cfg(test)]
mod tests;

/// De PL011 van virt.
pub const UART0: Pa = Pa(0x0900_0000);

/// De kern-RAM: image, stack en heap (de Go-kern: `layout.HopRAMStart` en
/// `HopRAMSize`, 240 MB). Normal, gecached.
pub const KERN_RAM: Region = Region {
    base: Pa(0x4000_0000),
    size: 0x0f00_0000,
};

/// De DMA-regio: de bovenste 16 MB van HOP's partitie, buiten de
/// kern-RAM en dus niet gecached gemapt (`layout.DMABase`).
pub const DMA: Region = Region {
    base: Pa(0x4f00_0000),
    size: 0x0100_0000,
};

/// De NIC-helft van de DMA-regio (`layout.NetDMABase`, `NetDMASize`):
/// virtio-net onderin, de schijf de bovenste helft ([`BLK_DMA`]). Twee
/// subregio's in plaats van één gedeelde allocator: in de Go-kern kon de
/// globale DMA-allocator anders geheugen uit de NVMe-helft uitdelen.
pub const NET_DMA: Region = Region {
    base: Pa(0x4f00_0000),
    size: 0x0080_0000,
};

/// De schijf-helft van de DMA-regio (de plek van de NVMe in Go): de ringen
/// en de databuffer van virtio-blk (`driver_virtioblk::DMA_NEED`, ruim 1
/// MiB van de 8).
pub const BLK_DMA: Region = Region {
    base: Pa(0x4f80_0000),
    size: 0x0080_0000,
};

const _: () = {
    assert!(NET_DMA.base.0 == DMA.base.0 && NET_DMA.end().0 == BLK_DMA.base.0);
    assert!(BLK_DMA.end().0 == DMA.end().0);
    assert!(driver_virtioblk::DMA_NEED <= BLK_DMA.size);
    assert!(KERN_RAM.end().0 == DMA.base.0 && KERN_RAM.contains(DTB_FALLBACK));
};

/// Waar QEMU de DTB legt als het image een ELF is dat niet op de RAM-basis
/// begint (`hw/arm/boot.c`: "put the DTB at the base of RAM"). Daarom
/// linkt `hopos/link.ld` het image op 0x4020_0000: de eerste 2 MB zijn van
/// de DTB.
pub const DTB_FALLBACK: Pa = Pa(0x4000_0000);

/// De distributor van de GICv3 (`VIRT_GIC_DIST`).
pub const GICD: Pa = Pa(0x0800_0000);
/// De redistributor-reeks (`VIRT_GIC_REDIST`): 128 KB per core. Het frame
/// van de OS-core zoekt `start_interrupts` op zijn MPIDR op.
pub const GICR: Pa = Pa(0x080a_0000);
/// De lengte van de redistributor-reeks: 123 frames van 128 KB.
pub const GICR_LEN: u64 = 0x00f6_0000;
/// De PPI van de EL1-fysieke timer (CNTP): INTID 30.
pub const TIMER_PPI: u32 = 30;
/// De PPI van de EL2-fysieke timer (CNTHP): INTID 26. De deadline van de
/// executor terwijl een bewoner de OS-core heeft (`cpu::el2::OsCore`): een
/// bewoner op EL1 ziet deze timer niet en kan hem niet uitzetten.
pub const HYP_TIMER_PPI: u32 = 26;
/// De kick van de OS-core: SGI 8, gestuurd door de EL2-switcher van een
/// app-core die de kern nodig heeft terwijl die geen SEV hoort (PORT.md
/// beslissing 2). Boven de 0..7 die Linux-achtige firmware voor zichzelf
/// houdt.
pub const KICK_SGI: u32 = 8;

/// QEMU virt plaatst 32 virtio-mmio-transports vanaf 0x0a00_0000 (stride
/// 0x200, SPI 16 + n). Zonder FDT scannen we ze op DeviceID.
pub const VIRTIO_MMIO: Pa = Pa(0x0a00_0000);
const VIRTIO_STRIDE: u64 = 0x200;
const VIRTIO_SLOTS: u64 = 32;

/// Zonder FDT: het aantal cores dat `image/qemu-run.sh` vraagt.
const CORES_DEFAULT: usize = 4;

/// De UART. Eén per board.
// SAFETY: 0x0900_0000 is de PL011 van QEMU virt en blijft gemapt (de
// identity map in `mmu` zet de eerste gigabyte als Device).
static UART: Pl011 = unsafe { Pl011::new(UART0) };

/// De GIC, de controller van `cpu::irq`. Het redistributor-frame is dat van
/// de OS-core; `start_interrupts` zoekt het op met `find_redistributor` en
/// zet het.
// SAFETY: GICD en GICR zijn de GICv3-blokken van QEMU virt en liggen in de
// Device-gigabyte van de identity map.
static GIC: Gic<SysRegIcc> = unsafe { Gic::new(GICD, GICR, SysRegIcc) };

/// De bel van de NIC: de dispatch luidt hem, de RX-pomp wacht erop.
static NIC_BELL: Signal = Signal::new();

/// De NIC-lijn en zijn ack, gezet door `probe_nic`, gelezen door de
/// dispatch-taak. Beide draaien op de executor van core 0.
static NIC_IRQ: Local<Cell<Option<(u32, IrqAck)>>> = Local::new(Cell::new(None));

/// De device-ack van de NIC-lijn bij de dispatcher: virtio InterruptACK,
/// de lijn valt.
fn nic_ack() {
    if let Some((_, a)) = NIC_IRQ.get().get() {
        a.ack();
    }
}

/// De bel van de schijf: de dispatch luidt hem, de hopfs-actor wacht erop
/// tijdens een blok-verzoek (de wachter van `blkdev::Queue`).
static DISK_BELL: Signal = Signal::new();

/// De schijflijn en zijn ack, gezet en scherp gezet door `probe_disk` (de
/// GIC is dan op), gelezen door de dispatch-taak.
static DISK_IRQ: Local<Cell<Option<(u32, BlkAck)>>> = Local::new(Cell::new(None));

/// De device-ack van de schijflijn bij de dispatcher.
fn disk_ack() {
    if let Some((_, a)) = DISK_IRQ.get().get() {
        a.ack();
    }
}

/// De DTB van deze boot.
static DTB: board::dtb::Dtb = board::dtb::Dtb::new();
/// Het bij boot gevonden DRAM (bytes, 0 = onbekend).
static MEM_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Het aantal cores uit de FDT, 0 = onbekend.
static CORES: AtomicUsize = AtomicUsize::new(0);

/// Leeft er een NIC uit `probe_nic`? Gezet na een gelukte probe (één per
/// boot).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Is de schijf al geprobed? `probe_disk` mag één keer.
static DISK_CLAIMED: AtomicBool = AtomicBool::new(false);

fn console_write(b: &[u8]) {
    UART.write(b);
}

fn console_nowait(b: &[u8]) -> usize {
    UART.write_nowait(b)
}

/// QEMU virt als board.
pub struct QemuVirt;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = QemuVirt;

impl QemuVirt {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Zoekt de virtio-net: uit de FDT als die er is, anders door de 32
    /// vaste slots te scannen. Geeft basis en INTID.
    fn find_virtio_net() -> Option<(Pa, u32)> {
        Self::find_virtio(|base| {
            // SAFETY: `find_virtio` geeft alleen adressen in het
            // virtio-mmio-venster van virt, in de Device-gigabyte.
            unsafe { driver_virtionet::is_modern_net(base) }
        })
    }

    /// Het eerste virtio-mmio-slot waarop `is` ja zegt, met zijn INTID:
    /// uit de FDT als die er is, anders door de 32 vaste slots te scannen.
    fn find_virtio(is: impl Fn(Pa) -> bool) -> Option<(Pa, u32)> {
        let in_window =
            |pa: Pa| pa.0 >= VIRTIO_MMIO.0 && pa.0 < VIRTIO_MMIO.0 + VIRTIO_SLOTS * VIRTIO_STRIDE;
        if let Some(list) = DTB.fdt().and_then(|f| f.virtio_mmio().ok()) {
            return list
                .iter()
                .map(|t| (Pa(t.reg.base), t.intid))
                .filter(|&(pa, _)| in_window(pa))
                .find(|&(pa, _)| is(pa));
        }
        (0..VIRTIO_SLOTS)
            .map(|i| (VIRTIO_MMIO.add(i * VIRTIO_STRIDE), 48 + i as u32))
            .find(|&(pa, _)| is(pa))
    }
}

impl Default for QemuVirt {
    fn default() -> Self {
        Self::new()
    }
}

/// De i6300esb op PCIe (`wdt`), als QEMU hem geeft (`WDT=1` van
/// image/qemu-run.sh); anders draait de node onbewaakt, en staat de canary
/// toch op de console.
impl board::Watchdog for QemuVirt {
    type Armed = wdt::Desc;

    fn arm(&self, timeout_ms: u64) -> Result<wdt::Desc, &'static str> {
        wdt::arm(timeout_ms)
    }

    fn pet(&self) {
        wdt::pet();
    }

    fn disarm(&self) -> bool {
        wdt::off()
    }
}

/// Geen thermometer.
impl board::Thermal for QemuVirt {}

/// Geen klokknop.
impl board::ClockKnob for QemuVirt {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;
}

impl Board for QemuVirt {
    type Nic = VirtioNet;
    type Sleeper = cpu::idle::ArmSleeper;
    /// De virtio-blk op de mmio-bus.
    type Disk = VirtioBlk;

    const NAME: &'static str = "qemuvirt";
    const DTB_IN_X0: bool = true;
    const NO_BOOT_MEDIUM: bool = true;
    /// De kern slaapt hier in WFI, die geen SEV hoort (hopos cage.rs
    /// `APP_IDLE_MODE`, gemeten 30-09).
    const APP_IDLE_YIELD: bool = true;

    fn console(&self) -> fn(&[u8]) {
        UART.init();
        console_write
    }

    fn console_nowait(&self) -> Option<fn(&[u8]) -> usize> {
        Some(console_nowait)
    }

    fn firmware(&self) -> &'static str {
        "boot: QEMU virt, EL2, PSCI via SMC (no firmware below us)"
    }

    /// De DTB: x0 als de firmware hem gaf, anders de RAM-basis waar QEMU
    /// hem voor een ELF-image legt. Vroeg lezen: wat de kern straks vraagt
    /// (DRAM, cores) staat daarna in atomics.
    fn discover(&self, dtb: u64) {
        let Some((pa, f)) = [dtb, DTB_FALLBACK.0]
            .into_iter()
            .find_map(|pa| DTB.find(pa, KERN_RAM).map(|f| (pa, f)))
        else {
            cpu::println!(
                "WARNING HOPOS_RAM_CHECK_SKIPPED: no valid DTB (x0={dtb:#x}) - trusting the static layout"
            );
            return;
        };
        MEM_TOTAL.store(f.mem_total().unwrap_or(0), Relaxed);
        CORES.store(f.cpu_count().unwrap_or(0), Relaxed);
        let gic_ok = f
            .gic_v3()
            .is_some_and(|g| g.dist.base == GICD.0 && g.redist.base == GICR.0);
        cpu::println!(
            "fdt: {} bytes at {pa:#x}, bootargs {:?}{}",
            f.size(),
            f.bootargs().unwrap_or(""),
            if gic_ok {
                ""
            } else {
                ", GIC differs from the board plan"
            },
        );
    }

    fn clock(&self) -> executor::Clock {
        cpu::idle::now
    }

    /// WFI met de fysieke timer op de deadline. `cpu::idle` kiest WFI alleen
    /// waar het board bewezen heeft dat de timer-PPI de WFI wekt: GEMETEN
    /// 29-09 op QEMU 11.0.2 (`-cpu cortex-a53`), één wek per seconde op een
    /// lege executor, met PPI 30 scherp in de GIC (zonder dat wekt de timer
    /// de WFI nooit: de eerste boot hing daar). WFE is op QEMU-TCG een
    /// no-op en zou spinnen.
    fn sleeper(&self) -> Self::Sleeper {
        cpu::idle::ArmSleeper::new(cpu::idle::Mode::Wfi)
    }

    fn mem_total(&self) -> u64 {
        MEM_TOTAL.load(Relaxed)
    }

    fn cores(&self) -> usize {
        match CORES.load(Relaxed) {
            0 => CORES_DEFAULT,
            n => n,
        }
    }

    fn core_class(&self, _core: usize) -> CoreClass {
        // Homogene cores (allemaal cortex-a53): één klasse, de beste. Een
        // verzonnen 1-3/4-7/8-11-split maakte in de Go-kern met MaxSlots=3
        // álle slots "small", en elke mid/big-job permanent onplaatsbaar.
        CoreClass::Big
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
        // SAFETY: de reeks is de GICR van virt, gemapt als Device.
        let rd = unsafe { driver_gicv3::find_redistributor(GICR, GICR_LEN, mpidr) }
            .ok_or(Error::Irq("no redistributor frame for the OS core"))?;
        // SAFETY: `rd` is een frame uit die reeks, voor deze core.
        unsafe { GIC.set_redistributor(rd) };
        GIC.init()
            .map_err(|_| Error::Irq("redistributor stays asleep"))?;
        cpu::irq::use_controller(&GIC);
        // De timer-PPI moet scherp staan in de GIC: anders bereikt hij de
        // core niet en wekt hij de WFI van de slaap nooit. Zijn ack zet hem
        // uit tot de volgende slaap hem op de nieuwe deadline zet; zo valt
        // de lijn en wekt hij niet opnieuw.
        cpu::irq::enable(Line(TIMER_PPI), Some(cpu::idle::timer_off), None)
            .map_err(|_| Error::Irq("timer PPI refused"))?;
        // De OS-core: de EL2-timer (de deadline tijdens de beurt van een
        // bewoner) en de kick van de app-cores. Zonder scherpe lijn trapt
        // geen van beide naar EL2 en houdt een bewoner de core tot hij zelf
        // yieldt. De kick heeft zijn werk al gedaan als hij geclaimd wordt
        // (de core is terug bij de kern): alleen tellen.
        cpu::irq::enable(Line(HYP_TIMER_PPI), Some(cpu::idle::hyp_timer_off), None)
            .map_err(|_| Error::Irq("hyp timer PPI refused"))?;
        cpu::irq::enable(Line(KICK_SGI), Some(cpu::el2::count_kick), None)
            .map_err(|_| Error::Irq("kick SGI refused"))?;
        cpu::println!("irq: {}", GIC.describe());
        // Vanaf hier mag de vector komen: hij zet de vlag, wekt de
        // dispatch-taak via `cpu::irq::on_irq` en keert gemaskeerd terug.
        cpu::irq::unmask();
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        let pass = cpu::irq::global().dispatch();
        // De vector liet I dicht; de ronde is klaar, dus weer open.
        cpu::irq::unmask();
        // De kick telt nergens (hij staat in `os(kicks=)`); de schijf en
        // wat niemand kent onder `other`, zodat de tik zijn vorm houdt.
        let timer = pass
            .claims(Line(TIMER_PPI))
            .saturating_add(pass.claims(Line(HYP_TIMER_PPI)));
        let nic = NIC_IRQ.get().get().map_or(0, |(l, _)| pass.claims(Line(l)));
        let kick = pass.claims(Line(KICK_SGI));
        Dispatched {
            timer,
            nic,
            other: pass
                .claimed
                .saturating_sub(timer.saturating_add(nic).saturating_add(kick)),
        }
    }

    #[cfg(feature = "usb")]
    fn usb_hosts(&self) -> board::UsbHosts {
        usb::hosts()
    }

    #[cfg(feature = "gui")]
    fn framebuffer(&self) -> Option<board::fb::Desc> {
        ramfb::framebuffer()
    }

    /// Vindt het virtio-net-slot, zet de driver op in de NIC-DMA-regio en
    /// hangt zijn lijn aan de GIC. Een lijn die niet aan wil, is een fout:
    /// de pomp pollt niet (docs/sequence-hopos.md §14.1 O3).
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        let Some((base, intid)) = Self::find_virtio_net() else {
            return Ok(None);
        };
        // SAFETY: `base` is een virtio-mmio-slot van virt (Device-gemapt),
        // en NET_DMA is van deze driver alleen: buiten de kern-RAM, Normal
        // non-cacheable gemapt, en door niets anders uitgedeeld.
        let mut nic = unsafe { VirtioNet::new(base, NET_DMA.base, NET_DMA.size, cpu::idle::now) }
            .map_err(|_| Error::Nic("virtio-net init failed"))?;
        // `hopos.virtio.txq=N`: een zendring van N plekken (de 64 van de
        // RTL8125 op de ORION), voor de proef onder een trage draad
        // (`WIRE=1`, tools/qemu-test-bench.sh). Zonder: de hele queue.
        let txq = self
            .boot_param("hopos.virtio.txq")
            .parse::<u16>()
            .map_or(nic.queue_size(), |n| nic.limit_tx(n));
        if intid == 0 {
            return Err(Error::Irq("virtio-net without a line in the FDT"));
        }
        NIC_IRQ.get().set(Some((intid, nic.irq_ack())));
        cpu::irq::enable(Line(intid), Some(nic_ack), Some(&NIC_BELL))
            .map_err(|_| Error::Irq("virtio-net line refused"))?;
        nic.set_irq(&NIC_BELL);
        cpu::println!(
            "net: virtio-net at {:#x}, intid {intid}, queue {}, tx ring {txq}",
            base.0,
            nic.queue_size()
        );
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// De fysieke index van de core waar dit draait.
    fn this_core(&self) -> usize {
        slots::core_of(cpu::mpidr())
    }

    /// De OS-core die de config vraagt (`hopos.oscore=<small|mid|big|N>`,
    /// het venster in het image en dan de bootargs, `Board::boot_param`), met
    /// een reden als de vraag niet kon: dan de boot-core (0), luid. Op
    /// QEMU virt zijn alle cores big, dus `small` en `mid` vallen terug.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        board::os_core(
            self.boot_param("hopos.oscore"),
            self.cores(),
            |c| self.core_class(c),
            0,
        )
    }

    /// De kick van de OS-core voor de rotatie van `cpu::el2`: de SGI naar
    /// deze core (aanroepen op de OS-core), en de peek waarmee de kern na
    /// een terugkeer ziet of het de kick was.
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell {
            sgi1r: driver_gicv3::sgi1r(cpu::mpidr(), KICK_SGI),
            sgir: 0,
            intid: KICK_SGI,
            pending: cpu::gicv3::hppir1,
        }
    }

    /// Stuurt de kick naar deze core zelf: de zelftest van het IPI-pad.
    fn kick_self(&self) {
        cpu::gicv3::sgi1r(driver_gicv3::sgi1r(cpu::mpidr(), KICK_SGI));
    }

    /// De FDT-bootargs (QEMU `-append`).
    fn bootargs(&self) -> &'static str {
        DTB.fdt().and_then(|f| f.bootargs()).unwrap_or("")
    }

    /// Vindt en initialiseert de schijf (virtio-blk) in de schijf-helft van
    /// de DMA-regio. `Ok(None)` = geen schijf aan dit board; één keer.
    fn probe_disk(&self) -> Result<Option<VirtioBlk>, Error> {
        if DISK_CLAIMED.swap(true, Relaxed) {
            return Err(Error::Twice("probe_disk"));
        }
        let Some((base, intid)) = Self::find_virtio(|base| {
            // SAFETY: `find_virtio` geeft alleen adressen in het
            // virtio-mmio-venster van virt, in de Device-gigabyte.
            unsafe { driver_virtioblk::is_modern_blk(base) }
        }) else {
            return Ok(None);
        };
        // SAFETY: `base` is een virtio-mmio-slot van virt (Device-gemapt),
        // en BLK_DMA is van deze driver alleen: buiten de kern-RAM, Normal
        // non-cacheable gemapt (`mmu`), en door niets anders uitgedeeld.
        let mut disk = unsafe { VirtioBlk::new(base, BLK_DMA.base, BLK_DMA.size, cpu::idle::now) }
            .map_err(|_| Error::Disk("virtio-blk init failed"))?;
        // De lijn (30-09): de hopfs-actor wacht op de bel in plaats van de
        // OS-core vast te houden tot het device klaar is. De GIC staat al
        // (`start_interrupts` komt eerst); een lijn die niet aan wil, is een
        // fout.
        if intid == 0 {
            return Err(Error::Irq("virtio-blk without a line in the FDT"));
        }
        DISK_IRQ.get().set(Some((intid, disk.irq_ack())));
        cpu::irq::enable(Line(intid), Some(disk_ack), Some(&DISK_BELL))
            .map_err(|_| Error::Irq("virtio-blk line refused"))?;
        disk.set_irq(&DISK_BELL);
        cpu::println!(
            "disk: virtio-blk at {:#x}, intid {intid}, {} sectors, flush {}",
            base.0,
            disk.sectors(),
            if disk.can_flush() { "yes" } else { "no" }
        );
        Ok(Some(disk))
    }
}
