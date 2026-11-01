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
use kern::{Slot, SlotVec};
use sync::LocalCell;

/// De tijd van de executor van de OS-core.
pub(crate) use executor::ExecTimer;

/// De wandklok bij de boot zonder SNTP: 2026-09-29T00:00:00Z in
/// Unix-seconden (`calendar.timegm`, gerekend op 29-09-2026).
pub(crate) const BOOT_WALL_SECS: u64 = 1_790_640_000;

/// Unix-nanoseconden bij tellerstand 0; 0 = geen klok.
static WALL_OFF: AtomicU64 = AtomicU64::new(0);

/// De control-page van elke gebouwde kooi, op slotnummer. Alleen de
/// executor van de OS-core raakt hem (de kooi, de system-API, de
/// telemetrie).
static CTRL: LocalCell<SlotVec<dev::Pa>> = LocalCell::cell(SlotVec::new());

/// Zet de klok: `unix_ns` is nu, `now_ns` de teller-ns van hetzelfde
/// moment. De nieuwe offset gaat meteen op de control-page van elke levende
/// kooi; een verse kooi krijgt hem bij zijn bouw (`offset`). Geeft de
/// offset.
pub(crate) fn set(unix_ns: u64, now_ns: u64) -> u64 {
    // Een klok vóór het aanzetten van de teller bestaat niet; 0 zou "geen
    // klok" betekenen, dus minstens 1.
    let off = unix_ns.saturating_sub(now_ns).max(1);
    WALL_OFF.store(off, Relaxed);
    each_page(|_, page| {
        let at = page.add(abi::hopabi::CTRL_WALL_OFF);
        dev::write64(at, off);
        // De app leest zijn page met de MMU uit: naar DRAM ermee.
        dev::push(at, 8);
    });
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

/// De kooi van `slot` is gebouwd met zijn control-page op `ctrl`; faalt
/// alleen als de tabel niet kan groeien (dan past het slot niet).
pub(crate) fn attach(slot: Slot, ctrl: dev::Pa) -> kern::Result {
    CTRL.borrow_mut().insert(slot, ctrl).map(|_| ())
}

/// De control-page van `slot` als hij een gebouwde kooi heeft: de
/// watchdog leest er de heartbeat van Hop (watchdog.rs).
pub(crate) fn ctrl_page(slot: Slot) -> Option<dev::Pa> {
    CTRL.borrow().get(slot).copied()
}

/// Doet `f` op de control-page van elke gebouwde kooi, oplopend: de
/// telemetrie leest er de idle-teller en zet er de temperatuur en het zaad
/// op (telemetry.rs, seed.rs). `f` mag de tabel niet wijzigen.
pub(crate) fn each_page(mut f: impl FnMut(Slot, dev::Pa)) {
    for (slot, page) in CTRL.borrow().iter() {
        f(slot, *page);
    }
}

/// De kooi van `slot` is ingetrokken of losgelaten (`Kooi::revoke`,
/// `Cage::detach` bij elke stop): zijn page is niet meer van hem, en niemand
/// schrijft of leest er nog via deze tabel.
pub(crate) fn detach(slot: Slot) {
    CTRL.borrow_mut().remove(slot);
}
