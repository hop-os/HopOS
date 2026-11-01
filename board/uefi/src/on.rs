//! Een machine op het UEFI-board: de O6N en de Altra. Wat niet anders is
//! dan op elke UEFI-machine (console, heap, klok, slaap, GIC, plan, de
//! core-lijm, de config, de SBSA-watchdog), staat hier één keer en gaat door
//! naar [`Uefi`]; wat de machine eigen heeft (de NIC, de schijf, de
//! core-klassen, de thermometer, de klokknop), geeft haar [`Platform`].

use crate::Uefi;
#[cfg(feature = "usb")]
use board::UsbHosts;
use board::heap::Heap;
use board::{Board, ClockKnob, CoreClass, Dispatched, Error, Plan, Thermal, Watchdog};
use core::sync::atomic::Ordering::Relaxed;
use cpu::el2::Flavor;
use executor::Executor;
use sync::Signal;

/// Wat een machine boven het generieke UEFI-board heeft. De thermometer en
/// de klokknop zijn de traits van het board-contract zelf.
pub trait Platform: Sync + Thermal + ClockKnob + 'static {
    /// De NIC-driver.
    type Nic: netdev::Device;
    /// De schijf.
    type Disk: blkdev::Disk;

    /// De naam, voor de bootlog.
    const NAME: &'static str;
    /// De regel over wie ons bootte (`Board::firmware`).
    const FIRMWARE: &'static str;
    /// Het handvat: alle staat staat in statics.
    const NEW: Self;

    /// Na de `discover` van het UEFI-board: wat de machine zelf meldt.
    fn discover(&self, _uefi: &Uefi) {}

    /// De clusterklasse van `core`.
    fn core_class(&self, uefi: &Uefi, core: usize) -> CoreClass;

    /// Vindt en initialiseert de NIC. `Ok(None)` = geen NIC. Of hij al
    /// geclaimd is, houdt [`On`] bij.
    fn probe_nic(&self, uefi: &Uefi) -> Result<Option<Self::Nic>, Error>;

    /// Vindt en initialiseert de schijf. `Ok(None)` = geen schijf. Eén
    /// keer; dat houdt [`On`] bij.
    fn probe_disk(&self) -> Result<Option<Self::Disk>, Error>;

    /// De lijn van de schijf ([`Board::wire_disk`]); standaard niets.
    fn wire_disk(&self, _disk: &mut Self::Disk) {}

    /// De USB-hostcontrollers; standaard die van het UEFI-board.
    #[cfg(feature = "usb")]
    fn usb_hosts(&self, uefi: &Uefi) -> UsbHosts {
        uefi.usb_hosts()
    }
}

/// De machine `P` op het UEFI-board.
pub struct On<P> {
    uefi: Uefi,
    p: P,
}

impl<P: Platform> On<P> {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            uefi: Uefi::new(),
            p: P::NEW,
        }
    }
}

impl<P: Platform> Default for On<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// De SBSA-watchdog van het UEFI-board.
impl<P: Platform> Watchdog for On<P> {
    type Armed = <Uefi as Watchdog>::Armed;

    fn arm(&self, timeout_ms: u64) -> Result<Self::Armed, &'static str> {
        self.uefi.arm(timeout_ms)
    }

    fn pet(&self) {
        self.uefi.pet();
    }

    fn disarm(&self) -> bool {
        self.uefi.disarm()
    }
}

impl<P: Platform> Thermal for On<P> {
    fn open_thermal(&self) {
        self.p.open_thermal();
    }

    fn temp_milli_c(&self) -> i32 {
        self.p.temp_milli_c()
    }
}

impl<P: Platform> ClockKnob for On<P> {
    type Knob = P::Knob;
    type KnobError = P::KnobError;

    const HAS_KNOB: bool = P::HAS_KNOB;

    fn clock_knob(&self, mhz: Option<u32>) -> Option<Result<P::Knob, P::KnobError>> {
        self.p.clock_knob(mhz)
    }

    fn no_knob(&self, exec: &'static Executor) {
        self.p.no_knob(exec);
    }
}

impl<P: Platform> Board for On<P> {
    type Nic = P::Nic;
    type Sleeper = <Uefi as Board>::Sleeper;
    type Disk = P::Disk;

    const NAME: &'static str = P::NAME;
    const FLAVOR: Flavor = <Uefi as Board>::FLAVOR;

    fn console(&self) -> fn(&[u8]) {
        self.uefi.console()
    }

    fn console_nowait(&self) -> Option<fn(&[u8]) -> usize> {
        self.uefi.console_nowait()
    }

    fn firmware(&self) -> &'static str {
        P::FIRMWARE
    }

    fn init_heap(&self, heap: &Heap) {
        self.uefi.init_heap(heap);
    }

    fn discover(&self, dtb: u64) {
        self.uefi.discover(dtb);
        self.p.discover(&self.uefi);
    }

    fn clock(&self) -> executor::Clock {
        self.uefi.clock()
    }

    fn sleeper(&self) -> Self::Sleeper {
        self.uefi.sleeper()
    }

    fn mem_total(&self) -> u64 {
        self.uefi.mem_total()
    }

    fn cores(&self) -> usize {
        self.uefi.cores()
    }

    fn core_class(&self, core: usize) -> CoreClass {
        self.p.core_class(&self.uefi, core)
    }

    fn plan(&self) -> Plan {
        self.uefi.plan()
    }

    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        self.uefi.start_interrupts()
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        self.uefi.dispatch_interrupts()
    }

    /// De GOP van de eigen firmware, zoals het UEFI-board hem las (alleen
    /// in de gui-smaak, docs/gui.md).
    #[cfg(feature = "gui")]
    fn framebuffer(&self) -> Option<board::fb::Desc> {
        self.uefi.framebuffer()
    }

    #[cfg(feature = "usb")]
    fn usb_hosts(&self) -> UsbHosts {
        self.p.usb_hosts(&self.uefi)
    }

    /// Pas na een gelukte probe geclaimd: een mislukte (geen link) liet
    /// niets achter en mag opnieuw (hopos `nic_retry`).
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if crate::NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        let nic = self.p.probe_nic(&self.uefi)?;
        if nic.is_some() {
            crate::NIC_CLAIMED.store(true, Relaxed);
        }
        Ok(nic)
    }

    fn probe_disk(&self) -> Result<Option<Self::Disk>, Error> {
        if crate::DISK_CLAIMED.swap(true, Relaxed) {
            return Err(Error::Twice("probe_disk"));
        }
        self.p.probe_disk()
    }

    fn wire_disk(&self, disk: &mut Self::Disk) {
        self.p.wire_disk(disk);
    }

    fn this_core(&self) -> usize {
        self.uefi.this_core()
    }

    /// Met de klassen van de machine ([`Platform::core_class`]), niet die
    /// van de MADT: die zegt op de Cix overal 0, en dan viel `small` terug
    /// op de boot-core (tot 04-10).
    fn os_core(&self) -> (usize, Option<&'static str>) {
        self.uefi.os_core_by(|c| self.p.core_class(&self.uefi, c))
    }

    fn os_bell(&self) -> cpu::el2::Bell {
        self.uefi.os_bell()
    }

    fn kick_self(&self) {
        self.uefi.kick_self();
    }

    fn config(&self) -> &'static str {
        self.uefi.config()
    }

    fn flip_carry_seed(&self) -> bool {
        self.uefi.flip_carry_seed()
    }
}
