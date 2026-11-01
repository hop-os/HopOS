//! De slaap van een RISC-V-hart in machine mode: de [`Sleeper`] van de
//! executor op de CLINT-wekker, en de klok ([`now`]) uit de TIME-CSR.
//!
//! Eén principe met de Go-generatie (Derek, 01-08): elke bewoner van een
//! hart meldt "ik doe niets tot T" langs hetzelfde pad. De kern staat al in
//! machine mode en slaapt dus direct (Go: `MSleep`, board/licheerv/clint.go);
//! een gekooide app bereikt dezelfde slaap met zijn `ecall`-yield naar de
//! switcher ([`super::switch`]).
//!
//! De slaap: MIE uit, `ready()` nog één keer, `mtimecmp` op de deadline
//! (geklemd op [`WFI_CAP_NS`]), MTIE aan, `wfi`, MTIE uit en de wekker
//! ontwapend terug, MIE zoals hij stond. `wfi` wekt op elke pending
//! interrupt met een `mie`-bit, óók met MIE uit, dus een PLIC-lijn of een
//! kick tussen de toets en de `wfi` is geen verloren wek: hij houdt de `wfi`
//! open en wordt na het openen van MIE als trap genomen.

use super::clint::{Clint, NEVER};
use super::csr;
use super::oscore::OsCore;
use crate::el2::{TURN_CAP_NS, Turn};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use executor::Sleeper;

pub use crate::idle::{Stats, ns_to_ticks, ticks_to_ns};

/// De vangrail van één slaap op de LicheeRV: 2 ms, het getal van de
/// Go-kern (`SleepCapTicks`, 2 × 25.000 tikken op de SG2002). Geen
/// zuinigheidsgetal maar een vangnet: raakt er ooit een wek zoek, dan kost
/// dat 2 ms in plaats van een hart dat nooit meer opkijkt. Op de SG2002 bleek
/// "mtimecmp is bruikbaar" iets anders dan "een wfi erop wekt betrouwbaar"
/// (01-08, boots 6 en 7).
pub const WFI_CAP_NS: u64 = 2_000_000;

/// De vangrail op QEMU virt: 10 ms, dezelfde als de WFI-slaap van arm64
/// (`cpu::idle::WFI_CAP_NS`): QEMU wekt de `wfi` op de wekker, dus de
/// vangrail hoeft daar niet krap. Gemeten 30-09: met beide grenzen ~780
/// slaaprondes per seconde (HOPOS_TICK 3 sleeps=2334), gedragen door de
/// timers van de executor (de failsafe van de switch), niet door de vangrail.
pub const WFI_CAP_QEMU_NS: u64 = 10_000_000;

/// De timebase: tikken per seconde van de TIME-CSR. RISC-V heeft geen
/// register waaruit die volgt (ARM heeft CNTFRQ_EL0); het board zet hem
/// ([`set_hz`]) uit zijn DTB of zijn datasheet. Default 10 MHz, de waarde van
/// QEMU virt; de SG2002 heeft 25 MHz.
static HZ: AtomicU64 = AtomicU64::new(10_000_000);

/// Zet de timebase. Eén keer, bij boot, vóór de eerste klok-lees.
pub fn set_hz(hz: u64) {
    if hz > 0 {
        HZ.store(hz, Relaxed);
    }
}

/// De timebase in tikken per seconde.
#[must_use]
pub fn hz() -> u64 {
    HZ.load(Relaxed)
}

/// De ruwe teller (TIME-CSR).
#[must_use]
pub fn counter() -> u64 {
    csr::rdtime()
}

/// Monotone nanoseconden: de [`executor::Clock`] van deze architectuur.
#[must_use]
pub fn now() -> u64 {
    ticks_to_ns(csr::rdtime(), hz())
}

/// De [`Sleeper`] van het hart van de kern: `wfi` op de eigen `mtimecmp`,
/// of, als het hart bewoners heeft ([`RvSleeper::host`]), een beurt voor de
/// volgende die aan de beurt is.
pub struct RvSleeper {
    clint: Option<Clint>,
    hart: usize,
    cap_ns: u64,
    os: Option<OsCore>,
    /// De meetlat.
    pub stats: Stats,
}

impl RvSleeper {
    /// Een slaper voor `hart` op `clint`; `None` = geen bewezen wekker, en
    /// dan pollt de kern op zijn deadline (spinnen kost stroom maar kan niet
    /// hangen: de goede kant om naar te falen, Go 30-07).
    #[must_use]
    pub fn new(clint: Option<Clint>, hart: usize) -> Self {
        Self {
            clint,
            hart,
            cap_ns: WFI_CAP_NS,
            os: None,
            stats: Stats::default(),
        }
    }

    /// Een andere vangrail dan [`WFI_CAP_NS`], voor een board dat bewezen
    /// heeft dat zijn wekker betrouwbaar wekt.
    #[must_use]
    pub const fn with_cap(mut self, cap_ns: u64) -> Self {
        self.cap_ns = cap_ns;
        self
    }

    /// Maakt dit hart de OS-core (PORT.md beslissing 2): vanaf de volgende
    /// idle-ronde is idle een beurt voor zijn bewoners
    /// ([`super::oscore::OsCore::run`]). Alleen op het hart van de kern zelf,
    /// en één keer. Zonder bewezen wekker kan de kern een bewoner niet
    /// terughalen op zijn deadline: dan geen rotatie, luid.
    pub fn host(&mut self, os: OsCore) {
        if self.clint.is_none() {
            crate::println!(
                "oscore: hart {} has no proven CLINT, no residents next to the kern HOPOS_OS_CORE_NONE",
                self.hart
            );
            return;
        }
        let (bits, used) = os.asid();
        crate::println!(
            "oscore: hart {} hosts residents in the idle of the kern, asid_bits={bits} ({}) HOPOS_OS_CORE_UP",
            self.hart,
            if used {
                "an ASID per resident, no TLB flush per switch"
            } else {
                "a TLB flush per switch"
            }
        );
        self.os = Some(os);
    }

    fn nap(&self, clint: Clint, now: u64, until: Option<u64>) -> u64 {
        let hz = hz();
        let cap = now.saturating_add(self.cap_ns);
        let u = until.map_or(cap, |u| u.min(cap));
        let start = csr::rdtime();
        let deadline = start.saturating_add(ns_to_ticks(u.saturating_sub(now), hz));
        clint.set_timecmp(self.hart, deadline);
        csr::mie_set(csr::MIP_MTIP);
        csr::wfi();
        csr::mie_clear(csr::MIP_MTIP);
        clint.set_timecmp(self.hart, NEVER);
        csr::rdtime().wrapping_sub(start)
    }
}

impl RvSleeper {
    /// De idle van de OS-core: een beurt voor de volgende bewoner, tot
    /// hooguit de deadline van de executor (en de vangrail van
    /// [`TURN_CAP_NS`]). `None`: er draaide er een, de kern is terug en
    /// slaapt niet. Anders de deadline waarop hij mag slapen: de zijne, of
    /// de vroegste wektijd van een bewoner als die eerder komt.
    fn resident(&mut self, now: u64, until: Option<u64>) -> Option<Option<u64>> {
        let os = self.os.as_mut()?;
        let hz = hz();
        let cap = now.saturating_add(TURN_CAP_NS);
        let u = until.map_or(cap, |u| u.min(cap));
        let deadline = csr::rdtime().saturating_add(ns_to_ticks(u.saturating_sub(now), hz));
        match os.run(deadline) {
            Turn::Ran(_) => None,
            Turn::Idle { wake: None } => Some(until),
            Turn::Idle { wake: Some(t) } => {
                let w = now.saturating_add(ticks_to_ns(t.saturating_sub(csr::rdtime()), hz));
                Some(Some(until.map_or(w, |u| u.min(w))))
            }
        }
    }
}

impl Sleeper for RvSleeper {
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        let prev = csr::mask();
        if ready() {
            csr::restore(prev);
            self.stats.caught.fetch_add(1, Relaxed);
            return;
        }
        let until = match self.os {
            Some(_) => match self.resident(now, until) {
                // Een beurt telt niet als slaap van de kern: de tijd staat
                // in de meetlat van de OS-core (`el2::OS_STATS`).
                None => {
                    csr::restore(prev);
                    return;
                }
                Some(u) => u,
            },
            None => until,
        };
        let slept = match self.clint {
            Some(c) => self.nap(c, now, until),
            None => 0,
        };
        csr::restore(prev);
        self.stats.idle_ticks.fetch_add(slept, Relaxed);
        self.stats.wakes.fetch_add(1, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dev::Pa;

    #[test]
    fn conversions_at_the_board_rates() {
        assert_eq!(ticks_to_ns(25_000_000, 25_000_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(1, 25_000_000), 40); // de 40 ns van de SG2002
        assert_eq!(ticks_to_ns(1, 10_000_000), 100); // QEMU virt
        assert_eq!(ns_to_ticks(WFI_CAP_NS, 25_000_000), 50_000); // Go's SleepCapTicks
        assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(ticks_to_ns(5, 0), 0);
    }

    #[test]
    fn sleep_arms_and_disarms_the_alarm() {
        let mut block = vec![0u32; 0x5000 / 4];
        // SAFETY: `block` is een nep-CLINT in host-geheugen.
        let c = unsafe { Clint::new(Pa(block.as_mut_ptr() as u64)) };
        let mut s = RvSleeper::new(Some(c), 0);
        s.sleep(0, Some(1000), &|| true);
        assert_eq!(s.stats.caught.load(Relaxed), 1);
        s.sleep(0, Some(1000), &|| false);
        assert_eq!(s.stats.wakes.load(Relaxed), 1);
        assert_eq!(c.timecmp(0), NEVER);
        assert_eq!(csr::mie() & csr::MIP_MTIP, 0);
    }
}
