//! De SBSA Generic Watchdog: het zelfherstel-vangnet op UEFI/ACPI-machines
//! (`OLD/metal/board/uefi/watchdog.go`). Alleen de hardware: wapenen,
//! aaien, uitzetten. Het beleid (wanneer aaien, wanneer niet) is één keer
//! voor elk board, in `hopos::watchdog` en `kern::watchdog`.
//!
//! De frames komen uit de GTDT. QEMU virt heeft er geen; dan zegt
//! [`arm`] waarom niet en dekt QEMU dit pad niet. De eerste echte proef is
//! de Altra; de O6N heeft er een met een eigen gebrek (hieronder).
//!
//! Registers (SBSA/BSA): control-frame WCS +0x000 (bit 0 = enable), WOR
//! +0x008 (de timeout in teller-tikken, 32 bits); refresh-frame WRR +0x000
//! (elke schrijf herstart de teller). De teller loopt op CNTFRQ (Altra
//! 25 MHz: een WOR van 32 bits haalt ~171 s, ruim genoeg voor 12 s).

use crate::facts;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use dev::Pa;
use fw::acpi::Watchdog;

/// Het refresh-frame, het control-frame en de WOR-waarde van de gewapende
/// watchdog (0 = niet gewapend). Eén schrijver ([`arm`], op de kern-core),
/// daarna gelezen door [`pet`].
static REFRESH: AtomicU64 = AtomicU64::new(0);
static CONTROL: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);

const WCS: u64 = 0x000;
const WOR: u64 = 0x008;
const WRR: u64 = 0x000;

/// De watchdog uit de GTDT, met zijn frames gemapt; of waarom niet.
fn frames() -> Result<Watchdog, &'static str> {
    let gtdt = facts::tables(*b"GTDT").next().ok_or("no GTDT")?;
    let w = fw::acpi::gtdt_watchdog(gtdt).ok_or("no SBSA watchdog in the GTDT (QEMU?)")?;
    if !crate::map_device(w.refresh, 0x1000) || !crate::map_device(w.control, 0x1000) {
        return Err("SBSA frames unreachable");
    }
    Ok(w)
}

/// De WOR-waarde voor `timeout_ms` op een teller van `hz`: de HELFT, want
/// SBSA is tweetraps (na WOR tikken de WS0-interrupt, die niemand
/// afhandelt; pas na nog eens WOR de WS1-reset), zodat de reset op de
/// gevraagde tijd valt (Linux' `sbsa_gwdt` halveert om dezelfde reden).
/// Minstens 1: WOR = 0 is een onmiddellijke reset-lus; hoogstens 32 bits:
/// liever een langere timeout dan geen.
#[must_use]
pub fn wor_ticks(timeout_ms: u64, hz: u64) -> u32 {
    let t = timeout_ms.saturating_mul(hz) / 2000;
    u32::try_from(t.max(1)).unwrap_or(u32::MAX)
}

/// De werkelijke timeout van een WOR van `ticks` op `hz`: twee keer WOR.
#[must_use]
pub fn effective_ms(ticks: u32, hz: u64) -> u64 {
    u64::from(ticks).saturating_mul(2000) / hz.max(1)
}

/// Wapent de watchdog met `timeout_ms` (of korter, als WOR volloopt: de
/// beschrijving zegt de echte). `Ok` met een beschrijving voor de
/// "armed"-regel, of de reden waarom het niet kan.
pub fn arm(timeout_ms: u64) -> Result<Desc, &'static str> {
    let w = frames()?;
    let ticks = wor_ticks(timeout_ms, cpu::idle::freq());
    dev::write32(Pa(w.control).add(WOR), ticks);
    dev::write32(Pa(w.refresh).add(WRR), 1);
    dev::write32(Pa(w.control).add(WCS), 1);
    dev::mb();
    REFRESH.store(w.refresh, Relaxed);
    CONTROL.store(w.control, Relaxed);
    TICKS.store(ticks, Relaxed);
    Ok(Desc {
        timeout_ms: effective_ms(ticks, cpu::idle::freq()),
        refresh: w.refresh,
        control: w.control,
    })
}

/// Herstart de teller: WRR, én WOR opnieuw. Dat laatste is per spec ook
/// een refresh (WCV = teller + WOR), en het is de enige weg die op de O6N
/// werkt: daar doet een schrijf naar het refresh-frame niets
/// (cixtech/cix-linux-main#25, BIOS 1.2.1). Twee schrijfacties per aai
/// kosten niets; één stille reset kost een node.
pub fn pet() {
    let (r, c) = (REFRESH.load(Relaxed), CONTROL.load(Relaxed));
    if r == 0 || c == 0 {
        return;
    }
    dev::write32(Pa(r).add(WRR), 1);
    dev::write32(Pa(c).add(WOR), TICKS.load(Relaxed));
}

/// Wat er gewapend is, voor de consoleregel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    /// De timeout zoals hij werkelijk staat (WOR maal twee).
    pub timeout_ms: u64,
    /// Het refresh-frame.
    pub refresh: u64,
    /// Het control-frame.
    pub control: u64,
}

impl core::fmt::Display for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "SBSA watchdog, {}.{} s (refresh {:#x}, control {:#x})",
            self.timeout_ms / 1000,
            (self.timeout_ms % 1000) / 100,
            self.refresh,
            self.control
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wor_is_half_the_timeout_and_never_zero() {
        // 12 s op 25 MHz (de Altra): 6 s aan tikken.
        assert_eq!(wor_ticks(12_000, 25_000_000), 150_000_000);
        // Op 1 GHz (Armv8.6 en later, dus de A720's van de O6N) past 6 s
        // niet in 32 bits: WOR loopt vol op 4,29 s, de reset valt op 8,6 s.
        assert_eq!(wor_ticks(12_000, 1_000_000_000), u32::MAX);
        assert_eq!(effective_ms(u32::MAX, 1_000_000_000), 8_589);
        assert_eq!(wor_ticks(0, 25_000_000), 1);
        assert_eq!(wor_ticks(1_000_000, 1_000_000_000), u32::MAX);
    }
}
