//! De kooi op arm64: wat `crate::kooi` van de architectuur vraagt
//! ([`Isa`]), over `cpu::el2` (stage-2 en de switcher), `cpu::smp` (CPU_ON)
//! en `dev`. Het beleid (bouwen, starten, stoppen, intrekken, stil) is van
//! `kooi.rs` en dezelfde als op riscv64 (`cage_riscv.rs`).
//!
//! Wat hier staat: de stage-2-tabel van een kooi (de bouwer van
//! `cpu::el2::stage2`, de enige: de intrekking en de switcher lezen
//! dezelfde tabellen via hetzelfde kooiblok, en hij doet het
//! cache-onderhoud voor de walker van de app-core), de woorden van de
//! trampoline op de control-page, het startschot van een app-core
//! (mailbox plus SEV, of koud PSCI CPU_ON, of erbij in een draaiende
//! rotatie), de SMP-eenheden, en de wekker van Apple. Dit is de ARM-helft
//! van `OLD/metal/kern/slots/cage_arm64.go` plus het schrijfwerk van
//! `armSlot`.

use crate::kooi::{Built, Isa, Kooi, KooiCores, err};
use abi::hopabi::{
    CTRL_DOOR_IRQ, CTRL_IDLE_MODE, CTRL_MBOX_PA, CTRL_S2_TABLE, CTRL_VEC_PA, IDLE_YIELD,
};
use abi::layout::{
    self, CTX_CTRL_PA, CTX_KICK_NONE, CTX_KICK_TARGET, CTX_NEXT_PA, CTX_SMP, CtxState, LINK_BASE,
    Plan, SLOT_CAP,
};
use board::Board;
use core::cell::OnceCell;
use core::sync::atomic::Ordering::Relaxed;
use core::time::Duration;
use cpu::el2::{self, CoreState, Flavor, Installed, Join, Start, roster};
use cpu::println;
use dev::Pa;
use executor::Executor;
use kern::cage::{CageError, CoreClass};
use sync::Local;
use vboard::slots::mpidr;

/// De kooi van deze node, en zijn cores.
pub(crate) type SlotCage = Kooi<Arm>;
/// Zie [`SlotCage`].
pub(crate) type SlotCores = KooiCores<Arm>;

/// De EL2-smaak van de switcher (cpu::el2 `Flavor`), gekozen door het board
/// (`Board::FLAVOR`) en niet door een losse bouwvlag:
/// - `Nvhe` (E2H=0, slapen in WFE) op QEMU virt, de Pi's, de Radxa en de
///   Altra;
/// - `Vhe` (E2H=1, de EL1-registers als `_EL12`) op de O6N: op de A720
///   stierf een EL1 onder nVHE binnen een halve seconde (Go, 17-09), dus
///   daar is VHE geen keuze maar een eis;
/// - `AppleVhe` op Apple silicium: E2H is er RES1 en de kick is de fast IPI.
///
/// De smaak `Vhe` eist een kern onder E2H = 1: de OS-core-rotatie gebruikt
/// de `_EL12`-encoderingen op de core van de kern zelf, en die zijn onder
/// E2H = 0 UNDEFINED (29-09, QEMU neoverse-n1: EC 0x0 in
/// `hopos_os_vhe_enter` bij de zelftest). Op het UEFI-board volgen de twee
/// uit dezelfde feature `vhe` van board-uefi (`el2::VHE`), dus ze verschillen
/// nooit. De feature `vhe` van hopos zet die aan: zo worden de VHE-kern en
/// de VHE-switcher op QEMU (`CPU=neoverse-n1`, EDK2) bewezen vóór ze op de
/// O6N draaien.
pub(crate) const FLAVOR: Flavor = <crate::Machine as board::Board>::FLAVOR;

// Alleen het UEFI-board kent een kern onder E2H = 1 (Apple is er VHE-only
// van zichzelf); `vhe` op een ander board gaf een nVHE-kern met een
// VHE-rotatie, en die valt bij de eerste zelftest.
#[cfg(feature = "vhe")]
const _: () = assert!(
    matches!(FLAVOR, Flavor::Vhe),
    "the feature `vhe` needs a board whose kern runs under E2H = 1 (board-uefi, board-o6n)"
);

/// Hoe een app-core idlet (`CTRL_IDLE_MODE`). Op QEMU virt een yield naar
/// de switcher (HVC #1): de kern slaapt daar in WFI, die geen SEV hoort,
/// dus een app die publiceert en dan op zijn antwoord wacht, bereikte de
/// kern pas op diens volgende deadline (de failsafe van de switch, 1 ms).
/// De switcher belt de OS-core bij een yield (`cpu::el2::OsCore::listen`).
/// GEMETEN 30-09, `tools/qemu-test-hop.sh` (4 cores): de `dial_us` van
/// appspike 3840 en 3543 met WFE-idle, 2852 en 2852 met yield-idle, bij 38
/// kicks per appspike-run. Een board met WFE in de kern hoort de SEV wel en
/// laat het veld leeg (de app kiest dan zelf).
///
/// Op Apple om een andere reden (board.go `IdleMode`, 02-09): WFE op EL1
/// slaapt op de M4 niet, dus een app-core met één bewoner spon op 100 %.
/// Met de yield slaapt de switcher in WFI en kickt de [`waker`] hem op zijn
/// wektijd; zonder die wekker sliep hij tot een toevallige kick, dus de
/// modus hoort pas bij de wekker (GEMETEN 02-09 in Go: 74 % cpu en 1,3 M
/// rondes/s werd 0 % en 47 wekken/s).
const APP_IDLE_MODE: u64 = if <crate::Machine as board::Board>::APP_IDLE_YIELD {
    IDLE_YIELD
} else {
    0
};

/// De foutcodes van de kooi ([`crate::kooi::code`]) plus die van CPU_ON.
mod code {
    pub(super) use crate::kooi::code::*;
    /// CPU_ON faalde (PSCI, of de haak van het board: `cpu::smp::cpu_on`);
    /// de code erbij is 0x100 plus de fout in PSCI-vorm.
    pub(super) const PSCI: u32 = 0x100;
}

/// Hoe lang [`Arm::start`] op een gedeelde app-core wacht tot de
/// rotatie een nieuwe bewoner oppikt. Een buur die idle is, geeft de core
/// binnen een event-stream-periode (~1,5 ms) of meteen na de kick; langer
/// is een buur die rekent, en dan wacht de lifecycle verder zonder de
/// kern-core te houden (`Cage::pending`, `kern::slots::RECLAIM_WAIT`), tot
/// en met het offeren van de vasthouder. Het wachten hier is een spin op
/// de kern-core: de kooi-trait is synchroon, en de park-race hieronder
/// moet in dezelfde stap gesloten worden.
const JOIN_WAIT_NS: u64 = 5_000_000;

/// Het app-adresvenster van een partitie van `size` bytes: het canonieke
/// venster vanaf [`LINK_BASE`] tot de rand van het 39-bit-regime
/// (`cageLinkWindow`).
pub(crate) fn link_window(size: u64) -> u64 {
    size.min(el2::stage2::IPA_LIMIT - LINK_BASE)
}

/// De tabelopslag achter een partitie van `size` (`cageReserve`).
pub(crate) fn reserve(size: u64) -> u64 {
    el2::stage2::table_reserve(LINK_BASE, size)
}

/// De grootste SMP-eenheid die de kooi aan elkaar ketent: meer dan de
/// app-cores van elk board dat we hebben (de O6N: 12).
const SMP_MAX: usize = 16;

/// De arm64-kant van de kooi: de geïnstalleerde switch-code in de
/// plan-regio (de switcher, de trampolines en de parkeerlus).
pub(crate) struct Arm {
    installed: Installed,
}

impl Arm {
    /// De geïnstalleerde switch-code.
    pub(crate) fn installed(&self) -> &Installed {
        &self.installed
    }
}

/// Zet de switch-code in de plan-regio en de app-cores klaar (vectoren,
/// parkeerlus, sched-blokken, lege ctx-staten). Eén keer bij boot, vóór de
/// eerste dispatch; er leven nog geen bewoners.
pub(crate) fn new(plan: Plan) -> Result<SlotCage, el2::Error> {
    let installed = el2::install_switch_code(&plan, FLAVOR)?;
    el2::init_app_cores(&plan, &installed, false)?;
    // De boot-core, als de kern bij boot naar de OS-core verhuisde (main,
    // `hopos.oscore`): nu is er een parkeerlus, en wordt hij een
    // geparkeerde app-core, vóór de eerste dispatch.
    if let Some(m) = el2::held() {
        let phys = vboard::slots::core_of(m);
        let core = plan.logical_core(phys).ok_or(el2::Error::UnparkedCore {
            core: phys,
            mbox: 0,
        })?;
        el2::release_held(&plan, core)?;
        println!("oscore: boot core {phys} parked as app core {core} HOPOS_OSCORE_PARKED");
    }
    Ok(Kooi::new(plan, Arm { installed }))
}

/// FLIP: neemt de zittende kooi-regio over (hopos/src/flip.rs): de
/// switch-code moet dezelfde som hebben als de onze (`cpu::el2::adopt`), en
/// er wordt niets geschreven, want er draaien cores in. De bewoners zelf
/// komen terug met `Kooi::adopt_slot`.
pub(crate) fn adopt(plan: Plan) -> Result<SlotCage, el2::Error> {
    let installed = el2::adopt(&plan, FLAVOR)?;
    Ok(Kooi::new(plan, Arm { installed }))
}

/// De MPIDR-affiniteit van logische `core`, zoals de switcher hem bij een
/// yield in `CTX_KICK_TARGET` zet (aff0..aff2).
fn affinity(plan: &Plan, core: layout::Core) -> u64 {
    mpidr(plan.phys_core(core)) & 0xFF_FFFF
}

impl Isa for Arm {
    const MAX_CORES: usize = SMP_MAX;
    // De switcher wisselt alleen op een yield: een buur die rekent, houdt
    // de core tot de kern hem offert (`kern::slots::RECLAIM_WAIT`).
    const TIME_SLICE: bool = false;
    const PARKS: bool = true;
    // `dc civac` gaat naar het hele inner-shareable domein: de vorige
    // huurder draaide cacheable op deze fysieke regels, en waar de kern de
    // pool Device ziet (Apple, rk3566) gaan de nullen langs de cache (op de
    // A76 gemeten 10-07).
    const SCRUB: bool = true;

    /// De stage-2-tabel van de kooi in zijn kooiblok (dat ook het ctx-blok
    /// wist), de woorden van de trampoline op de control-page, en de
    /// contexten van een SMP-eenheid (`prepareSMPContexts`): per secundaire
    /// een gewist ctx-blok op zijn eigen core, en de vertrouwde wek-keten
    /// rond. Eén core: de keten van een vorige SMP-levensduur van dit slot
    /// gaat eraf. Het wekdoel van ook de primaire staat er meteen: een
    /// secundaire die de primaire wekt (HVC #4) vóór diens eerste yield,
    /// vond hem anders niet in de keten, en die wek was dan verloren tot de
    /// wektijd.
    fn build(&self, plan: &Plan, b: &Built) -> Result<u64, CageError> {
        let (s, ctx) = (b.s, b.ctx);
        let block = plan.cage_table_pa(s).map_err(|_| err(code::PLAN))?;
        let mbox = plan.park_mbox_pa(b.core).map_err(|_| err(code::PLAN))?;
        let l1 = el2::stage2::build(block, LINK_BASE, b.part.base, b.part.size).map_err(|e| {
            println!("cage: slot {s}: stage-2 refused: {e} HOPOS_CAGE_STAGE2");
            err(code::CAGE)
        })?;
        // Op de OS-core is idle een yield: WFE op EL1 zou de hele core laten
        // slapen, en op QEMU spinnen tot de volgende interrupt. Zijn eerste
        // beurt legt de kern zelf klaar: geen trampoline.
        let os = b.core.get() == 0;
        if os {
            el2::prepare(ctx, b.entry, b.ctrl.0);
        }
        for (off, v) in [
            (CTRL_S2_TABLE, l1.0),
            (CTRL_VEC_PA, plan.vec_base_pa().0),
            (CTRL_MBOX_PA, mbox.0),
            (CTRL_IDLE_MODE, if os { IDLE_YIELD } else { APP_IDLE_MODE }),
        ] {
            dev::write64(b.ctrl.add(off), v);
        }
        // Nog geen wekdoel. Niet nul maar `CTX_KICK_NONE`: nul is fysieke
        // core 0, en dat gaf de O6N-hang van 21-09.
        el2::ctx_write(ctx, CTX_KICK_TARGET, CTX_KICK_NONE);
        if b.cores <= 1 {
            el2::chain(&[ctx]);
            return Ok(l1.0);
        }
        let mut ring = [ctx; SMP_MAX];
        for (k, x) in ring.iter_mut().enumerate().take(b.cores).skip(1) {
            let c = layout::Core::new(b.core.get() + k).ok_or(err(code::SPAN))?;
            *x = el2::prepare_secondary(plan, c, s, b.ctrl, affinity(plan, c)).map_err(|e| {
                println!("cage: slot {s}: SMP context on core {c}: {e} HOPOS_CAGE_SMP");
                err(code::SPAN)
            })?;
        }
        el2::ctx_write(ctx, CTX_KICK_TARGET, affinity(plan, b.core));
        el2::chain(ring.get(..b.cores).unwrap_or(&[]));
        println!(
            "cage: slot {s}: {} cores from core {}, contexts chained HOPOS_CAGE_SMP",
            b.cores, b.core
        );
        Ok(l1.0)
    }

    fn start(&self, plan: &Plan, b: &Built) -> Result<(), CageError> {
        let (slot, c, ctx, tramp) = (b.s, b.core, b.ctx, self.installed.tramp);
        let phys = plan.phys_core(c);
        // Draait de core al (een buur uit dezelfde sharegroup), dan komt de
        // kooi erbij in de rotatie; anders is het een gewoon startschot.
        if matches!(el2::core_state(plan, c), Ok(CoreState::Running(_))) && join(plan, b, tramp)? {
            return Ok(());
        }
        match el2::dispatch(plan, c, ctx, tramp, b.ctrl.0) {
            Ok(Start::Woken) => {
                println!("cage: slot {slot} dispatched to parked core {c} (mailbox + SEV)");
                Ok(())
            }
            Ok(Start::Cold) => {
                // De eerste opgang van deze core: CPU_ON rechtstreeks de
                // trampoline in, x0 = de control-page. Daarna leeft hij in de
                // parkeerlus van HopOS en gaat elke dispatch via de mailbox.
                let target = mpidr(phys);
                let r = cpu::smp::cpu_on(target, tramp.0, b.ctrl.0);
                println!(
                    "cage: slot {slot} core {c} cold: CPU_ON mpidr={target:#x} entry={:#x} x0={:#x} -> {r:?}",
                    tramp.0, b.ctrl.0
                );
                r.map_err(|e| err(code::PSCI + e.code().unsigned_abs() as u32))
            }
            Err(e) => {
                println!("cage: slot {slot} core {c}: {e} HOPOS_CAGE_DISPATCH");
                Err(err(code::DISPATCH))
            }
        }
    }

    fn start_secondary(&self, plan: &Plan, b: &Built, c: layout::Core) -> Result<(), CageError> {
        let (slot, s) = (b.s, b.s);
        let ctx = plan.smp_ctx_pa(c).map_err(|_| err(code::PLAN))?;
        let mbox = plan.park_mbox_pa(c).map_err(|_| err(code::PLAN))?;
        let l1 = plan.cage_table_pa(s).map_err(|_| err(code::PLAN))?;
        // De handoff is node-owned: de EL1-staat komt van de page van de app,
        // al het EL2-gezag (tabel, VMID, mailbox, vectoren) van hier. Na de
        // kopie kan de app hem niet meer veranderen.
        let handoff = ctx.add(CTX_SMP);
        el2::prepare_smp(
            handoff,
            b.ctrl,
            l1.0,
            s.get() as u64,
            mbox,
            plan.vec_base_pa(),
        );
        let tramp = self.installed.smp_tramp;
        let phys = plan.phys_core(c);
        match el2::dispatch(plan, c, ctx, tramp, handoff.0) {
            Ok(Start::Woken) => {
                println!(
                    "cage: slot {slot} SMP core {c} (cpu {phys}) dispatched to parked core HOPOS_SMP_CORE"
                );
                Ok(())
            }
            Ok(Start::Cold) => {
                let target = mpidr(phys);
                let r = cpu::smp::cpu_on(target, tramp.0, handoff.0);
                println!(
                    "cage: slot {slot} SMP core {c} (cpu {phys}) cold: CPU_ON mpidr={target:#x} -> {r:?} HOPOS_SMP_CORE"
                );
                r.map_err(|e| err(code::PSCI + e.code().unsigned_abs() as u32))
            }
            Err(e) => {
                println!("cage: slot {slot} SMP core {c}: {e} HOPOS_SMP_DISPATCH_FAIL");
                Err(err(code::DISPATCH))
            }
        }
    }

    /// De tabel nul en de TLBI (met de SEV voor de WFE-slapers): elke core
    /// van het slot faultt op zijn volgende vertaalde toegang. Daarna elke
    /// context van een app-core uit zijn rotatie: een geyielde context met
    /// een verre wektijd hervatte anders pas op die wektijd en voelde de
    /// intrekking zo lang niet; dan liep de stop in quarantaine terwijl er
    /// niets meer draaide (`el2::evict`). Op de OS-core doet de rotatie van
    /// de kern dat (`el2::next`).
    fn revoke(&self, plan: &Plan, slot: layout::Slot, b: Option<&Built>) {
        if let Err(e) = el2::revoke(plan, slot) {
            println!("cage: slot {slot}: revoke: {e} HOPOS_CAGE_REVOKE");
        }
        let Some(b) = b.filter(|b| b.core.get() != 0) else {
            return;
        };
        if let Err(e) = el2::evict(plan, b.core, b.ctx) {
            println!(
                "cage: slot {slot}: evict from core {}: {e} HOPOS_CAGE_REVOKE",
                b.core
            );
        }
        for k in 1..b.cores {
            let Some(c) = layout::Core::new(b.core.get() + k) else {
                break;
            };
            if let Ok(x) = plan.smp_ctx_pa(c)
                && let Err(e) = el2::evict(plan, c, x)
            {
                println!("cage: slot {slot}: evict SMP core {c}: {e} HOPOS_CAGE_REVOKE");
            }
        }
    }

    fn core_state(plan: &Plan, c: layout::Core) -> Result<CoreState, el2::Error> {
        el2::core_state(plan, c)
    }

    fn kick(plan: &Plan, c: layout::Core) {
        el2::kick(FLAVOR, mpidr(plan.phys_core(c)));
    }

    /// De klasse van de fysieke core volgens het board. Stond op `None`
    /// (de aanname van QEMU virt), en dan plaatste een jobspec met
    /// `core-class` nooit: GEMETEN 01-10 op de M4, "big" gaf "no free run"
    /// met drie P-cores vrij. Een spec zonder klasse merkt hier niets van.
    fn class(phys: usize) -> Option<CoreClass> {
        Some(match crate::BOARD.core_class(phys) {
            board::CoreClass::Small => CoreClass::Small,
            board::CoreClass::Mid => CoreClass::Mid,
            board::CoreClass::Big => CoreClass::Big,
        })
    }
}

/// Zet `b` erbij op zijn draaiende app-core (een sharegroup) en wacht kort
/// tot de rotatie hem oppikt. `Ok(false)`: de core staat stil, en het
/// is een gewoon startschot.
///
/// De park-race uit share.go: de rotatie las de lijst nét vóór onze
/// append, zag niemand meer, en parkeert. Dan pikt niemand de boot-pending
/// bewoner op, en ziet deze wacht de core geparkeerd: dan alsnog het
/// mailbox-startschot, en dat is dan het enige (de parkeerlus leest geen
/// lijst). Ziet de wacht niets binnen [`JOIN_WAIT_NS`], dan rekent de buur,
/// en wacht de lifecycle verder (`Cage::pending`).
fn join(plan: &Plan, b: &Built, tramp: Pa) -> Result<bool, CageError> {
    let (slot, c, ctx) = (b.s, b.core, b.ctx);
    let joined = el2::join(plan, c, ctx, tramp, b.ctrl.0).map_err(|e| {
        println!("cage: slot {slot} on shared core {c}: {e} HOPOS_CAGE_DISPATCH");
        err(code::ROSTER)
    })?;
    if joined == Join::Idle {
        return Ok(false);
    }
    el2::kick(FLAVOR, mpidr(plan.phys_core(c)));
    let t0 = cpu::idle::now();
    let mut parked = false;
    let seen = dev::poll_until(cpu::idle::now, JOIN_WAIT_NS, || {
        if el2::ctx_state(ctx) != Some(CtxState::BootPending) {
            return true;
        }
        parked = matches!(el2::core_state(plan, c), Ok(CoreState::Parked));
        parked
    });
    let picked = seen.then_some(!parked);
    let mut others = 0usize;
    let _ = roster::residents(plan, c, |id| {
        if usize::from(id) != slot.get() {
            others += 1;
        }
    });
    match picked {
        Some(false) => {
            println!(
                "cage: slot {slot}: shared core {c} parked while it joined, dispatching HOPOS_SHARE_JOIN"
            );
            return Ok(false);
        }
        Some(true) => println!(
            "cage: slot {slot} joined shared core {c} next to {others} resident(s), up in {} us HOPOS_SHARE_JOIN",
            cpu::idle::now().saturating_sub(t0) / 1000
        ),
        None => println!(
            "cage: slot {slot} waits on shared core {c}: a neighbour has not yielded in {} ms HOPOS_SHARE_PENDING",
            JOIN_WAIT_NS / 1_000_000
        ),
    }
    Ok(true)
}

// De wekker van de app-cores (`kern/slots/waker.go`), alleen voor Apple: een
// app-core idlet daar met een yield en slaapt in de WFI van de switcher, en
// geen FIQ van een eigen timer wekt hem (02-09); de kern is de enige die
// het kan. De WFE-smaken hebben er geen nodig: daar wekt de event stream de
// switcher, en de SEV van de switch elke slaper.

/// Het plan van de wekker en de RX-kick, één keer gezet door
/// [`start_waker`]. Alleen de executor van de kern raakt het aan: de
/// wekker-taak en de switch (`slot_wake`).
static WAKE_PLAN: Local<OnceCell<Plan>> = Local::new(OnceCell::new());

/// Het ritme van de wekker: Go's 1 ms, de korrel van een wektijd. Een poll
/// met een reden: docs/description.md §4.11.
const WAKE_EVERY: Duration = Duration::from_millis(1);

/// Start de wekker (`StartWaker`), alleen op `AppleVhe`; de marker
/// `HOPOS_WAKER_UP` is de poort.
pub(crate) fn start_waker(exec: &'static Executor, plan: &Plan) {
    if !matches!(FLAVOR, Flavor::AppleVhe) || WAKE_PLAN.set(plan.clone()).is_err() {
        return;
    }
    match exec.spawn(waker(exec)) {
        Ok(()) => println!(
            "idle: waker on, the kern kicks app cores that sleep on WFI every {} ms HOPOS_WAKER_UP",
            WAKE_EVERY.as_millis()
        ),
        Err(e) => println!("idle: waker not spawned: {e:?} HOPOS_SLOT_SPAWN"),
    }
}

/// De wekker-taak: elke [`WAKE_EVERY`] één ronde. Een gewone timer, geen
/// uitstelbare: de wektijd van een bewoner wacht niet op werk van de kern.
async fn waker(exec: &'static Executor) {
    loop {
        exec.after(WAKE_EVERY).await;
        if let Some(plan) = WAKE_PLAN.get().get() {
            wake_sleeping(plan, cpu::idle::counter());
        }
    }
}

/// Eén ronde (`wakeSleeping`): elke draaiende app-core met een geyielde
/// bewoner die due is (zijn wektijd, een kick, of RX: [`el2::due`]) krijgt
/// één fast IPI. De switcher wordt wakker, ackt hem en hervat wie due is;
/// een kick te veel is een geackte FIQ op EL2.
fn wake_sleeping(plan: &Plan, now: u64) {
    let w = &super::WAKER;
    w.rounds.fetch_add(1, Relaxed);
    for c in (1..=plan.app_cores()).filter_map(layout::Core::new) {
        if !matches!(el2::core_state(plan, c), Ok(CoreState::Running(_))) {
            continue;
        }
        let mut due = false;
        let _ = roster::residents(plan, c, |id| {
            // Een id voorbij SLOT_CAP is de secundaire van deze core.
            let ctx = match layout::Slot::new(usize::from(id)).filter(|s| s.get() <= SLOT_CAP) {
                Some(s) => plan.ctx_pa(s),
                None => plan.smp_ctx_pa(c),
            };
            if let Ok(x) = ctx
                && el2::ctx_state(x) == Some(CtxState::Saved)
            {
                w.seen.fetch_add(1, Relaxed);
                due |= el2::due(x, now).is_none();
            }
        });
        if due {
            el2::kick(FLAVOR, mpidr(plan.phys_core(c)));
            w.kicks.fetch_add(1, Relaxed);
        }
    }
}

/// De RX-kick van slot `slot` na een schrijf in zijn ring (`wakeRX`): de
/// core van de bewoner, niet mpidr 0. Alleen als de deurbel gewapend is en
/// de kop sindsdien bewoog: anders kickte de switch bij elke
/// leeg-naar-niet-leeg-overgang van een bulk-transfer, en dat kostte HOP
/// naar app 3,5x (04-09).
///
/// De core komt uit `CTX_KICK_TARGET`: de switcher zet daar bij elke yield
/// de affiniteit van zijn core. De deurbel en de ring zijn van de eenheid,
/// dus ook elke geyielde secundaire (de keten `CTX_NEXT_PA`) gaat erop
/// wakker: daar sliep anders de pomp tot zijn eigen timer, tot een seconde
/// (04-09, rtt p99 145 ms tot 4,8 s op een app met twee cores).
pub(crate) fn wake_rx(slot: usize) {
    let Some(plan) = WAKE_PLAN.get().get() else {
        return;
    };
    let Some(prim) = layout::Slot::new(slot).and_then(|s| plan.ctx_pa(s).ok()) else {
        return;
    };
    if !el2::rx_due(prim) {
        return;
    }
    let mut next = el2::ctx_read(prim, CTX_NEXT_PA);
    let single = next == 0;
    for _ in 0..SMP_MAX {
        if next == 0 || next == prim.0 {
            break;
        }
        let x = Pa(next);
        if el2::ctx_state(x) == Some(CtxState::Saved) {
            kick_ctx(x);
        }
        next = el2::ctx_read(x, CTX_NEXT_PA);
    }
    // Draait de primaire, dan alleen voor een app met één core die zijn
    // deurbel als interrupt neemt (CTRL_DOOR_IRQ): de switcher maakt van de
    // IPI dan een virtuele FIQ. Anders ackt hij hem op EL2 en verder niets.
    if el2::ctx_state(prim) != Some(CtxState::Saved) && !(single && door_irq(prim)) {
        return;
    }
    kick_ctx(prim);
}

/// Neemt de bewoner van `ctx` zijn deurbel als interrupt (`CTRL_DOOR_IRQ`
/// op zijn control-page, door de app geschreven)?
fn door_irq(ctx: Pa) -> bool {
    let cp = el2::ctx_read(ctx, CTX_CTRL_PA);
    if cp == 0 {
        return false;
    }
    let pa = Pa(cp).add(CTRL_DOOR_IRQ);
    dev::pull(pa, 8);
    dev::read64(pa) != 0
}

/// De fast IPI naar de core waarop context `ctx` het laatst yieldde.
fn kick_ctx(ctx: Pa) {
    let target = el2::ctx_read(ctx, CTX_KICK_TARGET);
    if target != CTX_KICK_NONE {
        el2::kick(FLAVOR, target);
        super::WAKER.rx.fetch_add(1, Relaxed);
    }
}
