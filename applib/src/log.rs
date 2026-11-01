//! De logregels van een app: [`log!`](crate::log!) naar de outbox-ring.
//!
//! De regels komen uit `printk` van de Go-applib, en ze zijn streng omdat
//! dezelfde weg ook de paniek draagt: geen allocatie (een vaste regelbuffer
//! van 224 bytes op de stack), geen wachten, en bij een volle ring meteen
//! droppen en tellen. Logs mogen het werk nooit blokkeren, en een halve
//! paniekregel is oneindig veel meer dan de nul regels van vóór de haak
//! (gemeten 31-07: de apploader-OOM stierf vijfmaal onzichtbaar).
//!
//! Een bericht met regeleinden wordt één record per regel; een regel die
//! langer is dan de buffer gaat in stukken van 224 bytes de ring op. Een
//! lege regel is geen record.
//!
//! Dit module bezit niets: de outbox-producer is van het [`App`](crate::App),
//! en `log!` leent hem daar voor de duur van één bericht.

use crate::ring::{Kind, Writer};
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// De regelbuffer: de maat van `pkBuf` in de Go-applib.
pub const LINE_MAX: usize = 224;

/// Regels die niet op de ring pasten (vol, of nog geen outbox).
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Regels die de ring haalden.
pub static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Een regelbuffer die per byte vult en per regel een record schrijft.
///
/// De port van `printk`: de regels kunnen van een paniek komen, dus hier
/// staat niets dat alloceert of wacht.
pub struct Printk<'w> {
    out: Option<&'w mut Writer>,
    buf: [u8; LINE_MAX],
    n: usize,
}

impl<'w> Printk<'w> {
    /// Een lege regel naar `out` (`None`: alles wordt geteld als gedropt).
    #[must_use]
    pub const fn new(out: Option<&'w mut Writer>) -> Self {
        Self {
            out,
            buf: [0; LINE_MAX],
            n: 0,
        }
    }

    /// Eén byte. Een regeleinde of een volle buffer schrijft de regel weg.
    pub fn byte(&mut self, c: u8) {
        if c != b'\n'
            && let Some(slot) = self.buf.get_mut(self.n)
        {
            *slot = c;
            self.n += 1;
            return;
        }
        // Een regeleinde, of een regel die te lang werd: wegschrijven wat er
        // ligt, en in het tweede geval de byte daarna alsnog bufferen.
        self.flush();
        if c != b'\n' {
            self.buf[0] = c;
            self.n = 1;
        }
    }

    /// Schrijft wat er ligt als één record; een lege regel is geen record.
    pub fn flush(&mut self) {
        let n = core::mem::take(&mut self.n);
        let Some(line) = self.buf.get(..n).filter(|l| !l.is_empty()) else {
            return;
        };
        match self.out.as_mut().map(|w| w.write(Kind::LOG, line)) {
            Some(Ok(was_empty)) => {
                WRITTEN.fetch_add(1, Relaxed);
                // De kern leest de outbox op een bel: leeg naar niet-leeg
                // is precies het moment om te bellen, en niet vaker.
                if was_empty {
                    crate::net::outbox_kick();
                }
            }
            Some(Err(_)) | None => {
                DROPPED.fetch_add(1, Relaxed);
            }
        }
    }
}

impl fmt::Write for Printk<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &c in s.as_bytes() {
            self.byte(c);
        }
        Ok(())
    }
}

/// Schrijft `args` als logregel(s) naar `out`: de outbox. Het werk achter
/// `log!`.
pub fn emit_to(out: Option<&mut Writer>, args: fmt::Arguments<'_>) {
    write_lines(Printk::new(out), args);
}

fn write_lines(mut p: Printk<'_>, args: fmt::Arguments<'_>) {
    // `Printk::write_str` faalt nooit; een fout kan alleen uit een
    // `Display`-impl komen, en dan is wat er al staat nog steeds de regel.
    let _ = fmt::write(&mut p, args);
    p.flush();
}

/// Schrijft `args` naar de outbox van de kern. Vóór de main-schil het App
/// zette, of als de outbox al geleend is (een
/// `Display` die zelf logt, een paniek midden in een logregel), wordt het
/// bericht gedropt en geteld.
///
/// Een secundaire core van een SMP-app logt niet: de outbox heeft één
/// producer, de primaire ([`crate::smp`]). Zijn regel wordt gedropt en
/// geteld.
pub fn emit(args: fmt::Arguments<'_>) {
    if !crate::smp::on_primary() {
        DROPPED.fetch_add(1, Relaxed);
        return;
    }
    match crate::rt::app() {
        Some(app) => app.log(args),
        None => emit_to(None, args),
    }
}

/// Een logregel naar de outbox: `log!("slot {}: up", slot)`.
///
/// Engels, één regel, met marker en getallen (handboek §10). Alloceert niet
/// en wacht niet; bij een volle ring wordt de regel gedropt en geteld in
/// [`log::DROPPED`](crate::log::DROPPED).
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::emit(::core::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::Reader;
    use crate::ring::tests::Backing;

    fn lines(b: &Backing, cap: u64) -> Vec<String> {
        let mut r = Reader::open(b.pa(), cap).unwrap();
        let mut buf = [0u8; 512];
        let mut out = Vec::new();
        while let Some(rec) = r.read_into(&mut buf) {
            assert_eq!(rec.kind, Kind::LOG);
            out.push(String::from_utf8(rec.payload.to_vec()).unwrap());
        }
        out
    }

    #[test]
    fn formats_one_record_per_line() {
        let b = Backing::new(4096);
        let mut w = Writer::open(b.pa(), 4096).unwrap();
        emit_to(Some(&mut w), format_args!("slot {} up, RAM {} MB", 3, 64));
        emit_to(Some(&mut w), format_args!("a\nb\n\nc"));
        assert_eq!(lines(&b, 4096), ["slot 3 up, RAM 64 MB", "a", "b", "c"]);
    }

    #[test]
    fn overlong_line_goes_out_in_pieces() {
        let b = Backing::new(4096);
        let mut w = Writer::open(b.pa(), 4096).unwrap();
        let long = "x".repeat(LINE_MAX + 10);
        emit_to(Some(&mut w), format_args!("{long}"));
        let got = lines(&b, 4096);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), LINE_MAX);
        assert_eq!(got[1].len(), 10);
    }

    #[test]
    fn full_ring_drops_and_counts_instead_of_waiting() {
        let b = Backing::new(128);
        let mut w = Writer::open(b.pa(), 128).unwrap();
        let before = DROPPED.load(Relaxed);
        for i in 0..10 {
            emit_to(Some(&mut w), format_args!("line {i:019}"));
        }
        let got = lines(&b, 128);
        assert_eq!(got.len(), 4); // 4 × 32 bytes vullen de ring precies
        assert!(DROPPED.load(Relaxed) - before >= 6);
    }

    #[test]
    fn no_outbox_is_counted_not_fatal() {
        let before = DROPPED.load(Relaxed);
        emit_to(None, format_args!("into the void"));
        assert!(DROPPED.load(Relaxed) > before);
    }

    #[test]
    fn printk_byte_by_byte_matches_the_go_buffer() {
        let b = Backing::new(4096);
        let mut w = Writer::open(b.pa(), 4096).unwrap();
        let mut p = Printk::new(Some(&mut w));
        for &c in b"panic: boom\ngoroutine" {
            p.byte(c);
        }
        // De tweede regel ligt nog in de buffer tot een regeleinde of flush.
        assert_eq!(lines(&b, 4096), ["panic: boom"]);
        p.flush();
        assert_eq!(lines(&b, 4096), ["goroutine"]);
    }
}
