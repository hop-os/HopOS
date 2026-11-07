//! De PL011-UART: de console van QEMU virt en de Pi's.
//!
//! Hetzelfde PrimeCell-registerblok zit op QEMU virt (0x0900_0000), de Pi 4
//! (0xFE20_1000) en de Pi 5 (0x10_7d00_1000). Alleen de basis verschilt per
//! board; de offsets en de TX-FIFO-vol-bit zijn PrimeCell-standaard en leven
//! daarom één keer, hier, in plaats van per board hergedefinieerd (dan
//! corrigeer je een verkeerde offset maar in één board).
//!
//! Deze crate bezit het registerblok en niets anders: geen slot, geen
//! buffer. Wie meerdere schrijvers heeft (de console), zet het ene
//! toegestane slot erboven (handboek §1.3).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use core::fmt;
use core::mem::offset_of;
use dev::{Pa, Reg, Stall};

/// Het PL011-registerblok (ARM DDI 0183, tabel 3-1).
#[repr(C)]
struct Regs {
    /// Data register: schrijven zet een byte in de TX-FIFO.
    dr: Reg<u32>,
    /// Receive status / error clear.
    rsr: Reg<u32>,
    _r0: [u32; 4],
    /// Flag register: FIFO-standen en busy.
    fr: Reg<u32>,
    _r1: u32,
    /// IrDA low-power counter.
    ilpr: Reg<u32>,
    /// Integer baud rate divisor.
    ibrd: Reg<u32>,
    /// Fractional baud rate divisor.
    fbrd: Reg<u32>,
    /// Line control: woordlengte, FIFO aan.
    lcr_h: Reg<u32>,
    /// Control: UART, TX en RX aan.
    cr: Reg<u32>,
    /// Interrupt FIFO level select.
    ifls: Reg<u32>,
    /// Interrupt mask set/clear.
    imsc: Reg<u32>,
    /// Raw interrupt status.
    ris: Reg<u32>,
    /// Masked interrupt status.
    mis: Reg<u32>,
    /// Interrupt clear.
    icr: Reg<u32>,
}

const _: () = {
    assert!(offset_of!(Regs, dr) == 0x00);
    assert!(offset_of!(Regs, rsr) == 0x04);
    assert!(offset_of!(Regs, fr) == 0x18);
    assert!(offset_of!(Regs, ilpr) == 0x20);
    assert!(offset_of!(Regs, ibrd) == 0x24);
    assert!(offset_of!(Regs, fbrd) == 0x28);
    assert!(offset_of!(Regs, lcr_h) == 0x2c);
    assert!(offset_of!(Regs, cr) == 0x30);
    assert!(offset_of!(Regs, ifls) == 0x34);
    assert!(offset_of!(Regs, imsc) == 0x38);
    assert!(offset_of!(Regs, ris) == 0x3c);
    assert!(offset_of!(Regs, mis) == 0x40);
    assert!(offset_of!(Regs, icr) == 0x44);
};

/// FR: TX-FIFO vol.
const FR_TXFF: u32 = 1 << 5;
/// LCR_H: FIFO's aan.
const LCR_FEN: u32 = 1 << 4;
/// LCR_H: 8 databits.
const LCR_WLEN8: u32 = 0b11 << 5;
/// CR: UART aan.
const CR_UARTEN: u32 = 1 << 0;
/// CR: zender aan.
const CR_TXE: u32 = 1 << 8;
/// CR: ontvanger aan.
const CR_RXE: u32 = 1 << 9;

/// De poll-grens op een volle TX-FIFO: ~1M leesacties is veel meer dan 16
/// tekens op 115200 baud. Een ongeklokte of dode PL011 (de Pi 5-debug-UART
/// zonder sessie) leest all-ones en zou de console anders eeuwig gijzelen;
/// na deze grens staat de lijn als gestokt ([`Stall`]) in plaats van de boot
/// te laten hangen (Go-driver, `metal/driver/pl011`).
const POLL_LIMIT: u32 = 1 << 20;
/// Het budget per byte op een gestokte lijn: één blik. Een PL011 heeft geen
/// lezer die later aanhaakt; komt hij terug, dan is zijn FIFO leeg en past
/// de eerste byte meteen.
const POLL_STALLED: u32 = 1;

/// De baudrate als de kern hem zelf zet ([`Pl011::with_clock`]).
const BAUD: u32 = 115_200;

/// Eén PL011 op een vaste basis.
pub struct Pl011 {
    base: Pa,
    tx: Stall,
    /// De UART-klok in Hz, of 0: de baudrate blijft zoals de firmware hem
    /// zette.
    clock: u32,
}

impl Pl011 {
    /// Een PL011 op `base`.
    ///
    /// # Safety
    ///
    /// `base` is de basis van een gemapt PL011-registerblok dat zolang het
    /// programma draait blijft bestaan (een layout-adres van het board).
    #[must_use]
    pub const unsafe fn new(base: Pa) -> Self {
        Self {
            base,
            tx: Stall::new(POLL_LIMIT, POLL_STALLED),
            clock: 0,
        }
    }

    /// Dezelfde PL011, maar [`init`](Self::init) zet de baudrate zelf
    /// ([`BAUD`]) uit de UART-klok `hz`, zoals Linux' pl011 hem uit de klok
    /// van de DT zet. Op de Pi 4 is dat nodig: de firmware draagt de UART
    /// over op een baudrate waarop niemand hem leest (ze meldt zelf
    /// "103448 Hz"; 07-11 op ijzer: stil tot de kern hem zette).
    #[must_use]
    pub const fn with_clock(mut self, hz: u32) -> Self {
        self.clock = hz;
        self
    }

    fn regs(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van `new` zegt dat `base` een gemapt
        // PL011-blok is dat voor altijd blijft; `Regs` is precies dat blok.
        unsafe { dev::regs(self.base) }
    }

    /// Zet de UART aan: 8N1 met FIFO's, TX en RX aan. Met een klok
    /// ([`with_clock`](Self::with_clock)) ook de baudrate; anders blijft hij
    /// staan zoals de firmware hem zette (QEMU negeert hem).
    pub fn init(&self) {
        let r = self.regs();
        r.cr.write(0);
        if self.clock != 0 {
            // Deler = klok / (16 x baud), in 1/64e: IBRD het gehele deel,
            // FBRD de rest; LCR_H hieronder zet ze in werking.
            let div = (u64::from(self.clock) * 4 + u64::from(BAUD) / 2) / u64::from(BAUD);
            r.ibrd.write((div >> 6) as u32);
            r.fbrd.write((div & 0x3f) as u32);
        }
        r.lcr_h.write(LCR_FEN | LCR_WLEN8);
        r.imsc.write(0);
        r.icr.write(0x7ff);
        r.cr.write(CR_UARTEN | CR_TXE | CR_RXE);
    }

    /// De stand van de lijn ([`Stall`]), voor wie de UART per schrijf
    /// opnieuw opbouwt.
    #[must_use]
    pub fn tx(&self) -> &Stall {
        &self.tx
    }

    /// Stuurt één byte, begrensd wachtend op ruimte in de TX-FIFO.
    pub fn putc(&self, c: u8) {
        let r = self.regs();
        self.tx
            .put(|| r.fr.read() & FR_TXFF == 0, || r.dr.write(u32::from(c)));
    }

    /// Schrijft van `b` wat er nu in de TX-FIFO past, zonder te wachten,
    /// met `\n` als `\r\n`; geeft hoeveel bytes van `b` er helemaal uit
    /// zijn. Een `\n` waarvan alleen de `\r` nog paste, telt niet mee: de
    /// volgende schrijf begint er opnieuw mee, en de terminal ziet één `\r`
    /// extra. Wacht nooit, ook niet op een gestokte lijn: daar ziet de pomp
    /// dat er niets meer uitgaat.
    ///
    /// Voor de pomp van de console (hopos `conport`): de schrijver van een
    /// regel wacht niet op de baudrate (Linux: `uart_port.xmit` met de
    /// TX-interrupt, in plaats van de console die per teken op TXFF spint).
    pub fn write_nowait(&self, b: &[u8]) -> usize {
        let r = self.regs();
        for (i, &c) in b.iter().enumerate() {
            if c == b'\n' {
                if r.fr.read() & FR_TXFF != 0 {
                    return i;
                }
                r.dr.write(u32::from(b'\r'));
            }
            if r.fr.read() & FR_TXFF != 0 {
                return i;
            }
            r.dr.write(u32::from(c));
        }
        b.len()
    }

    /// Schrijft `b`, met `\n` als `\r\n` (een terminal wil beide).
    pub fn write(&self, b: &[u8]) {
        for &c in b {
            if c == b'\n' {
                self.putc(b'\r');
            }
            self.putc(c);
        }
    }
}

impl fmt::Write for &Pl011 {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    /// Een nep-registerblok in gewoon geheugen: `dev` doet op de host
    /// normale geheugentoegang.
    fn fake() -> (Box<[u32; 32]>, Pl011) {
        let mut mem = Box::new([0u32; 32]);
        let pa = Pa(mem.as_mut_ptr() as usize as u64);
        // SAFETY: de box leeft zolang de test loopt en is groot genoeg voor
        // het blok.
        (mem, unsafe { Pl011::new(pa) })
    }

    #[test]
    fn writes_bytes_to_dr_and_expands_newline() {
        let (mem, u) = fake();
        u.putc(b'x');
        assert_eq!(mem[0], u32::from(b'x'));
        let mut w = &u;
        writeln!(w, "a").unwrap();
        // De laatste byte in DR is de '\n', na een '\r'.
        assert_eq!(mem[0], u32::from(b'\n'));
    }

    #[test]
    fn a_stuck_fifo_stalls_the_line_until_a_byte_fits() {
        let (mut mem, u) = fake();
        mem[0x18 / 4] = FR_TXFF; // TX-FIFO blijft vol
        u.putc(b'x');
        assert!(u.tx().is_stalled());
        assert_eq!(mem[0], 0, "the byte fell");
        mem[0x18 / 4] = 0;
        u.putc(b'y'); // de FIFO is terug: de byte gaat erin
        assert_eq!(mem[0], u32::from(b'y'));
        assert!(!u.tx().is_stalled());
    }

    #[test]
    fn write_nowait_stops_at_a_full_fifo_and_never_spins() {
        let (mut mem, u) = fake();
        assert_eq!(u.write_nowait(b"ab\n"), 3);
        assert_eq!(mem[0], u32::from(b'\n'));
        mem[0x18 / 4] = FR_TXFF;
        mem[0] = 0;
        assert_eq!(u.write_nowait(b"cd"), 0);
        assert_eq!(mem[0], 0, "a full FIFO gets nothing");
        assert!(
            !u.tx().is_stalled(),
            "full is not stuck: the pump comes back"
        );
    }

    #[test]
    fn init_sets_115200_from_a_48_mhz_clock() {
        let (mem, u) = fake();
        let u = u.with_clock(48_000_000);
        u.init();
        assert_eq!((mem[0x24 / 4], mem[0x28 / 4]), (26, 3));
        assert_eq!(mem[0x30 / 4], CR_UARTEN | CR_TXE | CR_RXE);
    }

    #[test]
    fn init_enables_tx_and_rx() {
        let (mem, u) = fake();
        u.init();
        assert_eq!(mem[0x30 / 4], CR_UARTEN | CR_TXE | CR_RXE);
        assert_eq!(mem[0x2c / 4], LCR_FEN | LCR_WLEN8);
    }
}
