//! De vroege console: één haak naar de UART van het board, en het
//! `print!`/`println!`-paar dat de kern gebruikt.
//!
//! Het board zet bij boot een `fn(&[u8])` ([`set_sink`]); daarna schrijft
//! elke logregel daarheen. Zonder haak verdwijnt de uitvoer stil: vóór het
//! board zijn UART heeft, is er niets om naar te schrijven.
//!
//! De console-UART is het tweede van de twee sloten die het handboek
//! toestaat (§1.3): een logregel mag uit elke taak en elke core komen, en
//! een halve regel is nog altijd meer dan geen regel. Het slot is privé aan
//! deze module, de lening leeft binnen één functie, en er is een meetlat
//! ([`lock_stats`]). Een ISR logt niet (de vector zet een vlag); het enige
//! pad uit exception-context is [`emergency`], en dat pakt het slot met
//! een grens en schrijft daarna hoe dan ook.

use core::fmt::{self, Write as _};
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};

/// De haak: een `fn(&[u8])` als rauwe pointer; null = geen console.
static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Het console-slot.
static LOCK: AtomicBool = AtomicBool::new(false);
/// Meetlat: hoe vaak het slot gepakt is.
static TAKEN: AtomicU64 = AtomicU64::new(0);
/// Meetlat: de langste wacht, in spin-rondes.
static LONGEST_SPIN: AtomicU64 = AtomicU64::new(0);
/// Meetlat: hoe vaak de grens verstreek en er zonder slot geschreven werd.
static FORCED: AtomicU64 = AtomicU64::new(0);
/// Is er een noodregel geschreven ([`emergency`])? Vanaf dan wacht elke
/// regel weer op de UART: na een panic of een fatale exception draait er
/// geen taak meer die een achterstand wegschrijft.
static URGENT: AtomicBool = AtomicBool::new(false);

/// De spin-grens van het slot. Op één core is er nooit een wachter (een
/// taak geeft het slot terug vóór hij iets anders doet); op meer cores is
/// een regel een paar honderd bytes op de UART-FIFO. Na de grens schrijven
/// we zonder slot: door elkaar lopende tekens zijn beter dan een hang.
const SPIN_LIMIT: u64 = 1 << 22;

/// Zet de haak naar de UART van het board.
pub fn set_sink(f: fn(&[u8])) {
    SINK.store(f as *mut (), Release);
}

fn sink() -> Option<fn(&[u8])> {
    let p = SINK.load(Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: `SINK` wordt alleen door `set_sink` geschreven, met een
    // geldige `fn(&[u8])`; een functiepointer en een datapointer zijn op
    // onze targets even groot, en null hebben we hierboven uitgesloten.
    Some(unsafe { core::mem::transmute::<*mut (), fn(&[u8])>(p) })
}

/// Pakt het slot met de grens; `false` = de grens verstreek.
fn lock() -> bool {
    let mut spins = 0u64;
    while LOCK
        .compare_exchange_weak(false, true, Acquire, Relaxed)
        .is_err()
    {
        spins += 1;
        if spins > SPIN_LIMIT {
            FORCED.fetch_add(1, Relaxed);
            return false;
        }
        core::hint::spin_loop();
    }
    TAKEN.fetch_add(1, Relaxed);
    LONGEST_SPIN.fetch_max(spins, Relaxed);
    true
}

fn unlock(held: bool) {
    if held {
        LOCK.store(false, Release);
    }
}

/// De schrijver achter het slot: stuurt elk stuk tekst naar de haak.
struct Sink(fn(&[u8]));

impl fmt::Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        (self.0)(s.as_bytes());
        Ok(())
    }
}

/// Schrijft `b` naar de console, onder het slot.
pub fn write_bytes(b: &[u8]) {
    let Some(f) = sink() else { return };
    let held = lock();
    f(b);
    unlock(held);
}

/// Draait `f` onder het console-slot: de pomp van de UART (hopos
/// conport.rs) verzet zo dezelfde positie als een schrijver.
pub fn locked<R>(f: impl FnOnce() -> R) -> R {
    let held = lock();
    let r = f();
    unlock(held);
    r
}

/// De uitvoering van [`print!`](crate::print): één opgemaakte regel onder
/// het slot.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>, newline: bool) {
    let Some(f) = sink() else { return };
    let held = lock();
    let mut w = Sink(f);
    let _ = w.write_fmt(args);
    if newline {
        f(b"\n");
    }
    unlock(held);
}

/// Schrijft vanuit exception-context (een fatale exception, de panic):
/// het slot met de grens, en daarna hoe dan ook. Een fout die zichzelf
/// niet kan melden, is een fout die niemand ooit vindt.
pub fn emergency(args: fmt::Arguments<'_>) {
    URGENT.store(true, Release);
    _print(args, true);
}

/// Is er een noodregel geweest? De sink van de binary schrijft dan alles
/// wachtend, de achterstand eerst.
#[must_use]
pub fn urgent() -> bool {
    URGENT.load(Acquire)
}

/// De meetlat van het console-slot: (keren gepakt, langste wacht in
/// spin-rondes, keren geforceerd na de grens).
#[must_use]
pub fn lock_stats() -> (u64, u64, u64) {
    (
        TAKEN.load(Relaxed),
        LONGEST_SPIN.load(Relaxed),
        FORCED.load(Relaxed),
    )
}

/// Schrijft naar de console, zoals `std::print!`.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*), false)
    };
}

/// Schrijft een regel naar de console, zoals `std::println!`.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::_print(::core::format_args!(""), true)
    };
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*), true)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    fn capture(b: &[u8]) {
        OUT.lock().unwrap().extend_from_slice(b);
    }

    #[test]
    fn println_goes_through_the_sink_under_the_lock() {
        set_sink(capture);
        crate::println!("tick {}", 3);
        crate::print!("a");
        write_bytes(b"b\n");
        let out = String::from_utf8(OUT.lock().unwrap().clone()).unwrap();
        assert!(out.contains("tick 3\nab\n"), "{out:?}");
        let (taken, _, forced) = lock_stats();
        assert!(taken >= 3);
        assert_eq!(forced, 0);
        assert!(!LOCK.load(Relaxed));
    }
}
