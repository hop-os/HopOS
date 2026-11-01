//! De DW-watchdog van de SG2002 (`snps,dw-wdt` op 0x0301_0000, pclk 25 MHz;
//! de registers in `driver_dwwdt`), achter `board::Watchdog` (`arm`, `pet`,
//! [`Desc`]), zodat de watchdog-taak van de kern (`hopos/src/watchdog.rs`)
//! hem aait zoals elke andere (Go, `OLD/metal/board/licheerv/wdt.go`).
//!
//! Alleen wapenen als de probe van de boot antwoordde (`probed`): een
//! bus-fout op het WDT-blok overleeft de kern niet, en die gok nam
//! `discover` al één keer, met een regel ervoor. Eenmaal gewapend is de
//! DW-WDT niet meer uit te zetten (CR.enable is write-once tot de reset):
//! `off` weigert dus eerlijk.

use crate::WDT;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use driver_dwwdt::{self as dw, DwWdt};

/// De vendor-lijm van de TOP_WDT (Go, verbatim).
const GLUE: u64 = 0x1C;
/// CR: aan, met de vendor-pulslengte en response = directe reset (Go).
const CR_ARM: u32 = 0x11;
/// De klok van de teller: pclk, 25 MHz.
const HZ: u64 = 25_000_000;

/// Antwoordde de probe van de boot?
static PROBED: AtomicBool = AtomicBool::new(false);
/// De gewapende TOP (0 = niet gewapend; TOP 0 bestaat wel, maar is 2,6 ms
/// en wordt hier nooit gekozen).
static ARMED: AtomicU32 = AtomicU32::new(0);

/// De timeout van TOP `t`: 2^(16 + t) cycli.
#[must_use]
pub const fn top_ms(t: u32) -> u64 {
    dw::top_ms(t, HZ)
}

/// De kleinste TOP (1..=15) waarvan de timeout minstens `timeout_ms` is;
/// 15 (~86 s) als niets volstaat.
#[must_use]
pub fn top_for(timeout_ms: u64) -> u32 {
    dw::top_for(timeout_ms, HZ, 1)
}

/// Meldt de uitkomst van de probe (`LicheeRv::watchdog_probe`).
pub fn probed(ok: bool) {
    PROBED.store(ok, Relaxed);
}

/// De reset-routering van het RTC-domein (Go, `WatchdogArm`, verbatim de
/// enable-helft van `__system_reset` van de vendor-FSBL): zonder dit loopt
/// de teller af en blijft de node dood staan (Go, 02-08). Adres en waarde.
const RTC_ROUTING: [(u64, u32); 2] = [
    (0x0502_60E0, 0x0001), // rtc_core: watchdog reset enable
    (0x0502_60C8, 0x0001), // rtc_core: power cycle enable
];
/// Na een wachttijd van 100 µs (het RTC-domein is traag, de FSBL wacht ook).
const RTC_CTRL: [(u64, u32); 3] = [
    (0x0502_50AC, 0x0000_0000), // rtcsys_rstn_src_sel: WDT naar heel rtcsys
    (0x0502_5004, 0x0000_AB18), // RTC_CTRL0 unlock
    (0x0502_5008, 0x0040_0040), // rtc_ctrl: watchdog reset enable
];

/// Wapent de watchdog op minstens `timeout_ms` (de volgorde van Go: de
/// reset-routering van het RTC-domein, TORR, de lijm, een restart, dan CR =
/// enable met de vendor-pulslengte en response = directe reset).
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    if !PROBED.load(Relaxed) {
        return Err("DW-WDT did not answer the boot probe, not armed");
    }
    for (pa, v) in RTC_ROUTING {
        dev::write32(dev::Pa(pa), v);
    }
    crate::wait_us(100);
    for (pa, v) in RTC_CTRL {
        dev::write32(dev::Pa(pa), v);
    }
    let t = top_for(timeout_ms);
    let w = DwWdt::new(WDT);
    w.set_top(t);
    dev::write32(WDT.add(GLUE), 0x20);
    w.kick();
    w.enable(CR_ARM);
    dev::mb();
    ARMED.store(t, Relaxed);
    Ok(Desc {
        timeout_ms: top_ms(t),
        top: t,
    })
}

/// Zet de teller terug op vol.
pub fn pet() {
    if ARMED.load(Relaxed) != 0 {
        DwWdt::new(WDT).kick();
    }
}

/// Wat er gewapend is, voor de consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// De timeout zoals hij werkelijk staat.
    pub timeout_ms: u64,
    /// De TOP-waarde.
    pub top: u32,
}

impl core::fmt::Display for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "DW-WDT at {:#x}, TOP {} ({} ms), reset on expiry",
            WDT.0, self.top, self.timeout_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_covers_the_asked_timeout() {
        assert_eq!(top_ms(15), 85_899); // Go's ~86 s
        assert_eq!(top_for(12_000), 13); // 21,5 s: de eerste boven 12 s
        assert!(top_ms(top_for(12_000)) >= 12_000);
        assert!(top_ms(top_for(12_000) - 1) < 12_000);
        assert_eq!(top_for(1_000_000), 15);
        assert!(arm(12_000).is_err()); // geen probe: niet wapenen
    }
}
