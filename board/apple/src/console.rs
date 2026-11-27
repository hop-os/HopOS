//! De console van de Mac mini: de dockchannel en de Samsung-UART, allebei
//! al opgezet door iBoot of m1n1 (klok, baud, pinnen); wij pollen en
//! schrijven alleen.
//!
//! - De dockchannel (`aapl,dock-channels`) is een FIFO naar de
//!   Type-C-debugpoort, en m1n1's console op de M2 en later: TX-vrije
//!   plaatsen op +0x4014, een byte in op +0x4004 (m1n1
//!   `dockchannel_uart.c`). Op de laptop is het `/dev/cu.kis-100000-ch-0`
//!   na `macvdmtool reboot debugusb` (GEMETEN 28-08).
//! - uart0 (`uart-1,samsung`): UTRSTAT op +0x10 (bit 1 = TX leeg), UTXH op
//!   +0x20 (m1n1 `uart.c`). Op de M4 droeg de debug-console hier niets van
//!   (28-08), maar het kost per byte één poll en een board dat zonder de
//!   dockchannel opkomt, heeft dan nog een weg.
//!
//! Vaste adressen, en dat is een keuze (Go 29-08): de ADT weet ze ook, maar
//! de eerste bytes console-uitvoer mogen niet hangen aan een boom die zelf
//! pas werkt als het DRAM gemapt is. `discover` meldt het als de ADT het
//! anders ziet.
//!
//! Een console mag de node nooit ophouden: de poll is begrensd, en een lijn
//! die stokt krijgt daarna een klein budget tot een byte weer past. Zo kost
//! een lezerloze FIFO geen 2 ms per byte, maar komt een lezer die later
//! aanhaakt wél weer in beeld (Go `console.go`).

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use dev::{Pa, Stall};

/// De dockchannel uit de chip-tabel (de t8132: GEMETEN 28-08), tot de ADT
/// iets anders zegt ([`adopt_adt`]); 0 = nog geen.
static DOCK: AtomicU64 = AtomicU64::new(crate::chip::CHIP.dock);
/// uart0 van de chip (de t8132: GEMETEN 28-08; de t8152: m1n1
/// `EARLY_UART_BASE`).
pub const UART0: u64 = crate::chip::CHIP.uart0;

/// De dockchannel waar de console nu naartoe schrijft (0 = geen).
#[must_use]
pub fn dockchannel() -> u64 {
    DOCK.load(Relaxed)
}

/// De dockchannel volgens de ADT, zodra die gelezen is. Wijkt hij af van
/// de vaste waarde, dan gaat de console erheen: op een chip die we nog
/// niet maten is de boom de betere bron (de eerste regels gingen dan naar
/// uart0 of nergens heen). Geeft de oude waarde als hij wisselde.
pub fn adopt_adt(d: u64) -> Option<u64> {
    if d == 0 || d >= 512 << 30 {
        return None;
    }
    let old = DOCK.swap(d, Relaxed);
    (old != d).then_some(old)
}

const DOCK_TX: u64 = 0x4004;
const DOCK_TX_FREE: u64 = 0x4014;
const UTRSTAT: u64 = 0x10;
const UTXH: u64 = 0x20;
const UTRSTAT_TXBE: u32 = 1 << 1;

/// De poll per byte; en als de vorige byte al niet paste. Hier groter dan
/// op een PL011: de dockchannel is een FIFO naar een lezer die later kan
/// aanhaken.
const POLL_MAX: u32 = 20_000;
const POLL_STALLED: u32 = 256;

static DOCK_LINE: Stall = Stall::new(POLL_MAX, POLL_STALLED);
static UART_LINE: Stall = Stall::new(POLL_MAX, POLL_STALLED);

/// Wacht begrensd tot `ready` en schrijft dan `c` (`dev::Stall`).
fn put(status: Pa, ready: impl Fn(u32) -> bool, data: Pa, c: u8, tx: &Stall) {
    tx.put(
        || ready(dev::read32(status)),
        || dev::write32(data, u32::from(c)),
    );
}

fn put_both(c: u8) {
    let d = dockchannel();
    if d != 0 {
        let d = Pa(d);
        put(
            d.add(DOCK_TX_FREE),
            |v| v != 0,
            d.add(DOCK_TX),
            c,
            &DOCK_LINE,
        );
    }
    let u = Pa(UART0);
    put(
        u.add(UTRSTAT),
        |v| v & UTRSTAT_TXBE != 0,
        u.add(UTXH),
        c,
        &UART_LINE,
    );
}

/// De console-haak van de kern: `\n` wordt `\r\n` (de tty aan de andere kant
/// staat raw; m1n1 doet hetzelfde).
pub fn write(b: &[u8]) {
    for &c in b {
        if c == b'\n' {
            put_both(b'\r');
        }
        put_both(c);
    }
}

/// Eén regel op de dockchannel met `v` als 16 hexcijfers, zonder stack-
/// buffers of formattering: dit draait met de MMU uit (`head::apple_early`).
pub(crate) fn raw_line(dock: Pa, text: &[u8], v: u64) {
    let byte = |c: u8| {
        put(
            dock.add(DOCK_TX_FREE),
            |s| s != 0,
            dock.add(DOCK_TX),
            c,
            &DOCK_LINE,
        )
    };
    for &c in b"\r\n".iter().chain(text) {
        byte(c);
    }
    for i in (0..16).rev() {
        let n = ((v >> (4 * i)) & 0xf) as u8;
        byte(if n < 10 { b'0' + n } else { b'a' + n - 10 });
    }
    byte(b'\r');
    byte(b'\n');
}
