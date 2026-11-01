//! SMP in een app: één executor per core ([`EXECS`]), en werk van de ene
//! core naar de andere met [`spawn_on`].
//!
//! Een jobspec met `cores: N` krijgt van de kern een aaneengesloten span van
//! N app-cores in één kooi: één stage-2-tabel, één VMID, één partitie en dus
//! één heap. De app ziet eerst alleen zijn primaire core. De andere kan hij
//! niet zelf starten: de park-mailboxen liggen buiten elke stage-2-map,
//! precies zodat een app dat niet kan. Dus vraagt de runtime ze één voor één
//! aan de kern ([`bring_up`], zoals Go's `smp.task`): de handoff op de
//! control-page (stack, entry, stub), `CTRL_SMP_REQ = slot + k`, en wachten
//! tot de kern het verzoek op nul zet. De kern kopieert de handoff naar een
//! eigen, vertrouwd blok en dispatcht de core in de kooi van de app; de
//! SMP-trampoline valt op EL1 in [`__applib_smp_start`], die de stack zet en
//! naar [`secondary_main`] springt. Die start de executor van zijn core.
//!
//! Wat een secundaire mag (handboek §1, "cores onderling: werk via ringen
//! met een kick, tellers als atomics, nooit een gedeelde tabel"):
//!
//! - taken draaien op zijn eigen executor, en werk van en naar andere cores
//!   gaat met [`spawn_on`] (de spawn-brievenbus van een executor is een
//!   MPSC-rij, de enige plek van een executor die een andere core aanraakt);
//! - tellers en vlaggen als atomics;
//! - alloceren: de heap draagt daarvoor het slot uit §1.3
//!   ([`crate::heap`]).
//!
//! Wat hij NIET doet: loggen (de outbox heeft één producer, de primaire; een
//! regel van een secundaire wordt gedropt en geteld), het `App` aanraken, of
//! een `Local` van een andere core lenen. Een resultaat gaat als atomic of
//! als taak terug naar core 0.
//!
//! Het wekken: een core die idle is, slaapt op EL2 in de switcher, met een
//! wektijd; een SEV wekt de switcher, maar die hervat alleen wie aan de
//! beurt is. [`kick`] doet daarom HVC #4 met de affiniteit van de doelcore
//! (de switcher zet diens wek-latch en wektijd op nu) en daarna de SEV. Het
//! wekdoel van elke context staat er vanaf de bouw (de kern, `arm_smp`),
//! dus ook een wek vóór de eerste yield komt aan.
//!
//! Wat in Go de grootste kostenpost van SMP was, vervalt: de GC van Go
//! stopte bij een SMP-app élke core (stop-the-world met `preemptM` via
//! dezelfde HVC #4), en een core die de wek miste hield zo de hele app vast
//! (04-09, de lock-wachter die seconden sliep). In Rust is er geen
//! collector en geen runtime die threads opeist: geheugen gaat terug bij
//! `Drop`, op de core die het laatst eigenaar was, onder het ene slot van de
//! heap, en geen core wacht ooit op een andere om verder te mogen. Wat
//! blijft is de wek van een taak naar een andere core, en die is expliciet.

use crate::app::App;
use crate::arch;
use crate::clock;
use crate::ctrl::{AppStatus, Ctrl};
use crate::rt::Exec;
use abi::hopabi::{
    CTRL_SMP_FN, CTRL_SMP_G0, CTRL_SMP_MAIR, CTRL_SMP_MP, CTRL_SMP_REQ, CTRL_SMP_SP, CTRL_SMP_STUB,
    CTRL_SMP_TCR, CTRL_SMP_TTBR0, CTRL_SMP_VBAR,
};
use core::alloc::Layout;
use core::fmt;
use core::future::Future;
use core::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use core::time::Duration;
use dev::Pa;
use executor::{Executor, Sleeper, SpawnError};
use sync::Local;

/// Het grootste aantal cores van één app, de primaire meegeteld. De kern
/// geeft uitsluitend toegewezen cores vrij. Twaalf plaatsen dragen ook
/// een O6N-job met elf cores; kleinere jobs reserveren geen extra stacks.
pub const MAX_CORES: usize = 12;

/// De stack van een secundaire core, uit de heap van de app.
pub const SMP_STACK: usize = 512 << 10;

/// Hoe lang [`bring_up`] op de kern wacht, per verzoek en per opgang.
pub const BRING_UP_LIMIT: Duration = Duration::from_secs(2);

/// De langste yield van een secundaire. In de yield maken alleen zijn
/// wektijd en een HVC #4 van een sibling ([`kick`]) hem weer due; de SEV
/// van een waker op een andere core of de kick van de kern bij een stop
/// niet. Deze grens is daar de vangrail, zoals de 10 ms van de OS-core
/// (`TURN_CAP_NS`): "geen deadline" als oneindig lezen is geen zuinigheid
/// maar een hang. In WFE geldt hij niet: daar ziet de lus elke wek en elke
/// vlag na hooguit één tik van de event-stream.
pub const SECONDARY_NAP: Duration = Duration::from_millis(10);

/// De executors, één per core: [`crate::EXEC`] is die van core 0.
///
/// Elk element is een [`Local`] van zijn eigen core. De ene uitzondering is
/// [`spawn_on`]: die zet vanaf een andere core een taak in de
/// spawn-brievenbus van het doel, en `Executor::spawn` raakt niets anders
/// aan dan die brievenbus (een MPSC-rij, `Sync`) en de heap (het slot uit
/// §1.3). De takenlijst, het timerwiel en de klok blijven van hun core.
pub static EXECS: [Local<Exec>; MAX_CORES] = [const { Local::new(Executor::new()) }; MAX_CORES];

/// Een affiniteit die nog niet bekend is (de core is niet op).
const NONE: u64 = u64::MAX;

/// De MPIDR-affiniteit van core `k`, gezet door die core zelf bij zijn
/// opgang (de primaire in [`init`]). [`NONE`] zolang hij niet op is.
static AFF: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(NONE) }; MAX_CORES];

/// De control-page, voor de slaper van een secundaire (die het `App` niet
/// aanraakt).
static CTRL_PA: AtomicU64 = AtomicU64::new(0);

/// Het aantal cores dat de kern gaf, geklemd op [`MAX_CORES`].
static GRANTED: AtomicU64 = AtomicU64::new(1);

/// De meetlat: taken naar een andere core, wekken, en panieken op een
/// secundaire (die zet zijn core terug; de reden gaat niet over de outbox).
pub static REMOTE_SPAWNS: AtomicU64 = AtomicU64::new(0);
/// Wekken naar een andere core (HVC #4 plus SEV).
pub static KICKS: AtomicU64 = AtomicU64::new(0);
/// Panieken op een secundaire core.
pub static SECONDARY_PANICS: AtomicU64 = AtomicU64::new(0);

/// Of [`bring_up`] klaar is (gelukt of niet).
static BRUNG_UP: AtomicBool = AtomicBool::new(false);

/// Waarom [`spawn_on`] niet lukte.
#[derive(Debug, PartialEq, Eq)]
pub enum SmpError {
    /// Deze app heeft core `core` niet.
    NoCore {
        /// De gevraagde core.
        core: usize,
        /// Het aantal cores van de app.
        cores: usize,
    },
    /// De executor van de doelcore nam de taak niet.
    Spawn(SpawnError),
}

impl fmt::Display for SmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCore { core, cores } => {
                write!(f, "core {core} is not one of this app's {cores} core(s)")
            }
            Self::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl App {
    /// Het aantal cores dat de kern deze app gaf (1 = geen SMP), geklemd op
    /// [`MAX_CORES`]. Een taak op core `k < cores()` mag al gespawnd worden
    /// vóór die core op is; hij draait zodra hij er is.
    #[must_use]
    pub fn cores(&self) -> usize {
        clamp_cores(self.ctrl().cores())
    }
}

fn clamp_cores(n: u64) -> usize {
    usize::try_from(n).unwrap_or(MAX_CORES).clamp(1, MAX_CORES)
}

/// Het aantal cores van deze app ([`App::cores`]), ook vanaf een
/// secundaire.
#[must_use]
pub fn cores() -> usize {
    clamp_cores(GRANTED.load(Relaxed))
}

/// De index van deze core binnen de app: 0 is de primaire.
#[must_use]
pub fn current() -> usize {
    let me = arch::core_id();
    AFF.iter().position(|a| a.load(Acquire) == me).unwrap_or(0)
}

/// Draait deze code op de primaire core? Zonder SMP altijd.
#[must_use]
pub fn on_primary() -> bool {
    let p = AFF[0].load(Acquire);
    p == NONE || p == arch::core_id()
}

/// Staat core `k` op (zijn executor draait)?
#[must_use]
pub fn is_up(k: usize) -> bool {
    AFF.get(k).is_some_and(|a| a.load(Acquire) != NONE)
}

/// Hoeveel cores er op zijn, de primaire meegeteld.
#[must_use]
pub fn online() -> usize {
    (0..MAX_CORES).filter(|k| is_up(*k)).count()
}

/// Wekt core `k` als die slaapt (HVC #4 naar zijn affiniteit, dan een
/// SEV). Een core die nog niet op is, of deze core zelf, krijgt alleen de
/// SEV: wie opkomt of al draait, ziet zijn brievenbus in zijn volgende
/// ronde.
pub fn kick(k: usize) {
    let aff = AFF.get(k).map_or(NONE, |a| a.load(Acquire));
    if aff != NONE && aff != arch::core_id() {
        KICKS.fetch_add(1, Relaxed);
        arch::hvc_wake(aff);
    } else {
        dev::notify();
    }
}

/// Zet `fut` op de executor van core `core` van deze app en wekt die core.
///
/// Het werk van één taak blijft op zijn core (taken verhuizen nooit); een
/// resultaat gaat terug als atomic of als een [`spawn_on`] naar de
/// vraagsteller.
pub fn spawn_on(core: usize, fut: impl Future<Output = ()> + 'static) -> Result<(), SmpError> {
    spawn_in(&EXECS, cores(), current(), core, fut, kick)
}

/// Het werk van [`spawn_on`] over een willekeurige executor-tabel, voor de
/// host-tests: `from` is de vragende core, `kick` de wek.
fn spawn_in<const N: usize>(
    execs: &'static [Local<Exec>; N],
    cores: usize,
    from: usize,
    core: usize,
    fut: impl Future<Output = ()> + 'static,
    kick: impl Fn(usize),
) -> Result<(), SmpError> {
    let exec = execs
        .get(core)
        .filter(|_| core < cores)
        .ok_or(SmpError::NoCore { core, cores })?;
    exec.get().spawn(fut).map_err(SmpError::Spawn)?;
    if core != from {
        REMOTE_SPAWNS.fetch_add(1, Relaxed);
        kick(core);
    }
    Ok(())
}

/// Legt de primaire vast: zijn affiniteit, de control-page en het aantal
/// cores. Door de main-schil, vóór de eerste taak.
pub(crate) fn init(app: &App) {
    AFF[0].store(arch::core_id(), Release);
    CTRL_PA.store(app.ctrl().addr(0).0, Release);
    GRANTED.store(app.cores() as u64, Release);
}

/// Vraagt de extra cores één voor één aan de kern en wacht tot elk zijn
/// executor draait. Eén handoff-venster op de control-page, dus nooit twee
/// verzoeken tegelijk; deze ene taak is de serialisatie.
pub(crate) async fn bring_up(app: &'static App) {
    let exec: &'static Exec = crate::rt::EXEC.get();
    let ctrl = app.ctrl();
    let want = app.cores();
    if ctrl.cores() > MAX_CORES as u64 {
        crate::log!(
            "applib: the kern granted {} cores, this runtime carries {MAX_CORES} HOPOS_APP_SMP",
            ctrl.cores()
        );
    }
    for k in 1..want {
        let t0 = clock::now_ns();
        match request(exec, ctrl, app.slot(), k).await {
            Ok(()) => crate::log!(
                "applib: core {k} of {want} up in {} us HOPOS_APP_SMP_UP",
                clock::now_ns().saturating_sub(t0) / 1000
            ),
            Err(why) => {
                crate::log!("applib: core {k} of {want}: {why} HOPOS_APP_SMP_FAIL");
                break;
            }
        }
    }
    BRUNG_UP.store(true, Release);
}

/// Is [`bring_up`] klaar?
#[must_use]
pub fn brought_up() -> bool {
    BRUNG_UP.load(Acquire)
}

/// Waarom een core niet opkwam.
enum Why {
    /// Geen heap voor zijn stack.
    Stack,
    /// De kern beantwoordde het verzoek niet.
    Unanswered,
    /// De kern beantwoordde, maar de core kwam niet op (geweigerd, of de
    /// dispatch faalde; de reden staat op de console van de kern).
    NotUp,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let limit = BRING_UP_LIMIT.as_millis();
        match self {
            Self::Stack => write!(f, "no heap for a {SMP_STACK}-byte stack"),
            Self::Unanswered => write!(f, "request not answered in {limit} ms"),
            Self::NotUp => write!(f, "answered, but the core was not up in {limit} ms"),
        }
    }
}

/// Eén core: stack, handoff, verzoek, en de twee wachten.
async fn request(exec: &'static Exec, ctrl: Ctrl, slot: u64, k: usize) -> Result<(), Why> {
    let layout = Layout::from_size_align(SMP_STACK, 16).map_err(|_| Why::Stack)?;
    // SAFETY: de layout heeft een positieve maat. Het blok wordt nooit
    // teruggegeven: het is de stack van een core die leeft zo lang de app.
    let base = unsafe { alloc::alloc::alloc(layout) };
    if base.is_null() {
        return Err(Why::Stack);
    }
    let top = (base.addr() + SMP_STACK) as u64;
    // De handoff: alleen EL1-staat. Het regime (TTBR0, TCR, MAIR, VBAR) is
    // dat van deze core, van de levende registers (crate::mmu): de stub van
    // de secundaire zet het plus SCTLR M/C/I vóór zijn eerste
    // geheugentoegang, zodat beide cores dezelfde tabellen en dezelfde
    // cacheable kijk op de heap hebben. Tot 30-09 was dit nul (een app
    // draaide met de MMU uit). Het EL2-gezag (tabel, VMID, mailbox)
    // kopieert de kern uit zijn eigen boekhouding, nooit van deze page.
    let (ttbr0, tcr, mair, vbar) = crate::mmu::regime();
    for (off, v) in [
        (CTRL_SMP_SP, top),
        (CTRL_SMP_MP, k as u64),
        (CTRL_SMP_G0, 0),
        (CTRL_SMP_FN, entry::main_addr()),
        (CTRL_SMP_STUB, entry::stub_addr()),
        (CTRL_SMP_TTBR0, ttbr0),
        (CTRL_SMP_TCR, tcr),
        (CTRL_SMP_MAIR, mair),
        (CTRL_SMP_VBAR, vbar),
    ] {
        ctrl.set(off, v);
    }
    dev::mb();
    // De virtuele CPU relatief aan het kooinummer, zoals de kern hem leest.
    ctrl.set(CTRL_SMP_REQ, slot.wrapping_add(k as u64));
    arch::hvc_kick_os();
    let step = Duration::from_millis(1);
    let deadline = exec.now().saturating_add(BRING_UP_LIMIT.as_nanos() as u64);
    while ctrl.get(CTRL_SMP_REQ) != 0 {
        if exec.now() >= deadline {
            return Err(Why::Unanswered);
        }
        exec.after(step).await;
    }
    let deadline = exec.now().saturating_add(BRING_UP_LIMIT.as_nanos() as u64);
    while !is_up(k) {
        if exec.now() >= deadline {
            return Err(Why::NotUp);
        }
        exec.after(step).await;
    }
    Ok(())
}

/// De Rust-kant van een secundaire core: [`__applib_smp_start`] zette de
/// stack en springt hierheen met `k`, zijn index binnen de app.
extern "C" fn secondary_main(k: u64) -> ! {
    let k = usize::try_from(k).unwrap_or(0);
    let Some(exec) = EXECS.get(k).filter(|_| k > 0) else {
        arch::park_exit();
    };
    // De event-stream is per core (CNTKCTL_EL1): zonder hem wekt de WFE van
    // deze core alleen op een SEV.
    clock::start_event_stream();
    let exec: &'static Exec = exec.get();
    exec.set_clock(clock::now_ns);
    let ctrl = Ctrl::at(Pa(CTRL_PA.load(Acquire)));
    // Pas nu op: vanaf hier mag een wek van een andere core ons raken.
    if let Some(a) = AFF.get(k) {
        a.store(arch::core_id(), Release);
    }
    exec.run(&mut CoreSleeper::new(ctrl, crate::sleep::Hw, k))
}

/// De slaap van een secundaire core: een yield naar de switcher, begrensd
/// op [`SECONDARY_NAP`], of WFE tot zijn eigen deadline, zoals de
/// control-page zegt. Geen deurbel (alleen de primaire leest de RX-ring)
/// en geen eigen woorden op de page: zijn slaap en zijn rondes staan in
/// zijn eigen woord van `sleep::SECONDARY_IDLE` (bijgewerkt na elke WFE)
/// en `sleep::SECONDARY_WAKES`, en de primaire publiceert de som
/// (`CtrlIdle` is de idle-tijd van álle cores van de app; de dvfs van de
/// kern deelt door `CtrlCores`).
///
/// Is de app weg (de primaire zette `Exited`), dan gaat deze core ook:
/// HVC #0, en de switcher meldt zijn context dood en parkeert de core. Zo
/// bevestigt een gewone stop élke core zonder intrekking.
///
/// Het stopverzoek alleen (de kill-vlag) is nog geen reden: de primaire
/// krijgt dan zijn gratie om `main` af te ronden en [`App::shutdown`] te
/// doen, en een taak op deze core hoort die gratie ook te krijgen. Tot
/// 24-10 parkeerde een secundaire al bij de vlag (uit 04-10, van vóór de
/// nette stop van 05-10) en liet zo zijn taak halverwege staan (F11). De
/// stop eindigt toch: de primaire zet `Exited` langs elke weg (de nette
/// exit, de geforceerde na de gratie in [`crate::rt::watch`], een paniek,
/// een fault in de vector van `crate::mmu`), en wie nooit terugkomt, velt de kern na de termijn met de
/// intrekking van de hele kooi.
pub struct CoreSleeper<I: crate::sleep::Idle> {
    ctrl: Ctrl,
    idle: I,
    /// Zijn index binnen de app (1..): zijn woord in `sleep::SECONDARY_IDLE`.
    core: usize,
    /// Zijn slaap tot nu, in tikken.
    idle_ticks: u64,
    /// Keren geslapen.
    pub naps: u64,
}

impl<I: crate::sleep::Idle> CoreSleeper<I> {
    /// Een slaper voor core `core` (1..) op de control-page `ctrl`, met core
    /// `idle`.
    pub fn new(ctrl: Ctrl, idle: I, core: usize) -> Self {
        Self {
            ctrl,
            idle,
            core,
            idle_ticks: 0,
            naps: 0,
        }
    }

    /// De core, voor de tests.
    pub fn idle(&self) -> &I {
        &self.idle
    }

    /// Is de app weg?
    #[must_use]
    pub fn app_gone(&self) -> bool {
        gone(&self.ctrl)
    }

    /// De slaap zelf, als de app er nog is.
    fn nap(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        if ready() {
            return;
        }
        let yields = self.ctrl.is_shared() || self.ctrl.is_yield_mode();
        let until = if yields {
            let cap = now.saturating_add(SECONDARY_NAP.as_nanos() as u64);
            Some(until.map_or(cap, |u| u.min(cap)))
        } else {
            until
        };
        let hz = self.idle.counter_hz();
        let deadline = clock::wake_at(now, until, self.idle.counter(), hz);
        self.naps = self.naps.wrapping_add(1);
        if let Some(w) = crate::sleep::SECONDARY_WAKES.get(self.core) {
            w.store(self.naps, Relaxed);
        }
        let word = crate::sleep::SECONDARY_IDLE.get(self.core);
        let base = self.idle_ticks;
        let slept = if yields {
            self.idle.hvc_yield(deadline)
        } else {
            // Tot werk, een vlag of de eigen deadline (sleep::wfe_until): een
            // wek zonder werk is geen ronde. Een stop, een buur of een taak
            // van een andere core ziet hij na de eerstvolgende WFE (een SEV,
            // of de tik van de event-stream); geen ronde per 10 ms meer.
            let ctrl = &self.ctrl;
            let woke = || ready() || ctrl.is_shared() || ctrl.is_yield_mode() || gone(ctrl);
            let progress = |s: u64| {
                if let Some(w) = word {
                    w.store(base.wrapping_add(s), Relaxed);
                }
            };
            crate::sleep::wfe_until(&mut self.idle, deadline, &woke, &progress)
        };
        // Tot 30-09 telde niemand deze slaap: een stille tweecore-app las
        // voor de dvfs als half bezig ("busy slot 2 (544 permille idle)" op
        // de Pi 4 en 5), en de kern bleef op 1500 MHz.
        self.idle_ticks = base.wrapping_add(slept);
        if let Some(w) = word {
            w.store(self.idle_ticks, Relaxed);
        }
    }
}

/// Is de app weg: zette de primaire `Exited`? Het stopverzoek telt niet
/// (zie [`CoreSleeper`]).
fn gone(ctrl: &Ctrl) -> bool {
    ctrl.status() == Some(AppStatus::Exited)
}

impl<I: crate::sleep::Idle> Sleeper for CoreSleeper<I> {
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        if self.app_gone() {
            arch::park_exit();
        }
        self.nap(now, until, ready);
    }
}

/// De paniek van een secundaire: tellen en de core teruggeven. De reden
/// gaat niet over de outbox (die is van de primaire); de kern ziet de
/// context dood, en de app op core 0 ziet [`SECONDARY_PANICS`].
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code, reason = "alleen de paniekhaak van het target roept hem")
)]
pub(crate) fn secondary_panicked() -> ! {
    SECONDARY_PANICS.fetch_add(1, Relaxed);
    if let Some(a) = AFF.get(current()) {
        a.store(NONE, Release);
    }
    arch::park_exit()
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod entry {
    //! De EL1-ingang van een secundaire core.

    // De SMP-trampoline van de kern (`cpu::el2`, `smpEL2Tramp`) ERET't
    // hierheen op EL1 met de MMU uit, met x0 = de stack-top, x1 = de
    // core-index, x3 = de Rust-entry, en het EL1-regime van de primaire:
    // x4 = TTBR0, x5 = MAIR, x6 = TCR, x7 = VBAR (`request`,
    // crate::mmu::regime). Interrupts dicht, de vectortabel, en als de
    // primaire een stage-1 heeft (TTBR0 niet 0) dezelfde tabellen met
    // SCTLR M, C en I, vóór de eerste geheugentoegang: de stack komt uit de
    // heap van de primaire, die daar cacheable in schreef, en met de MMU
    // uit zou deze core een oude kopie uit het geheugen lezen. De tabellen
    // zijn Normal-NC en de walker leest NC (crate::mmu), dus deze core ziet
    // wat de primaire schreef zonder onderhoud. Dan de stack, en door.
    core::arch::global_asm!(
        ".section .text.applib_smp, \"ax\"",
        ".global __applib_smp_start",
        "__applib_smp_start:",
        "    msr daifset, #0xf",
        "    mov x9, #0x300000",
        "    msr cpacr_el1, x9",
        "    isb",
        // Als `.inst`: de assembler van het softfloat-target kent fpcr en
        // fpsr niet als doel, de hardware wel (MSR S3_3_C4_C4_0 en _1).
        "    .inst 0xd51b441f",
        "    .inst 0xd51b443f",
        "    isb",
        "    cbz x7, 1f",
        "    msr vbar_el1, x7",
        "1:  cbz x4, 2f",
        "    msr mair_el1, x5",
        "    msr tcr_el1, x6",
        "    msr ttbr0_el1, x4",
        "    isb",
        "    tlbi vmalle1",
        "    dsb nsh",
        "    isb",
        "    mrs x9, sctlr_el1",
        "    orr x9, x9, #(1 << 0)",
        "    orr x9, x9, #(1 << 2)",
        "    orr x9, x9, #(1 << 12)",
        "    bic x9, x9, #(1 << 19)",
        "    msr sctlr_el1, x9",
        "    isb",
        "    ic iallu",
        "    dsb nsh",
        "    isb",
        "2:  mov sp, x0",
        "    mov x0, x1",
        "    br x3",
    );

    unsafe extern "C" {
        /// De stub hierboven; alleen zijn adres wordt genomen.
        fn __applib_smp_start();
    }

    /// Het adres van de stub (een IPA: de app draait op zijn linkadres).
    pub(super) fn stub_addr() -> u64 {
        (__applib_smp_start as *const ()).addr() as u64
    }

    /// Het adres van [`super::secondary_main`].
    pub(super) fn main_addr() -> u64 {
        (super::secondary_main as *const ()).addr() as u64
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod entry {
    //! Host-stub: er is geen tweede core om te starten.

    pub(super) fn stub_addr() -> u64 {
        0
    }

    pub(super) fn main_addr() -> u64 {
        (super::secondary_main as *const ()).addr() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CTRL_IDLE, CTRL_IDLE_MODE, CTRL_KILL, CTRL_STATUS, IDLE_YIELD};
    use crate::ctrl::tests::Page;
    use crate::sleep::Idle;
    use alloc::boxed::Box;
    use core::cell::Cell;
    use core::sync::atomic::AtomicUsize;
    use std::vec::Vec;

    fn two_execs() -> &'static [Local<Exec>; 2] {
        let e: &'static [Local<Exec>; 2] = Box::leak(Box::new([
            Local::new(Executor::new()),
            Local::new(Executor::new()),
        ]));
        for x in e {
            x.get().set_clock(|| 0);
        }
        e
    }

    /// De wekken van de host-tests, per core.
    static KICKED: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

    fn kicked(k: usize) {
        KICKED[k].fetch_add(1, Relaxed);
    }

    fn kicks() -> [usize; 2] {
        [KICKED[0].load(Relaxed), KICKED[1].load(Relaxed)]
    }

    // Twee executors op de host als twee cores: core 0 zet een taak op core
    // 1, die telt en een antwoord terugzet op core 0. Elke sprong naar een
    // andere core wekt die core; een spawn op de eigen core wekt niemand.
    #[test]
    fn spawn_on_moves_work_between_two_executors() {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        static ANSWERED: AtomicU64 = AtomicU64::new(0);
        let execs = two_execs();
        let task = async move {
            for _ in 0..1000 {
                COUNT.fetch_add(1, Relaxed);
            }
            // Het antwoord terug naar core 0.
            let reply = async {
                ANSWERED.store(COUNT.load(Relaxed), Relaxed);
            };
            spawn_in(execs, 2, 1, 0, reply, kicked).unwrap();
        };
        spawn_in(execs, 2, 0, 1, task, kicked).unwrap();
        assert_eq!(kicks(), [0, 1]);
        // Core 0 heeft niets; core 1 heeft de taak in zijn brievenbus.
        assert!(!execs[0].get().has_ready());
        assert!(execs[1].get().has_ready());
        assert!(execs[1].get().step());
        assert_eq!(COUNT.load(Relaxed), 1000);
        assert_eq!(kicks(), [1, 1]);
        assert!(execs[0].get().step());
        assert_eq!(ANSWERED.load(Relaxed), 1000);
        // Op de eigen core: geen wek.
        spawn_in(execs, 2, 0, 0, async {}, kicked).unwrap();
        assert_eq!(kicks(), [1, 1]);
        // Een core die de app niet heeft.
        assert_eq!(
            spawn_in(execs, 2, 0, 2, async {}, kicked),
            Err(SmpError::NoCore { core: 2, cores: 2 })
        );
        assert_eq!(
            spawn_in(execs, 1, 0, 1, async {}, kicked),
            Err(SmpError::NoCore { core: 1, cores: 1 })
        );
    }

    // Een taak op core 1 die op zijn eigen timer wacht, en core 0 die intussen
    // doorwerkt: de executors delen niets dan de brievenbus.
    #[test]
    fn each_core_keeps_its_own_tasks() {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let execs = two_execs();
        for _ in 0..3 {
            spawn_in(
                execs,
                2,
                0,
                1,
                async {
                    RUNS.fetch_add(1, Relaxed);
                    sync::yield_now().await;
                    RUNS.fetch_add(1, Relaxed);
                },
                |_| {},
            )
            .unwrap();
        }
        while execs[1].get().step() {}
        assert_eq!(RUNS.load(Relaxed), 6);
        assert_eq!(execs[1].get().live_tasks(), 0);
        assert_eq!(execs[0].get().live_tasks(), 0);
    }

    /// Een nep-core voor de slaper.
    #[derive(Default)]
    struct Fake {
        now: Cell<u64>,
        wfes: u32,
        yields: Vec<u64>,
    }

    impl Idle for Fake {
        fn counter(&self) -> u64 {
            self.now.get()
        }
        fn counter_hz(&self) -> u64 {
            1_000_000_000
        }
        fn wfe(&mut self) -> u64 {
            self.wfes += 1;
            self.now.set(self.now.get() + 5_000);
            5_000
        }
        fn hvc_yield(&mut self, deadline: u64) -> u64 {
            self.yields.push(deadline);
            0
        }
    }

    // De secundaire yieldt als de page dat zegt, met zijn wektijd begrensd op
    // de vangrail; zonder deadline ook op de vangrail, nooit een uur.
    #[test]
    fn a_secondary_yields_within_the_guard_rail() {
        let mut p = Page::new();
        p.put(CTRL_IDLE_MODE, IDLE_YIELD);
        let mut s = CoreSleeper::new(p.ctrl(), Fake::default(), 3);
        s.sleep(0, None, &|| false);
        s.sleep(0, Some(1_000_000), &|| false);
        s.sleep(0, Some(1_000_000_000), &|| false);
        assert_eq!(s.idle().yields, [10_000_000, 1_000_000, 10_000_000]);
        // Werk: niet slapen.
        s.sleep(0, None, &|| true);
        assert_eq!(s.idle().yields.len(), 3);
        assert_eq!(crate::sleep::SECONDARY_WAKES[3].load(Relaxed), 3);
    }

    // Zonder yield-modus: WFE tot zijn eigen deadline, één ronde, zonder
    // vangrail; zijn woord in SECONDARY_IDLE loopt mee met elke WFE, niet
    // pas aan het eind.
    #[test]
    fn a_secondary_sleeps_in_wfe_until_its_deadline() {
        let p = Page::new();
        let word = &crate::sleep::SECONDARY_IDLE[4];
        let mut s = CoreSleeper::new(p.ctrl(), Fake::default(), 4);
        let seen = Cell::new(0u64);
        s.sleep(0, Some(50_000_000), &|| {
            let w = word.load(Relaxed);
            assert!(w >= seen.get(), "the word went back");
            seen.set(w);
            false
        });
        assert_eq!(s.idle().wfes, 10_000);
        assert_eq!(s.naps, 1);
        assert!(s.idle().yields.is_empty());
        // De toets na de laatste WFE, nog in de lus, zag de hele slaap al.
        assert_eq!(seen.get(), 50_000_000);
        assert_eq!(word.load(Relaxed), 50_000_000);
    }

    // Zonder deadline slaapt een secundaire in WFE tot er werk is of tot
    // zijn app weggaat; geen ronde per 10 ms om dat te zien. Het
    // stopverzoek alleen wekt hem niet: de app is pas weg bij `Exited`.
    #[test]
    fn a_secondary_without_a_timer_wakes_for_work_or_a_stop() {
        let p = Page::new();
        let ctrl = p.ctrl();
        let mut s = CoreSleeper::new(ctrl, Fake::default(), 6);
        let n = Cell::new(0u32);
        s.sleep(0, None, &|| {
            n.set(n.get() + 1);
            n.get() > 5_000
        });
        // De eerste toets is die vóór de slaap; elke volgende na een WFE.
        assert_eq!((s.idle().wfes, s.naps), (4_999, 1));
        let n = Cell::new(0u32);
        s.sleep(0, None, &|| {
            n.set(n.get() + 1);
            if n.get() == 3_000 {
                ctrl.set(CTRL_KILL, 1);
            }
            n.get() > 6_000
        });
        // De vlag kwam na 2 998 WFE's; hij sliep door tot het werk.
        assert_eq!((s.idle().wfes, s.naps), (4_999 + 5_999, 2));
        assert!(!s.app_gone());
        let n = Cell::new(0u32);
        s.sleep(0, None, &|| {
            n.set(n.get() + 1);
            if n.get() == 3_000 {
                ctrl.set(CTRL_STATUS, AppStatus::Exited as u64);
            }
            false
        });
        assert_eq!((s.idle().wfes, s.naps), (4_999 + 5_999 + 2_998, 3));
        assert!(s.app_gone());
    }

    /// De dvfs van de kern leest `CtrlIdle` als de idle-tijd van álle cores
    /// (hij deelt door `CtrlCores`, per 10 ms): de primaire publiceert zijn
    /// eigen slaap plus die van de secundaire, en dat na elke WFE. Tot 30-09
    /// alleen de eigen ("544 permille idle"), en daarna alleen aan het eind
    /// van een lange slaap (de Pi 4 met stempel I: 266 tot 653 permille).
    #[test]
    fn the_primary_publishes_the_sleep_of_every_core_while_it_sleeps() {
        use crate::sleep::AppSleeper;
        let page = Page::new();
        let mut second = CoreSleeper::new(page.ctrl(), Fake::default(), 5);
        second.sleep(0, Some(1_000_000), &|| false); // 1 ms, 200 WFE's
        let mut first = AppSleeper::with(page.ctrl(), Fake::default());
        // Midden in een slaap van 50 ms (de heartbeat): na de tiende WFE
        // staan 50 µs van de primaire en 1 ms van de secundaire al op de page.
        let n = Cell::new(0u32);
        let mid = Cell::new(0u64);
        first.sleep(0, Some(50_000_000), &|| {
            n.set(n.get() + 1);
            if n.get() == 12 {
                mid.set(page.word(CTRL_IDLE));
            }
            false
        });
        // Andere tests schrijven tegelijk hun eigen woorden: minstens.
        assert!(mid.get() >= 10 * 5_000 + 1_000_000, "{}", mid.get());
        assert_eq!(first.idle_ticks(), 50_000_000);
        assert!(page.word(CTRL_IDLE) >= 51_000_000);
    }

    #[test]
    fn a_secondary_leaves_with_its_app() {
        let mut p = Page::new();
        let s = CoreSleeper::new(p.ctrl(), Fake::default(), 1);
        assert!(!s.app_gone());
        // Het stopverzoek: de primaire rondt nog af, deze core ook (F11).
        p.put(CTRL_KILL, 1);
        assert!(!s.app_gone());
        p.put(CTRL_STATUS, AppStatus::Exited as u64);
        assert!(s.app_gone());
    }

    #[test]
    fn cores_are_clamped_to_the_runtime() {
        assert_eq!(clamp_cores(0), 1);
        assert_eq!(clamp_cores(2), 2);
        assert_eq!(clamp_cores(12), MAX_CORES);
    }
}
