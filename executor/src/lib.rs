//! Eén executor per core (handboek §4).
//!
//! De executor is de governor uit de Go-kern, maar dan als voordeur: hij
//! bezit de takenlijst en het timerwiel, en als er niets te doen is vraagt
//! hij het board te slapen tot de vroegste deadline of tot een wek. Een
//! wek is één bit (in [`Executor::ready`], het bit van zijn slot) plus
//! `dev::notify`, en dat mag uit een ISR of van een andere core komen.
//!
//! Een ronde kost wat er klaar staat, niet de grootte van de tabel: de
//! bits staan in acht woorden, en een ronde leest die en pollt alleen wat
//! erin staat (de ready-bitmap van een RTOS-scheduler, Linux'
//! `sched_find_first_bit`); de timerscans stoppen bij de hoogste bezette
//! plaats. Tot 03-10 vroeg elke ronde 512 slots af met een atomaire swap,
//! elke slaap 512 met een acquire-load (op RISC-V een `fence` per slot) en
//! elke ronde 256 timerplaatsen: op de OS-core, waar de kern tussen elke
//! twee beurten van zijn bewoners rondes draait, was dat twee derde van een
//! rtt tussen twee bewoners (QEMU virt met `-icount shift=0`, één instructie
//! per ns: riscv64 359 naar 108 us, arm64 252 naar 85 us).
//!
//! De ronde ([`Executor::step`]): eerst de gespawnde taken een plaats
//! geven, dan de timers die verstreken zijn wekken, dan alle taken met een
//! gezet bit pollen, en als er in die ronde niets gebeurd is slapen.
//! De verloren-wek-race is dicht doordat de [`Sleeper`] het `ready`-
//! predicaat nog één keer toetst mét interrupts gemaskeerd.
//!
//! Een taak is een future in een box op de heap ([`Executor::spawn`]; een
//! volle heap is [`SpawnError::OutOfMemory`]), in een vaste tabel van
//! `TASKS` plaatsen. `spawn` zet hem in een rij; de volgende ronde geeft
//! hem een plaats, en is de tabel dan vol, dan gaat hij weg en telt
//! [`Stats::dropped`] (de kern zegt dat op zijn tik, `HOPOS_TASK_DROPPED`).
//! Het aantal taken staat dus niet bij de compiler, alleen het plafond.
//!
//! Wat hier niet staat: prioriteiten, werk-stelen, een tweede core. Elke
//! core heeft zijn eigen executor en zijn eigen taken; tussen cores gaan
//! berichten door een ring.

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

extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicPtr, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;
use sync::mpsc::Mailbox;

/// Een taak: een `'static` future die niets teruggeeft. Gespawnd op de
/// executor van de core waar hij leeft, en daar blijft hij: taken verhuizen
/// nooit, dus `Send` is geen eis. De executor zelf woont in een
/// [`sync::Local`]; dat is de belofte die dit sluitend maakt.
pub type Task = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// De klok: monotone nanoseconden sinds boot. Het board levert hem.
pub type Clock = fn() -> u64;

/// De slaap van het board: wat de executor doet als er niets te doen is.
///
/// `sleep` keert terug bij een wek (een `Signal::set`, een timer, een
/// interrupt) of zodra `until` bereikt is. De implementatie MOET, nadat zij
/// interrupts gemaskeerd heeft en vóór de WFE/WFI, `ready()` nog één keer
/// toetsen: dat is de deur die de verloren wek dichthoudt.
pub trait Sleeper {
    /// Slaap tot `until` (nanoseconden, `None` = tot een wek) of tot
    /// `ready()` waar is.
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool);
}

/// Waarom een `spawn` niet lukte.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// De heap kon de taak niet plaatsen.
    OutOfMemory,
    /// De spawn-brievenbus zit vol: de executor heeft nog geen ronde gedraaid.
    Full,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory placing the task"),
            Self::Full => f.write_str("spawn mailbox full"),
        }
    }
}

/// De meetlat van de executor (handboek §4): zonder deze getallen is "de
/// node slaapt" niet te onderscheiden van "de node spint".
#[derive(Default)]
pub struct Stats {
    /// Rondes met werk.
    pub rounds: AtomicU64,
    /// Gepolde taken, totaal.
    pub polls: AtomicU64,
    /// Keren geslapen.
    pub sleeps: AtomicU64,
    /// Taken die geen slot kregen (gedropt): `spawn` gaf `Ok`, maar de
    /// tabel was vol toen de ronde hem een plaats wilde geven.
    pub dropped: AtomicU64,
    /// Timers die geen slot kregen (de wachter spint op ronde-korrel).
    pub timer_overflows: AtomicU64,
    /// Nanoseconden in de slaper (handboek §4: de slaaptijd). Wat een tik
    /// daar niet van heeft, was werk: zo is "de core zit vol" een getal.
    pub slept_ns: AtomicU64,
}

struct Slot {
    /// Het woord in [`Executor::ready`] en het bit van dit slot daarin: wat
    /// de waker zet. Gezet bij het plaatsen van een taak, vóór er een waker
    /// van bestaat (en elke keer dezelfde waarde).
    word: AtomicPtr<AtomicU64>,
    bit: AtomicU64,
    task: RefCell<Option<Task>>,
    /// De taak van dit slot wordt nu gepolld (hij is even uit `task`). Een
    /// nieuwe taak mag hier dan niet in: de lopende poll zet zijn taak na
    /// afloop terug en zou de nieuwe overschrijven. Dat gebeurde met een
    /// taak die zelf rondes draait (`step`, zoals `Nested` in Hop): de
    /// binnenste ronde zette een verse spawn (de RX-pomp van appnet) in het
    /// "lege" slot van de wachtende taak, en de buitenste ronde gooide hem
    /// weg. Gemeten 29-09 op QEMU virt: de pomp van Hop stond na 14
    /// timer-wekken stil en een SYN op :8080 kreeg nooit antwoord.
    polling: Cell<bool>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            word: AtomicPtr::new(core::ptr::null_mut()),
            bit: AtomicU64::new(0),
            task: RefCell::new(None),
            polling: Cell::new(false),
        }
    }

    /// Vrij voor een nieuwe taak: geen taak, en niet midden in een poll.
    fn is_free(&self) -> bool {
        !self.polling.get() && self.task.borrow().is_none()
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop_waker);

unsafe fn clone(p: *const ()) -> RawWaker {
    RawWaker::new(p, &VTABLE)
}

unsafe fn wake(p: *const ()) {
    // SAFETY: `p` komt uit `Executor::waker` en wijst naar een `Slot` in een
    // executor die voor altijd leeft (`&'static self`).
    let slot = unsafe { &*p.cast::<Slot>() };
    let word = slot.word.load(Acquire);
    if !word.is_null() {
        // SAFETY: `word` wijst naar een woord van `Executor::ready` in
        // dezelfde executor, die voor altijd leeft.
        unsafe { &*word }.fetch_or(slot.bit.load(Relaxed), Release);
    }
    dev::notify();
}

unsafe fn drop_waker(_: *const ()) {}

/// Eén plaats in het timerwiel.
struct Timer {
    /// De deadline, in nanoseconden op de klok.
    at: u64,
    /// De generatie van de registratie (zie `Executor::timer_gen`).
    owner: u32,
    /// Uitstelbaar: telt niet mee voor de slaap (zie
    /// [`Executor::after_deferrable`]).
    deferrable: bool,
    waker: Waker,
}

/// De woorden van [`Executor::ready`]: plaats voor 512 taken, de
/// standaard van de kern.
const READY_WORDS: usize = 8;

/// Een executor met plaats voor `TASKS` taken en `TIMERS` lopende timers.
///
/// Leeft voor altijd: in de kern een `static` in een [`sync::Local`], in
/// een test een gelekte `Box`. Wakers wijzen naar zijn slots.
pub struct Executor<const TASKS: usize = 512, const TIMERS: usize = 256> {
    slots: [Slot; TASKS],
    /// De gereed-bits: bit `i % 64` van woord `i / 64` is slot `i`.
    ready: [AtomicU64; READY_WORDS],
    /// Zo groot als de takentabel: bij boot spawnt de kern één servicer per
    /// slot vóór de eerste ronde van de executor, en de Altra heeft er 128
    /// (03-10, de eerste v3-boot: bus van 64 vol, "first placement not
    /// spawned: Full", geen Hop).
    spawn: Mailbox<Task, TASKS>,
    timers: RefCell<[Option<Timer>; TIMERS]>,
    /// Eén voorbij de hoogste bezette plaats in `timers`: de scans van een
    /// ronde stoppen daar. Een plaats wordt van onderen af genomen, dus dit
    /// is de handvol timers die echt loopt, niet de hele tabel.
    timers_hi: Cell<usize>,
    /// De vroegste deadline in het wiel, of eerder (een `After` die vóór
    /// zijn tijd gedropt is, laat hem staan): een ronde scant het wiel
    /// alleen als die verstreken is. 03-10: elke ronde scande, en op de
    /// OS-core draait de kern tussen twee beurten van zijn bewoners een
    /// paar rondes zonder dat er iets verloopt.
    timers_at: Cell<u64>,
    /// De generatie van de volgende registratie: elke plaats in het wiel
    /// draagt de generatie van zijn huidige bewoner, zodat een `After` die
    /// zijn plaats al kwijt is (verlopen, hergebruikt) nooit die van een
    /// ander wist.
    timer_gen: Cell<u32>,
    /// De gereed-bits van het woord dat de lopende ronde afwerkt en die
    /// nog aan de beurt komen: de ronde haalt een woord in één swap leeg,
    /// dus zonder dit ziet [`Self::has_ready`] vanuit een taak niet dat er
    /// in deze ronde na hem nog een klaarstaat (de pomp van applib vraagt
    /// het, hop-cost5).
    rest: AtomicU64,
    clock: Cell<Option<Clock>>,
    /// De meetlat.
    pub stats: Stats,
}

/// Het timerwiel van een executor als [`sync::Timer`]: de tijd van de
/// binary (de kern, de USB-taak) en van de system-client van een app.
#[derive(Copy, Clone)]
pub struct ExecTimer<const TASKS: usize = 512, const TIMERS: usize = 256>(
    pub &'static Executor<TASKS, TIMERS>,
);

impl<const TASKS: usize, const TIMERS: usize> sync::Timer for ExecTimer<TASKS, TIMERS> {
    fn now(&self) -> u64 {
        self.0.now()
    }

    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        self.0.after(d)
    }

    fn sleep_deferrable(&self, d: Duration) -> impl Future<Output = ()> {
        self.0.after_deferrable(d)
    }
}

impl<const TASKS: usize, const TIMERS: usize> Default for Executor<TASKS, TIMERS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const TASKS: usize, const TIMERS: usize> Executor<TASKS, TIMERS> {
    /// Een lege executor zonder klok.
    #[must_use]
    pub const fn new() -> Self {
        const {
            assert!(
                TASKS <= 64 * READY_WORDS,
                "TASKS past niet in de gereed-bits"
            )
        };
        Self {
            slots: [const { Slot::new() }; TASKS],
            ready: [const { AtomicU64::new(0) }; READY_WORDS],
            spawn: Mailbox::new(),
            timers: RefCell::new([const { None }; TIMERS]),
            timers_hi: Cell::new(0),
            timers_at: Cell::new(u64::MAX),
            timer_gen: Cell::new(0),
            rest: AtomicU64::new(0),
            clock: Cell::new(None),
            stats: Stats {
                rounds: AtomicU64::new(0),
                polls: AtomicU64::new(0),
                sleeps: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                timer_overflows: AtomicU64::new(0),
                slept_ns: AtomicU64::new(0),
            },
        }
    }

    /// Zet de klok. Vóór de eerste `after`; het board doet dit bij boot.
    pub fn set_clock(&self, c: Clock) {
        self.clock.set(Some(c));
    }

    /// Nu, in nanoseconden; 0 zolang er geen klok is.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.clock.get().map_or(0, |c| c())
    }

    /// Zet een taak in de rij; hij krijgt zijn slot in de volgende ronde.
    pub fn spawn(&self, f: impl Future<Output = ()> + 'static) -> Result<(), SpawnError> {
        let task: Task = try_box(f).ok_or(SpawnError::OutOfMemory)?;
        self.spawn.try_send(task).map_err(|_| SpawnError::Full)
    }

    /// Slaapt `d` lang op het timerwiel.
    pub fn after(&'static self, d: Duration) -> After<TASKS, TIMERS> {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.until(self.now().saturating_add(ns))
    }

    /// Als [`after`](Self::after), maar uitstelbaar (Linux:
    /// `TIMER_DEFERRABLE`): hij vuurt in de eerste ronde na zijn deadline,
    /// maar wekt een slapende core niet. [`next_deadline`](Self::next_deadline)
    /// ziet hem niet, dus de slaper slaapt door tot een echte timer, een
    /// interrupt of een wek.
    ///
    /// Voor een vangnet dat alleen nodig is zolang de core bezig is: de
    /// failsafe van de switch (net::switch) en de tik van de servicers
    /// (kern::slots). Een core die slaapt heeft daar een deur of een
    /// vangrail voor. Als gewone timers wekten ze een stille OS-core elke 1
    /// en 2 ms (QEMU 30-09, Hop plus bench: ~710 wekken per seconde, erna
    /// ~210).
    pub fn after_deferrable(&'static self, d: Duration) -> After<TASKS, TIMERS> {
        let mut a = self.after(d);
        a.deferrable = true;
        a
    }

    /// Slaapt tot tijdstip `deadline` (nanoseconden op de klok).
    pub fn until(&'static self, deadline: u64) -> After<TASKS, TIMERS> {
        After {
            exec: self,
            deadline,
            deferrable: false,
            slot: None,
        }
    }

    /// Ligt er werk: een gezet bit, een spawn in de rij, of (vanuit een
    /// taak) een taak die in deze ronde nog na hem gepolld wordt?
    #[must_use]
    pub fn has_ready(&self) -> bool {
        !self.spawn.is_empty()
            || self.rest.load(Relaxed) != 0
            || self.ready.iter().any(|w| w.load(Relaxed) != 0)
    }

    /// De vroegste deadline van een timer die een slapende core mag wekken
    /// (dus niet de [uitstelbare](Self::after_deferrable)).
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers
            .borrow()
            .iter()
            .take(self.timers_hi.get())
            .flatten()
            .filter(|t| !t.deferrable)
            .map(|t| t.at)
            .min()
    }

    fn waker(slot: &'static Slot) -> Waker {
        let raw = RawWaker::new(core::ptr::from_ref(slot).cast::<()>(), &VTABLE);
        // SAFETY: de vtable hierboven houdt zich aan het `RawWaker`-contract:
        // clone en drop doen niets met eigendom, wake raakt alleen atomics.
        unsafe { Waker::from_raw(raw) }
    }

    fn drain_spawns(&'static self) -> bool {
        let mut worked = false;
        while let Some(task) = self.spawn.try_recv() {
            worked = true;
            let free = self.slots.iter().enumerate().find(|(_, s)| s.is_free());
            match free {
                Some((i, slot)) => {
                    *slot.task.borrow_mut() = Some(task);
                    let word = &self.ready[i / 64];
                    slot.bit.store(1 << (i % 64), Relaxed);
                    slot.word
                        .store(core::ptr::from_ref(word).cast_mut(), Release);
                    word.fetch_or(1 << (i % 64), Release);
                }
                None => {
                    self.stats.dropped.fetch_add(1, Relaxed);
                    drop(task);
                }
            }
        }
        worked
    }

    fn expire_timers(&self, now: u64) -> bool {
        if now < self.timers_at.get() {
            return false;
        }
        let mut worked = false;
        let mut hi = 0;
        let mut at = u64::MAX;
        let mut timers = self.timers.borrow_mut();
        for (i, entry) in timers.iter_mut().enumerate().take(self.timers_hi.get()) {
            if entry.as_ref().is_some_and(|t| t.at <= now)
                && let Some(t) = entry.take()
            {
                t.waker.wake();
                worked = true;
            }
            if let Some(t) = entry {
                hi = i + 1;
                at = at.min(t.at);
            }
        }
        self.timers_hi.set(hi);
        self.timers_at.set(at);
        worked
    }

    /// Eén ronde: spawns plaatsen, verstreken timers wekken, gereed taken
    /// pollen. `true` als er iets gebeurd is.
    pub fn step(&'static self) -> bool {
        let mut worked = self.drain_spawns();
        worked |= self.expire_timers(self.now());
        // Een geneste ronde (een taak die zelf rondes draait) laat de rest
        // van de buitenste zoals hij hem vond.
        let outer = self.rest.load(Relaxed);
        for (w, word) in self.ready.iter().enumerate() {
            // Eerst kijken, dan pas de atomaire swap: een AMO per leeg woord
            // is op de C906 geen kleingeld.
            if word.load(Relaxed) == 0 {
                continue;
            }
            let mut bits = word.swap(0, AcqRel);
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                self.rest.store(bits, Relaxed);
                let Some(slot) = self.slots.get(i) else {
                    continue;
                };
                worked |= self.poll(slot);
            }
        }
        self.rest.store(outer, Relaxed);
        if worked {
            self.stats.rounds.fetch_add(1, Relaxed);
        }
        worked
    }

    /// Pollt de taak van `slot`, als hij er een heeft. `true` = gepolld.
    fn poll(&'static self, slot: &'static Slot) -> bool {
        // De taak komt uit zijn slot zolang hij gepolld wordt: zo houdt
        // niemand een lening op de tabel terwijl vreemde code draait.
        let Some(mut task) = slot.task.borrow_mut().take() else {
            return false;
        };
        let waker = Self::waker(slot);
        let mut cx = Context::from_waker(&waker);
        self.stats.polls.fetch_add(1, Relaxed);
        slot.polling.set(true);
        let pending = task.as_mut().poll(&mut cx).is_pending();
        slot.polling.set(false);
        if pending {
            *slot.task.borrow_mut() = Some(task);
        }
        true
    }

    /// De hoofdlus: rondes draaien, en slapen als een ronde niets deed.
    pub fn run(&'static self, sleeper: &mut dyn Sleeper) -> ! {
        loop {
            if self.step() {
                continue;
            }
            self.stats.sleeps.fetch_add(1, Relaxed);
            let t = self.now();
            sleeper.sleep(t, self.next_deadline(), &|| self.has_ready());
            let slept = self.now().saturating_sub(t);
            self.stats.slept_ns.fetch_add(slept, Relaxed);
        }
    }

    /// Hoeveel taken een slot hebben.
    #[must_use]
    pub fn live_tasks(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.task.borrow().is_some())
            .count()
    }
}

/// De future van [`Executor::after`] en [`Executor::until`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct After<const TASKS: usize, const TIMERS: usize> {
    exec: &'static Executor<TASKS, TIMERS>,
    deadline: u64,
    deferrable: bool,
    /// De plaats in het wiel en de generatie waarmee wij hem namen.
    slot: Option<(usize, u32)>,
}

impl<const TASKS: usize, const TIMERS: usize> Future for After<TASKS, TIMERS> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.exec.now() >= this.deadline {
            return Poll::Ready(());
        }
        let mut timers = this.exec.timers.borrow_mut();
        // Een plaats die wij eerder namen kan verlopen en hergebruikt zijn:
        // dan is de generatie erin niet meer de onze en zoeken we opnieuw.
        if let Some((i, g)) = this.slot
            && timers[i].as_ref().is_none_or(|t| t.owner != g)
        {
            this.slot = None;
        }
        if this.slot.is_none()
            && let Some(i) = timers.iter().position(Option::is_none)
        {
            let g = this.exec.timer_gen.get().wrapping_add(1);
            this.exec.timer_gen.set(g);
            this.slot = Some((i, g));
        }
        match this.slot {
            Some((i, g)) => {
                if i >= this.exec.timers_hi.get() {
                    this.exec.timers_hi.set(i + 1);
                }
                this.exec
                    .timers_at
                    .set(this.exec.timers_at.get().min(this.deadline));
                timers[i] = Some(Timer {
                    at: this.deadline,
                    owner: g,
                    deferrable: this.deferrable,
                    waker: cx.waker().clone(),
                });
            }
            None => {
                // Geen timerslot: spin op ronde-korrel, en tel het.
                this.exec.stats.timer_overflows.fetch_add(1, Relaxed);
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }
}

impl<const TASKS: usize, const TIMERS: usize> Drop for After<TASKS, TIMERS> {
    fn drop(&mut self) {
        // Alleen onze eigen registratie wissen: draagt de plaats een andere
        // generatie, dan heeft `expire_timers` hem al teruggegeven en nam een
        // andere timer hem over. Een onvoorwaardelijke wis liet die voor
        // altijd slapen (gemeten 29-09 op QEMU virt: met een tik per lees van
        // elke system-verbinding hing zo in vijf van vijf runs een servicer
        // of de slot-keten).
        if let Some((i, g)) = self.slot.take() {
            let mut timers = self.exec.timers.borrow_mut();
            if timers[i].as_ref().is_some_and(|t| t.owner == g) {
                timers[i] = None;
            }
        }
    }
}

/// `Box::pin` dat faalt in plaats van abort bij een volle heap.
fn try_box<F: Future<Output = ()> + 'static>(f: F) -> Option<Task> {
    let layout = Layout::new::<F>();
    if layout.size() == 0 {
        // Een lege future alloceert niets; `Box::new` kan hier niet falen.
        return Some(Box::pin(f));
    }
    // SAFETY: de layout heeft een positieve grootte.
    let p = unsafe { alloc::alloc::alloc(layout) }.cast::<F>();
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is vers gealloceerd voor precies een `F` en nog niet
    // geïnitialiseerd; na `write` bezit de `Box` hem met dezelfde layout.
    let boxed = unsafe {
        p.write(f);
        Box::from_raw(p)
    };
    Some(Box::into_pin(boxed) as Task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use sync::Signal;

    static NOW: AtomicU64 = AtomicU64::new(0);
    fn fake_now() -> u64 {
        NOW.load(SeqCst)
    }

    fn exec() -> &'static Executor<8, 4> {
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(fake_now);
        e
    }

    #[test]
    fn task_waits_on_signal_and_finishes() {
        static BELL: Signal = Signal::new();
        static DONE: AtomicBool = AtomicBool::new(false);
        let e = exec();
        e.spawn(async {
            BELL.wait().await;
            DONE.store(true, SeqCst);
        })
        .unwrap();
        assert!(e.step()); // geplaatst en één keer gepolld: Pending
        assert!(!e.step()); // niets te doen
        assert_eq!(e.live_tasks(), 1);
        BELL.set();
        assert!(e.has_ready());
        assert!(e.step());
        assert!(DONE.load(SeqCst));
        assert_eq!(e.live_tasks(), 0);
    }

    #[test]
    fn a_task_sees_who_comes_after_it_in_the_same_round() {
        // De pomp van applib vraagt na een ontvangst of de lezer die hij
        // wekte nog moet draaien. Die staat in hetzelfde woord, dat de
        // ronde al leeghaalde: `has_ready` moet hem toch zien, en een taak
        // die als laatste draait niemand.
        static SAW: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
        let e = exec();
        for saw in &SAW {
            e.spawn(async move { saw.store(e.has_ready(), SeqCst) })
                .unwrap();
        }
        assert!(e.step());
        assert!(SAW[0].load(SeqCst), "de eerste ziet de tweede");
        assert!(!SAW[1].load(SeqCst), "na de laatste komt niemand meer");
        assert!(!e.has_ready(), "buiten de ronde telt alleen het woord");
    }

    #[test]
    fn a_wake_past_the_first_word_is_polled_and_only_it() {
        // Slot 64 en verder staan in het tweede woord van de gereed-bits.
        static BELL: Signal = Signal::new();
        static POLLS: AtomicU64 = AtomicU64::new(0);
        let e: &'static Executor<70, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(fake_now);
        for _ in 0..66 {
            e.spawn(async {
                loop {
                    POLLS.fetch_add(1, SeqCst);
                    BELL.wait().await;
                }
            })
            .unwrap();
        }
        assert!(e.step());
        assert_eq!(POLLS.load(SeqCst), 66);
        assert!(!e.has_ready());
        assert!(!e.step());
        // Eén bel wekt de ene wachter die zich als laatste registreerde
        // (een `Signal` houdt één waker): slot 65, in woord 1.
        BELL.set();
        assert!(e.has_ready());
        assert!(e.step());
        assert_eq!(POLLS.load(SeqCst), 67);
        assert!(!e.has_ready());
    }

    #[test]
    fn a_spawn_during_a_nested_round_is_not_lost() {
        // Hop's `Nested`: een taak spawnt en draait dan zelf rondes. De
        // binnenste ronde mag de nieuwe taak niet in het slot van de
        // wachtende taak zetten, anders overschrijft de buitenste ronde hem.
        static GO: Signal = Signal::new();
        static RAN: AtomicBool = AtomicBool::new(false);
        let e = exec();
        e.spawn(async move {
            e.spawn(async {
                GO.wait().await;
                RAN.store(true, SeqCst);
            })
            .unwrap();
            e.step(); // de binnenste ronde: plaatst en pollt de nieuwe taak
            core::future::pending::<()>().await;
        })
        .unwrap();
        e.step();
        assert_eq!(e.live_tasks(), 2, "the nested spawn was overwritten");
        GO.set();
        e.step();
        assert!(RAN.load(SeqCst));
    }

    #[test]
    fn timer_fires_when_the_clock_passes() {
        static FIRED: AtomicBool = AtomicBool::new(false);
        let e = exec();
        NOW.store(1_000, SeqCst);
        e.spawn(async move {
            e.after(Duration::from_nanos(500)).await;
            FIRED.store(true, SeqCst);
        })
        .unwrap();
        assert!(e.step());
        assert_eq!(e.next_deadline(), Some(1_500));
        NOW.store(1_400, SeqCst);
        assert!(!e.step());
        NOW.store(1_500, SeqCst);
        assert!(e.step()); // timer gewekt
        assert!(e.step() || FIRED.load(SeqCst)); // en de taak gepolld
        assert!(FIRED.load(SeqCst));
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn spawn_from_inside_a_task_and_ping_pong() {
        static A: Signal = Signal::new();
        static B: Signal = Signal::new();
        static ROUNDS: AtomicU64 = AtomicU64::new(0);
        let e = exec();
        e.spawn(async move {
            e.spawn(async {
                for _ in 0..3 {
                    A.wait().await;
                    B.set();
                }
            })
            .unwrap();
            for _ in 0..3 {
                A.set();
                B.wait().await;
                ROUNDS.fetch_add(1, SeqCst);
            }
        })
        .unwrap();
        let mut n = 0;
        while e.step() {
            n += 1;
            assert!(n < 100, "loopt niet uit");
        }
        assert_eq!(ROUNDS.load(SeqCst), 3);
        assert_eq!(e.live_tasks(), 0);
    }

    /// Een verstreken timer die pas daarna gedropt wordt, wist niet de
    /// registratie van een timer die zijn plaats in het wiel al overnam.
    #[test]
    fn late_drop_of_an_expired_timer_keeps_the_next_one() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        let mut cx = Context::from_waker(Waker::noop());
        CLOCK.store(1_000, SeqCst);
        let mut a = e.after(Duration::from_nanos(100));
        assert!(Pin::new(&mut a).poll(&mut cx).is_pending());
        CLOCK.store(1_100, SeqCst);
        assert!(e.expire_timers(clock())); // a's plaats is weer vrij
        let mut b = e.after(Duration::from_nanos(1_000));
        assert!(Pin::new(&mut b).poll(&mut cx).is_pending()); // b neemt hem
        drop(a);
        assert_eq!(e.next_deadline(), Some(2_100), "b's timer was wiped");
        drop(b);
        assert_eq!(e.next_deadline(), None);
    }

    /// Een uitstelbare timer bepaalt de slaap niet, maar vuurt wel in de
    /// eerste ronde na zijn deadline: de failsafe van de switch zonder de
    /// 1000 wekken per seconde.
    #[test]
    fn a_deferrable_timer_does_not_set_the_sleep_but_fires_in_the_next_round() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        static FIRED: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        CLOCK.store(1_000, SeqCst);
        e.spawn(async move {
            e.after_deferrable(Duration::from_nanos(100)).await;
            FIRED.fetch_add(1, SeqCst);
            e.after(Duration::from_nanos(1_000)).await;
            FIRED.fetch_add(1, SeqCst);
        })
        .unwrap();
        assert!(e.step());
        assert_eq!(e.next_deadline(), None, "a deferrable timer set the sleep");
        CLOCK.store(5_000, SeqCst); // de core sliep lang door, iets anders wekte
        assert!(e.step()); // de uitstelbare vuurt
        while e.step() {}
        assert_eq!(FIRED.load(SeqCst), 1);
        assert_eq!(e.next_deadline(), Some(6_000)); // een gewone telt weer
        CLOCK.store(6_000, SeqCst);
        while e.step() {}
        assert_eq!(FIRED.load(SeqCst), 2);
        assert_eq!(e.live_tasks(), 0);
    }

    #[test]
    fn full_table_drops_and_counts() {
        // De bus is zo groot als de tabel: een negende spawn vóór de eerste
        // ronde is Full; een spawn ná de ronde past in de bus en valt pas
        // bij het leegmaken op de volle tabel, geteld.
        let e = exec();
        for _ in 0..8 {
            e.spawn(core::future::pending()).unwrap();
        }
        assert!(e.spawn(core::future::pending()).is_err());
        e.step();
        assert_eq!(e.live_tasks(), 8);
        e.spawn(core::future::pending()).unwrap();
        e.step();
        assert_eq!(e.live_tasks(), 8);
        assert_eq!(e.stats.dropped.load(SeqCst), 1);
    }
}
