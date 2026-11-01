//! De console van de kern: één ring op een vaste plek, met drie lezers.
//!
//! Elke byte die de kern op zijn console zet, gaat één keer in deze ring
//! ([`Ring::append`], onder het console-slot van `cpu::console`, van elke
//! core). Drie lezers lezen hem, elk met een eigen positie:
//!
//! - de UART ([`Ring::pump`]): de schrijver wacht niet op de baudrate (de
//!   Pi 4, 04-10: een regel kostte 7,7 ms op 115200 baud, en zolang stond
//!   de executor stil);
//! - TCP op 5555 ([`stream`]): eerst wat er in de ring staat, dan live;
//! - Hop met `NEXT_LOG` 0 ([`Ring::hop_line`]), regel voor regel.
//!
//! De ring ligt op de vaste plek van de zwarte doos van het board, buiten
//! elk kernimage, en de kop draagt de schrijfpositie en de posities van de
//! UART en van Hop. Een flip en een watchdog-reset laten hem dus staan
//! ([`Ring::open`]): de nieuwe kern schrijft verder, de UART zet eerst wat
//! nog niet op de draad stond, Hop leest verder waar hij was, en
//! `nc node 5555` laat zien wat de vorige kern als laatste zei (de
//! post-mortem). Elke schrijf gaat meteen naar DRAM, want een reset veegt
//! geen cache.
//!
//! Een schrijver wekt de wachtende lezers ([`Ring::grown`]); niemand pollt.
//! Een lezer die een hele ring achterloopt, verliest de oudste bytes; de
//! schrijver wacht op niemand.

use crate::cage::Timer;
use crate::system::Conn;
use core::sync::atomic::{
    AtomicU8, AtomicU64,
    Ordering::{AcqRel, Acquire, Release},
};
use core::task::Poll;
use core::time::Duration;
use sync::{AtomicWaker, Either, Signal, select};

/// De plek van de ring: bytes op een offset vanaf zijn begin. `put` gaat
/// meteen naar DRAM, `get` haalt vers (een andere core of een vorige kern
/// schreef).
pub trait Mem: Sync {
    /// Schrijft `b` op `off`.
    fn put(&self, off: u64, b: &[u8]);
    /// Leest `out.len()` bytes vanaf `off`.
    fn get(&self, off: u64, out: &mut [u8]);
}

/// De kop: magic, ringmaat, schrijfpositie, UART, Hop; één cacheregel.
pub const HEAD: u64 = 64;
/// "HOPCON01": een andere vorm (een oudere kern, rommel na stroom aan) is
/// een verse ring.
const MAGIC: u64 = u64::from_le_bytes(*b"HOPCON01");
const W_SIZE: u64 = 8;
const W_HEAD: u64 = 16;
const W_UART: u64 = 24;
const W_HOP: u64 = 32;
/// Hooguit zoveel lezers op 5555 tegelijk.
pub const MAX_READERS: u8 = 4;
/// Zo lang mag een schrijf naar een lezer vastzitten (een peer die zonder
/// RST verdween, of een nulvenster); daarna is hij weg. Zonder deze grens
/// liep op de O6N en de Radxa (02-10) de console vol na een dag `nc | head`.
pub const STALL: Duration = Duration::from_secs(10);
/// Wat een client hoort als alle plaatsen bezet zijn, vóór de close: een
/// stille weigering leek op een dode node.
pub const FULL: &[u8] = b"console: full, 4 readers\n";
const _: () = assert!(MAX_READERS == 4);

/// De ring: `size` bytes data achter de kop, met monotone posities.
///
/// # Invariants
///
/// De bytes `[head - min(head, size), head)` staan in de data, byte `p` op
/// `HEAD + p % size`; `uart` en `hop` zijn nooit groter dan `head`, en
/// `head` nooit groter dan `next`, het einde van de schrijf die loopt. Alleen
/// de schrijver onder het console-slot verzet `next`, `head` en `uart`. Bit `i` van
/// `seats` is de lezer op 5555 met wekker `wakers[i]`.
pub struct Ring {
    mem: &'static dyn Mem,
    size: u64,
    head: AtomicU64,
    next: AtomicU64,
    uart: AtomicU64,
    hop: AtomicU64,
    /// Bytes die de ring overschreef voor de UART ze had.
    lost: AtomicU64,
    seats: AtomicU8,
    wakers: [AtomicWaker; MAX_READERS as usize],
    /// Nieuwe bytes voor Hop (`NEXT_STORE` met `NEXT_STORE_LOGS`).
    news: Signal,
}

impl Ring {
    /// Een ring van `len` bytes (kop plus data) op `mem`; dicht tot
    /// [`Ring::open`].
    #[must_use]
    pub const fn new(mem: &'static dyn Mem, len: u64) -> Ring {
        assert!(len > HEAD);
        Ring {
            mem,
            size: len - HEAD,
            head: AtomicU64::new(0),
            next: AtomicU64::new(0),
            uart: AtomicU64::new(0),
            hop: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            seats: AtomicU8::new(0),
            wakers: [const { AtomicWaker::new() }; MAX_READERS as usize],
            news: Signal::new(),
        }
    }

    fn word(&self, at: u64) -> u64 {
        let mut b = [0; 8];
        self.mem.get(at, &mut b);
        u64::from_le_bytes(b)
    }

    fn set(&self, at: u64, v: u64) {
        self.mem.put(at, &v.to_le_bytes());
    }

    /// Neemt de ring over die er al ligt (van een vorige kern: een flip of
    /// een reset), of begint een verse. Geeft hoeveel bytes van vóór deze
    /// boot erin staan.
    pub fn open(&self) -> u64 {
        let head = self.word(W_HEAD);
        if self.word(0) != MAGIC || self.word(W_SIZE) != self.size {
            let fresh = [
                (0, 0),
                (W_SIZE, self.size),
                (W_HEAD, 0),
                (W_UART, 0),
                (W_HOP, 0),
            ];
            for (w, v) in fresh.into_iter().chain([(0, MAGIC)]) {
                self.set(w, v);
            }
            return 0;
        }
        self.uart.store(self.word(W_UART).min(head), Release);
        self.hop.store(self.word(W_HOP).min(head), Release);
        self.head.store(head, Release);
        self.next.store(head, Release);
        head.min(self.size)
    }

    /// De schrijfpositie: alles ervoor is geschreven.
    pub fn head(&self) -> u64 {
        self.head.load(Acquire)
    }

    /// De oudste positie die nog in de ring staat.
    pub fn oldest(&self) -> u64 {
        self.head().saturating_sub(self.size)
    }

    /// Schrijft `b` achteraan; de oudste bytes vallen eruit. Eerst de
    /// bytes, dan de positie: een reset ertussen verliest hoogstens dit
    /// stuk. Alleen onder het console-slot.
    pub fn append(&self, b: &[u8]) {
        let head = self.head();
        let skip = b.len().saturating_sub(self.size as usize);
        let (mut at, mut rest) = (head + skip as u64, b.get(skip..).unwrap_or_default());
        self.next.store(at + rest.len() as u64, Release);
        while !rest.is_empty() {
            let off = at % self.size;
            let (part, more) = rest.split_at(rest.len().min((self.size - off) as usize));
            self.mem.put(HEAD + off, part);
            (at, rest) = (at + part.len() as u64, more);
        }
        self.head.store(at, Release);
        self.set(W_HEAD, at);
        self.wakers.iter().for_each(AtomicWaker::wake);
        self.news.set();
    }

    /// De bel voor Hop: er kwam iets bij sinds hij hem nam.
    pub fn news(&self) -> &Signal {
        &self.news
    }

    /// Kopieert de bytes vanaf `seen` naar `out`; geeft het aantal en de
    /// volgende positie. Wie te ver achterliep, begint bij de oudste.
    /// Schreef een andere core intussen over het begin heen (ook een schrijf
    /// die nog loopt), dan geeft hij niets en de positie erachter: de
    /// volgende lees is weer heel.
    pub fn read(&self, seen: u64, out: &mut [u8]) -> (usize, u64) {
        let head = self.head();
        let from = seen.clamp(head.saturating_sub(self.size), head);
        let n = ((head - from) as usize).min(out.len());
        let (mut at, mut rest) = (from, out.get_mut(..n).unwrap_or_default());
        while !rest.is_empty() {
            let off = at % self.size;
            let (part, more) = rest.split_at_mut(rest.len().min((self.size - off) as usize));
            self.mem.get(HEAD + off, part);
            (at, rest) = (at + part.len() as u64, more);
        }
        let gone = self.next.load(Acquire).saturating_sub(self.size);
        if gone > from {
            return (0, gone);
        }
        (n, at)
    }

    /// Wacht tot er iets na `seen` staat, voor de lezer op plaats `seat`.
    pub async fn grown(&self, seat: usize, seen: u64) {
        core::future::poll_fn(|cx| {
            if let Some(w) = self.wakers.get(seat) {
                w.register(cx.waker());
            }
            if self.head() == seen {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }

    /// Geeft de UART wat er nog niet op de draad staat, tot `put` er minder
    /// neemt dan hij kreeg (de FIFO is vol); `put` zegt hoeveel. `true` =
    /// er blijft iets liggen. Alleen onder het console-slot.
    pub fn pump(&self, mut put: impl FnMut(&[u8]) -> usize) -> bool {
        let mut buf = [0u8; 64];
        loop {
            let at = self.uart.load(Acquire);
            let gone = self.oldest().saturating_sub(at);
            self.lost.fetch_add(gone, Release);
            let (n, next) = self.read(at, &mut buf);
            if n == 0 && next == at {
                return false;
            }
            let took = put(buf.get(..n).unwrap_or_default()).min(n);
            let to = next - (n - took) as u64;
            self.uart.store(to, Release);
            self.set(W_UART, to);
            if took < n {
                return true;
            }
        }
    }

    /// Tot waar de UART de ring heeft.
    pub fn uart(&self) -> u64 {
        self.uart.load(Acquire)
    }

    /// De bytes die de UART verloor sinds de vorige vraag.
    pub fn take_lost(&self) -> u64 {
        self.lost.swap(0, AcqRel)
    }

    /// De UART staat bij, op hoogstens `max` bytes na: wat daarvoor ligt,
    /// krijgt hij niet meer (een wachtende schrijver zet de rest eerst weg).
    pub fn uart_skip(&self, max: u64) {
        let at = self.uart.load(Acquire).max(self.head().saturating_sub(max));
        self.uart.store(at, Release);
        self.set(W_UART, at);
    }

    /// De UART begint opnieuw bij de oudste byte (een lezer die pas later
    /// aanhaakt, `hopos.replay`).
    pub fn uart_rewind(&self) {
        self.uart.store(self.oldest(), Release);
    }

    /// De volgende regel voor Hop (`NEXT_LOG` 0) in `dst`, zonder `\n`; een
    /// regel langer dan `dst` komt in stukken. `None` zolang er geen hele
    /// regel klaarstaat.
    pub fn hop_line(&self, dst: &mut [u8]) -> Option<usize> {
        loop {
            let at = self.hop.load(Acquire);
            let (n, next) = self.read(at, dst);
            if n == 0 && next > at {
                self.hop.store(next, Release);
                continue;
            }
            let (len, used) = match dst.get(..n)?.iter().position(|c| *c == b'\n') {
                Some(i) => (i, i + 1),
                None if n == dst.len() && n > 0 => (n, n),
                None => return None,
            };
            let to = next - (n - used) as u64;
            self.hop.store(to, Release);
            self.set(W_HOP, to);
            return Some(len);
        }
    }

    /// Een lezersplaats op 5555, als er nog een is; [`Ring::leave`] geeft
    /// hem terug.
    pub fn seat(&self) -> Option<usize> {
        let taken = self.seats.fetch_update(AcqRel, Acquire, |s| {
            (s.trailing_ones() < u32::from(MAX_READERS)).then(|| s | (1 << s.trailing_ones()))
        });
        taken.ok().map(|s| s.trailing_ones() as usize)
    }

    /// Geeft plaats `seat` terug.
    pub fn leave(&self, seat: usize) {
        self.seats.fetch_and(!(1 << seat), AcqRel);
    }
}

/// Speelt de ring af naar `conn` vanaf de oudste byte en volgt hem, tot de
/// lezer weggaat: EOF of een fout op de leeskant, een mislukte schrijf, of
/// een schrijf die [`STALL`] vastzit.
pub async fn stream(
    ring: &Ring,
    seat: usize,
    conn: &mut impl Conn,
    timer: &impl Timer,
    buf: &mut [u8],
) {
    let (mut seen, mut sink) = (ring.oldest(), [0u8; 64]);
    loop {
        let (n, next) = ring.read(seen, buf);
        if n > 0 {
            match select(
                conn.write(buf.get(..n).unwrap_or_default()),
                timer.sleep(STALL),
            )
            .await
            {
                Either::Left(Ok(w)) if w > 0 => seen = next - (n - w.min(n)) as u64,
                _ => return,
            }
        } else {
            seen = next;
            // Wat de lezer stuurt, gaat weg; EOF of een fout is: weg.
            if let Either::Left(Ok(0) | Err(_)) =
                select(conn.read(&mut sink), ring.grown(seat, seen)).await
            {
                return;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Result;
    use crate::testutil::{FakeTimer, block_on};
    use std::boxed::Box;
    use std::sync::Mutex;
    use std::vec::Vec;

    /// Geheugen in een vector.
    struct Bytes(Mutex<Vec<u8>>);

    impl Mem for Bytes {
        fn put(&self, off: u64, b: &[u8]) {
            let o = off as usize;
            self.0.lock().unwrap()[o..o + b.len()].copy_from_slice(b);
        }
        fn get(&self, off: u64, out: &mut [u8]) {
            let o = off as usize;
            out.copy_from_slice(&self.0.lock().unwrap()[o..o + out.len()]);
        }
    }

    /// Een open ring van `data` bytes op vers geheugen (ook voor de toetsen
    /// van `NEXT_LOG` 0 in system.rs).
    pub(crate) fn ring(data: u64) -> Ring {
        let (r, _) = at(data, None);
        r.open();
        r
    }

    /// Een ring van `data` bytes op vers geheugen, of op `mem`.
    fn at(data: u64, mem: Option<&'static Bytes>) -> (Ring, &'static Bytes) {
        let mem = mem.unwrap_or_else(|| {
            Box::leak(Box::new(Bytes(Mutex::new(
                std::vec![0xa5; (HEAD + data) as usize],
            ))))
        });
        (Ring::new(mem, HEAD + data), mem)
    }

    #[test]
    fn the_ring_keeps_the_newest_and_a_late_reader_starts_at_the_oldest() {
        let (r, _) = at(8, None);
        assert_eq!(r.open(), 0);
        r.append(b"abc");
        let mut out = [0u8; 16];
        assert_eq!(r.read(0, &mut out), (3, 3));
        r.append(b"defghij");
        let (n, next) = r.read(0, &mut out);
        assert_eq!((&out[..n], next), (&b"cdefghij"[..], 10));
        let mut small = [0u8; 3];
        assert_eq!(r.read(5, &mut small), (3, 8));
        assert_eq!(&small, b"fgh");
        r.append(b"0123456789AB"); // langer dan de ring: alleen de staart.
        assert_eq!(r.read(0, &mut out).0, 8);
        assert_eq!(&out[..8], b"456789AB");
    }

    #[test]
    fn a_new_kernel_carries_the_ring_and_every_reader_goes_on() {
        let (r, mem) = at(64, None);
        r.open();
        r.append(b"kern A: one\nkern A: two\n");
        let mut line = [0u8; 32];
        assert_eq!(r.hop_line(&mut line), Some(11));
        assert!(r.pump(|b| b.len().min(12)));
        // De flip of de reset: dezelfde plek, een nieuwe kern.
        let (b, _) = at(64, Some(mem));
        assert_eq!(b.open(), 24);
        assert_eq!(b.hop_line(&mut line), Some(11));
        assert_eq!(&line[..11], b"kern A: two");
        let mut wire = Vec::new();
        b.pump(|x| {
            wire.extend_from_slice(x);
            x.len()
        });
        assert_eq!(wire, b"kern A: two\n");
        // Een andere maat (of rommel) is een verse ring.
        let (c, _) = at(32, Some(mem));
        assert_eq!(c.open(), 0);
    }

    #[test]
    fn hop_gets_whole_lines_and_a_long_line_in_pieces() {
        let (r, _) = at(64, None);
        r.open();
        let mut dst = [0u8; 4];
        r.append(b"abcdefg\nhalf");
        assert_eq!(r.hop_line(&mut dst), Some(4));
        assert_eq!(r.hop_line(&mut dst), Some(3));
        assert_eq!(&dst[..3], b"efg");
        assert_eq!(r.hop_line(&mut dst), Some(4)); // "half": past precies.
        r.append(b"\n");
        assert_eq!(r.hop_line(&mut dst), Some(0));
        assert_eq!(r.hop_line(&mut dst), None);
    }

    #[test]
    fn the_uart_takes_what_fits_and_counts_what_it_lost() {
        let (r, _) = at(8, None);
        r.open();
        r.append(b"0123");
        let mut wire = Vec::new();
        let more = r.pump(|b| {
            let n = b.len().min(2);
            wire.extend_from_slice(&b[..n]);
            n
        });
        assert_eq!((more, r.take_lost(), &wire[..]), (true, 0, &b"01"[..]));
        r.append(b"456789ab"); // de UART ligt nu een hele ring achter.
        wire.clear();
        let more = r.pump(|b| {
            wire.extend_from_slice(b);
            b.len()
        });
        assert_eq!(
            (more, r.take_lost(), &wire[..]),
            (false, 2, &b"456789ab"[..])
        );
        r.append(b"cd");
        r.uart_skip(0);
        assert!(!r.pump(|_| panic!("nothing is due")));
        r.uart_rewind();
        assert!(!r.pump(|b| b.len()));
        assert_eq!((r.uart(), r.take_lost()), (14, 0));
    }

    #[test]
    fn readers_are_capped() {
        let (r, _) = at(8, None);
        let seats: Vec<_> = (0..MAX_READERS).map(|_| r.seat().unwrap()).collect();
        assert_eq!(seats, [0, 1, 2, 3]);
        assert!(r.seat().is_none());
        r.leave(1);
        assert_eq!(r.seat(), Some(1));
        assert_eq!(FULL, b"console: full, 4 readers\n");
    }

    struct Client {
        got: Vec<u8>,
        closed: bool,
        stuck: bool,
    }

    impl Conn for Client {
        fn read(&mut self, _: &mut [u8]) -> impl Future<Output = Result<usize>> {
            let closed = self.closed;
            async move {
                if !closed {
                    core::future::pending::<()>().await;
                }
                Ok(0)
            }
        }
        fn write(&mut self, b: &[u8]) -> impl Future<Output = Result<usize>> {
            let stuck = self.stuck;
            if !stuck {
                self.got.extend_from_slice(b);
            }
            let n = b.len();
            async move {
                if stuck {
                    core::future::pending::<()>().await;
                }
                Ok(n)
            }
        }
        fn remote_ip4(&self) -> u32 {
            0
        }
    }

    // Een lezer krijgt de ring en geeft zijn plaats terug zodra hij sluit.
    #[test]
    fn a_reader_gets_the_history_and_leaves_on_eof() {
        let (r, _) = at(64, None);
        r.open();
        r.append(b"HOPOS_CONPORT_UP\n");
        let mut c = Client {
            got: Vec::new(),
            closed: true,
            stuck: false,
        };
        block_on(stream(&r, 0, &mut c, &FakeTimer::default(), &mut [0u8; 16]));
        assert_eq!(c.got, b"HOPOS_CONPORT_UP\n");
    }

    // Een peer die zonder RST verdween: de schrijf zit vast, na STALL weg.
    #[test]
    fn a_stuck_reader_is_dropped_after_stall() {
        let (r, _) = at(64, None);
        r.open();
        r.append(b"x\n");
        let mut c = Client {
            got: Vec::new(),
            closed: false,
            stuck: true,
        };
        let timer = FakeTimer::default();
        block_on(stream(&r, 0, &mut c, &timer, &mut [0u8; 16]));
        assert_eq!(timer.now.get(), STALL.as_nanos() as u64);
    }
}
