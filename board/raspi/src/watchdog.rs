//! De hardware-watchdog van de Pi's: de PM-watchdog van de BCM-familie
//! (BCM2711 `watchdog@7e100000`, BCM2712 `watchdog@7d200000`; Linux
//! `bcm2835_wdt`), in de vorm van `board_uefi::watchdog` (`arm`, `pet`,
//! `off`, [`Desc`]), zodat de watchdog-taak van de kern
//! (`hopos/src/watchdog.rs`) hem aait zoals elke andere. Go:
//! `OLD/metal/board/raspi/watchdog.go`.
//!
//! Een hardwareteller die de hele SoC reset als hij niet op tijd geaaid
//! wordt: PM_WDOG [19:0] is de timeout in tikken van 1/65536 s (hoogstens
//! ~16 s), PM_RSTC krijgt WRCFG = FULL_RESET, en elke schrijf eist het
//! wachtwoord 0x5a in de topbyte. Direct MMIO, niet via de mailbox.
//!
//! De teller loopt pas als WRCFG op FULL_RESET staat: GEMETEN 30-09 op de
//! Pi 5 (BCM2712) blijft PM_WDOG zonder WRCFG op zijn laadwaarde staan
//! (0xc0000 na 2 ms). Daarom wapent [`arm`] eerst (PM_WDOG laden, dan
//! FULL_RESET) en kijkt hij daarna, na [`PROBE_NS`], of de teller afloopt.
//! Een teller die dan nog stilstaat is geen watchdog: WRCFG gaat terug naar
//! clear en de kern zegt het hardop (`HOPOS_WD_NONE` met de reden). Staat
//! WRCFG al op FULL_RESET (de vorige kern wapende hem, een flip), dan
//! volstaat een herlaad.
//!
//! QEMU `raspi4b` modelleert het PM-blok alleen als reset-knop: een schrijf
//! van FULL_RESET reset de machine METEEN (hw/misc/bcm2835_powermgt.c), dus
//! daar valt niets te wapenen en niets te proeven. De proef
//! (tools/qemu-rpi4-test.sh) ziet die reset als verwacht, meteen na
//! `discover` (er is geen knop om de watchdog uit te zetten, O2).
//!
//! Dit bezit de drie statics van de gewapende watchdog; het beleid (wanneer
//! aaien) is `kern::watchdog`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use dev::Pa;

/// PM_RSTC: de reset-configuratie.
const RSTC: u64 = 0x1c;
/// PM_WDOG: de teller.
const WDOG: u64 = 0x24;
/// Het wachtwoord in de topbyte van elke schrijf.
const PASSWORD: u32 = 0x5a00_0000;
/// Het WRCFG-veld van PM_RSTC.
const WRCFG_MASK: u32 = 0x0000_0030;
/// WRCFG: volledige reset bij aflopen.
const WRCFG_FULL_RESET: u32 = 0x0000_0020;
/// PM_RSTC "reset": de stop van Linux (`PM_RSTC_RESET`), WRCFG = clear.
const RSTC_STOP: u32 = 0x0000_0102;
/// Het tikkenveld van PM_WDOG.
const TICKS_MASK: u32 = 0x000f_ffff;
/// Tikken per seconde.
pub const TICKS_PER_SEC: u64 = 65_536;
/// De langste timeout: Go begrensde op 15 s, ruim onder de 20 bits.
pub const MAX_MS: u64 = 15_000;
/// Hoe lang [`arm`] de teller laat lopen voor hij kijkt: 2 ms is 131
/// tikken, ruim boven de leesresolutie.
pub const PROBE_NS: u64 = 2_000_000;

/// Het PM-blok van deze SoC (0 = geen), gezet in `discover`.
static BASE: AtomicU64 = AtomicU64::new(0);
/// De gewapende timeout in tikken (0 = niet gewapend).
static TICKS: AtomicU32 = AtomicU32::new(0);

/// Meldt het PM-blok van deze SoC; `discover` roept dit.
pub(crate) fn set_base(pm: Pa) {
    BASE.store(pm.0, Relaxed);
}

/// De tikken voor `timeout_ms`, begrensd op [`MAX_MS`] en minstens één.
#[must_use]
pub fn ticks_for(timeout_ms: u64) -> u32 {
    let t = timeout_ms.min(MAX_MS) * TICKS_PER_SEC / 1000;
    u32::try_from(t.max(1)).unwrap_or(TICKS_MASK) & TICKS_MASK
}

/// De timeout van `ticks` in milliseconden.
#[must_use]
pub fn ticks_ms(ticks: u32) -> u64 {
    u64::from(ticks) * 1000 / TICKS_PER_SEC
}

/// Wapent de watchdog op `timeout_ms` (hoogstens [`MAX_MS`]). `Ok` met de
/// beschrijving voor de "armed"-regel, of de reden waarom niet.
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    let base = BASE.load(Relaxed);
    if base == 0 {
        return Err("no PM watchdog on this board");
    }
    let d = arm_at(Pa(base), timeout_ms, cpu::idle::now)?;
    TICKS.store(d.ticks, Relaxed);
    Ok(d)
}

/// [`arm`] op het blok `pm`, met de klok `now` voor de proef.
fn arm_at(pm: Pa, timeout_ms: u64, now: fn() -> u64) -> Result<Desc, &'static str> {
    let ticks = ticks_for(timeout_ms);
    let rstc = dev::read32(pm.add(RSTC));
    let inherited = rstc & WRCFG_MASK == WRCFG_FULL_RESET;
    dev::write32(pm.add(WDOG), PASSWORD | ticks);
    if !inherited {
        // Wapenen: WRCFG = FULL_RESET zet de teller in beweging (op ijzer
        // telt hij niet zonder, 30-09). Dan de proef: loopt hij echt?
        dev::write32(
            pm.add(RSTC),
            PASSWORD | (rstc & !WRCFG_MASK & !0xff00_0000) | WRCFG_FULL_RESET,
        );
        dev::mb();
        dev::delay(now, PROBE_NS);
        let left = dev::read32(pm.add(WDOG)) & TICKS_MASK;
        if left == 0 || left >= ticks {
            // Geen watchdog: WRCFG terug naar clear, zodat er ook geen
            // verrassing komt van een teller die later alsnog gaat lopen.
            dev::write32(pm.add(RSTC), PASSWORD | RSTC_STOP);
            dev::mb();
            cpu::println!(
                "watchdog: PM_WDOG reads {left:#x} {} ms after FULL_RESET with {ticks:#x} loaded: the counter does not run",
                PROBE_NS / 1_000_000
            );
            return Err("BCM PM watchdog counter does not run after FULL_RESET, not armed");
        }
    }
    dev::mb();
    Ok(Desc {
        timeout_ms: ticks_ms(ticks),
        ticks,
        base: pm.0,
        inherited,
    })
}

/// Herlaadt een watchdog die de VORIGE kern wapende (WRCFG staat op
/// FULL_RESET) op `timeout_ms`, ook als deze kern hem zelf nog niet heeft:
/// meteen na een flip-landing, vóór het framebuffer-geduld van 5 s en de
/// RNG-warm-up. GEMETEN 30-09 op de Pi 5: de vertrekkende kern liet 12 s
/// lopen, de nieuwe kwam pas na de zelftest aan zijn eigen `arm`, en de
/// firmware meldde `PM_RSTS 00001020`, een volledige watchdog-reset. Zonder
/// gewapende watchdog doet dit niets.
pub fn reload_if_armed(timeout_ms: u64) {
    let base = BASE.load(Relaxed);
    if base == 0 {
        return;
    }
    let pm = Pa(base);
    if dev::read32(pm.add(RSTC)) & WRCFG_MASK != WRCFG_FULL_RESET {
        return;
    }
    dev::write32(pm.add(WDOG), PASSWORD | ticks_for(timeout_ms));
    dev::mb();
}

/// Laadt de teller terug op vol.
pub fn pet() {
    let (base, ticks) = (BASE.load(Relaxed), TICKS.load(Relaxed));
    if base != 0 && ticks != 0 {
        dev::write32(Pa(base).add(WDOG), PASSWORD | ticks);
    }
}

/// Wat er gewapend is, voor de consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// De timeout zoals hij werkelijk staat.
    pub timeout_ms: u64,
    /// De tikken in PM_WDOG.
    pub ticks: u32,
    /// Het PM-blok.
    pub base: u64,
    /// Was hij al gewapend (door de vorige kern)?
    pub inherited: bool,
}

impl core::fmt::Display for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "BCM PM watchdog at {:#x}, {}.{} s ({} ticks of 1/65536 s){}",
            self.base,
            self.timeout_ms / 1000,
            (self.timeout_ms % 1000) / 100,
            self.ticks,
            if self.inherited {
                ", already armed by the previous kernel"
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Het nep-PM-blok van deze test (PM_RSTC en PM_WDOG op hun plek).
        static BLOCK: Cell<usize> = const { Cell::new(0) };
        /// Tikken die de teller per klok-lezing afloopt (0 = staat stil).
        static RATE: Cell<u32> = const { Cell::new(0) };
        static NOW: Cell<u64> = const { Cell::new(0) };
    }

    /// De klok: 1 ms per lezing, en de nep-teller loopt mee.
    fn now() -> u64 {
        let wdog = Pa(BLOCK.with(Cell::get) as u64).add(WDOG);
        let v = dev::read32(wdog);
        dev::write32(wdog, v.saturating_sub(RATE.with(Cell::get)));
        NOW.with(|n| {
            let t = n.get();
            n.set(t + 1_000_000);
            t
        })
    }

    fn block(rstc: u32, rate: u32) -> (Vec<u32>, Pa) {
        let mut b = vec![0u32; 0x40];
        b[(RSTC / 4) as usize] = rstc;
        let pa = Pa(b.as_mut_ptr() as usize as u64);
        BLOCK.with(|c| c.set(pa.0 as usize));
        RATE.with(|c| c.set(rate));
        (b, pa)
    }

    #[test]
    fn ticks_are_capped_at_fifteen_seconds() {
        assert_eq!(ticks_for(12_000), 786_432);
        assert_eq!(ticks_ms(786_432), 12_000);
        assert_eq!(ticks_for(60_000), ticks_for(15_000));
        assert!(ticks_for(15_000) <= TICKS_MASK);
        assert_eq!(ticks_for(0), 1);
    }

    #[test]
    fn a_counter_that_stands_still_after_full_reset_is_stopped_again() {
        // Een PM-blok dat ook na FULL_RESET niet telt: niet gewapend, en
        // WRCFG weer op clear (de stop van Linux), zodat er niets sluimert.
        let (b, pa) = block(0x102, 0);
        assert!(arm_at(pa, 12_000, now).is_err());
        assert_eq!(
            b[(RSTC / 4) as usize],
            PASSWORD | RSTC_STOP,
            "WRCFG not cleared"
        );
        assert_eq!(b[(WDOG / 4) as usize], PASSWORD | 786_432);
    }

    #[test]
    fn a_running_counter_is_armed_with_full_reset() {
        // Op ijzer loopt de teller na FULL_RESET: 65 tikken per ms.
        let (b, pa) = block(0x5a00_0112, 65);
        let d = arm_at(pa, 12_000, now).unwrap();
        assert_eq!(d.ticks, 786_432);
        assert!(!d.inherited);
        // Het wachtwoord, de andere RSTC-bits bewaard, WRCFG = full reset.
        assert_eq!(b[(RSTC / 4) as usize], PASSWORD | 0x102 | WRCFG_FULL_RESET);
        assert!(d.to_string().contains("12.0 s (786432 ticks"), "{d}");
    }

    #[test]
    fn an_armed_watchdog_is_reloaded_without_the_probe() {
        let (b, pa) = block(0x122, 0);
        let t0 = NOW.with(Cell::get);
        let d = arm_at(pa, 12_000, now).unwrap();
        assert!(d.inherited);
        assert_eq!(NOW.with(Cell::get), t0, "no probe");
        assert_eq!(b[(WDOG / 4) as usize], PASSWORD | 786_432);
        assert_eq!(b[(RSTC / 4) as usize], 0x122);
    }
}
