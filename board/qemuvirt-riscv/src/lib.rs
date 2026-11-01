//! QEMU virt (riscv64) in machine mode: ns16550, CLINT, PLIC, virtio-mmio
//! net en blk, FDT; de proefbank van de LicheeRV Nano.
//!
//! De machine is `-M virt -bios none -m 1G`: geen OpenSBI, want HopOS is op
//! RISC-V zelf de machine-mode-monitor (zie `cpu::riscv::boot`), zoals op de
//! LicheeRV waar het image het MONITOR-slot van de FIP inneemt. QEMU start
//! elk hart op 0x8000_0000 met a0 = hart-id en a1 = DTB.
//!
//! De adressen zijn die van `hw/riscv/virt.c` (stabiel): UART0 0x1000_0000
//! (IRQ 10), CLINT 0x0200_0000, PLIC 0x0c00_0000 (96 bronnen), acht
//! virtio-mmio-transports vanaf 0x1000_1000 (stride 0x1000, IRQ 1 + n).
//! De timebase is 10 MHz (`RISCV_ACLINT_DEFAULT_TIMEBASE_FREQ`).
//!
//! Dit crate bezit de adressen, de klok, de slaap en de bedrading van de
//! drivers. De drivers zelf kennen geen adres.

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

pub mod cage;
pub mod slots;

use abi::ring::Coherence;
use board::{Board, CoreClass, Dispatched, Error, Plan, Region};
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use cpu::irq::Line;
use cpu::riscv::clint::Clint;
use cpu::riscv::csr;
use cpu::riscv::plic::{Plic, machine_context};
use dev::Pa;
use driver_ns16550::Ns16550;
use driver_virtioblk::{IrqAck as BlkAck, VirtioBlk};
use driver_virtionet::{IrqAck, VirtioNet};
use sync::{Local, Signal};

/// De ns16550 van virt (byte-stride).
pub const UART0: Pa = Pa(0x1000_0000);
/// De CLINT.
pub const CLINT: Pa = Pa(0x0200_0000);
/// De PLIC.
pub const PLIC: Pa = Pa(0x0c00_0000);
/// Het aantal PLIC-bronnen van virt (`VIRT_IRQCHIP_NUM_SOURCES`).
pub const PLIC_SOURCES: u32 = 96;
/// De timebase van de TIME-CSR op QEMU virt.
pub const TIMEBASE_HZ: u64 = 10_000_000;
/// Het begin van het DRAM.
pub const DRAM: Pa = Pa(0x8000_0000);

/// De kern-RAM: image, stack en heap. 240 MB, zoals op arm64.
pub const KERN_RAM: Region = Region {
    base: Pa(0x8000_0000),
    size: 0x0f00_0000,
};
/// De DMA-regio: 16 MB achter de kern-RAM. QEMU is coherent, dus gecachet
/// is hier goed; op de C906 gaat alles door `dev::push`/`pull`.
pub const DMA: Region = Region {
    base: Pa(0x8f00_0000),
    size: 0x0100_0000,
};
/// De NIC-helft van de DMA-regio.
pub const NET_DMA: Region = Region {
    base: Pa(0x8f00_0000),
    size: 0x0080_0000,
};
/// De schijf-helft van de DMA-regio.
pub const BLK_DMA: Region = Region {
    base: Pa(0x8f80_0000),
    size: 0x0080_0000,
};

const _: () = {
    assert!(KERN_RAM.end().0 == DMA.base.0);
    assert!(NET_DMA.end().0 == BLK_DMA.base.0 && BLK_DMA.end().0 == DMA.end().0);
    assert!(driver_virtioblk::DMA_NEED <= BLK_DMA.size);
    assert!(DMA.end().0 <= slots::POOL[0].base);
};

/// Het eerste virtio-mmio-transport.
pub const VIRTIO_MMIO: Pa = Pa(0x1000_1000);
const VIRTIO_STRIDE: u64 = 0x1000;
const VIRTIO_SLOTS: u64 = 8;
/// De PLIC-bron van transport 0; transport n heeft `VIRTIO_IRQ + n`.
const VIRTIO_IRQ: u32 = 1;

/// Zonder FDT: het aantal harts dat `image/qemu-riscv-run.sh` vraagt.
const CORES_DEFAULT: usize = 2;

// SAFETY: 0x1000_0000 is de ns16550 van QEMU virt (byte-stride); machine
// mode heeft geen vertaling, dus hij is altijd bereikbaar.
static UART: Ns16550 = unsafe { Ns16550::new(UART0, 0) };

// SAFETY: de PLIC van QEMU virt, 96 bronnen, altijd bereikbaar in M-mode.
// De controller van `cpu::irq`.
static PLIC_DEV: Plic = unsafe { Plic::new(PLIC, PLIC_SOURCES) };

// SAFETY: de CLINT van QEMU virt in de SiFive-indeling.
const CLINT_DEV: Clint = unsafe { Clint::new(CLINT) };

/// De bel van de NIC.
static NIC_BELL: Signal = Signal::new();
/// De NIC-lijn en zijn ack.
static NIC_IRQ: Local<Cell<Option<(u32, IrqAck)>>> = Local::new(Cell::new(None));

/// De device-ack van de NIC-lijn bij de dispatcher: virtio InterruptACK.
fn nic_ack() {
    if let Some((_, a)) = NIC_IRQ.get().get() {
        a.ack();
    }
}

/// De bel van de schijf: de dispatch luidt hem, de hopfs-actor wacht erop
/// tijdens een blok-verzoek (de wachter van `blkdev::Queue`).
static DISK_BELL: Signal = Signal::new();
/// De ack van de schijflijn, gezet door `probe_disk`.
static DISK_ACK: Local<Cell<Option<BlkAck>>> = Local::new(Cell::new(None));

/// De device-ack van de schijflijn bij de dispatcher: virtio InterruptACK.
fn disk_ack() {
    if let Some(a) = DISK_ACK.get().get() {
        a.ack();
    }
}

/// De DTB van deze boot.
static DTB: board::dtb::Dtb = board::dtb::Dtb::new();
/// Het bij boot gevonden DRAM.
static MEM_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Het aantal harts uit de FDT.
static CORES: AtomicUsize = AtomicUsize::new(0);
/// Is de CLINT-probe geslaagd?
static CLINT_OK: AtomicBool = AtomicBool::new(false);
/// Leeft er een NIC uit `probe_nic`? Gezet na een gelukte probe (één per
/// boot).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);
static DISK_CLAIMED: AtomicBool = AtomicBool::new(false);

fn console_write(b: &[u8]) {
    UART.write_bytes(b);
}

/// Waar QEMU de DTB legt: bovenin de eerste 4 GB DRAM.
const DTB_RAM: Region = Region {
    base: DRAM,
    size: 1 << 32,
};

/// QEMU virt (riscv64) als board.
pub struct QemuVirtRiscv;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = QemuVirtRiscv;

impl QemuVirtRiscv {
    /// Het board. Alle staat staat in statics.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Het eerste virtio-mmio-transport waarop `is` ja zegt, met zijn
    /// PLIC-bron. Een vaste scan: de acht plekken van virt.
    fn find_virtio(is: impl Fn(Pa) -> bool) -> Option<(Pa, u32)> {
        (0..VIRTIO_SLOTS)
            .map(|i| (VIRTIO_MMIO.add(i * VIRTIO_STRIDE), VIRTIO_IRQ + i as u32))
            .find(|&(pa, _)| is(pa))
    }

    /// De index van dit hart in de CLINT: op virt is de CLINT gedeeld en
    /// is dat het hart-id (de LicheeRV heeft er een per core: daar 0).
    #[must_use]
    pub fn clint_hart(&self) -> usize {
        self.this_core()
    }

    /// De CLINT van dit board.
    #[must_use]
    pub const fn clint(&self) -> Clint {
        CLINT_DEV
    }

    /// Wat de kooi van app-hart `hart` moet weten: de SiFive-CLINT van virt
    /// is gedeeld (elk hart zijn eigen `msip` en `mtimecmp` op zijn index),
    /// QEMU wekt een `wfi` betrouwbaar op de wekker, en er is geen
    /// resetblok: de kill-tick is het mes. Zonder bewezen CLINT geen wekker
    /// (de switcher spint) en dus ook geen tick.
    #[must_use]
    pub fn app_hart(&self, hart: usize) -> cpu::riscv::switch::AppHart {
        let hz = cpu::riscv::idle::hz();
        let clint = CLINT_OK.load(Relaxed);
        cpu::riscv::switch::AppHart {
            mtimecmp: if clint {
                CLINT_DEV.mtimecmp(hart)
            } else {
                Pa(0)
            },
            msip: CLINT_DEV.msip(hart),
            // De kick van een bewoner: `msip` van hart 0, de kern (een
            // gedeelde CLINT, elk hart bereikt elke bel).
            kick: if hart == 0 { Pa(0) } else { CLINT_DEV.msip(0) },
            sleep_cap: cpu::riscv::idle::ns_to_ticks(cpu::riscv::idle::WFI_CAP_QEMU_NS, hz),
            tick: cpu::riscv::idle::ns_to_ticks(cpu::riscv::switch::KILL_TICK_NS, hz),
            attrs: cpu::riscv::sv39::Attrs::Spec,
            pmp: cpu::riscv::pmp::QEMU,
            resettable: false,
        }
    }

    /// Brengt app-hart `hart` naar de parkeerlus: op QEMU niets, want elk
    /// hart begint daar bij de reset (`-bios none`).
    pub fn start_app_hart(&self, _hart: usize) {}

    /// Het resetblok van een app-hart: QEMU virt heeft er geen.
    pub fn hold_app_hart(&self, _hart: usize) -> bool {
        false
    }

    /// De zelftest van de riscv-kooi op hart 1 ([`cage`]), één regel met
    /// marker. Alleen met een tweede hart en een bewezen wekker.
    fn cage_selftest(&self) {
        if self.cores() < 2 || !CLINT_OK.load(Relaxed) {
            cpu::println!(
                "cage: riscv switcher self-test skipped (needs 2 harts and the CLINT) HOPOS_RV_CAGE_SKIP"
            );
            return;
        }
        let (Ok(plan), Some(core)) = (slots::plan(self.cores(), 0), abi::layout::Core::new(1))
        else {
            cpu::println!("cage: riscv switcher self-test: no plan HOPOS_RV_CAGE_FAIL");
            return;
        };
        // Het ctx-blok van de proef achter zijn partitie in de pool, zoals
        // het kooi-geheugen een kooi-blok uit de pool neemt.
        let (Ok(sched), Ok(ctx)) = (
            plan.park_mbox_pa(core),
            Ok::<_, ()>(Pa(slots::POOL[0].base + 0x20_0000)),
        ) else {
            cpu::println!(
                "cage: riscv switcher self-test: no sched or ctx block HOPOS_RV_CAGE_FAIL"
            );
            return;
        };
        let hart = plan.phys_core(core);
        match cage::selftest(
            slots::CAGE_PA,
            Pa(sched.0),
            Pa(ctx.0),
            hart,
            slots::POOL[0].base,
            CLINT_DEV,
        ) {
            Ok(r) => cpu::println!(
                "cage: riscv switcher on hart {hart}: yield resumed at {:#x}, exit state {}, escape state {} with mcause {} mtval {:#x} vec {}, spinner state {} after the kill tick ({} us), sleeper state {} -> {} {}",
                r.resume,
                r.first,
                r.second,
                r.fault.0,
                r.fault.1,
                r.fault.2,
                r.spin.0,
                r.spin.1,
                r.sleeper.1,
                r.sleeper.0,
                if r.ok() {
                    "HOPOS_RV_CAGE_UP"
                } else {
                    "HOPOS_RV_CAGE_FAIL"
                }
            ),
            Err(e) => cpu::println!("cage: riscv switcher self-test: {e} HOPOS_RV_CAGE_FAIL"),
        }
    }
}

impl Default for QemuVirtRiscv {
    fn default() -> Self {
        Self::new()
    }
}

/// Geen watchdog bedraad.
impl board::Watchdog for QemuVirtRiscv {
    type Armed = &'static str;
}

/// Geen thermometer.
impl board::Thermal for QemuVirtRiscv {}

/// Geen klokknop.
impl board::ClockKnob for QemuVirtRiscv {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;
}

impl Board for QemuVirtRiscv {
    type Nic = VirtioNet;
    type Sleeper = cpu::riscv::idle::RvSleeper;
    /// De virtio-blk op de mmio-bus.
    type Disk = VirtioBlk;

    const NAME: &'static str = "qemuvirt-riscv";
    const PSCI: bool = false;
    const DTB_IN_X0: bool = true;
    const NO_BOOT_MEDIUM: bool = true;
    /// De app-harts zijn niet coherent met de kern; de host-ringen zijn
    /// beide de kern, op zijn eigen hart.
    const SLOT_RINGS: Coherence = Coherence::Maintained;
    const HOST_RINGS: Coherence = Coherence::Hardware;

    fn console(&self) -> fn(&[u8]) {
        console_write
    }

    /// Het "EL" op riscv is de privilege-modus: 3 = machine mode. De kooi
    /// (PMP) vraagt machine mode, zoals hij op ARM EL2 vraagt.
    fn privilege(&self, el: u8) -> Result<(), Error> {
        if el == 3 {
            Ok(())
        } else {
            Err(Error::Privilege { el })
        }
    }

    fn firmware(&self) -> &'static str {
        "boot: QEMU virt riscv64, machine mode, -bios none (HopOS is the monitor, no OpenSBI below us)"
    }

    fn discover(&self, dtb: u64) {
        cpu::riscv::idle::set_hz(TIMEBASE_HZ);
        match CLINT_DEV.probe(self.this_core(), csr::rdtime()) {
            Ok(()) => {
                CLINT_OK.store(true, Relaxed);
                cpu::println!("board: CLINT: mtimecmp writable, the kern sleeps on it (wfi)");
            }
            Err(e) => cpu::println!(
                "board: CLINT: {e}, the kern polls instead of sleeping HOPOS_CLINT_FAIL"
            ),
        }
        cpu::println!("{}", cpu::riscv::trng::WARNING);
        let Some(f) = DTB.find(dtb, DTB_RAM) else {
            cpu::println!(
                "WARNING HOPOS_RAM_CHECK_SKIPPED: no valid DTB (a1={dtb:#x}) - trusting the static layout"
            );
            return;
        };
        MEM_TOTAL.store(f.mem_total().unwrap_or(0), Relaxed);
        CORES.store(f.cpu_count().unwrap_or(0), Relaxed);
        cpu::println!(
            "fdt: {} bytes at {dtb:#x}, bootargs {:?}",
            f.size(),
            f.bootargs().unwrap_or("")
        );
        self.cage_selftest();
    }

    fn clock(&self) -> executor::Clock {
        cpu::riscv::idle::now
    }

    /// `wfi` op de eigen `mtimecmp` als de probe slaagde, anders pollen.
    fn sleeper(&self) -> Self::Sleeper {
        let clint = CLINT_OK.load(Relaxed).then_some(CLINT_DEV);
        cpu::riscv::idle::RvSleeper::new(clint, self.this_core())
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
        PLIC_DEV.set_context(machine_context(self.this_core()));
        cpu::irq::use_controller(&PLIC_DEV);
        cpu::println!("irq: {}", PLIC_DEV.describe());
        // Vanaf hier mag de trap komen: MEIE en MSIE aan, MIE aan. De
        // ingang zet de bron weer dicht tot de dispatch-taak claimde.
        csr::mie_set(csr::MIP_MEIP | csr::MIP_MSIP);
        csr::restore(csr::MSTATUS_MIE);
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        // De kick: de `msip` wissen, dan staat de bron weer open.
        CLINT_DEV.set_msip(self.this_core(), false);
        // De PLIC: de NIC en de schijf hebben een lijn, wat verder vuurt
        // gaat uit.
        let pass = cpu::irq::global().dispatch();
        // De ingang liet MEIE en MSIE dicht; de ronde is klaar.
        csr::mie_set(csr::MIP_MEIP | csr::MIP_MSIP);
        let nic = NIC_IRQ.get().get().map_or(0, |(l, _)| pass.claims(Line(l)));
        Dispatched {
            timer: 0,
            nic,
            other: pass.claimed.saturating_sub(nic),
        }
    }

    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        let Some((base, irq)) = Self::find_virtio(|base| {
            // SAFETY: `find_virtio` geeft alleen adressen in het
            // virtio-mmio-venster van virt.
            unsafe { driver_virtionet::is_modern_net(base) }
        }) else {
            return Ok(None);
        };
        // SAFETY: `base` is een virtio-mmio-transport van virt, en NET_DMA
        // is van deze driver alleen.
        let mut nic =
            unsafe { VirtioNet::new(base, NET_DMA.base, NET_DMA.size, cpu::riscv::idle::now) }
                .map_err(|_| Error::Nic("virtio-net init failed"))?;
        NIC_IRQ.get().set(Some((irq, nic.irq_ack())));
        cpu::irq::enable(Line(irq), Some(nic_ack), Some(&NIC_BELL))
            .map_err(|_| Error::Irq("virtio-net source refused by the PLIC"))?;
        nic.set_irq(&NIC_BELL);
        cpu::println!(
            "net: virtio-net at {:#x}, plic source {irq}, queue {}",
            base.0,
            nic.queue_size()
        );
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// Het hart waar dit draait.
    fn this_core(&self) -> usize {
        csr::mhartid() as usize
    }

    /// De OS-core: op riscv64 altijd het boot-hart (0). Een
    /// `hopos.oscore`-vraag wordt luid genegeerd: de verhuizing bestaat hier
    /// niet.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        let asked = !self.boot_param("hopos.oscore").is_empty();
        (
            0,
            asked.then_some("the riscv64 kern stays on its boot hart"),
        )
    }

    /// De bel van de OS-core-rotatie van arm64 (een GIC-SGI). Op riscv64
    /// is de kick van de kern-hart zijn `msip` ([`Self::kick_self`],
    /// `cpu::riscv::oscore`); dit is de stub die de gedeelde lijm laat
    /// bouwen.
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell {
            sgi1r: 0,
            sgir: 0,
            intid: 0,
            pending: || 1023,
        }
    }

    /// De kick naar dit hart zelf: de `msip`.
    fn kick_self(&self) {
        CLINT_DEV.set_msip(self.this_core(), true);
    }

    /// De FDT-bootargs (QEMU `-append`).
    fn bootargs(&self) -> &'static str {
        DTB.fdt().and_then(|f| f.bootargs()).unwrap_or("")
    }

    /// Vindt en initialiseert de schijf (virtio-blk), met zijn bron op de
    /// PLIC (de PLIC staat al: `start_interrupts` komt eerst). Eén keer.
    fn probe_disk(&self) -> Result<Option<VirtioBlk>, Error> {
        if DISK_CLAIMED.swap(true, Relaxed) {
            return Err(Error::Twice("probe_disk"));
        }
        let Some((base, irq)) = Self::find_virtio(|base| {
            // SAFETY: `find_virtio` geeft alleen adressen in het
            // virtio-mmio-venster van virt.
            unsafe { driver_virtioblk::is_modern_blk(base) }
        }) else {
            return Ok(None);
        };
        // SAFETY: `base` is een virtio-mmio-transport van virt, en BLK_DMA is
        // van deze driver alleen.
        let mut disk =
            unsafe { VirtioBlk::new(base, BLK_DMA.base, BLK_DMA.size, cpu::riscv::idle::now) }
                .map_err(|_| Error::Disk("virtio-blk init failed"))?;
        DISK_ACK.get().set(Some(disk.irq_ack()));
        cpu::irq::enable(Line(irq), Some(disk_ack), Some(&DISK_BELL))
            .map_err(|_| Error::Irq("virtio-blk source refused by the PLIC"))?;
        disk.set_irq(&DISK_BELL);
        cpu::println!(
            "disk: virtio-blk at {:#x}, plic source {irq}, {} sectors, flush {}",
            base.0,
            disk.sectors(),
            if disk.can_flush() { "yes" } else { "no" }
        );
        Ok(Some(disk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtio_slots_and_lines() {
        let found = QemuVirtRiscv::find_virtio(|pa| pa == VIRTIO_MMIO.add(7 * VIRTIO_STRIDE));
        assert_eq!(found, Some((Pa(0x1000_8000), 8)));
        assert_eq!(QemuVirtRiscv::new().privilege(3), Ok(()));
        assert_eq!(
            QemuVirtRiscv::new().privilege(1),
            Err(Error::Privilege { el: 1 })
        );
    }
}
