//! De hardware-watchdog van QEMU virt: de i6300esb op de PCIe-host
//! (`-device i6300esb -action watchdog=reset`, de knop `WDT=1` van
//! image/qemu-run.sh), achter `board::Watchdog` (`arm`, `pet`, `off`,
//! [`Desc`]), zodat de watchdog-taak van de kern (`hopos/src/watchdog.rs`)
//! hem aait zoals op ijzer. De registers zijn van `driver_i6300esb`.
//!
//! Waarom: zonder watchdog leidt een falende Hop op QEMU nergens toe, en
//! een falende Hop is een falende node (Derek, 23-10): de kern laat hem
//! liggen, de watchdog reset, en de koude boot start de Hop van het
//! bootmedium. Dat pad toetst tools/qemu-test-hop-fail.sh.
//!
//! Dit bezit het vinden van de functie (8086:25ab op bus 0, zoals usb.rs de
//! xHCI vindt), zijn BAR 0 (uit het eigen stuk MMIO-venster, `pcie`) en of
//! deze kern hem wapende. Zonder `-device i6300esb` is er niets, en blijft
//! het "no watchdog on this board", zoals voorheen.
//!
//! Een reset van QEMU zet het model terug (uit, de BAR weg), maar houdt de
//! vlag dat de watchdog hem deed: de volgende boot zegt het in zijn
//! "armed"-regel. QEMU laadt bij die reset ook de `-device loader`-blobs
//! opnieuw (ROM's), dus de staging draagt dan weer de Hop van de start.

use crate::pcie::{ECAM, ECAM_LAST_BUS, WDT_MMIO};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use dev::Pa;
use driver_i6300esb::{self as esb, Esb};
use driver_pcie::{Bar, Ecam, MmioWindow};

/// De gevonden watchdog: het adres van zijn config-ruimte en van zijn BAR
/// 0 (0 = nog niet gevonden). Eén schrijver (de eerste `arm`, op de
/// executor van de OS-core), gelezen door `pet` en `off` van dezelfde taak.
static CFG: AtomicU64 = AtomicU64::new(0);
static REGS: AtomicU64 = AtomicU64::new(0);

/// Wat er gewapend is, voor de consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// BAR 0.
    pub regs: u64,
    /// De timeout in seconden (beide trappen samen).
    pub secs: u32,
    /// Wat `init` aantrof.
    pub found: esb::Found,
}

impl core::fmt::Display for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ms = esb::timeout_ms(self.secs);
        write!(
            f,
            "i6300esb at {:#x} on PCIe, {}.{} s in two stages",
            self.regs,
            ms / 1000,
            (ms % 1000) / 100
        )?;
        if self.found.running {
            f.write_str(", already armed by the previous kernel")?;
        }
        if self.found.fired {
            f.write_str(", the last reset came from this watchdog")?;
        }
        Ok(())
    }
}

/// De watchdog: de eerste keer gezocht en zijn BAR toegewezen, daarna het
/// gevonden adres. `None` zonder `-device i6300esb`.
fn find() -> Option<Esb> {
    if let Some(e) = found() {
        return Some(e);
    }
    // SAFETY: het ECAM-venster van virt ligt in de Device-gigabyte van de
    // identity map en blijft gemapt; QEMU legt er met `highmem-ecam=off`
    // bus 0 tot 15.
    let ecam = unsafe { Ecam::new(ECAM, 0, ECAM_LAST_BUS) };
    let f = driver_pcie::find(&ecam, 0, |f| {
        f.vendor == esb::VENDOR && f.device == esb::DEVICE
    })?;
    let mut win = MmioWindow::new(WDT_MMIO.base.0, WDT_MMIO.size);
    let regs = match f.assign_bars(&ecam, &mut win) {
        Ok([Bar::Mem { addr, .. }, ..]) if addr != 0 => addr,
        _ => return None,
    };
    let cfg = ECAM.0 + f.bdf.ecam_offset();
    CFG.store(cfg, Relaxed);
    REGS.store(regs, Relaxed);
    Some(Esb::new(Pa(cfg), Pa(regs)))
}

/// Wapent de watchdog op minstens `timeout_ms` (in hele seconden, beide
/// trappen samen), zonder slot: een flip-kern wapent hem opnieuw.
/// De volgorde van Linux `esb_probe`: init, de timeout, dan start.
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    let e = find().ok_or("no watchdog on this board (no -device i6300esb on the PCIe host)")?;
    let found = e.init();
    let secs = esb::secs_for(timeout_ms);
    e.set_secs(secs);
    e.start(false);
    if !e.enabled() {
        return Err("i6300esb did not enable");
    }
    Ok(Desc {
        regs: e.regs().0,
        secs,
        found,
    })
}

/// De aai; zonder watchdog niets.
pub fn pet() {
    if let Some(e) = found() {
        e.ping();
    }
}

/// De watchdog als hij al gevonden is; zoekt niet (een aai doet geen
/// enumeratie).
fn found() -> Option<Esb> {
    let (cfg, regs) = (CFG.load(Relaxed), REGS.load(Relaxed));
    (cfg != 0 && regs != 0).then(|| Esb::new(Pa(cfg), Pa(regs)))
}
