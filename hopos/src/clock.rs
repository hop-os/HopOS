//! De wandklok van de kern: de offset van Unix-nanoseconden op de teller,
//! en de control-pages waar hij op moet staan.
//!
//! De teller (CNTPCT, in ns via `cpu::idle::now`) is gedeeld over alle
//! cores en de drop naar EL1 zet CNTVOFF op 0, dus één offset geldt exact
//! voor de kern en voor elke app: `wall = offset + teller-ns`. Een app leest
//! hem van `CTRL_WALL_OFF` op zijn control-page (`applib::App::wall_ns`);
//! 0 betekent "geen klok".
//!
//! Dit bezit de offset en een tabel van de control-pages van gebouwde
//! kooien (per slot het fysieke adres, 0 = geen). Beide zijn atomics met één
//! schrijver per woord: de offset zet de boot (of, na een flip, de
//! overdracht van de vorige kern, vóór `main`) en daarna alleen `SET_CLOCK`
//! van Hop, de tabel zet de lifecycle-actor (via de kooi-lijm). Alles draait
//! op de executor van core 0, dus er is niets te beschermen (handboek §1.3).
//!
//! Zonder SNTP (nog niet geport) zet de boot een vaste waarde: op QEMU is
//! dat 2026-09-29T00:00:00Z, luid gemeld. Hop's tijdstempels lopen dan wel
//! op, maar niet tegen de echte tijd; dat is beter dan tellen vanaf boot
//! (1970), want de leader vergelijkt tijden van nodes.
//!
//! Hier staat ook de monotone klok van de executor als `sync::Timer`
//! ([`ExecTimer`]), voor de lifecycle, de servicers, de committer, het
//! netwerkvlak en de USB-taak.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use kern::{SLOT_CAP, Slot};

/// De tijd van de executor van de OS-core.
pub(crate) use executor::ExecTimer;

/// De wandklok bij de boot zonder SNTP: 2026-09-29T00:00:00Z in
/// Unix-seconden (`calendar.timegm`, gerekend op 29-09-2026).
pub(crate) const BOOT_WALL_SECS: u64 = 1_790_640_000;

/// Unix-nanoseconden bij tellerstand 0; 0 = geen klok.
static WALL_OFF: AtomicU64 = AtomicU64::new(0);

/// De control-page van elke gebouwde kooi (fysiek adres; 0 = geen).
static CTRL: [AtomicU64; SLOT_CAP + 1] = [const { AtomicU64::new(0) }; SLOT_CAP + 1];

/// Zet de klok: `unix_ns` is nu, `now_ns` de teller-ns van hetzelfde
/// moment. De nieuwe offset gaat meteen op de control-page van elke levende
/// kooi; een verse kooi krijgt hem bij zijn bouw (`offset`). Geeft de
/// offset.
pub(crate) fn set(unix_ns: u64, now_ns: u64) -> u64 {
    // Een klok vóór het aanzetten van de teller bestaat niet; 0 zou "geen
    // klok" betekenen, dus minstens 1.
    let off = unix_ns.saturating_sub(now_ns).max(1);
    WALL_OFF.store(off, Relaxed);
    for c in CTRL.iter() {
        let pa = c.load(Relaxed);
        if pa != 0 {
            let at = dev::Pa(pa).add(abi::hopabi::CTRL_WALL_OFF);
            dev::write64(at, off);
            // De app leest zijn page met de MMU uit: naar DRAM ermee.
            dev::push(at, 8);
        }
    }
    off
}

/// De offset voor een verse control-page.
pub(crate) fn offset() -> u64 {
    WALL_OFF.load(Relaxed)
}

/// De offset van de vorige kern, uit de flip-overdracht (`flip::land`, vóór
/// er een kooi is): de teller liep door de sprong heen door, dus hij geldt
/// nog. 0 = de vorige kern had geen klok, en dan zet `main` de vaste.
pub(crate) fn restore(off: u64) {
    WALL_OFF.store(off, Relaxed);
}

/// De kooi van `slot` is gebouwd met zijn control-page op `ctrl`.
pub(crate) fn attach(slot: Slot, ctrl: dev::Pa) {
    if let Some(c) = CTRL.get(slot.get()) {
        c.store(ctrl.0, Relaxed);
    }
}

/// De control-page van slot `i` als hij een gebouwde kooi heeft: de
/// telemetrie leest er de idle-teller en de heartbeat en zet er de
/// temperatuur op (telemetry.rs, watchdog.rs).
pub(crate) fn ctrl_page(i: usize) -> Option<dev::Pa> {
    match CTRL.get(i).map(|c| c.load(Relaxed)) {
        Some(0) | None => None,
        Some(pa) => Some(dev::Pa(pa)),
    }
}

/// De kooi van `slot` is ingetrokken of losgelaten (`Kooi::revoke`,
/// `Cage::detach` bij elke stop): zijn page is niet meer van hem, en niemand
/// schrijft of leest er nog via deze tabel.
pub(crate) fn detach(slot: Slot) {
    if let Some(c) = CTRL.get(slot.get()) {
        c.store(0, Relaxed);
    }
}
