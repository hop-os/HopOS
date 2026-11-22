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
//! `sched_find_first_bit`); de timers staan in een min-heap, dus een ronde
//! kijkt naar de vroegste en niet naar de rest. Tot 03-10 vroeg elke ronde
//! 512 slots af met een atomaire swap, elke slaap 512 met een acquire-load
//! (op RISC-V een `fence` per slot) en elke ronde 256 timerplaatsen: op de
//! OS-core, waar de kern tussen elke twee beurten van zijn bewoners rondes
//! draait, was dat twee derde van een rtt tussen twee bewoners (QEMU virt met `-icount shift=0`, één instructie
//! per ns: riscv64 359 naar 108 us, arm64 252 naar 85 us).
//!
//! De ronde ([`Executor::step`]): eerst de gespawnde taken een plaats
//! geven, dan de timers die verstreken zijn wekken, dan alle taken met een
//! gezet bit pollen, en als er in die ronde niets gebeurd is slapen.
//! De verloren-wek-race is dicht doordat de [`Sleeper`] het `ready`-
//! predicaat nog één keer toetst mét interrupts gemaskeerd.
//!
//! Een taak is een future in een box op de heap ([`Executor::spawn`]; een
//! volle heap is [`SpawnError::OutOfMemory`]), in een tabel die met `TASKS`
//! plaatsen in de executor zelf begint en in blokken van 64 op de heap
//! groeit tot [`TASK_CEILING`] (de 512 van de gereed-bits). `spawn` neemt
//! zijn plaats meteen (een teller, [`Executor::spawn`]) en zet de taak in
//! een rij; de volgende ronde zet hem op die plaats. Geen plaats is dus een
//! fout van `spawn` ([`SpawnError::TooMany`]), nooit een stille drop. Tot
//! 21-11 was de tabel vast, gaf `spawn` toch `Ok`, en ging een taak die de
//! ronde geen plaats gaf weg ([`Stats::dropped`], `HOPOS_TASK_DROPPED`; dat
//! kan nu niet meer, de teller blijft als vangnet).
//!
//! De timers ([`timers`]) hebben geen vast plafond meer: `TIMERS` is de
//! beginmaat, de tabel verdubbelt op het koude pad tot [`TIMER_CEILING`] en
//! krimpt nooit. [`Stats::timers_peak`] is de hoogste stand. Tot 21-11 was
//! `TIMERS` een vaste tabel en spinde een timer zonder plaats.
//!
//! De langste beurt ([`Executor::longest_poll`]): per poll meet de ronde hoe
//! lang de taak de core hield, en houdt de langste bij met de naam van de
//! taak (zijn type, `core::any::type_name`). Dat is de regel van
//! `docs/apps.md` ("Lange berekeningen": één poll blijft kort) als getal,
//! zoals Erlangs `long_schedule` en Node's `monitorEventLoopDelay`. Eén
//! kloklees per poll: het einde van de ene poll is het begin van de
//! volgende. Een geneste ronde (een taak die zelf `step` draait) telt niet
//! mee in de beurt van die taak: de taken erin hebben hun eigen beurt.
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

mod timers;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicPtr, AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;
use sync::mpsc::Mailbox;
use timers::{Handle, Timers};

/// Het plafond van de timers van één executor: zoveel lopende timers tegelijk
/// en niet meer (een plaats kost ongeveer 64 bytes, met de heaps erbij; dit
/// is dus ruim 4 MiB). Het houdt een lek (een taak die timers stapelt) uit
/// de heap; een timer erboven spint als voorheen en telt
/// [`Stats::timer_overflows`].
pub const TIMER_CEILING: usize = 1 << 16;

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
    /// De spawn-brievenbus zit vol: er zijn al `TASKS` taken gespawnd sinds
    /// de vorige ronde. Na een `yield_now` is er weer plaats.
    Full,
    /// De takentabel is op [`TASK_CEILING`].
    TooMany,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory placing the task"),
            Self::Full => f.write_str("spawn mailbox full"),
            Self::TooMany => write!(f, "task table full ({TASK_CEILING} tasks)"),
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
    /// ronde vond geen plaats. Sinds 21-11 neemt `spawn` de plaats zelf,
    /// dus dit blijft nul; een vangnet dat luid zegt als dat ooit niet zo is.
    pub dropped: AtomicU64,
    /// De hoogste stand van taken tegelijk (geplaatst plus in de rij).
    pub tasks_peak: AtomicU64,
    /// Blokken van 64 plaatsen die de takentabel op de heap bijnam.
    pub task_grows: AtomicU64,
    /// Polls van een timer die geen plaats kreeg: de heap kon de tabel niet
    /// laten groeien, of [`TIMER_CEILING`] is bereikt. De wachter spint dan
    /// op ronde-korrel, zoals tot 21-11 bij elke volle tabel van `TIMERS`.
    pub timer_overflows: AtomicU64,
    /// De hoogste stand van lopende timers tegelijk (de "high-water"): wat
    /// een app echt nodig heeft, los van de beginmaat `TIMERS`.
    pub timers_peak: AtomicU64,
    /// Keren dat de timertabel groeide: elke keer één allocatie (handboek
    /// §6: de meetlat telt allocaties). In de vaste toestand staat hij stil.
    pub timer_grows: AtomicU64,
    /// Nanoseconden in de slaper (handboek §4: de slaaptijd). Wat een tik
    /// daar niet van heeft, was werk: zo is "de core zit vol" een getal.
    pub slept_ns: AtomicU64,
}

/// De langste beurt: hoe lang één poll van één taak de core hield
/// ([`Executor::longest_poll`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LongestPoll {
    /// Nanoseconden op de klok van de executor; 0 = nog geen poll (of geen
    /// klok).
    pub ns: u64,
    /// De taak: het type van zijn future (`core::any::type_name`), met zijn
    /// generieke argumenten. Leeg zolang `ns` 0 is.
    pub task: &'static str,
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
    /// De naam van de taak in dit slot, voor [`LongestPoll::task`].
    name: Cell<&'static str>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            word: AtomicPtr::new(core::ptr::null_mut()),
            bit: AtomicU64::new(0),
            task: RefCell::new(None),
            polling: Cell::new(false),
            name: Cell::new(""),
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

/// De woorden van [`Executor::ready`]: plaats voor 512 taken, de
/// standaard van de kern.
const READY_WORDS: usize = 8;

/// Het plafond van de takentabel: de gereed-bits.
pub const TASK_CEILING: usize = 64 * READY_WORDS;

/// Plaatsen per blok dat de takentabel bijneemt.
const CHUNK: usize = 64;

/// Een blok van de takentabel op de heap. Eenmaal gezet blijft het: wakers
/// wijzen naar zijn slots.
type Chunk = [Slot; CHUNK];

/// Een executor met plaats voor `TASKS` taken, en een timertabel die met
/// plaats voor `TIMERS` timers begint en groeit (tot [`TIMER_CEILING`]).
///
/// Leeft voor altijd: in de kern een `static` in een [`sync::Local`], in
/// een test een gelekte `Box`. Wakers wijzen naar zijn slots.
pub struct Executor<const TASKS: usize = 512, const TIMERS: usize = 256> {
    slots: [Slot; TASKS],
    /// De blokken na `slots`: slot `TASKS + 64 k + j` is plaats `j` van blok
    /// `k`. Gezet door wie de plaats als eerste nodig heeft ([`Self::spawn`],
    /// van elke core: een compare-exchange op een lege pointer), nooit
    /// weggehaald, dus een `&'static Slot` erin blijft geldig.
    chunks: [AtomicPtr<Chunk>; READY_WORDS],
    /// Taken met een plaats: geplaatst, of in de spawn-rij met een plaats
    /// die voor hen bestaat. `spawn` hoogt hem op (en weigert boven
    /// [`TASK_CEILING`]), een taak die klaar is verlaagt hem.
    claimed: AtomicUsize,
    /// De gereed-bits: bit `i % 64` van woord `i / 64` is slot `i`.
    ready: [AtomicU64; READY_WORDS],
    /// Zo groot als de takentabel: bij boot spawnt de kern één servicer per
    /// slot vóór de eerste ronde van de executor, en de Altra heeft er 128
    /// (03-10, de eerste v3-boot: bus van 64 vol, "first placement not
    /// spawned: Full", geen Hop).
    spawn: Mailbox<(Task, &'static str), TASKS>,
    /// De timers: twee min-heaps op een tabel die groeit ([`timers`]). Een
    /// ronde kijkt alleen naar de vroegste (03-10: elke ronde scande de
    /// hele tabel, en op de OS-core draait de kern tussen twee beurten van
    /// zijn bewoners een paar rondes zonder dat er iets verloopt). Elke
    /// registratie draagt een generatie, zodat een `After` die zijn plaats
    /// al kwijt is (verlopen, hergebruikt) nooit die van een ander wist.
    /// De lening duurt nooit over een wek of een drop van een waker.
    timers: RefCell<Timers>,
    /// De gereed-bits van het woord dat de lopende ronde afwerkt en die
    /// nog aan de beurt komen: de ronde haalt een woord in één swap leeg,
    /// dus zonder dit ziet [`Self::has_ready`] vanuit een taak niet dat er
    /// in deze ronde na hem nog een klaarstaat (de pomp van applib vraagt
    /// het, hop-cost5).
    rest: AtomicU64,
    clock: Cell<Option<Clock>>,
    /// De langste beurt sinds de start ([`Self::longest_poll`]).
    longest: Cell<LongestPoll>,
    /// De langste beurt sinds de vorige [`Self::take_longest_poll`].
    window: Cell<LongestPoll>,
    /// Hoe diep we in polls zitten: boven 0 is een ronde genest.
    depth: Cell<u32>,
    /// Nanoseconden in geneste rondes tijdens de lopende poll: die gaan van
    /// zijn beurt af.
    nested: Cell<u64>,
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
            chunks: [const { AtomicPtr::new(core::ptr::null_mut()) }; READY_WORDS],
            claimed: AtomicUsize::new(0),
            spawn: Mailbox::new(),
            timers: RefCell::new(Timers::new()),
            rest: AtomicU64::new(0),
            clock: Cell::new(None),
            longest: Cell::new(LongestPoll { ns: 0, task: "" }),
            window: Cell::new(LongestPoll { ns: 0, task: "" }),
            depth: Cell::new(0),
            nested: Cell::new(0),
            stats: Stats {
                rounds: AtomicU64::new(0),
                polls: AtomicU64::new(0),
                sleeps: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                tasks_peak: AtomicU64::new(0),
                task_grows: AtomicU64::new(0),
                timer_overflows: AtomicU64::new(0),
                timers_peak: AtomicU64::new(0),
                timer_grows: AtomicU64::new(0),
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

    /// Neemt een plaats voor een taak en zet hem in de rij; de volgende
    /// ronde zet hem op een vrije plaats, en die is er dan (de teller
    /// `claimed`: een taak met een plaats is geplaatst of in de rij, en elke
    /// `spawn` zorgt dat er voor zijn nummer een slot bestaat). Mag van elke
    /// core: het raakt alleen atomics, de rij en een vers blok.
    ///
    /// # Errors
    ///
    /// [`SpawnError::OutOfMemory`] als de heap de taak of een nieuw blok niet
    /// kon plaatsen, [`SpawnError::TooMany`] boven [`TASK_CEILING`] taken,
    /// [`SpawnError::Full`] als de rij vol is (`TASKS` spawns in één ronde).
    pub fn spawn(&self, f: impl Future<Output = ()> + 'static) -> Result<(), SpawnError> {
        let name = core::any::type_name_of_val(&f);
        let task: Task = try_box(f).ok_or(SpawnError::OutOfMemory)?;
        let n = self
            .claimed
            .fetch_update(AcqRel, Acquire, |c| (c < TASK_CEILING).then_some(c + 1))
            .map_err(|_| SpawnError::TooMany)?;
        let placed = self.ensure_slot(n).and_then(|()| {
            self.spawn
                .try_send((task, name))
                .map_err(|_| SpawnError::Full)
        });
        if placed.is_err() {
            self.claimed.fetch_sub(1, AcqRel);
            return placed;
        }
        let now = u64::try_from(n.saturating_add(1)).unwrap_or(u64::MAX);
        self.stats.tasks_peak.fetch_max(now, Relaxed);
        Ok(())
    }

    /// Zorgt dat slot `n` bestaat: elk blok tot en met het zijne staat.
    /// Waarom dat genoeg is: van de taken met een plaats nam de laatste zijn
    /// nummer toen de rest al een plaats had, dus dat nummer is minstens
    /// hun aantal min één, en zijn `spawn` zette de slots tot en met dat
    /// nummer neer. De ronde vindt dus voor elke taak in de rij een slot.
    fn ensure_slot(&self, n: usize) -> Result<(), SpawnError> {
        let Some(j) = n.checked_sub(TASKS) else {
            return Ok(());
        };
        for cell in self.chunks.iter().take(j / CHUNK + 1) {
            if !cell.load(Acquire).is_null() {
                continue;
            }
            let fresh = new_chunk().ok_or(SpawnError::OutOfMemory)?;
            match cell.compare_exchange(core::ptr::null_mut(), fresh, AcqRel, Acquire) {
                Ok(_) => {
                    self.stats.task_grows.fetch_add(1, Relaxed);
                }
                // Een andere core was eerder: zijn blok blijft, het onze gaat
                // terug.
                Err(_) => free_chunk(fresh),
            }
        }
        Ok(())
    }

    /// Slot `i`: in de executor zelf, of in een blok dat al staat.
    fn slot(&'static self, i: usize) -> Option<&'static Slot> {
        if let Some(s) = self.slots.get(i) {
            return Some(s);
        }
        if i >= TASK_CEILING {
            return None;
        }
        let j = i.checked_sub(TASKS)?;
        let p = self.chunks.get(j / CHUNK)?.load(Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: een gezet blok is een geldige, geïnitialiseerde `Chunk`
        // (`new_chunk`, gepubliceerd met Release en hier met Acquire
        // gelezen) die nooit meer weggaat; de executor leeft voor altijd
        // (`&'static self`), dus het blok ook.
        let chunk: &'static Chunk = unsafe { &*p };
        chunk.get(j % CHUNK)
    }

    /// Voor hoeveel taken de tabel nu plaats heeft.
    #[must_use]
    pub fn task_capacity(&self) -> usize {
        let blocks = self
            .chunks
            .iter()
            .filter(|c| !c.load(Relaxed).is_null())
            .count();
        TASKS
            .saturating_add(blocks.saturating_mul(CHUNK))
            .min(TASK_CEILING)
    }

    /// Slaapt `d` lang op de timers van de executor.
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
        self.timers.borrow().next_wake()
    }

    /// Hoeveel timers nu lopen.
    #[must_use]
    pub fn live_timers(&self) -> usize {
        self.timers.borrow().live()
    }

    /// Voor hoeveel timers de tabel nu plaats heeft (de beginmaat `TIMERS`,
    /// of wat hij daarna groeide).
    #[must_use]
    pub fn timer_capacity(&self) -> usize {
        self.timers.borrow().capacity()
    }

    fn waker(slot: &'static Slot) -> Waker {
        let raw = RawWaker::new(core::ptr::from_ref(slot).cast::<()>(), &VTABLE);
        // SAFETY: de vtable hierboven houdt zich aan het `RawWaker`-contract:
        // clone en drop doen niets met eigendom, wake raakt alleen atomics.
        unsafe { Waker::from_raw(raw) }
    }

    fn drain_spawns(&'static self) -> bool {
        let mut worked = false;
        while let Some((task, name)) = self.spawn.try_recv() {
            worked = true;
            let free = (0..TASK_CEILING)
                .map_while(|i| self.slot(i).map(|s| (i, s)))
                .find(|(_, s)| s.is_free());
            match free {
                Some((i, slot)) => {
                    *slot.task.borrow_mut() = Some(task);
                    slot.name.set(name);
                    let word = &self.ready[i / 64];
                    slot.bit.store(1 << (i % 64), Relaxed);
                    slot.word
                        .store(core::ptr::from_ref(word).cast_mut(), Release);
                    word.fetch_or(1 << (i % 64), Release);
                }
                None => {
                    // Kan niet (zie `ensure_slot`); als het toch gebeurt:
                    // luid, en de plaats terug.
                    self.stats.dropped.fetch_add(1, Relaxed);
                    self.claimed.fetch_sub(1, AcqRel);
                    drop(task);
                }
            }
        }
        worked
    }

    fn expire_timers(&self, now: u64) -> bool {
        let mut worked = false;
        // Eén lening per timer, en de wek erbuiten: een waker die zelf een
        // timer zet (een vreemde executor, een test) vindt de tabel vrij.
        loop {
            let due = self.timers.borrow_mut().pop_due(now);
            let Some(waker) = due else {
                return worked;
            };
            waker.wake();
            worked = true;
        }
    }

    /// Eén ronde: spawns plaatsen, verstreken timers wekken, gereed taken
    /// pollen. `true` als er iets gebeurd is.
    pub fn step(&'static self) -> bool {
        let start = self.now();
        let mut worked = self.drain_spawns();
        worked |= self.expire_timers(start);
        // Het begin van de eerste beurt; daarna is het einde van elke poll
        // het begin van de volgende (één kloklees per poll).
        let mut mark = self.now();
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
                let Some(slot) = self.slot(i) else {
                    continue;
                };
                worked |= self.poll(slot, &mut mark);
            }
        }
        self.rest.store(outer, Relaxed);
        if self.depth.get() > 0 {
            // Genest: deze ronde gaat van de beurt van de taak die hem draait af.
            let spent = self.now().saturating_sub(start);
            self.nested.set(self.nested.get().saturating_add(spent));
        }
        if worked {
            self.stats.rounds.fetch_add(1, Relaxed);
        }
        worked
    }

    /// Pollt de taak van `slot`, als hij er een heeft. `true` = gepolld.
    /// `mark` is het begin van deze beurt; na de poll het einde ervan.
    fn poll(&'static self, slot: &'static Slot, mark: &mut u64) -> bool {
        // De taak komt uit zijn slot zolang hij gepolld wordt: zo houdt
        // niemand een lening op de tabel terwijl vreemde code draait.
        let Some(mut task) = slot.task.borrow_mut().take() else {
            return false;
        };
        let waker = Self::waker(slot);
        let mut cx = Context::from_waker(&waker);
        self.stats.polls.fetch_add(1, Relaxed);
        slot.polling.set(true);
        let outer = self.nested.replace(0);
        self.depth.set(self.depth.get().saturating_add(1));
        let pending = task.as_mut().poll(&mut cx).is_pending();
        self.depth.set(self.depth.get().saturating_sub(1));
        let end = self.now();
        let held = end
            .saturating_sub(*mark)
            .saturating_sub(self.nested.replace(outer));
        *mark = end;
        self.note(held, slot.name.get());
        slot.polling.set(false);
        if pending {
            *slot.task.borrow_mut() = Some(task);
        } else {
            drop(task);
            // Klaar: de plaats is weer vrij voor een volgende `spawn`.
            self.claimed.fetch_sub(1, AcqRel);
        }
        true
    }

    /// Legt een beurt van `ns` van taak `task` vast als hij de langste is.
    fn note(&self, ns: u64, task: &'static str) {
        if ns <= self.window.get().ns {
            return;
        }
        let poll = LongestPoll { ns, task };
        self.window.set(poll);
        if ns > self.longest.get().ns {
            self.longest.set(poll);
        }
    }

    /// De langste beurt sinds de start: de langste poll van één taak, met
    /// die taak.
    #[must_use]
    pub fn longest_poll(&self) -> LongestPoll {
        self.longest.get()
    }

    /// De langste beurt sinds de vorige aanroep (of de start), en begint
    /// opnieuw: wat een periodieke regel meldt. Eén lezer per executor,
    /// anders ziet elk de helft.
    pub fn take_longest_poll(&self) -> LongestPoll {
        self.window.take()
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
    pub fn live_tasks(&'static self) -> usize {
        (0..TASK_CEILING)
            .map_while(|i| self.slot(i))
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
    /// Onze registratie: de plaats en de generatie waarmee wij hem namen.
    slot: Option<Handle>,
}

impl<const TASKS: usize, const TIMERS: usize> Future for After<TASKS, TIMERS> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.exec.now() >= this.deadline {
            return Poll::Ready(());
        }
        let exec = this.exec;
        let mut timers = exec.timers.borrow_mut();
        // Nog onze registratie: alleen de waker vernieuwen. Een plaats die
        // wij eerder namen kan verlopen en hergebruikt zijn: dan is de
        // generatie erin niet meer de onze en registreren we opnieuw.
        if let Some(h) = this.slot
            && timers.refresh(h, cx.waker())
        {
            return Poll::Pending;
        }
        this.slot = None;
        let added = timers.add(
            this.deadline,
            this.deferrable,
            cx.waker(),
            TIMERS,
            TIMER_CEILING,
        );
        let live = timers.live();
        drop(timers);
        match added {
            Ok((h, grew)) => {
                this.slot = Some(h);
                if grew {
                    exec.stats.timer_grows.fetch_add(1, Relaxed);
                }
                let live = u64::try_from(live).unwrap_or(u64::MAX);
                exec.stats.timers_peak.fetch_max(live, Relaxed);
            }
            Err(timers::NoRoom) => {
                // Geen plaats (heap vol of plafond): spin op ronde-korrel, en
                // tel het.
                exec.stats.timer_overflows.fetch_add(1, Relaxed);
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
        if let Some(h) = self.slot.take() {
            // De waker gaat pas buiten de lening weg.
            let waker = self.exec.timers.borrow_mut().remove(h);
            drop(waker);
        }
    }
}

/// Een vers blok van de takentabel op de heap, of `None` bij een volle heap.
fn new_chunk() -> Option<*mut Chunk> {
    let layout = Layout::new::<Chunk>();
    // SAFETY: een `Chunk` heeft een positieve grootte.
    let p = unsafe { alloc::alloc::alloc(layout) }.cast::<Chunk>();
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is vers gealloceerd voor precies een `Chunk` en wordt hier
    // geïnitialiseerd, vóór iemand hem ziet.
    unsafe { p.write([const { Slot::new() }; CHUNK]) };
    Some(p)
}

/// Geeft een blok terug dat nooit gepubliceerd is.
fn free_chunk(p: *mut Chunk) {
    // SAFETY: `p` komt uit `new_chunk` met dezelfde layout, is
    // geïnitialiseerd en heeft nooit een andere eigenaar gehad (de
    // compare-exchange die hem zou publiceren faalde).
    unsafe {
        core::ptr::drop_in_place(p);
        alloc::alloc::dealloc(p.cast::<u8>(), Layout::new::<Chunk>());
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

    /// Een waker die zijn nummer opschrijft.
    struct Mark(usize, std::sync::Arc<std::sync::Mutex<Vec<usize>>>);
    impl std::task::Wake for Mark {
        fn wake(self: std::sync::Arc<Self>) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    fn marks() -> std::sync::Arc<std::sync::Mutex<Vec<usize>>> {
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    /// Vijfduizend timers tegelijk op een executor die met plaats voor vier
    /// begint: geen overloop, elke timer wekt in de eerste ronde waarin de
    /// klok zijn deadline passeert en niet eerder, de geannuleerde nooit, en
    /// de hoogste stand staat in de meetlat.
    #[test]
    fn thousands_of_timers_fire_on_time_and_cancelled_ones_never() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        const N: usize = 5_000;
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        CLOCK.store(1_000, SeqCst);
        let log = marks();
        let mut timers = Vec::new();
        let mut deadline = Vec::new();
        for i in 0..N {
            // Deadlines door elkaar: 1_001 ..= 1_000 + 997, met herhalingen.
            let d = u64::try_from((i * 7_919) % 997 + 1).unwrap();
            let mut a = e.after(Duration::from_nanos(d));
            let w = Waker::from(std::sync::Arc::new(Mark(i, log.clone())));
            assert!(
                Pin::new(&mut a)
                    .poll(&mut Context::from_waker(&w))
                    .is_pending()
            );
            timers.push(Some(a));
            deadline.push(1_000 + d);
        }
        assert_eq!(e.stats.timer_overflows.load(SeqCst), 0);
        assert_eq!(e.live_timers(), N);
        assert_eq!(e.stats.timers_peak.load(SeqCst), N as u64);
        let grows = e.stats.timer_grows.load(SeqCst);
        assert!(grows <= 12, "doubling, not one per timer: {grows}");
        // Annuleer elke derde.
        for (i, t) in timers.iter_mut().enumerate() {
            if i % 3 == 0 {
                *t = None;
            }
        }
        let live = N - N.div_ceil(3);
        assert_eq!(e.live_timers(), live);
        assert_eq!(
            e.timer_capacity(),
            e.timer_capacity().max(N),
            "never shrinks"
        );
        // De klok in stappen van één: wat wekt, wekt op zijn eigen deadline.
        let mut fired = 0;
        for now in 1_001..=2_000 {
            CLOCK.store(now, SeqCst);
            e.expire_timers(now);
            let woke: Vec<usize> = log.lock().unwrap().drain(..).collect();
            for i in &woke {
                assert_eq!(deadline[*i], now, "timer {i} woke at {now}");
                assert_ne!(i % 3, 0, "a cancelled timer woke");
            }
            fired += woke.len();
            let next = e.next_deadline();
            assert!(next.is_none_or(|n| n > now));
        }
        assert_eq!(fired, live);
        assert_eq!(e.live_timers(), 0);
        // De verlopen futures zijn klaar en hun drop raakt niets.
        for t in timers.iter_mut().flatten() {
            assert!(
                Pin::new(t)
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_ready()
            );
        }
        drop(timers);
        // Opnieuw evenveel: de tabel heeft de plaats al, geen allocatie meer.
        let again: Vec<_> = (0..N)
            .map(|_| {
                let mut a = e.after(Duration::from_secs(1));
                assert!(
                    Pin::new(&mut a)
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                a
            })
            .collect();
        assert_eq!(
            e.stats.timer_grows.load(SeqCst),
            grows,
            "steady state allocates"
        );
        drop(again);
        assert_eq!(e.live_timers(), 0);
        assert_eq!(e.next_deadline(), None);
    }

    /// Een timer die opnieuw gepolld wordt (een `select` die om iets anders
    /// wakker werd) houdt één registratie, en de nieuwste waker wekt.
    #[test]
    fn a_repoll_keeps_one_registration_and_the_newest_waker() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        let log = marks();
        let mut a = e.after(Duration::from_nanos(50));
        for i in 0..10 {
            let w = Waker::from(std::sync::Arc::new(Mark(i, log.clone())));
            assert!(
                Pin::new(&mut a)
                    .poll(&mut Context::from_waker(&w))
                    .is_pending()
            );
            assert_eq!(e.live_timers(), 1);
        }
        CLOCK.store(50, SeqCst);
        assert!(e.expire_timers(50));
        assert_eq!(*log.lock().unwrap(), vec![9]);
        assert!(
            Pin::new(&mut a)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }

    /// Duizend taken die elk een paar keer slapen, door de echte ronde: ze
    /// komen allemaal aan, en een gewone timer bepaalt de slaap, een
    /// uitstelbare niet.
    #[test]
    fn a_thousand_sleeping_tasks_through_step() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        static DONE: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        // 512 taken (de standaard van de kern), elk twee timers in een rij.
        let e: &'static Executor<512, 16> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        for i in 0..512u64 {
            e.spawn(async move {
                e.after(Duration::from_nanos(10 + i)).await;
                e.after_deferrable(Duration::from_nanos(1_000)).await;
                DONE.fetch_add(1, SeqCst);
            })
            .unwrap();
        }
        assert!(e.step());
        assert_eq!(e.live_timers(), 512);
        assert_eq!(e.next_deadline(), Some(10));
        CLOCK.store(600, SeqCst);
        while e.step() {}
        assert_eq!(e.live_timers(), 512, "each now on its deferrable timer");
        assert_eq!(e.next_deadline(), None, "deferrable timers set the sleep");
        CLOCK.store(10_000, SeqCst);
        while e.step() {}
        assert_eq!(DONE.load(SeqCst), 512);
        assert_eq!(e.live_tasks(), 0);
        assert_eq!(e.stats.timer_overflows.load(SeqCst), 0);
        assert_eq!(e.stats.timers_peak.load(SeqCst), 512);
    }

    #[test]
    fn a_spawn_burst_past_the_mailbox_is_an_error_and_the_table_grows() {
        // De bus is zo groot als de beginmaat: een negende spawn vóór de
        // eerste ronde is Full (een fout, geen stille drop). Na de ronde
        // past hij, en de tabel neemt een blok bij in plaats van hem te
        // droppen.
        let e = exec();
        for _ in 0..8 {
            e.spawn(core::future::pending()).unwrap();
        }
        assert_eq!(e.spawn(core::future::pending()), Err(SpawnError::Full));
        e.step();
        assert_eq!(e.live_tasks(), 8);
        e.spawn(core::future::pending()).unwrap();
        e.step();
        assert_eq!(e.live_tasks(), 9);
        assert_eq!(e.task_capacity(), 8 + 64);
        assert_eq!(e.stats.task_grows.load(SeqCst), 1);
        assert_eq!(e.stats.dropped.load(SeqCst), 0);
    }

    /// Tot het plafond van 512 groeit de tabel in blokken; de 513e spawn is
    /// `TooMany`, en zodra een taak klaar is mag er weer een bij.
    #[test]
    fn the_task_table_grows_to_the_ceiling_and_then_refuses() {
        static GO: Signal = Signal::new();
        let e = exec();
        let mut n = 0;
        while n < TASK_CEILING {
            for _ in 0..8 {
                if n == 0 {
                    e.spawn(async { GO.wait().await }).unwrap();
                } else {
                    e.spawn(core::future::pending()).unwrap();
                }
                n += 1;
            }
            e.step();
        }
        assert_eq!(e.live_tasks(), TASK_CEILING);
        assert_eq!(e.task_capacity(), TASK_CEILING);
        assert_eq!(e.stats.tasks_peak.load(SeqCst), TASK_CEILING as u64);
        // 8 in de executor, (512 - 8) / 64 = 7,9: acht blokken, het laatste
        // half bruikbaar.
        assert_eq!(e.stats.task_grows.load(SeqCst), 8);
        assert_eq!(e.spawn(core::future::pending()), Err(SpawnError::TooMany));
        GO.set();
        e.step();
        assert_eq!(e.live_tasks(), TASK_CEILING - 1);
        e.spawn(core::future::pending()).unwrap();
        e.step();
        assert_eq!(e.live_tasks(), TASK_CEILING);
        assert_eq!(e.stats.dropped.load(SeqCst), 0);
    }

    /// Een taak die 20 ms rekent zonder yield is de langste beurt, met zijn
    /// naam; korte taken eromheen niet. Op de echte klok van de host.
    #[test]
    fn a_task_that_computes_for_20_ms_is_the_longest_poll() {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        fn clock() -> u64 {
            let t0 = START.get_or_init(std::time::Instant::now);
            u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX)
        }
        async fn compute_20_ms() {
            let t = std::time::Instant::now();
            let mut x = 0u64;
            while t.elapsed() < Duration::from_millis(20) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        assert_eq!(e.longest_poll(), LongestPoll::default());
        for _ in 0..3 {
            e.spawn(async { sync::yield_now().await }).unwrap();
        }
        e.spawn(compute_20_ms()).unwrap();
        while e.step() {}
        let l = e.longest_poll();
        assert!(l.ns >= 20_000_000, "{l:?}");
        assert!(l.ns < 2_000_000_000, "{l:?}");
        assert!(l.task.contains("compute_20_ms"), "{l:?}");
        assert_eq!(e.take_longest_poll(), l);
        // Het venster begint opnieuw; de langste sinds de start blijft.
        assert_eq!(e.take_longest_poll(), LongestPoll::default());
        e.spawn(async {}).unwrap();
        while e.step() {}
        let w = e.take_longest_poll();
        assert!(w.ns < l.ns, "{w:?}");
        assert_eq!(e.longest_poll(), l);
    }

    /// Op een nepklok: de beurt is precies wat de poll de klok liet lopen,
    /// en een geneste ronde gaat van de beurt van de buitenste taak af (de
    /// taak erin heeft zijn eigen beurt).
    #[test]
    fn a_nested_round_is_not_the_outer_tasks_poll() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        async fn inner_7_ms() {
            CLOCK.fetch_add(7_000_000, SeqCst);
        }
        async fn outer_2_plus_3_ms(e: &'static Executor<8, 4>) {
            CLOCK.fetch_add(2_000_000, SeqCst);
            e.spawn(inner_7_ms()).unwrap();
            e.step();
            CLOCK.fetch_add(3_000_000, SeqCst);
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        e.spawn(outer_2_plus_3_ms(e)).unwrap();
        while e.step() {}
        let l = e.longest_poll();
        assert_eq!(l.ns, 7_000_000, "{l:?}");
        assert!(l.task.contains("inner_7_ms"), "{l:?}");
        // De buitenste: 5 ms, dus nooit de langste; een taak van 6 ms wel
        // in een vers venster, niet sinds de start.
        e.take_longest_poll();
        e.spawn(async {
            CLOCK.fetch_add(6_000_000, SeqCst);
        })
        .unwrap();
        while e.step() {}
        assert_eq!(e.take_longest_poll().ns, 6_000_000);
        assert_eq!(e.longest_poll(), l);
    }

    /// Taken in de blokken worden gewekt en gepolld zoals die in de
    /// executor zelf (hun bit staat in een ander woord), en een taak die
    /// klaar is geeft zijn plaats terug: duizend korte taken na elkaar
    /// blijven binnen één blok.
    #[test]
    fn tasks_in_grown_blocks_wake_and_finished_tasks_give_their_place_back() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        static DONE: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.load(SeqCst)
        }
        let e: &'static Executor<8, 4> = Box::leak(Box::new(Executor::new()));
        e.set_clock(clock);
        for i in 0..200u64 {
            e.spawn(async move {
                e.after(Duration::from_nanos(1 + i)).await;
                DONE.fetch_add(1, SeqCst);
            })
            .unwrap();
            if i % 8 == 7 {
                e.step();
            }
        }
        while e.step() {}
        assert_eq!(e.live_tasks(), 200);
        CLOCK.store(1_000, SeqCst);
        while e.step() {}
        assert_eq!(DONE.load(SeqCst), 200);
        assert_eq!(e.live_tasks(), 0);
        let grown = e.stats.task_grows.load(SeqCst);
        assert_eq!(grown, 3, "8 + 3 x 64 = 200");
        // Duizend korte taken in rondes van acht: geen blok meer erbij.
        for i in 0..1_000 {
            e.spawn(async {}).unwrap();
            if i % 8 == 7 {
                e.step();
            }
        }
        while e.step() {}
        assert_eq!(e.live_tasks(), 0);
        assert_eq!(e.stats.task_grows.load(SeqCst), grown);
        assert_eq!(e.stats.dropped.load(SeqCst), 0);
    }
}
