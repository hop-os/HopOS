//! De console van de kern (`kern::conport::Ring`): één ring op de plek van
//! de zwarte doos van het board, de tee die erin schrijft, en de UART als
//! zijn eerste lezer. De andere twee: de listener op 5555 (net.rs) en
//! `NEXT_LOG` 0 van Hop (`main::system`). Op een headless board (de Pi 4,
//! de Radxa, de O6N zonder UART aan de Mac) is 5555 de enige plek waar de
//! kern zijn redenering kwijt kan, ook die van een kern die net stierf.

use board::Board as _;
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use core::time::Duration;
use cpu::println;
use dev::Pa;
use executor::Executor;
use kern::conport::{Mem, Ring};
use sync::Signal;
use vboard::slots::BLACK_BOX;

/// De maat: de doos van het board (32 KiB op elk board dat er een heeft:
/// de boot plus een kwartier zonder tik). Zonder doos (riscv64: nog geen
/// plek die bewezen een reset overleeft) 32 KiB in .bss, die een reset
/// niet overleeft.
const LEN: u64 = if BLACK_BOX.size == 0 {
    32 << 10
} else {
    BLACK_BOX.size
};
const _: () = assert!(BLACK_BOX.size == 0 || BLACK_BOX.base.is_multiple_of(64));

/// De ring van een board zonder doos.
static BSS: [AtomicU8; if BLACK_BOX.size == 0 { LEN as usize } else { 0 }] =
    [const { AtomicU8::new(0) }; if BLACK_BOX.size == 0 { LEN as usize } else { 0 }];

/// De plek van de ring. De doos ligt buiten het kernimage, de staging, de
/// DTB, de pool en de DMA, en nooit waar de firmware of de lader bij een
/// verse boot schrijft; elke schrijf gaat meteen naar DRAM.
struct Place;

impl Mem for Place {
    fn put(&self, off: u64, b: &[u8]) {
        if BLACK_BOX.size == 0 {
            BSS.iter()
                .skip(off as usize)
                .zip(b)
                .for_each(|(x, c)| x.store(*c, Relaxed));
            return;
        }
        dev::copy_in(Pa(BLACK_BOX.base + off), b);
        dev::push(Pa(BLACK_BOX.base + off), b.len());
    }

    fn get(&self, off: u64, out: &mut [u8]) {
        if BLACK_BOX.size == 0 {
            BSS.iter()
                .skip(off as usize)
                .zip(out)
                .for_each(|(x, c)| *c = x.load(Relaxed));
            return;
        }
        dev::pull(Pa(BLACK_BOX.base + off), out.len());
        dev::copy_out(out, Pa(BLACK_BOX.base + off));
    }
}

/// De console van de kern.
pub(crate) static RING: Ring = Ring::new(&Place, LEN);

/// Zo vaak kijkt de pomp terug zolang de UART achterloopt: een UART heeft
/// geen lijn op deze kern (R1), en een FIFO van 16 tekens is op 115200 baud
/// na 1,4 ms leeg.
const UART_STEP: Duration = Duration::from_millis(1);
/// Zo lang mag de UART niets aannemen voor de pomp hem opgeeft en de
/// console weer wachtend schrijft (een UART zonder klok).
const UART_STUCK_NS: u64 = 1_000_000_000;
/// Hoeveel achterstand een wachtende schrijver eerst wegzet (de boot, een
/// noodregel): 8 KiB, 0,7 s op 115200. De rest staat op 5555.
const FLUSH_MAX: u64 = 8 * 1024;

/// [`drain`] draait: de UART krijgt zonder wachten wat in zijn FIFO past.
static PUMPING: AtomicBool = AtomicBool::new(false);
/// De bel van [`drain`]: de tee liet bytes liggen.
static BEHIND: Signal = Signal::new();
/// De core van het glas (`gui::glass`): de OS-core.
static GLASS_CORE: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Staat 5555 aan (de keuze van `kern::nodecfg::console_enabled`)?
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Opent de ring (wat een vorige kern erin liet, blijft) en zet de tee als
/// console-sink.
pub(crate) fn install() {
    let carried = RING.open();
    cpu::console::set_sink(tee);
    println!(
        "console: {carried} bytes from before this boot in the ring (a flip or a reset leaves them), tcp/5555 replays them HOPOS_CONSOLE_CARRIED"
    );
}

/// De OS-core neemt het glas (na een verhuizing van de kern).
pub(crate) fn here() {
    GLASS_CORE.store(crate::BOARD.this_core(), Release);
}

/// De sink, onder het console-slot, van elke core: de ring, dan de UART
/// (zonder wachten zodra [`drain`] draait, anders wachtend met hoogstens
/// [`FLUSH_MAX`] achterstand ervoor), dan het glas op de OS-core.
pub(crate) fn tee(b: &[u8]) {
    let board = &crate::BOARD;
    RING.append(b);
    match board.console_nowait() {
        Some(put) if PUMPING.load(Acquire) && !cpu::console::urgent() => {
            if RING.pump(put) {
                BEHIND.set();
            }
        }
        _ => {
            let wait = board.console();
            RING.uart_skip(FLUSH_MAX);
            RING.pump(|c| {
                wait(c);
                c.len()
            });
        }
    }
    if GLASS_CORE.load(Acquire) == board.this_core() {
        crate::gui::glass(b);
    }
}

/// De pomp van de UART, een taak op de OS-core: vanaf zijn eerste ronde
/// schrijft de tee niet meer wachtend, en de pomp geeft de UART wat er in
/// zijn FIFO past, elke [`UART_STEP`] zolang hij achterloopt, anders na de
/// bel van de tee.
pub(crate) async fn drain(exec: &'static Executor) {
    let Some(put) = crate::BOARD.console_nowait() else {
        return;
    };
    PUMPING.store(true, Release);
    let (mut at, mut since) = (RING.uart(), exec.now());
    loop {
        let more = cpu::console::locked(|| RING.pump(put));
        let now = exec.now();
        if RING.uart() != at || !more {
            (at, since) = (RING.uart(), now);
        }
        if !more {
            // Eén regel per achterstand, pas als de UART weer bij is.
            let lost = RING.take_lost();
            if lost != 0 {
                println!(
                    "console: the UART lost {lost} bytes the ring overwrote first; tcp/5555 had them HOPOS_CONSOLE_DROPPED"
                );
                continue;
            }
            BEHIND.wait().await;
            continue;
        }
        if now.saturating_sub(since) >= UART_STUCK_NS {
            PUMPING.store(false, Release);
            println!(
                "console: the UART took nothing for {} ms, writing it waiting again HOPOS_CONSOLE_STUCK",
                UART_STUCK_NS / 1_000_000
            );
            return;
        }
        exec.after(UART_STEP).await;
    }
}

/// De UART begint opnieuw bij de oudste byte van de ring, voor een lezer
/// die pas na de boot aanhaakt (`hopos.replay`, de M4 over de dockchannel).
pub(crate) fn rewind() {
    cpu::console::locked(|| RING.uart_rewind());
    BEHIND.set();
}

/// De keuze van de config, vóór de listener (net.rs).
pub(crate) fn enable(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// Staat 5555 aan?
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}
