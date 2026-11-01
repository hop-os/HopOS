//! Het interrupt-contract van HopOS: een lijn, een controller, en de
//! dispatcher die ze verbindt.
//!
//! Dit bezit: de tabel lijn-naar-wachter (per lijn de device-ack en de bel
//! van de wachter) en de dispatcher die claimt, het device ackt, de bel
//! luidt en completeert. Niet van hier: de controllers zelf (driver/gicv3,
//! driver/gicv2, driver/aic, de PLIC in [`crate::riscv::plic`]) en de vector
//! (het boot-spoor), die alleen [`on_irq`] roept.
//!
//! Tot 02-09 had HopOS géén interrupt-afhandeling: DAIF gemaskeerd, alles
//! gepold, en HOP's RX-lus sliep 300 µs per ronde, ruim 3.000 wekmomenten
//! per seconde op een node die niets doet en 300 µs latency op élk
//! app-pakket. Het model sindsdien, en hier in Rust-vorm: de vector zet een
//! vlag en keert terug met I gemaskeerd; de dispatch-taak van de kern
//! (`Board::dispatch_interrupts`) ziet de vlag, laat [`Dispatcher::dispatch`]
//! claimen tot de controller niets meer heeft, en opent I weer. Geen logica
//! in exception-context (handboek §5): op de Ampere gaf runtime-code in de
//! vector een stille hang binnen seconden (19-09), op de M4 een verloren
//! heropening van het I-masker onder load (21-09).
//!
//! Twee regels, beide isolatie:
//!
//! - Interrupts zijn uitsluitend HOP-werk. Een app-core wordt nooit een
//!   target van de controller en houdt zijn maskers dicht; een app heeft
//!   geen lijn en geen controller.
//! - Elke wacht op een bel heeft één termijn (docs/sequence-hopos.md §13.1
//!   R2): geen vangrail ernaast. Een verloren wek is een fout die we
//!   repareren, niet afdekken.

use core::cell::{Cell, RefCell};
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use sync::{Local, Signal};

/// Hoeveel lijnen er tegelijk een wachter kunnen hebben. HOP bedient een
/// handvol devices (NIC, UART, NVMe); zestien is ruim.
pub const MAX_LINES: usize = 16;

/// Hoe vaak één lijn binnen één dispatch-ronde mag terugkomen voordat de
/// ronde wordt afgebroken.
///
/// Een lijn die binnen één ronde blijft terugkomen is een level-bron die
/// niemand laat zakken, of een NIC onder last. Zonder grens spint de
/// dispatcher voor eeuwig in claim, EOI, claim met I gemaskeerd: de
/// O6N-"freeze bij de eerste wachttijd" (17-09/18-09), geen pets meer,
/// watchdog na 12 s. Ruim boven wat een NIC-burst legitiem doet (een ack
/// per claim laat de lijn zakken), ver onder "voor eeuwig".
pub const STRAY_LIMIT: u32 = 256;

/// Eén interruptlijn zoals de controller hem nummert (GICv3: INTID, SPI n =
/// 32+n; AIC: het hw-nummer; PLIC: de source-id).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Line(pub u32);

/// Hoe een lijn afgaat: het board kiest, per lijn, bij [`enable_as`].
///
/// Waarom dit bij de registratie hoort en niet los bij de controller: een
/// MSI die via een brug een SPI wordt (de MIP van de Pi 5) is een flank,
/// een SPI staat in de GIC standaard op level, en een level-lijn achter
/// een puls blijft staan tot iemand hem laat zakken. Staat de soort in de
/// registratie, dan zet de dispatcher hem vóór de enable (de GIC wil dat
/// zo, IHI 0048B 4.3.13) en weet hij ook wat een lijn die blijft
/// terugkomen betekent: bij level zonder ack laat niemand hem ooit zakken
/// ([`Dispatcher::dispatch`]). Les van 30-09 (de eerste Pi 5-boot).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Trigger {
    /// Level: de lijn staat zolang het device hem vasthoudt. De
    /// standaard van een SPI en van de meeste devices.
    #[default]
    Level,
    /// Flank: elke puls is één interrupt (een MSI).
    Edge,
}

/// De device-kant van de bevestiging: wat het device nodig heeft om zijn
/// lijn weer los te laten (virtio: InterruptACK), gedaan vóór de wachter
/// gewekt wordt. Een kale functie: wat hij nodig heeft (de ack-waarde van
/// de driver) leest hij uit een `static` van het board.
pub type Ack = fn();

/// Wat een interrupt-controller moet kunnen. Vier werkwoorden en een
/// instelling, en niets over prioriteiten of groepen: die zijn van de
/// driver.
///
/// De methoden nemen `&self`: een controller is een registerblok, en
/// MMIO-registers zijn per definitie gedeeld (`dev::Reg`).
pub trait Controller {
    /// Zet de soort van de lijn, vóór [`Controller::enable`]. Een
    /// controller die dat niet per lijn kan (de AIC, de PLIC: hun lijnen
    /// hebben één vaste soort) laat de standaard staan: niets te doen.
    fn set_trigger(&self, l: Line, t: Trigger) -> Result<(), Error> {
        let _ = (l, t);
        Ok(())
    }
    /// Maakt de lijn scherp én routeert hem naar de aanroepende core.
    fn enable(&self, l: Line) -> Result<(), Error>;
    /// Zet de lijn uit.
    fn disable(&self, l: Line);
    /// De lijn die vuurde (en bevestigt hem bij de controller); `None` =
    /// niets (meer) te claimen.
    fn claim(&self) -> Option<Line>;
    /// Sluit de afhandeling van `l` af (EOI, waar dat los van claim is).
    fn complete(&self, l: Line);
    /// Sluit de afhandeling van `l` af en houdt hem dicht tot
    /// [`Dispatcher::release`]: een level-lijn die alleen de driver laat
    /// zakken (Linux `IRQF_ONESHOT`). Standaard uit en dan de EOI; een
    /// controller die bij de claim al maskeert (de AIC), doet niets.
    fn hold(&self, l: Line) {
        self.disable(l);
        self.complete(l);
    }
}

/// Waarom een lijn niet scherp kon.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Er is (nog) geen controller geregistreerd.
    NoController,
    /// Alle [`MAX_LINES`] plaatsen zijn bezet.
    Full {
        /// De lijn die geen plaats kreeg.
        line: u32,
    },
    /// De controller kent deze lijn niet of weigert hem.
    Rejected {
        /// De lijn.
        line: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NoController => f.write_str("irq: no interrupt controller on this board"),
            Self::Full { line } => {
                write!(f, "irq: no room for INTID {line} (max {MAX_LINES} lines)")
            }
            Self::Rejected { line } => write!(f, "irq: controller rejected INTID {line}"),
        }
    }
}

/// De meetlat van de dispatcher.
#[derive(Default, Debug)]
pub struct Stats {
    /// Geclaimde interrupts, alle lijnen. Zonder dit getal is "de RX-lus
    /// wordt wakker" niet te onderscheiden van "de vangrail van `wait`
    /// liep af": dít zegt of het silicium sprak.
    pub fired: AtomicU64,
    /// Dispatch-rondes.
    pub passes: AtomicU64,
    /// Rondes afgebroken op [`STRAY_LIMIT`].
    pub stray_passes: AtomicU64,
    /// Onbekende lijnen die uitgezet zijn.
    pub unknown: AtomicU64,
    /// Level-lijnen zonder device-ack die uitgezet zijn omdat ze binnen één
    /// ronde bleven terugkomen.
    pub stuck: AtomicU64,
}

/// Wat één dispatch-ronde zag; het board telt er zijn `Dispatched` uit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Aantal claims in deze ronde, alle lijnen.
    pub claimed: u32,
    /// Claims van lijnen zonder wachter (elk uitgezet).
    pub unknown: u32,
    /// Een lijn zonder wachter die vuurde en is uitgezet (de laatste).
    pub disabled: Option<Line>,
    /// De lijn waarop de ronde werd afgebroken ([`STRAY_LIMIT`]).
    pub stray: Option<Line>,
    /// Een level-lijn zonder device-ack die op [`STRAY_LIMIT`] uitgezet is.
    pub stuck: Option<Line>,
    /// Claims per geregistreerde lijn (INTID, aantal), op de plek van de
    /// tabel.
    seen: [(u32, u32); MAX_LINES],
}

impl Pass {
    /// Hoe vaak lijn `l` in deze ronde geclaimd werd (0 voor een lijn
    /// zonder wachter: die telt in [`Pass::unknown`]).
    #[must_use]
    pub fn claims(&self, l: Line) -> u32 {
        self.seen
            .iter()
            .filter(|&&(id, _)| id == l.0)
            .map(|&(_, n)| n)
            .sum()
    }
}

#[derive(Copy, Clone)]
struct Entry {
    line: Line,
    ack: Option<Ack>,
    bell: Option<&'static Signal>,
    trigger: Trigger,
    /// Dicht na elke claim tot [`Dispatcher::release`] ([`Controller::hold`]).
    held: bool,
}

/// De dispatcher: de tabel lijn-naar-wachter.
///
/// Eén per systeem, op HOP's core; [`global`] is die ene. Hij woont in een
/// [`Local`]: alleen taken op HOP's executor raken hem aan, nooit een ISR
/// (de vector raakt alleen [`IRQ_PENDING`], een kale atomic).
pub struct Dispatcher {
    ctrl: Cell<Option<&'static dyn Controller>>,
    lines: RefCell<[Option<Entry>; MAX_LINES]>,
    /// De meetlat.
    pub stats: Stats,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    /// Een dispatcher zonder controller en zonder lijnen.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ctrl: Cell::new(None),
            lines: RefCell::new([None; MAX_LINES]),
            stats: Stats {
                fired: AtomicU64::new(0),
                passes: AtomicU64::new(0),
                stray_passes: AtomicU64::new(0),
                unknown: AtomicU64::new(0),
                stuck: AtomicU64::new(0),
            },
        }
    }

    /// Registreert de controller. Eén keer, vanuit de board-bedrading, ná
    /// de controller-init en vóór de eerste `enable`.
    pub fn use_controller(&self, c: &'static dyn Controller) {
        self.ctrl.set(Some(c));
    }

    /// Is er een controller?
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ctrl.get().is_some()
    }

    /// Registreert `l` als level-wek-doel (met de device-ack en de bel van
    /// de wachter) en maakt hem scherp bij de controller.
    pub fn enable(
        &self,
        l: Line,
        ack: Option<Ack>,
        bell: Option<&'static Signal>,
    ) -> Result<(), Error> {
        self.enable_as(l, Trigger::Level, ack, bell)
    }

    /// Registreert `l` als wek-doel van soort `trigger` (met de
    /// device-ack en de bel van de wachter), zet de soort bij de controller
    /// en maakt hem dan pas scherp. Een lijn zonder bel (een timer, de kick)
    /// heeft alleen zijn ack.
    pub fn enable_as(
        &self,
        l: Line,
        trigger: Trigger,
        ack: Option<Ack>,
        bell: Option<&'static Signal>,
    ) -> Result<(), Error> {
        self.register(l, trigger, ack, bell, false)
    }

    /// Registreert level-lijn `l` met de bel van zijn wachter en zonder
    /// device-ack: na elke claim blijft hij dicht ([`Controller::hold`]) tot
    /// de driver hem bediende en [`release`](Self::release) roept. Voor een
    /// device dat alleen zijn driver kan laten zakken (de ANS en zijn
    /// mailbox op de M4).
    pub fn enable_held(&self, l: Line, bell: &'static Signal) -> Result<(), Error> {
        self.register(l, Trigger::Level, None, Some(bell), true)
    }

    /// Zet een vastgehouden lijn ([`enable_held`](Self::enable_held)) weer
    /// open: de driver bediende hem. Staat hij nog, dan vuurt hij meteen
    /// opnieuw. Een lijn die niet vastgehouden wordt, laat hij staan.
    pub fn release(&self, l: Line) {
        if let (Some(ctrl), Some((_, e))) = (self.ctrl.get(), self.entry(l))
            && e.held
        {
            let _ = ctrl.enable(l);
        }
    }

    fn register(
        &self,
        l: Line,
        trigger: Trigger,
        ack: Option<Ack>,
        bell: Option<&'static Signal>,
        held: bool,
    ) -> Result<(), Error> {
        let ctrl = self.ctrl.get().ok_or(Error::NoController)?;
        {
            let mut lines = self.lines.borrow_mut();
            let pos = lines
                .iter()
                .position(|e| e.is_some_and(|e| e.line == l))
                .or_else(|| lines.iter().position(Option::is_none))
                .ok_or(Error::Full { line: l.0 })?;
            if let Some(slot) = lines.get_mut(pos) {
                *slot = Some(Entry {
                    line: l,
                    ack,
                    bell,
                    trigger,
                    held,
                });
            }
        }
        ctrl.set_trigger(l, trigger)?;
        ctrl.enable(l)
    }

    fn entry(&self, l: Line) -> Option<(usize, Entry)> {
        self.lines
            .borrow()
            .iter()
            .enumerate()
            .find_map(|(i, e)| e.filter(|e| e.line == l).map(|e| (i, e)))
    }

    /// Eén ronde: alle gevuurde lijnen claimen, per lijn het device acken,
    /// de bel luiden en de lijn completeren (of vasthouden).
    ///
    /// Onbekend = meteen uit (een lijn die de firmware aan liet staan:
    /// UEFI-timer, UART, een watchdog-waarschuwing). Een level-lijn zonder
    /// device-ack die [`STRAY_LIMIT`] keer terugkomt, laat niemand ooit
    /// zakken: uit (`pass.stuck`); zijn wachter hoort dan alleen zijn
    /// termijn, liever dat dan een dispatcher die de rotatie van de OS-core
    /// voor eeuwig onderbreekt (de vrees van 30-09 bij de eerste Pi
    /// 5-boot). Bekend met een ack, of een flank, maar
    /// blijvend = de ronde afbreken na [`STRAY_LIMIT`], en de lijn NIET
    /// uitzetten: een
    /// bediende lijn die blijft terugkomen is een NIC onder last (tg3: een
    /// status-update per frame houdt de lijn bij 25k frames/s praktisch
    /// continu hoog). Die voorgoed uitzetten was de M4-dood van 20-09: ná
    /// een pull 0 interrupts/s, elke frame op de failsafe van 10 ms, 118
    /// naar 45 MB/s, en de O6N kreeg de schuld (bundels 47-55). Afbreken
    /// laat de pomp en de wachter aan de beurt; de device-ack houdt de lijn
    /// intussen laag.
    ///
    /// De console hoort het zoals bij Linux (`note_interrupt`: "nobody
    /// cared", "Disabling IRQ"), en nooit per ronde: een lijn die uitgaat
    /// altijd (`HOPOS_IRQ_STUCK`, één keer per lijn want hij is daarna uit),
    /// de eerste storm die de lijn aan laat (`HOPOS_IRQ_STORM`; de rest telt
    /// in [`Stats::stray_passes`]) en de eerste onbekende lijn (de rest telt
    /// in [`Stats::unknown`]).
    pub fn dispatch(&self) -> Pass {
        let mut pass = Pass::default();
        let Some(ctrl) = self.ctrl.get() else {
            return pass;
        };
        self.stats.passes.fetch_add(1, Relaxed);
        while let Some(l) = ctrl.claim() {
            pass.claimed = pass.claimed.saturating_add(1);
            self.stats.fired.fetch_add(1, Relaxed);
            let Some((i, e)) = self.entry(l) else {
                ctrl.disable(l);
                ctrl.complete(l);
                if self.stats.unknown.fetch_add(1, Relaxed) == 0 {
                    crate::println!(
                        "irq: INTID {} fired but nobody serves it (left enabled by the firmware?), line disabled",
                        l.0
                    );
                }
                pass.unknown = pass.unknown.saturating_add(1);
                pass.disabled = Some(l);
                continue;
            };
            if let Some(ack) = e.ack {
                ack();
            }
            // Al gewekt en nog niet opgehaald: één is genoeg (level).
            if let Some(b) = e.bell {
                b.set();
            }
            if e.held {
                ctrl.hold(l);
            } else {
                ctrl.complete(l);
            }
            let Some(seen) = pass.seen.get_mut(i) else {
                continue;
            };
            *seen = (l.0, seen.1.saturating_add(1));
            if seen.1 > STRAY_LIMIT {
                // De eerste storm die de lijn aan laat: alle eerdere waren
                // lijnen die uitgingen.
                let first =
                    self.stats.stray_passes.fetch_add(1, Relaxed) == self.stats.stuck.load(Relaxed);
                pass.stray = Some(l);
                if e.trigger == Trigger::Level && e.ack.is_none() {
                    ctrl.disable(l);
                    self.stats.stuck.fetch_add(1, Relaxed);
                    pass.stuck = Some(l);
                    crate::println!(
                        "irq: INTID {} came back {STRAY_LIMIT} times in one pass, a level line without a device ack that nobody lowers: line disabled HOPOS_IRQ_STUCK",
                        l.0
                    );
                } else if first {
                    crate::println!(
                        "irq: INTID {} came back {STRAY_LIMIT} times in one pass, pass ended, line stays enabled HOPOS_IRQ_STORM",
                        l.0
                    );
                }
                break;
            }
        }
        pass
    }
}

/// De vlag van de vector: gezet in exception-context, gewacht door de
/// dispatch-taak van de kern (de bel van `Board::start_interrupts`).
///
/// Een kale `Signal` (atomic plus waker) buiten elke `Local`, want de ISR
/// raakt nooit een `Local` (handboek §1.1).
pub static IRQ_PENDING: Signal = Signal::new();

/// De ingang voor de IRQ-vector van HOP's core: zet de vlag en wekt de
/// dispatcher. Verder niets: geen claim, geen allocatie, geen logica.
///
/// De vector keert daarna terug met I gemaskeerd (SPSR_EL2.I gezet, de
/// kern draait op EL2; op riscv blijft de `mie`-bit van de bron dicht): de
/// lijn staat nog tot de dispatcher hem claimt, en met I open werd dat een
/// storm. De dispatch-taak opent I weer na zijn ronde ([`unmask`]).
pub fn on_irq() {
    IRQ_PENDING.set();
}

static GLOBAL: Local<Dispatcher> = Local::new(Dispatcher::new());

/// De dispatcher van dit systeem.
#[must_use]
pub fn global() -> &'static Dispatcher {
    GLOBAL.get()
}

/// Registreert de controller bij [`global`].
pub fn use_controller(c: &'static dyn Controller) {
    global().use_controller(c);
}

/// Maakt level-lijn `l` scherp met optionele device-ack en bel
/// ([`Dispatcher::enable`]).
pub fn enable(l: Line, ack: Option<Ack>, bell: Option<&'static Signal>) -> Result<(), Error> {
    global().enable(l, ack, bell)
}

/// Maakt level-lijn `l` scherp met de bel van zijn wachter, dicht na elke
/// claim tot [`release`] ([`Dispatcher::enable_held`]).
pub fn enable_held(l: Line, bell: &'static Signal) -> Result<(), Error> {
    global().enable_held(l, bell)
}

/// Zet vastgehouden lijn `l` weer open ([`Dispatcher::release`]).
pub fn release(l: Line) {
    global().release(l);
}

/// Maakt lijn `l` van soort `trigger` scherp met optionele device-ack en
/// bel ([`Dispatcher::enable_as`]).
pub fn enable_as(
    l: Line,
    trigger: Trigger,
    ack: Option<Ack>,
    bell: Option<&'static Signal>,
) -> Result<(), Error> {
    global().enable_as(l, trigger, ack, bell)
}

/// Opent I (DAIFClr #2) op deze core: het einde van elke dispatch-ronde (de
/// vector keert gemaskeerd terug).
pub fn unmask() {
    arch::unmask_irq();
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    /// Opent I (DAIFClr #2); F blijft zoals hij stond. Op Apple is F de
    /// weg van de timer-FIQ die WFI wekt zonder ooit genomen te worden.
    pub(super) fn unmask_irq() {
        // SAFETY: raakt alleen het I-masker van deze core. Geen `nomem`:
        // geheugentoegang mag niet over het openen heen schuiven.
        unsafe { core::arch::asm!("msr daifclr, #2", options(nostack)) };
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Host-stub: er is geen masker.
    pub(super) fn unmask_irq() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::vec::Vec;

    /// Een controller met een rij pending lijnen en een logboek.
    #[derive(Default)]
    struct Fake {
        pending: RefCell<Vec<u32>>,
        log: RefCell<Vec<(char, u32)>>,
        refuse: Option<u32>,
    }

    impl Controller for Fake {
        fn set_trigger(&self, l: Line, t: Trigger) -> Result<(), Error> {
            if t == Trigger::Edge {
                self.log.borrow_mut().push(('f', l.0));
            }
            Ok(())
        }
        fn enable(&self, l: Line) -> Result<(), Error> {
            if self.refuse == Some(l.0) {
                return Err(Error::Rejected { line: l.0 });
            }
            self.log.borrow_mut().push(('e', l.0));
            Ok(())
        }
        fn disable(&self, l: Line) {
            self.log.borrow_mut().push(('d', l.0));
        }
        fn claim(&self) -> Option<Line> {
            let mut p = self.pending.borrow_mut();
            if p.is_empty() {
                None
            } else {
                Some(Line(p.remove(0)))
            }
        }
        fn complete(&self, l: Line) {
            self.log.borrow_mut().push(('c', l.0));
        }
    }

    fn setup(fake: Fake) -> (&'static Dispatcher, &'static Fake) {
        let d: &'static Dispatcher = Box::leak(Box::new(Dispatcher::new()));
        let f: &'static Fake = Box::leak(Box::new(fake));
        d.use_controller(f);
        (d, f)
    }

    fn bell() -> &'static Signal {
        Box::leak(Box::new(Signal::new()))
    }

    #[test]
    fn a_held_line_stays_closed_until_its_driver_releases_it() {
        let (d, f) = setup(Fake::default());
        let s = bell();
        d.enable_held(Line(590), s).unwrap();
        d.release(Line(77));
        f.pending.borrow_mut().push(590);
        let pass = d.dispatch();
        assert_eq!(pass.claims(Line(590)), 1);
        assert!(s.take());
        d.release(Line(590));
        assert_eq!(
            *f.log.borrow(),
            [('e', 590), ('d', 590), ('c', 590), ('e', 590)],
            "dicht en EOI bij de claim, open bij de release"
        );
    }

    #[test]
    fn enable_without_controller_fails() {
        let d: &'static Dispatcher = Box::leak(Box::new(Dispatcher::new()));
        assert_eq!(
            d.enable(Line(33), None, None).err(),
            Some(Error::NoController)
        );
    }

    #[test]
    fn known_line_acks_rings_and_completes() {
        static ACKS: AtomicU32 = AtomicU32::new(0);
        let (d, f) = setup(Fake::default());
        let s = bell();
        d.enable(
            Line(79),
            Some(|| {
                ACKS.fetch_add(1, SeqCst);
            }),
            Some(s),
        )
        .unwrap();
        assert!(!s.is_set());
        f.pending.borrow_mut().extend([79, 79]);
        let pass = d.dispatch();
        assert_eq!(pass.claimed, 2);
        assert_eq!(pass.claims(Line(79)), 2);
        assert_eq!((pass.unknown, pass.disabled), (0, None));
        assert_eq!(ACKS.load(SeqCst), 2);
        assert!(s.take()); // twee vuren, één wek: level, samengevoegd
        assert_eq!(*f.log.borrow(), [('e', 79), ('c', 79), ('c', 79)]);
        assert_eq!(d.stats.fired.load(SeqCst), 2);
    }

    // Een lijn zonder bel (een timer, de kick) heeft alleen zijn ack; de
    // ronde telt per lijn, zodat het board timer, NIC en de rest uit één
    // ronde haalt.
    #[test]
    fn a_line_without_a_bell_and_the_count_per_line() {
        static TIMER: AtomicU32 = AtomicU32::new(0);
        let (d, f) = setup(Fake::default());
        d.enable(
            Line(30),
            Some(|| {
                TIMER.fetch_add(1, SeqCst);
            }),
            None,
        )
        .unwrap();
        let nic = bell();
        d.enable(Line(79), Some(|| {}), Some(nic)).unwrap();
        f.pending.borrow_mut().extend([30, 79, 27, 79]);
        let pass = d.dispatch();
        assert_eq!(TIMER.load(SeqCst), 1);
        assert_eq!(
            (
                pass.claimed,
                pass.claims(Line(30)),
                pass.claims(Line(79)),
                pass.unknown
            ),
            (4, 1, 2, 1)
        );
        assert_eq!(pass.claims(Line(27)), 0);
        assert!(nic.is_set());
    }

    #[test]
    fn unknown_line_is_disabled_once() {
        let (d, f) = setup(Fake::default());
        f.pending.borrow_mut().push(27);
        let pass = d.dispatch();
        assert_eq!(pass.disabled, Some(Line(27)));
        assert_eq!(pass.unknown, 1);
        assert_eq!(*f.log.borrow(), [('d', 27), ('c', 27)]);
        assert_eq!(d.stats.unknown.load(SeqCst), 1);
    }

    #[test]
    fn stray_line_ends_the_pass_but_stays_enabled() {
        let (d, f) = setup(Fake::default());
        // Een NIC onder last: bediend (een ack per claim), dus nooit uit.
        let s = bell();
        d.enable(Line(40), Some(|| {}), Some(s)).unwrap();
        f.pending
            .borrow_mut()
            .extend(core::iter::repeat_n(40, STRAY_LIMIT as usize + 10));
        let pass = d.dispatch();
        assert_eq!(pass.stray, Some(Line(40)));
        assert_eq!(pass.claimed, STRAY_LIMIT + 1);
        assert_eq!(pass.claims(Line(40)), STRAY_LIMIT + 1);
        assert!(s.is_set());
        assert!(!f.log.borrow().iter().any(|&(op, _)| op == 'd'));
        // De rest blijft pending voor de volgende ronde.
        assert_eq!(f.pending.borrow().len(), 9);
    }

    // Een level-lijn die niemand laat zakken (geen device-ack) komt binnen
    // één ronde STRAY_LIMIT keer terug: uit, en de ronde meldt hem. Een
    // flank zonder ack die even hard komt, is verkeer en blijft aan; de
    // soort gaat vóór de enable naar de controller.
    #[test]
    fn stuck_level_line_without_ack_is_disabled_an_edge_is_not() {
        let (d, f) = setup(Fake::default());
        let s = bell();
        d.enable(Line(41), None, Some(s)).unwrap();
        f.pending
            .borrow_mut()
            .extend(core::iter::repeat_n(41, STRAY_LIMIT as usize + 10));
        let pass = d.dispatch();
        assert_eq!(pass.stuck, Some(Line(41)));
        assert_eq!(pass.stray, Some(Line(41)));
        assert!(s.is_set());
        assert!(f.log.borrow().contains(&('d', 41)));
        assert_eq!(d.stats.stuck.load(SeqCst), 1);

        let (d, f) = setup(Fake::default());
        d.enable_as(Line(166), Trigger::Edge, None, None).unwrap();
        assert_eq!(f.log.borrow()[..2], [('f', 166), ('e', 166)]);
        f.pending
            .borrow_mut()
            .extend(core::iter::repeat_n(166, STRAY_LIMIT as usize + 10));
        let pass = d.dispatch();
        assert_eq!(pass.stray, Some(Line(166)));
        assert_eq!(pass.stuck, None);
        assert!(!f.log.borrow().iter().any(|&(op, _)| op == 'd'));
    }

    #[test]
    fn refused_line_and_full_table() {
        let (d, _) = setup(Fake {
            refuse: Some(5),
            ..Fake::default()
        });
        assert_eq!(
            d.enable(Line(5), None, None).err(),
            Some(Error::Rejected { line: 5 })
        );
        let (d, _) = setup(Fake::default());
        for i in 0..MAX_LINES as u32 {
            d.enable(Line(100 + i), None, None).unwrap();
        }
        assert_eq!(
            d.enable(Line(7), None, None).err(),
            Some(Error::Full { line: 7 })
        );
        // Opnieuw een bestaande lijn is een her-registratie, geen nieuwe plaats.
        assert!(d.enable(Line(100), None, None).is_ok());
    }
}
