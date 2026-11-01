//! Het beleid van de node-watchdog, één keer voor elk board
//! (`OLD/metal/cmd/hopos/watchdog.go`, met zijn policy-tests).
//!
//! In Go waren er vier kopieën gegroeid en die liepen uit elkaar (12-08
//! gezien): de Pi's en UEFI wapenden vroeg maar aaiden blind, precies de
//! aaier die de gemeten doofheid van 02-08 (nieuwe verbindingen en ICMP
//! dood, alle interne lussen kerngezond) nooit gezien zou hebben. Beleid
//! hoort één keer te bestaan; per board blijven alleen de twee
//! hardware-werkwoorden over ([`Hardware`]: wapenen, aaien).
//!
//! Het ene beleid, in twee fasen:
//!
//! 1. **De guard.** Wapenen als eerste stap van de boot, en blind aaien tot
//!    het eerste levensteken, hoogstens [`GUARD_SECS`] lang, gemeten op de
//!    rauwe teller (een klokzet van Hop mag die grens niet verschuiven).
//!    Elke boot krijgt dezelfde termijn: koud, na een flip, na een reset
//!    (O1 van 01-11, docs/sequence-hopos.md §13.2). Komt het levensteken
//!    niet, dan geen pets meer en reset de hardware: een Hop die al bij
//!    zijn eerste start valt, of een node die geen net krijgt, is een
//!    falende node (F5). Tot 01-11 aaide een koude boot hier onbegrensd
//!    blind, en had de flip een eigen tweede guard met een eigen klok.
//! 2. **Levensteken.** Vanaf het eerste bewijs dat de node leeft (de
//!    binary bepaalt wat dat is: het net op en de heartbeat van Hop die
//!    loopt) aait het beleid alleen nog op bewijs. Stopt het bewijs, dan
//!    stopt het aaien en reset de hardware de node (HOP-leven = node-leven).
//!    Een verloren adres ([`Policy::request_reset`]) houdt de pets voorgoed
//!    in: de stack kan niet van adres wisselen, dus een koude boot is de
//!    weg terug.
//!
//! Dit is rekenwerk zonder klok en zonder ijzer: de binary geeft de
//! tellerstand en de gezondheid, en logt de [`Event`]s die terugkomen.

/// De termijn van het eerste levensteken in seconden, voor elke boot.
/// GEMETEN 06-09 op de M4: een mislukte flip zonder grens liet de node
/// zeven minuten donker in plaats van binnen 30 s te resetten. Twee
/// minuten dekt een DHCP-sessie en de start van Hop ruim (§13.9 vraag 1).
pub const GUARD_SECS: u64 = 120;

/// De hardware-helft die een board levert. Het beleid roept `arm` één keer
/// en `pet` elke aai-ronde.
pub trait Hardware {
    /// Wapent de watchdog; `false` = onbruikbaar (de binary kent de reden).
    fn arm(&mut self) -> bool;
    /// Laadt de teller opnieuw.
    fn pet(&mut self);
}

/// In welke fase het beleid staat.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nog niet gestart (of geen hardware).
    Idle,
    /// Gewapend, blind aaien tot het eerste levensteken.
    Guard,
    /// Aaien op bewijs.
    Live,
    /// Aaien ingehouden: de hardware reset de node.
    Withheld,
}

/// Wat het beleid meldt; de binary maakt er één consoleregel met marker van.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// De hardware liet zich niet wapenen: deze boot is onbewaakt.
    Unguarded,
    /// Gewapend; blind aaien tot het levensteken, hoogstens [`GUARD_SECS`].
    Armed,
    /// De guard liep af vóór het levensteken: geen pets meer
    /// (`HOPOS_BOOT_GUARD_EXPIRED`).
    GuardExpired,
    /// Het levensteken kwam: vanaf nu aaien op bewijs
    /// (`HOPOS_CANARY_LIVE`).
    Live,
    /// Het bewijs ontbrak zoveel rondes op rij: pet ingehouden
    /// (`HOPOS_CANARY_MISS`).
    Miss(u64),
    /// Een reset is gevraagd: pets ingehouden (`HOPOS_RESET_REQUESTED`).
    ResetRequested(&'static str),
}

/// Het beleid. Eén eigenaar: de watchdog-taak van de binary.
#[derive(Debug)]
pub struct Policy {
    /// De rauwe tellerstand waarop de guard afloopt.
    deadline: u64,
    phase: Phase,
    misses: u64,
    reset: Option<&'static str>,
}

impl Policy {
    /// Wapent de watchdog en zet de guard: [`GUARD_SECS`] vanaf `counter`
    /// op de rauwe teller (`hz` tikken per seconde).
    pub fn start(hw: &mut impl Hardware, counter: u64, hz: u64) -> (Policy, Event) {
        let mut p = Policy {
            deadline: counter.saturating_add(GUARD_SECS.saturating_mul(hz)),
            phase: Phase::Idle,
            misses: 0,
            reset: None,
        };
        if !hw.arm() {
            return (p, Event::Unguarded);
        }
        p.phase = Phase::Guard;
        (p, Event::Armed)
    }

    /// De fase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Een reset gevraagd (een verloren adres): de volgende ronde houdt de
    /// pets voorgoed in.
    pub fn request_reset(&mut self, reason: &'static str) {
        self.reset.get_or_insert(reason);
    }

    /// Eén aai-ronde: `alive` is het bewijs van deze ronde.
    pub fn tick(&mut self, hw: &mut impl Hardware, counter: u64, alive: bool) -> Option<Event> {
        match self.phase {
            Phase::Idle | Phase::Withheld => return None,
            Phase::Guard | Phase::Live => {}
        }
        if let Some(r) = self.reset {
            self.phase = Phase::Withheld;
            return Some(Event::ResetRequested(r));
        }
        if self.phase == Phase::Guard {
            if alive {
                self.phase = Phase::Live;
                hw.pet();
                return Some(Event::Live);
            }
            if counter >= self.deadline {
                self.phase = Phase::Withheld;
                return Some(Event::GuardExpired);
            }
            hw.pet();
            return None;
        }
        if alive {
            hw.pet();
            self.misses = 0;
            return None;
        }
        self.misses += 1;
        Some(Event::Miss(self.misses))
    }
}

#[cfg(test)]
mod tests {
    //! De policy-tests uit `watchdog_policy_test.go`, naar het ene beleid
    //! van 01-11.
    use super::*;

    #[derive(Default)]
    struct Fake {
        pets: u32,
        arms: u32,
        refuse: bool,
    }

    impl Hardware for Fake {
        fn arm(&mut self) -> bool {
            self.arms += 1;
            !self.refuse
        }
        fn pet(&mut self) {
            self.pets += 1;
        }
    }

    /// Elke boot, ook een koude, krijgt de termijn: blind tot de deadline,
    /// daarna geen pet meer, ook niet als het bewijs later toch komt (F5).
    #[test]
    fn every_boot_has_one_guard() {
        let mut hw = Fake::default();
        let (mut p, e) = Policy::start(&mut hw, 1_000, 10);
        assert_eq!(e, Event::Armed);
        let deadline = 1_000 + GUARD_SECS * 10;
        assert_eq!(p.tick(&mut hw, deadline - 1, false), None);
        assert_eq!(hw.pets, 1, "blind pet inside the guard");
        assert_eq!(p.tick(&mut hw, deadline, false), Some(Event::GuardExpired));
        assert_eq!(p.phase(), Phase::Withheld);
        assert_eq!(p.tick(&mut hw, deadline + 1, true), None);
        assert_eq!((hw.arms, hw.pets), (1, 1), "pets after the guard expired");
    }

    /// `TestBootGuardUsesRawCounterDespiteClockEpochChange`: de deadline
    /// staat op de rauwe teller, een klokzet van Hop raakt hem niet.
    #[test]
    fn guard_uses_the_raw_counter() {
        let mut hw = Fake::default();
        let hz = 24_000_000;
        let (mut p, _) = Policy::start(&mut hw, hz, hz);
        let want = hz + GUARD_SECS * hz;
        let epoch_offset: u64 = 1_788_652_800_000_000_000; // 2026-09-06 in ns
        assert!(epoch_offset > want, "invalid clock-shift fixture");
        assert_eq!(p.tick(&mut hw, want - 1, false), None);
        assert_eq!(p.tick(&mut hw, want, false), Some(Event::GuardExpired));
        assert_eq!(hw.pets, 1);
    }

    #[test]
    fn liveness_then_misses_then_a_requested_reset() {
        let mut hw = Fake::default();
        let (mut p, e) = Policy::start(&mut hw, 1, 1);
        assert_eq!(e, Event::Armed);
        assert_eq!(p.tick(&mut hw, 2, false), None);
        assert_eq!(p.tick(&mut hw, 3, false), None);
        assert_eq!(hw.pets, 2, "blind pets in the guard");
        assert_eq!(p.tick(&mut hw, 5, true), Some(Event::Live));
        // Na het levensteken telt de deadline van de guard niet meer.
        assert_eq!(p.tick(&mut hw, 1 << 40, false), Some(Event::Miss(1)));
        assert_eq!(p.tick(&mut hw, 1 << 40, false), Some(Event::Miss(2)));
        assert_eq!(hw.pets, 3, "a miss withholds the pet");
        assert_eq!(p.tick(&mut hw, 1 << 40, true), None);
        assert_eq!(hw.pets, 4);
        p.request_reset("address lost");
        assert_eq!(
            p.tick(&mut hw, 1 << 40, true),
            Some(Event::ResetRequested("address lost"))
        );
        assert_eq!(p.tick(&mut hw, 1 << 40, true), None);
        assert_eq!(hw.pets, 4, "pets after a reset request");
    }

    #[test]
    fn no_hardware_is_unguarded() {
        let mut hw = Fake {
            refuse: true,
            ..Fake::default()
        };
        let (mut p, e) = Policy::start(&mut hw, 1, 10);
        assert_eq!(e, Event::Unguarded);
        assert_eq!(p.tick(&mut hw, 3, true), None);
        assert_eq!((hw.arms, hw.pets), (1, 0));
    }
}
