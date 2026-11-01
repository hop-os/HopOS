//! De console over TCP (`kern::conport`; Go: `kern/conport` en
//! `driver/conlog`): een ring van 256 KiB ([`RING_BYTES`]) achter de
//! UART-sink, waar `nc node 5555` eerst de bewaarde console uit krijgt en
//! daarna live meeleest. Op een headless board (de Pi 4, de Radxa, de O6N
//! zonder UART aan de Mac) is dit de enige plek waar de kern zijn
//! redenering kwijt kan; Derek noemde hem op 30-09 "killer" toen hij ontbrak.
//!
//! Dit bezit de ring, de tee en de lezersplaatsen; de listener en de
//! lezertaken staan in net.rs (de node-stack), en de aan/uit-keuze komt uit
//! de config (kern::nodecfg: `hopos.console`, anders `hopos.insecure`).
//!
//! Eigendom: de ring is van de OS-core (`LocalCell`, zoals het glas in
//! gui.rs, dat de tee op dezelfde core als laatste schrijft). De tee schrijft
//! alleen op die core; een regel van een andere core gaat naar de UART en
//! niet in de ring of op het glas. Een lening die al loopt (een noodregel
//! uit exception-context midden in een regel) slaat de ring over, de UART
//! krijgt alles. De lezers lenen de ring per hap
//! (`snapshot`), nooit over een await heen.
//!
//! De tee geeft de bytes van de OS-core ook aan de regelring van de kern
//! ([`KERN_LOG`], `kern::system::KernLog`): de console van de kern voor
//! `NEXT_LOG` 0, zodat Hop hem naast die van zijn apps toont. De regels van
//! de apps markeert hun schrijver (`glue::KernConsole::app_line`); die gaan
//! niet in die ring, want Hop haalt ze al per slot.
//!
//! De tee geeft elke regel ook aan de zwarte doos (`flip::black_box`): een
//! ring op een vaste plek buiten het kernimage die een watchdog-reset
//! overleeft, want deze ring in de BSS doet dat niet. Die schrijft vanaf elke
//! core; de volgende koude boot drukt de staart af (`HOPOS_FLIP_BLACKBOX`).
//!
//! De UART is een lezer van de ring zoals de TCP-lezers ([`drain`]): op de
//! OS-core wacht een regel niet meer op de baudrate. Tot 04-10 schreef de
//! tee elke regel wachtend naar de UART, en op 115200 baud kostte een
//! regel van 100 tekens 7,7 ms waarin de executor van de kern stilstond:
//! de switch, de node-stack en elke bewoner van de OS-core. GEMETEN 04-10
//! op de Pi 4 (vitals `test=all`, vijf runs per kern): de twee logregels
//! van vitals vlak voor zijn rtt-toets hielden de servicer 9,8 ms bezig en
//! de SYN van de tweede dial 5 ms in de ring; de rtt naar de kern had een
//! p99 van 5361 tot 9282 us, met deze pomp 367 tot 592 (p50 336, gelijk).
//! Wachtend blijft: een board zonder `console_nowait`, de boot tot de pomp
//! draait, een regel van een andere core, en alles na een noodregel.

use board::Board as _;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use core::time::Duration;
use cpu::println;
use executor::Executor;
use kern::conport::{Readers, Ring, Uart};
use kern::system::KernLog;
use sync::{LocalCell, Signal};

/// De ring: 256 KiB. Een boot is 10 KB tot `HOP_UP` (de Pi 5, 30-09),
/// maar de tik is 300 bytes per seconde, dus 64 KiB was na drie minuten
/// alleen nog tikken en de boot was weg (de Pi 4 op 5555, 30-09). Een
/// kwart megabyte houdt de boot een kwartier vast; .bss, geen heap.
const RING_BYTES: usize = 256 * 1024;

/// Zo vaak kijkt de pomp terug zolang de UART achterloopt: een FIFO van
/// 16 tekens is op 115200 baud na 1,4 ms leeg.
const UART_STEP: Duration = Duration::from_millis(1);

/// Zo lang mag de UART niets aannemen voor de pomp hem opgeeft en de
/// console weer wachtend schrijft (een UART zonder klok: de wachtende
/// schrijver verklaart hem dan dood, `Pl011::is_dead`).
const UART_STUCK_NS: u64 = 1_000_000_000;

/// Hoeveel van de achterstand een wachtende schrijver eerst nog wegzet (de
/// noodregel, de flip): 8 KiB, 0,7 s op 115200. De rest staat in de ring,
/// op 5555 en in de zwarte doos.
const FLUSH_MAX: usize = 8 * 1024;

/// De ring en de UART als zijn lezer, samen van één eigenaar: de OS-core.
struct Log {
    ring: Ring<RING_BYTES>,
    uart: Uart,
}

/// De ring zelf; alleen de OS-core raakt hem aan.
static LOG: LocalCell<Log> = LocalCell::cell(Log {
    ring: Ring::new(),
    uart: Uart::new(),
});
/// De UART zonder wachten (`Board::console_nowait`), gezet door [`drain`]
/// zodra die draait. Null = elke regel gaat wachtend naar de UART.
static NOWAIT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// De bel van [`drain`]: de tee liet bytes liggen.
static BEHIND: Signal = Signal::new();
/// De core die de ring bezit (`usize::MAX` = nog geen).
static RING_CORE: AtomicUsize = AtomicUsize::new(usize::MAX);
/// De sink vóór ons (de UART van het board), zoals `kmain` hem zette.
/// Null = nog geen tee.
static PREV: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// Staat de TCP-poort aan (de keuze van kern::nodecfg)?
static ENABLED: AtomicBool = AtomicBool::new(false);
/// De lezersplaatsen: hoogstens `kern::conport::MAX_READERS` tegelijk.
pub(crate) static READERS: Readers = Readers::new();
/// De console van de kern regel voor regel, zonder de regels van de apps,
/// voor `NEXT_LOG` 0 (de system-API leest hem, `main::system`). Net als de
/// ring hierboven van de OS-core; 64 KiB in .bss (de maat staat bij
/// `kern::system::KERN_LOG_BYTES`).
pub(crate) static KERN_LOG: KernLog = KernLog::new();

/// Hangt de ring achter `prev` en zet de tee als console-sink; de core van
/// nu bezit de ring tot [`here`] hem overdraagt.
pub(crate) fn install(prev: fn(&[u8])) {
    PREV.store(prev as *mut (), Release);
    RING_CORE.store(crate::BOARD.this_core(), Release);
    cpu::console::set_sink(tee);
}

/// De OS-core neemt de ring over (na een verhuizing van de kern, vóór het
/// glas): vanaf nu schrijft alleen deze core erin.
pub(crate) fn here() {
    RING_CORE.store(crate::BOARD.this_core(), Release);
}

/// De vorige sink (de UART, wachtend), als die er is.
fn prev() -> Option<fn(&[u8])> {
    let p = PREV.load(Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: `PREV` wordt alleen door `install` geschreven, met een
    // geldige `fn(&[u8])`; een functiepointer en een datapointer zijn op
    // onze targets even groot, en null is uitgesloten (zelfde vorm als
    // `cpu::console::sink`).
    Some(unsafe { core::mem::transmute::<*mut (), fn(&[u8])>(p) })
}

/// De UART zonder wachten, zodra [`drain`] draait en er geen noodregel
/// was.
fn nowait() -> Option<fn(&[u8]) -> usize> {
    let p = NOWAIT.load(Acquire);
    if p.is_null() || cpu::console::urgent() {
        return None;
    }
    // SAFETY: `NOWAIT` wordt alleen door `drain` geschreven, met de
    // `fn(&[u8]) -> usize` van het board; null is uitgesloten.
    Some(unsafe { core::mem::transmute::<*mut (), fn(&[u8]) -> usize>(p) })
}

/// De sink: eerst de zwarte doos, dan de UART, dan de ring en het glas
/// (`gui::glass`) op de eigen core.
///
/// De doos eerst, zoals in Go (`conlog.Route`): hangt de UART-poll, dan
/// staat de regel toch al in DRAM. Op de OS-core met een draaiende
/// [`drain`] gaat de regel de ring in en krijgt de UART wat er nu in zijn
/// FIFO past; anders schrijft de tee hem wachtend, na de achterstand.
pub(crate) fn tee(b: &[u8]) {
    crate::flip::black_box(b);
    if RING_CORE.load(Acquire) != crate::BOARD.this_core() {
        if let Some(prev) = prev() {
            prev(b);
        }
        return;
    }
    KERN_LOG.feed(b);
    match (nowait(), LOG.try_borrow_mut()) {
        (Some(put), Ok(mut log)) => {
            let Log { ring, uart } = &mut *log;
            ring.write(b);
            if uart.pump(ring, put) {
                BEHIND.set();
            }
        }
        (_, Ok(mut log)) => {
            let Log { ring, uart } = &mut *log;
            if let Some(prev) = prev() {
                flush_into(ring, uart, prev);
                prev(b);
            }
            ring.write(b);
            uart.caught_up(ring);
        }
        // Een lening die al loopt (een noodregel midden in een regel): de
        // UART krijgt hem meteen, de ring niet.
        (_, Err(_)) => {
            if let Some(prev) = prev() {
                prev(b);
            }
        }
    }
    crate::gui::glass(b);
}

/// Zet hoogstens [`FLUSH_MAX`] van de achterstand wachtend op de UART.
fn flush_into(ring: &Ring<RING_BYTES>, uart: &mut Uart, prev: fn(&[u8])) {
    let mut budget = FLUSH_MAX;
    uart.pump(ring, |b| {
        let n = b.len().min(budget);
        if let Some(chunk) = b.get(..n) {
            prev(chunk);
        }
        budget -= n;
        n
    });
    uart.caught_up(ring);
}

/// Zet de achterstand van de UART er wachtend op, vóór iets dat de pomp
/// niet overleeft (de sprong van een flip). Alleen op de OS-core.
pub(crate) fn flush() {
    if RING_CORE.load(Acquire) != crate::BOARD.this_core() {
        return;
    }
    if let (Some(prev), Ok(mut log)) = (prev(), LOG.try_borrow_mut()) {
        let Log { ring, uart } = &mut *log;
        flush_into(ring, uart, prev);
    }
}

/// De pomp van de UART, een taak op de OS-core: zet de UART op niet
/// wachten (`Board::console_nowait`) en geeft hem daarna wat er in zijn
/// FIFO past, elke [`UART_STEP`] zolang hij achterloopt, en anders na de
/// bel van de tee. Pas vanaf zijn eerste poll gaat de tee niet meer
/// wachtend: tot de executor draait, staat elke bootregel meteen op de
/// UART.
pub(crate) async fn drain(exec: &'static Executor) {
    let Some(put) = crate::BOARD.console_nowait() else {
        return;
    };
    // De achterstand van vóór de pomp is er al (de tee schreef wachtend).
    NOWAIT.store(put as *mut (), Release);
    // De laatste positie van de UART en sinds wanneer hij daar staat, en
    // wat de ring hem afnam.
    let (mut at, mut since, mut lost) = (0u64, exec.now(), 0u64);
    loop {
        let (more, now_at) = match LOG.try_borrow_mut() {
            Ok(mut log) => {
                let Log { ring, uart } = &mut *log;
                let more = uart.pump(ring, put);
                lost = lost.saturating_add(uart.take_dropped());
                (more, uart.at())
            }
            Err(_) => (true, at),
        };
        let now = exec.now();
        if now_at != at || !more {
            (at, since) = (now_at, now);
        }
        if !more {
            // Eén regel per achterstand, pas als de UART weer bij is: een
            // regel zolang hij achterligt, duwt zelf weer bytes uit de ring
            // (04-10, P63: 1000 van die regels per seconde, zonder eind).
            if lost != 0 {
                println!(
                    "console: the UART lost {lost} bytes the ring overwrote first; they are on tcp/5555 and in the black box HOPOS_CONSOLE_DROPPED"
                );
                lost = 0;
                continue;
            }
            BEHIND.wait().await;
            since = exec.now();
            continue;
        }
        if now.saturating_sub(since) >= UART_STUCK_NS {
            NOWAIT.store(core::ptr::null_mut(), Release);
            println!(
                "console: the UART took nothing for {} ms, writing it waiting again HOPOS_CONSOLE_STUCK",
                UART_STUCK_NS / 1_000_000
            );
            return;
        }
        exec.after(UART_STEP).await;
    }
}

/// Een hap uit de ring vanaf `seen`, voor `kern::conport::stream`. Een
/// ring die net beschreven wordt geeft niets; de volgende poll wel.
pub(crate) fn snapshot(seen: u64, out: &mut [u8]) -> (usize, u64) {
    match LOG.try_borrow() {
        Ok(l) => l.ring.since(seen, out),
        Err(_) => (0, seen),
    }
}

/// Waar een nieuwe lezer begint: het oudste dat nog in de ring staat, niet
/// nul, zodat wie na de boot verbindt de hele geschiedenis krijgt die er is.
pub(crate) fn oldest() -> u64 {
    LOG.try_borrow().map_or(0, |l| l.ring.oldest())
}

/// Herhaalt het begin van deze boot op de vorige sink (de UART of de
/// dockchannel), hoogstens `max` bytes, zonder de ring opnieuw te vullen.
/// Voor een board waarvan de lezer pas na de boot aanhaakt (de M4, 30-09:
/// de kis-poort verschijnt pas 30 s na een herstart, en de bootregels zijn
/// dan al weg). Alleen op de core die de ring bezit; geeft het aantal
/// bytes.
pub(crate) fn replay(max: usize) -> usize {
    let Some(prev) = prev() else {
        return 0;
    };
    let mut seen = oldest();
    let mut total = 0;
    let mut buf = [0u8; 512];
    while total < max {
        let want = (max - total).min(buf.len());
        let (n, next) = snapshot(seen, &mut buf[..want]);
        if n == 0 {
            break;
        }
        prev(&buf[..n]);
        seen = next;
        total += n;
    }
    total
}

/// De keuze van de config: aan of uit. Vóór de listener (net.rs) gezet.
pub(crate) fn enable(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// Staat de poort aan?
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}
