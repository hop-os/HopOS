//! De slot-kant van de kern-binary: de kooi-lijm ([`cage`]), de
//! lifecycle-actor en de servicers, en Hop.
//!
//! Wat hier gebeurt, is de board-glue van `kern::slots`: het plan van het
//! board wordt een `PartitionPool` en een `Places`, de kooi en de cores
//! gaan als waarde de actor in, en per slot draait één servicer-taak uit
//! een vaste pool die de outbox leegtrekt naar de console (`slot N:
//! <regel>`). Er wordt nooit per start gespawnd.
//!
//! De kern plaatst alleen Hop. Elke andere app plaatst Hop via de
//! system-API; de QEMU-rol `app` (een gestaged image dat de kern zelf
//! plaatste) is weg sinds 01-11 (docs/sequence-hopos.md §14.1 O7): een
//! productiebinary zonder testpad, en de toetsen lopen de echte weg.
//!
//! # Hop
//!
//! Draagt de staging de rol Hop (`vboard::slots::staged_role`,
//! image/qemu-run.sh), dan plaatst de kern het image één keer, in slot 1,
//! als de bevoorrechte bewoner (PORT.md beslissing 1 en 2): `Placement::hop`
//! (sharegroup `hop`, die de OS-core met de kern deelt: Hop draait in de
//! idle van de kern, `cpu::el2::OsCore`), 64 MiB, de env met de `HOPOS_*`-keuzes,
//! en het `Privilege`-token hoort bij slot 1 vanaf de boot (`main`). Zijn
//! start draagt de uplink-poorten (8080 agent, 9080 leader), die de
//! lifecycle doorzet zoals die van een jobspec.
//!
//! Eén taak is de supervisor van Hop ([`Supervisor`]): plaatsen, bewaken
//! (elke wissel één regel), en herstarten alleen als Hop erom vraagt:
//!
//! - **een HOP_RESTART van Hop zelf** (de haak [`hop_restart`] stuurt de
//!   supervisor een bericht): Hop schrijft zijn antwoord en zijn staat weg
//!   en vertrekt zelf; de kern wacht daar hoogstens zijn termijn op, en
//!   belt pas daarna (`HOPOS_HOP_STOPPED reason=request left=self|bell`,
//!   geen `HOPOS_HOP_EXIT`: die exit is gevraagd), dan het nieuwe of
//!   hetzelfde image (`HOPOS_HOP_RESTART reason=request`). Elke nieuwe Hop
//!   krijgt hetzelfde slot, dezelfde env, poorten, volume en `Placement`,
//!   en dus de bevoegdheid (het token hoort bij slot 1), en vindt zijn
//!   staat in de kern terug (`STATE_GET`; `HOPOS_HOP_STATE kept=` zegt
//!   hoeveel er klaarligt). De andere bewoners draaien door.
//! - **een Hop die faalt** (een fault, een exit die niemand vroeg, of een
//!   nieuw image dat niet plaatst) is een node die faalt (Derek, 23-10):
//!   de diagnoseregel (`HOPOS_HOP_FAULT`, `HOPOS_HOP_EXIT`), `HOPOS_HOP_DOWN`
//!   en `fatal` (docs/sequence-hopos.md §13.4). Hop blijft liggen zoals hij
//!   viel, de kern parkeert, de pets stoppen, de hardware reset, en de
//!   koude boot start de Hop van het bootmedium. Falen gaat
//!   naar het oude: is het ijzer de oorzaak, dan ontdoet een verse start in
//!   dezelfde boot hem juist niet van zijn fout. Geen back-off, geen
//!   terugval op een vorig image.

extern crate alloc;

// De kooi per architectuur: `cage.rs` (stage-2 en de EL2-switcher) of
// `cage_riscv.rs` (PMP plus Sv39 en de M-mode-switcher), onder het beleid
// van `kooi.rs`, dat de kern de traits geeft; de lifecycle hieronder is
// architectuur-neutraal.
#[cfg_attr(not(target_arch = "riscv64"), path = "cage.rs")]
#[cfg_attr(target_arch = "riscv64", path = "cage_riscv.rs")]
mod cage;

/// De architectuur-naad van deze lijm: wat op arm64 `cpu::el2` is (de
/// toestand van een core, de kick van een slot, de OS-core), is op riscv64
/// van de kooi-lijm zelf (`cage_riscv.rs`). Op module-niveau, zodat de
/// lijm eronder geen `cfg` kent (handboek §7).
#[cfg(not(target_arch = "riscv64"))]
mod arch {
    use super::cage::FLAVOR;
    use board::Board;
    use cpu::el2;
    use cpu::println;

    pub(super) use cpu::el2::{OsCore, core_state};

    /// De kick van `slot` na een schrijf in zijn RX-ring: op de WFE-smaken
    /// de SEV, die elke WFE-slaper wekt, dus het slot kiest geen doel; die
    /// gaf de switch zelf al (`dev::notify` in `Core::wake`, net vóór deze
    /// haak), een tweede `dsb sy; sev` per frame is loos (03-10). Op Apple
    /// de fast IPI naar de core van het slot (`wakeRX`).
    pub(super) fn wake(slot: usize) {
        if matches!(FLAVOR, el2::Flavor::AppleVhe) {
            super::cage::wake_rx(slot);
        }
    }

    pub(super) use super::cage::start_waker;

    /// Kan de EL2-smaak de OS-core delen (met Hop en de groep `system`)?
    /// Apple niet.
    pub(super) const SHARES_OS_CORE: bool = !matches!(FLAVOR, el2::Flavor::AppleVhe);

    /// De rotatie van de OS-core, na de zelftest van de overgang (timer,
    /// yield, kick).
    pub(super) fn os_core(plan: &abi::layout::Plan) -> Result<el2::OsCore, el2::Error> {
        let board = &crate::BOARD;
        let bell = board.os_bell();
        let mut os = el2::OsCore::new(plan, FLAVOR, Some(bell))?;
        let ms = cpu::idle::freq() / 1000;
        let t = probe(&mut os, false, ms, &|| {});
        let y = probe(&mut os, true, 100 * ms, &|| {});
        let k = probe(&mut os, false, 100 * ms, &|| board.kick_self());
        let back = |r: &Tried| r.probe.map(|p| p.back);
        let ok = back(&t) == Some(el2::Back::Timer)
            && back(&y) == Some(el2::Back::Yield)
            && back(&k) == Some(el2::Back::Ipi);
        let show = |r: Tried| Shown {
            tried: r,
            ms,
            kick: bell.intid,
        };
        println!(
            "oscore: cpu {} self-test timer={} yield={} kick={} {}",
            plan.os_core(),
            show(t),
            show(y),
            show(k),
            if ok {
                "HOPOS_OS_SELFTEST ok"
            } else {
                "HOPOS_OS_SELFTEST_FAIL"
            }
        );
        // Een rotatie die haar zelftest niet haalt, host niet (F14, §13.3).
        if !ok {
            cpu::fatal!("oscore: the self-test of the rotation failed");
        }
        Ok(os)
    }

    /// Eén proef en hoe vaak hij het deed.
    #[derive(Copy, Clone)]
    struct Tried {
        probe: Option<el2::Probe>,
        tries: u32,
    }

    /// Eén proef van de zelftest, met eerst de interrupts die al wachten
    /// afgehandeld (`crate::drain_interrupts`), en opnieuw als een
    /// device-lijn hem onderbrak (`el2::selftest_tries`, waarom staat
    /// daar).
    fn probe(os: &mut el2::OsCore, yield_: bool, ticks: u64, kick: &dyn Fn()) -> Tried {
        let (probe, tries) = el2::selftest_tries(
            crate::drain_interrupts,
            || os.selftest(yield_, ticks, kick),
            |p| p.back,
        );
        Tried { probe, tries }
    }

    /// Een proef voor de zelftest-regel: `(Timer, 1344 us)`, en bij een
    /// onderbreking de vectorindex, de INTID die de controller na de
    /// terugkeer liet zien, een lijn die al vóór de overgang pending stond,
    /// en het aantal pogingen.
    struct Shown {
        tried: Tried,
        ms: u64,
        kick: u32,
    }

    impl core::fmt::Display for Shown {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let Some(p) = self.tried.probe else {
                return f.write_str("(none: no heap)");
            };
            let us = p.ticks.saturating_mul(1000) / self.ms.max(1);
            write!(f, "({:?}, {us} us", p.back)?;
            if !matches!(p.back, el2::Back::Timer | el2::Back::Yield | el2::Back::Ipi) {
                write!(f, ", vec {}, INTID {}", p.vec, p.after)?;
            }
            if let Some(i) = p.stale(self.kick) {
                write!(f, ", INTID {i} pending before entry")?;
            }
            if self.tried.tries > 1 {
                write!(f, ", try {}", self.tried.tries)?;
            }
            f.write_str(")")
        }
    }
}

#[cfg(target_arch = "riscv64")]
mod arch {
    pub(super) use super::cage::{OsCore, SHARES_OS_CORE, core_state, os_core};

    /// De kick na een RX-schrijf: de bel van elk app-hart.
    pub(super) fn wake(_slot: usize) {
        super::cage::wake_all();
    }

    /// Geen wekker: een app-hart slaapt op de CLINT-timer van zijn
    /// switcher.
    pub(super) fn start_waker(_exec: &'static executor::Executor, _plan: &abi::layout::Plan) {}
}

use crate::clock::ExecTimer;
use crate::glue::{DevMem, KernConsole, SlotOutbox};
use abi::layout::{CtxState, RING_DATA_CAP};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use board::Board;
use board::stage::StagedRole;
use cage::SlotCores;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use cpu::el2::{self, CoreState};
use cpu::println;
use executor::Executor;
use kern::nodecfg::NodeCfg;
use kern::partmem::{Geometry, PartitionPool};
use kern::pool::{Placement, Places};
use kern::slots::{
    Envelope, Lifecycle, Mount, Reply, Request, Response, ServicerCtl, Servicers, SlotStatus,
    StartSpec, call, servicer_task,
};
use kern::system::Fail;
use kern::{Region, Slot};
use sync::mpsc::Mailbox;
use sync::{Local, Signal, select};
use vboard::slots as vslots;

/// De brievenbus van de lifecycle-actor. Hij staat in `main` naast de
/// system-listener, die er zijn verzoeken heen stuurt; de actor die hem
/// leest, woont hier.
static INBOX: &Mailbox<Envelope<'static>, { crate::LIFECYCLE_DEPTH }> = &crate::LIFECYCLE;

/// De servicer-tabel: de actor schrijft, de servicers en `admit` van de
/// system-listener lezen.
static SERVICERS: &Servicers = &crate::SERVICERS;

/// De antwoordplek van de supervisor van Hop ([`HopNode::supervise`]):
/// `ask`, `status`, `stop`, `place` en `drop_streams`. Eén aanroeper: de
/// supervisor is één taak die op zijn antwoord wacht vóór de volgende
/// vraag. Een andere taak die de actor iets vraagt, krijgt een eigen plek:
/// een [`Reply`] kent één wachter, en een tweede `recv` verdringt de waker
/// van de eerste, die dan voorgoed wacht.
static BOOT_REPLY: Reply = Reply::new();

/// Het slot van Hop: de plaatsing bij boot, en het slot dat de bevoegdheid
/// draagt (`main` slaat het token vóór de system-listener start).
pub(crate) const HOP_SLOT: usize = 1;

/// De partitie van Hop, staart inbegrepen. 64 MiB is ruim: de release-ELF
/// is 594 KiB laadbaar (gemeten 29-09), de rest is heap voor agent, leader,
/// de HTTP-verbindingen en de download-buffer (64 KiB per hap). Op de
/// LicheeRV (256 MB, pool 200 MB) 10 MiB: Hop meet er 0,5 tot 0,9 MB
/// (03-10, slot 1 als systeemtaak), en elke MB is daar een app-MB. Het
/// board zegt het (`Board::HOP_MEM`).
const HOP_MEM: u64 = <crate::Machine as board::Board>::HOP_MEM;

/// Het volume van Hop: `/hop` in zijn zicht, `/volumes/hop` op hopfs. Daar
/// bewaart `agentd-hopos` zijn `agent-state.json` en leest hij hem na een
/// herstart terug (`Node::restore`); zijn eigen root (`/.tasks/slot1`) is
/// bij elke start leeg, het volume niet.
const HOP_VOLUME: (&[u8], &[u8]) = (b"/hop", b"/volumes/hop");

/// De agent-poort van Hop; de leader luistert op poort + 1000 (zoals Go).
/// Beide worden op de uplink doorgezet ([`HOP_PORTS`]).
pub(crate) const HOP_PORT: u16 = 8080;

/// De poorten van Hop op de uplink: de poorten van zijn start, zoals die
/// van een jobspec (`StartSpec::ports`). De lifecycle zet ze door
/// (`Cage::publish`, tcp en udp) en draagt ze over een flip, waar de
/// switch van de nieuwe kern ze overneemt (`net::Carried`). Tot 04-10 zette de kern ze
/// buiten de lifecycle door, alleen tcp, met twee eigen lussen.
const HOP_PORTS: [u16; 2] = [HOP_PORT, HOP_PORT + 1000];

/// Het ritme van de bewaking van Hop: een regel per wissel, en om de
/// [`HOP_BEAT_EVERY`] rondes een hartslagregel.
const HOP_WATCH: Duration = Duration::from_secs(1);

/// Om de hoeveel rondes van [`HOP_WATCH`] de bewaking de hartslag toont.
const HOP_BEAT_EVERY: u64 = 30;

/// De buffer van een servicer: één maximaal record (de halve outbox).
const SERVICE_BUF: usize = (RING_DATA_CAP / 2) as usize;

/// Start de slot-machinerie op de executor van core 0: de kooi, de actor,
/// een servicer per slot, en de plaatsing van het gestagede image. Faalt
/// een stap, dan één regel met marker en draait de node door zonder slots:
/// het netwerk en de console hebben er niets mee te maken.
///
/// Met `adopt` (het hok van de vorige kern, de warme flip) neemt de kooi de
/// zittende switch-code over in plaats van hem te installeren, krijgt elke
/// bewoner zijn kooi, ringen en servicer terug zonder dat er één byte van
/// zijn wereld verandert, en plaatst de kern Hop niet opnieuw: Hop draait
/// door (`HOPOS_FLIP_ADOPT`). Een bewoner die niet terugkomt, is
/// `HOPOS_FATAL`: een half hok is erger dan een koude boot.
///
/// Hop krijgt zijn env uit `hopos.cfg`. `system` is de system-API van deze
/// boot: de supervisor van Hop leest er de maat van Hop's staat uit en
/// ruimt er de stromen van een gestopte Hop op.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(
    exec: &'static Executor,
    role: Result<StagedRole, u64>,
    adopt: Option<Vec<kern::slots::SlotState>>,
    cfg: NodeCfg<'static>,
    system: &'static crate::KernSystem,
) {
    let board = &crate::BOARD;
    // FLIP: een geadopteerde Hop zit waar hij zat, met zijn groep en core.
    let carried = adopt
        .as_ref()
        .and_then(|v| v.iter().find(|st| st.slot == HOP_SLOT));
    init_hop_group(&cfg, carried);
    let hop_core = carried.map(|st| st.core);
    let plan = match os_plan() {
        Ok(p) => p,
        Err(e) => {
            println!("slots: plan refused: {e} HOPOS_SLOT_PLAN");
            return;
        }
    };
    // Een geadopteerd hok krijgt geen byte in de plan-regio: er draaien
    // cores in de switch-code (`cpu::el2::adopt` eist de som).
    let mut cage = match &adopt {
        Some(_) => cage::adopt(plan.clone())
            .unwrap_or_else(|e| cpu::fatal!("slots: cage adoption: {e} HOPOS_FLIP_ADOPT_FAIL")),
        None => match cage::new(plan.clone()) {
            Ok(c) => c,
            Err(e) => {
                println!("slots: cage init: {e} HOPOS_CAGE_FAIL");
                return;
            }
        },
    };
    for st in adopt.iter().flatten() {
        let r = Slot::new(st.slot).map_or(Err(kern::cage::CageError { code: 0 }), |s| {
            // De SMP-breedte mee, anders vindt een revoke de secundaires
            // van een SMP-app niet.
            cage.adopt_slot(
                s,
                Region::new(st.part_base, st.part_size),
                st.core,
                st.cores,
            )
        });
        if let Err(e) = r {
            cpu::fatal!(
                "slots: slot {} not re-attached: cage code {} HOPOS_FLIP_ADOPT_FAIL",
                st.slot,
                e.code
            );
        }
    }
    let sw = cage.isa().installed();
    println!(
        "slots: cage up HOPOS_CAGE_UP region={:#x} switch={:#x}+{:#x} tramp={:#x} hash={:#x} app-cores={}",
        plan.vec_base_pa().0,
        sw.entry.0,
        sw.len,
        sw.tramp.0,
        sw.hash,
        plan.app_cores()
    );
    arch::start_waker(exec, &plan);
    // De eerste meting: na de init moet elke app-core koud staan (mailbox
    // 0). Een ander woord is een core die al iets doet, of een map die de
    // regio niet raakt.
    for c in 1..=plan.app_cores() {
        if let Some(core) = abi::layout::Core::new(c) {
            println!(
                "slots: core {c} mailbox {:?}",
                arch::core_state(&plan, core)
            );
        }
    }

    let mut pool: Vec<Region> = Vec::new();
    if pool.try_reserve_exact(plan.pool().len()).is_err() {
        println!("slots: out of memory for the pool HOPOS_SLOT_PLAN");
        return;
    }
    pool.extend(plan.pool().iter().map(|r| Region::new(r.base, r.size)));
    let kern_ram = board.plan();
    let own = Region::new(
        kern_ram.kern_ram.base.0,
        kern_ram.dma.end().0 - kern_ram.kern_ram.base.0,
    );
    let geo = Geometry {
        link_window: cage::link_window,
        reserve: cage::reserve,
    };
    let parts = match PartitionPool::new(&pool, own, geo) {
        Ok(p) => p,
        Err(e) => {
            println!("slots: partition pool: {e} HOPOS_SLOT_PLAN");
            return;
        }
    };
    println!(
        "slots: pool {} MB in {} regions, largest {} MB",
        parts.capacity() >> 20,
        pool.len(),
        parts.largest() >> 20
    );
    // Wat Hop als pool ziet: de capaciteit min wat de codec straks uit de
    // pool reserveert (hopos/src/codec.rs), anders overschat Hop de node.
    let pool_bytes = parts
        .capacity()
        .saturating_sub(crate::codec::planned_bytes());
    let cores = SlotCores::new(plan.clone());
    // De actor buiten de future gebouwd (30-09): binnen `async move` stond
    // de verse `Lifecycle` als tijdelijke waarde in het frame van de
    // poll-functie, en dat frame (57 KB) reserveerde elke poll van de
    // lifecycle opnieuw, bovenop de rest van de stack. En in een `Box`
    // (02-10): als waarde in de future stond hij (54 KB) nog een keer in
    // de future van 56 KB, en die twee kopieën in het frame van `setup`
    // brachten de boot-stack met de display-smaak over zijn wachtpagina.
    let mut lc = Box::new(Lifecycle::new(
        cage,
        cores,
        ExecTimer(exec),
        KernConsole,
        parts,
        os_places(),
        SERVICERS,
        // De device-grants (gui.rs): de framebuffer in de gui-smaak,
        // kaal niets.
        crate::gui::slot_grants(),
    ));
    // Eerst alle eigendomsclaims terug, dan pas verzoeken: vóór de actor
    // zijn eerste bericht leest. De poorten pas als het net luistert.
    if let Some(states) = &adopt {
        match lc.adopt(states) {
            Ok(n) => println!(
                "HOPOS_FLIP_ADOPT {n} of {} resident(s) adopted, ownership restored before the first request",
                states.len()
            ),
            Err(e) => cpu::fatal!("slots: adoption refused: {e} HOPOS_FLIP_ADOPT_FAIL"),
        }
    }
    let adopted = adopt.is_some();
    let actor = async move {
        if adopted {
            crate::net::NET_READY.wait().await;
            lc.republish().await;
        }
        lc.run(INBOX).await
    };
    if let Err(e) = exec.spawn(actor) {
        println!("slots: lifecycle not spawned: {e:?} HOPOS_SLOT_SPAWN");
        return;
    }
    // Een geadopteerde Hop leeft nog; alleen de supervisor komt terug.
    let hop = HopNode {
        exec,
        plan,
        pool_bytes,
        cfg,
        system,
    };
    if let Some(core) = hop_core {
        if let Err(e) = exec.spawn(hop.supervise(Some(core))) {
            println!("slots: Hop supervisor not spawned: {e:?} HOPOS_SLOT_SPAWN");
        }
        return;
    }
    let spawned = match role {
        Ok(StagedRole::Hop) => exec.spawn(hop.supervise(None)),
        // O7: de kern plaatst alleen Hop.
        Ok(StagedRole::App) => {
            println!(
                "slots: the staging is no Hop, nothing placed (the kern places only Hop) HOPOS_SLOT_NONE"
            );
            return;
        }
        Err(word) => {
            println!(
                "slots: staged role word {word:#x} is neither app (0) nor hop (1), nothing placed HOPOS_SLOT_NONE"
            );
            return;
        }
    };
    if let Err(e) = spawned {
        println!("slots: first placement not spawned: {e:?} HOPOS_SLOT_SPAWN");
    }
}

/// [`HOP_PORTS`] als lijst; leeg als de heap op is (dan opent er niets, en
/// zegt de ontbrekende `HOPOS_SLOT_PUBLISH` het).
fn hop_ports() -> Vec<u16> {
    let mut v = Vec::new();
    if v.try_reserve_exact(HOP_PORTS.len()).is_ok() {
        v.extend_from_slice(&HOP_PORTS);
    }
    v
}

/// Deelt Hop de OS-core? Dan wacht de switch niet op zijn RX-ring
/// ([`resident`]). Eén schrijver (de plaatsing van Hop), gelezen door de
/// switch op dezelfde core. Alleen Hop zet het: wie verder de OS-core deelt
/// (een groep `system`), zette het ook vóór 01-11 niet.
static HOP_ON_OS: AtomicBool = AtomicBool::new(false);

/// Deelt de bewoner van `slot` de OS-core (Hop in slot 1)? De
/// `resident` van de switch: zijn consument draait pas als de executor
/// afgeeft, dus wachten op ruimte in zijn ring is stilstand (30-09).
pub(crate) fn resident(slot: usize) -> bool {
    slot == HOP_SLOT && HOP_ON_OS.load(Relaxed)
}

/// Zet of wist Hop (`slot`) als bewoner van de OS-core.
fn set_resident(slot: usize, on: bool) {
    if slot == HOP_SLOT {
        HOP_ON_OS.store(on, Relaxed);
    }
}

/// De kick van slot `slot` na een schrijf in zijn RX-ring (de `slot_wake`
/// van de switch); zie `arch::wake`.
pub(crate) fn wake(slot: usize) {
    arch::wake(slot);
}

/// De meetlat van de wekker van de app-cores (`waker.go` `WakerStats`),
/// cumulatief, op de tikregel: rondes, geyielde bewoners gezien, kicks van
/// de wekker en gerichte RX-kicks. Zonder deze vier is "de app slaapt en
/// wordt nooit gewekt" niet te onderscheiden van "de app spint". Alleen
/// Apple telt; elders blijven ze 0.
pub(crate) struct WakerStats {
    pub(crate) rounds: AtomicU64,
    pub(crate) seen: AtomicU64,
    pub(crate) kicks: AtomicU64,
    pub(crate) rx: AtomicU64,
}

/// De tellers van de wekker.
pub(crate) static WAKER: WakerStats = WakerStats {
    rounds: AtomicU64::new(0),
    seen: AtomicU64::new(0),
    kicks: AtomicU64::new(0),
    rx: AtomicU64::new(0),
};

/// Spawnt de servicer-taak van één levensduur van `slot` (`Cage::serve`
/// van de kooi), met zijn eigen recordbuffer: bij de plaatsing gemaakt, weg
/// na zijn evict. Een buffer of taak die de heap niet geeft, is een
/// weigering van de start.
pub(crate) fn spawn_servicer(slot: Slot, ctx: dev::Pa, ctl: Rc<ServicerCtl>) -> Result<(), ()> {
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(SERVICE_BUF).is_err() {
        println!("slot {slot}: no servicer buffer HOPOS_SLOT_SPAWN");
        return Err(());
    }
    buf.resize(SERVICE_BUF, 0);
    let exec = crate::EXEC.get();
    exec.spawn(servicer(exec, slot, ctx, ctl, buf))
        .map_err(|e| {
            println!("slot {slot}: servicer not spawned: {e:?} HOPOS_SLOT_SPAWN");
        })
}

/// De servicer-taak van één levensduur.
async fn servicer(
    exec: &'static Executor,
    slot: Slot,
    ctx: dev::Pa,
    ctl: Rc<ServicerCtl>,
    mut buf: Vec<u8>,
) {
    let timer = ExecTimer(exec);
    servicer_task(
        slot,
        ctl,
        |part| SlotOutbox::open(slot, part, ctx),
        &timer,
        &KernConsole,
        INBOX,
        &mut buf,
    )
    .await;
}

/// Stuurt één verzoek naar de actor, met het antwoord op [`BOOT_REPLY`]
/// (de supervisor en de boot-plaatsing).
async fn ask(req: Request) -> kern::Result<Response> {
    ask_at(&BOOT_REPLY, req).await
}

/// Stuurt één verzoek naar de actor, met het antwoord op `reply`: een plek
/// met op dit moment geen andere wachter.
async fn ask_at(reply: &'static Reply, req: Request) -> kern::Result<Response> {
    match call(INBOX, reply, req).await {
        Ok(Response::Failed(e)) | Err(e) => Err(e),
        Ok(r) => Ok(r),
    }
}

async fn status(slot: Slot) -> Option<SlotStatus> {
    match ask(Request::Status(slot)).await {
        Ok(Response::Status(s)) => Some(s),
        _ => None,
    }
}

/// De naam van deze node zonder NIC als `hopos.cfg` geen `hopos.node` zet
/// (Go: `cfg.Node.ID = "hopos-1"`); QEMU zet de zijne in
/// `kern::nodecfg::QEMU_CFG`.
const HOP_NODE: &str = "hopos-1";

/// De naam van deze node in Hop's cluster als de config geen `hopos.node`
/// zet: het board en de staart van de uplink-MAC
/// (`kern::nodecfg::default_node`), zodat elke node op de gedeelde config
/// (image/cfg) een eigen naam heeft. Eén regel op de console als hij
/// gebruikt wordt.
fn default_node(cfg: &NodeCfg<'_>) -> String {
    let Some(mac) = crate::net::uplink_mac() else {
        return HOP_NODE.into();
    };
    let name = kern::nodecfg::default_node(<crate::Machine as Board>::NAME, mac);
    if cfg.one("hopos.node").is_empty() {
        println!(
            "slots: no hopos.node, this node is {name} (board and uplink MAC) HOPOS_NODE_DEFAULT"
        );
    }
    name
}

/// Wat de supervisor van Hop van de boot meekrijgt: genoeg om Hop's env en
/// core-vraag te bouwen ([`HopNode::spec`]) en zijn staat te zien.
struct HopNode {
    exec: &'static Executor,
    plan: abi::layout::Plan,
    pool_bytes: u64,
    cfg: NodeCfg<'static>,
    system: &'static crate::KernSystem,
}

impl HopNode {
    /// De supervisor van Hop: plaatsen (of, na een warme flip, de
    /// meegekomen Hop overnemen op `resumed`, zijn core), bewaken, en bij
    /// een HOP_RESTART zijn vertrek afwachten en opnieuw plaatsen
    /// ([`Supervisor`]).
    async fn supervise(self, resumed: Option<usize>) {
        let Some(slot) = Slot::new(HOP_SLOT) else {
            return;
        };
        if let Some(core) = resumed {
            set_resident(slot.get(), core == 0);
        } else if staged_hop().is_none() {
            cpu::fatal!("slots: role hop but no staged image, Hop not started");
        }
        let node_ip = wait_uplink().await;
        let spec = self.spec(node_ip, resumed.is_some());
        let sup = Supervisor {
            exec: self.exec,
            plan: self.plan,
            slot,
            system: self.system,
            spec,
            current: Image::Staged,
            restarts: 0,
        };
        let core = match resumed {
            Some(core) => {
                println!(
                    "slot {slot}: Hop carried over the flip on core {core}, not restarted HOPOS_HOP_RESUMED"
                );
                resumed_image();
                core
            }
            None => match sup.put().await {
                Ok(core) => core,
                Err(e) => cpu::fatal!("slot {slot}: Hop not started: {e}"),
            },
        };
        sup.run(core).await;
    }

    /// De env en de core-vraag van Hop, zoals bij elke plaatsing. Op de
    /// boot is een fout het einde (`HOPOS_HOP_FAIL`, zoals altijd); na een
    /// flip draait de meegekomen Hop door, maar kan hij niet herstart
    /// worden.
    fn spec(&self, node_ip: Option<core::net::Ipv4Addr>, resumed: bool) -> Option<HopSpec> {
        let cut = if resumed {
            "Hop runs on but cannot be restarted HOPOS_HOP_RESTART_REFUSED no spec"
        } else {
            "Hop not started HOPOS_HOP_FAIL"
        };
        // De env van Hop komt uit de config van het board (kern::nodecfg).
        let node = default_node(&self.cfg);
        let hop_group = kern::pool::hop_group();
        let facts = kern::nodecfg::Facts {
            default_node: &node,
            node_ip,
            dns: crate::net::uplink_dns(),
            port: HOP_PORT,
            app_cores: self.plan.app_cores(),
            hop_on_os: hop_on_os(),
            os_shared: arch::SHARES_OS_CORE,
            hop_group: core::str::from_utf8(hop_group.as_slice()).unwrap_or("hop"),
            pool_bytes: self.pool_bytes,
            hop_mem: HOP_MEM,
        };
        let env = match crate::config::hop_env(&self.cfg, &facts) {
            Ok(e) => e,
            Err(e) => {
                println!("slots: Hop env: {e}, {cut}");
                return None;
            }
        };
        match hop_placement(&self.plan, &self.cfg) {
            Ok(at) => Some(HopSpec { env, at }),
            Err(e) => {
                println!("slots: Hop placement: {e}, {cut}");
                None
            }
        }
    }
}

/// FLIP: wat een herstart met hetzelfde image van een meegekomen Hop zou
/// plaatsen. Een image dat Hop de vorige kern gaf (HOP_RESTART), lag in
/// diens heap en kwam niet mee; deze kern kent alleen het gestagede, als
/// dat er nog is.
fn resumed_image() {
    match staged_hop() {
        Some(img) => println!(
            "slots: a restart of Hop with the same image places the staged one on this kernel ({} bytes); an image Hop gave the previous kernel stayed behind HOPOS_HOP_IMAGE_STAGED",
            img.len()
        ),
        None => println!(
            "slots: no staged Hop image left (the staging went to the flip): Hop runs on, a restart needs a new image HOPOS_HOP_IMAGE_NONE"
        ),
    }
}

/// Wat de supervisor bewaart om Hop opnieuw te plaatsen: de env (uit
/// `hopos.cfg` en de feiten van de node) en de core-vraag. Volume, poorten
/// en partitiemaat zijn constanten ([`HOP_VOLUME`], [`HOP_PORTS`],
/// [`HOP_MEM`]), en het slot is [`HOP_SLOT`]: daar hoort het
/// `Privilege`-token bij, dus elke Hop in dat slot is bevoegd.
struct HopSpec {
    env: kern::nodecfg::EnvBlob,
    at: Placement,
}

/// Het image van een Hop.
enum Image {
    /// Het gestagede ([`staged_hop`]), bij elke plaatsing
    /// opnieuw opgezocht.
    Staged,
    /// Een image dat Hop gaf (HOP_RESTART), in de kern-heap; het blijft
    /// daar voor een volgende herstart met "hetzelfde image".
    Taken(Vec<u8>),
}

impl Image {
    /// De bytes, of `None` als het gestagede image weg is.
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Image::Staged => staged_hop(),
            Image::Taken(v) => Some(v),
        }
    }

    fn source(&self) -> Source {
        match self {
            Image::Staged => Source::Staged,
            Image::Taken(_) => Source::Taken,
        }
    }
}

/// Waar "hetzelfde image" nu vandaan komt, voor de haak.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Source {
    Staged,
    Taken,
}

/// Een aangenomen HOP_RESTART: het nieuwe image (`None`: hetzelfde) en de
/// termijn van de nette stop.
struct Asked {
    image: Option<Vec<u8>>,
    grace: Duration,
}

/// De aangenomen herstart, van de haak ([`hop_restart`]) naar de
/// supervisor. Beide op de executor van de OS-core, zoals de flip
/// (`flip::PENDING`): een bericht, geen gedeelde tabel.
static ASKED: Local<Cell<Option<Asked>>> = Local::new(Cell::new(None));
/// De bel van de supervisor bij een aangenomen herstart.
static ASK_BELL: Signal = Signal::new();
/// Wat de supervisor nu zou plaatsen als "hetzelfde image", gezet zolang
/// hij een Hop bewaakt die hij kan herstarten; `None` daarbuiten. De haak
/// leest het om vooraf te weigeren in plaats van na zijn `Ok`.
static SAME: Local<Cell<Option<Source>>> = Local::new(Cell::new(None));

/// Hoe vaak de supervisor kijkt of een Hop die om zijn herstart vroeg al
/// vertrok. Er is geen bel voor de exit van een bewoner (de bewaking pollt
/// zijn status net zo, maar per seconde); 50 ms houdt de herstart kort
/// zonder de lifecycle-actor te belasten: hoogstens 600 vragen in de
/// langste termijn.
const LEAVE_POLL: Duration = Duration::from_millis(50);

/// Het vangnet als Hop binnen zijn termijn niet zelf vertrok: de stop-bel
/// met deze korte termijn, dan de intrekking. Hop had zijn kans al.
const BELL_STOP: Duration = Duration::from_secs(1);

/// De stop van een Hop die al vertrok: niets om netjes af te sluiten, de
/// kooi is al stil; de stop geeft partitie en poorten vrij.
const DEAD_STOP: Duration = Duration::from_secs(1);

/// Hoe een bewaking van Hop eindigde.
enum Watched {
    /// Een fault of een exit die niemand vroeg; de diagnoseregel staat al
    /// op de console.
    Crashed,
    /// Hop vroeg een herstart (HOP_RESTART).
    Asked(Asked),
}

/// De supervisor van Hop: één taak, die Hop plaatst, bewaakt en op zijn
/// vraag herstart. Hij bezit alles wat een herstart nodig heeft (de spec en
/// het image); de haak stuurt hem een bericht ([`ASKED`]).
struct Supervisor {
    exec: &'static Executor,
    plan: abi::layout::Plan,
    slot: Slot,
    system: &'static crate::KernSystem,
    /// `None`: deze Hop kan niet herstart worden (geen env na een flip).
    spec: Option<HopSpec>,
    /// Het image van de Hop die nu draait.
    current: Image,
    /// Herstarts in deze boot, voor `n=` in de marker.
    restarts: u64,
}

impl Supervisor {
    /// De lus: bewaken, en na elke vraag de volgende Hop. Een Hop die
    /// valt, of een nieuwe die niet plaatst, is het einde: `fatal`, en de
    /// watchdog reset de node (docs/sequence-hopos.md §13.4).
    async fn run(mut self, mut core: usize) {
        loop {
            SAME.set(self.spec.as_ref().map(|_| self.current.source()));
            let end = self.watch(core).await;
            SAME.set(None);
            match end {
                Watched::Crashed => self.stay_down(),
                Watched::Asked(a) => self.after_ask(a).await,
            }
            core = self.replace().await;
        }
    }

    /// Hop viel: hij blijft liggen zoals hij viel, voor de diagnose, en de
    /// node valt mee (`fatal`). Een herstart die hij vlak daarvoor nog
    /// vroeg, vervalt.
    fn stay_down(&self) -> ! {
        let slot = self.slot;
        if ASKED.take().is_some() {
            println!(
                "slot {slot}: the restart Hop asked for is dropped, he went down first HOPOS_HOP_RESTART_REFUSED crashed"
            );
        }
        println!(
            "slot {slot}: Hop stays down as he fell; a failing Hop is a failing node, the node watchdog resets to the Hop of the boot medium HOPOS_HOP_DOWN"
        );
        cpu::fatal!("slot {slot}: Hop fell")
    }

    /// Bewaakt Hop: elke wissel van app-status, core of fault één regel, om
    /// de [`HOP_BEAT_EVERY`] seconden de hartslag. Eindigt bij een fault of
    /// exit (luid, met de diagnose) of een gevraagde herstart.
    async fn watch(&self, core: usize) -> Watched {
        let slot = self.slot;
        let t0 = self.exec.now();
        let mut last: Option<(u64, u64, bool)> = None;
        let mut round: u64 = 0;
        loop {
            if let Some(a) = ASKED.take() {
                return Watched::Asked(a);
            }
            let Some(st) = status(slot).await else {
                cpu::fatal!("slot {slot}: Hop status unavailable");
            };
            let dt = self.exec.now().saturating_sub(t0);
            let seen = (st.cage.app, st.cage.fault_vec, st.cage.core_on);
            round = round.wrapping_add(1);
            if last != Some(seen) || round.is_multiple_of(HOP_BEAT_EVERY) {
                probe(&self.plan, slot, core, &st, dt);
                last = Some(seen);
            }
            if crashed(slot, &st) {
                return Watched::Crashed;
            }
            // Wakker op de bel of de volgende ronde: een herstart wacht
            // niet op de poll.
            let _ = select(ASK_BELL.wait(), self.exec.after(HOP_WATCH)).await;
        }
    }

    /// Na een aangenomen HOP_RESTART: Hop vertrekt zelf (hij schrijft eerst
    /// zijn antwoord en zijn staat weg en keert dan terug uit `main`), zoals
    /// een systemd-dienst die om zijn eigen herstart vraagt en dan zelf
    /// eindigt. De kern wacht daar hoogstens zijn termijn op; pas daarna de
    /// stop-bel als vangnet. Tot 22-10 stopte de kern hem na een vaste halve
    /// seconde, en een Hop die nog met SNTP bezig was, schreef zijn 202 nooit
    /// (de ring van de herstart: curl HTTP 000). Het nieuwe image wordt het
    /// huidige; het oude gaat weg (een falende nieuwe Hop valt terug op de
    /// koude boot, niet op een vorig image).
    async fn after_ask(&mut self, a: Asked) {
        let slot = self.slot;
        self.restarts = self.restarts.wrapping_add(1);
        let what = match &a.image {
            Some(img) => alloc::format!("a new image of {} bytes", img.len()),
            None => String::from("the same image"),
        };
        println!(
            "slot {slot}: Hop asked for a restart with {what}, waiting up to {} ms for him to leave HOPOS_HOP_RESTART reason=request image={} n={}",
            a.grace.as_millis(),
            if a.image.is_some() { "new" } else { "same" },
            self.restarts
        );
        // Zijn exit is hier verwacht, dus geen HOPOS_HOP_EXIT; een fault
        // onderweg blijft luid.
        let left = self.wait_leave(a.grace).await;
        let timeout = if left == "bell" {
            println!(
                "slot {slot}: Hop did not leave within {} ms, the stop bell with {} ms as a backstop",
                a.grace.as_millis(),
                BELL_STOP.as_millis()
            );
            BELL_STOP
        } else {
            DEAD_STOP
        };
        if stop(slot, timeout).await {
            println!(
                "slot {slot}: Hop stopped for the restart HOPOS_HOP_STOPPED reason=request left={left}"
            );
        }
        if let Some(img) = a.image {
            self.current = Image::Taken(img);
        }
    }

    /// Wacht hoogstens `grace` tot Hop zelf vertrok. Geeft hoe:
    /// `self` (een exit), `fault` (luid, met de diagnose), `gone` (geen
    /// status meer) of `bell` (nog daar na de termijn).
    async fn wait_leave(&self, grace: Duration) -> &'static str {
        let slot = self.slot;
        let until = self.exec.now().saturating_add(grace.as_nanos() as u64);
        loop {
            let Some(st) = status(slot).await else {
                return "gone";
            };
            if st.cage.fault_vec != 0 || el1_fault(&st.cage).is_some() {
                crashed(slot, &st);
                return "fault";
            }
            if st.cage.app == abi::hopabi::AppStatus::Exited as u64 {
                return "self";
            }
            if self.exec.now() >= until {
                return "bell";
            }
            self.exec.after(LEAVE_POLL).await;
        }
    }

    /// De volgende Hop in het slot: de oude stop (een no-op als hij al
    /// gestopt is), zijn open stromen weg, de staat die klaarligt op de
    /// console, en plaatsen. Geeft de core; lukt een stap niet, dan is er
    /// geen Hop en is het `fatal`.
    async fn replace(&self) -> usize {
        let slot = self.slot;
        if !stop(slot, DEAD_STOP).await {
            cpu::fatal!("slot {slot}: the old Hop did not stop, no new one");
        }
        let n = self.system.drop_streams(&BOOT_REPLY).await;
        if n > 0 {
            println!(
                "slot {slot}: {n} open stream(s) of the old Hop dropped, their slots are back in the pool HOPOS_HOP_STREAMS_DROPPED"
            );
        }
        let kept = self.system.hop_state_len();
        println!(
            "slot {slot}: {kept} byte(s) of Hop state wait in the kern for the new Hop HOPOS_HOP_STATE kept={kept}"
        );
        match self.put().await {
            Ok(core) => core,
            Err(e) => cpu::fatal!("slot {slot}: the new Hop did not place: {e}"),
        }
    }

    /// Plaatst het huidige image in het slot van Hop, met de spec, het
    /// volume en de poorten van elke plaatsing (de switch moet er zijn,
    /// anders is er niets door te zetten en faalt de start op zijn poorten).
    /// Eén regel `HOPOS_HOP_START`. Geeft de core.
    async fn put(&self) -> Result<usize, Fail> {
        let slot = self.slot;
        let no = Fail::Kern(kern::Error::NoEnt);
        let spec = self.spec.as_ref().ok_or(no)?;
        let img = self.current.bytes().ok_or(no)?;
        let volume = kern_mounts(&[HOP_VOLUME])?;
        let ports = if crate::net::switch_up() {
            hop_ports()
        } else {
            Vec::new()
        };
        // Vóór de plaatsing: zodra de poort aan de switch hangt, mag hij
        // niet meer op Hop's ring wachten. Na de plaatsing volgt de echte
        // core.
        set_resident(slot.get(), hop_on_os());
        let env = spec.env.as_bytes();
        let entry = place(slot, img, HOP_MEM, spec.at.clone(), env, volume, ports).await?;
        let st = status(slot).await;
        let core = st.and_then(|s| s.core).map_or(0, |(c, _)| c.get());
        set_resident(slot.get(), core == 0);
        let part = st.and_then(|s| s.partition).unwrap_or_default();
        let cpu = abi::layout::Core::new(core).map_or(core, |c| self.plan.phys_core(c));
        println!(
            "HOPOS_HOP_START slot={slot} core={core} cpu={cpu} entry={entry:#x} part={:#x}+{:#x} image={} env={} class={}",
            part.base,
            part.size,
            img.len(),
            env.len(),
            crate::BOARD.core_class(cpu)
        );
        Ok(core)
    }
}

/// Is Hop gevallen? Een fault (op EL2 gezien of door de app op EL1
/// gemeld) of een exit, elk luid met de diagnose. De kern stopt hem pas
/// daarna, dus de regel zegt wat er in de kooi stond.
fn crashed(slot: Slot, st: &SlotStatus) -> bool {
    if st.cage.fault_vec != 0 {
        println!(
            "slot {slot}: Hop faulted vec={} esr={:#x} ({}) far={:#x} HOPOS_HOP_FAULT",
            st.cage.fault_vec - 1,
            st.cage.fault_esr,
            kern::cage::esr_class(st.cage.fault_esr),
            st.cage.fault_far
        );
        return true;
    }
    if let Some(line) = el1_fault(&st.cage) {
        println!("slot {slot}: Hop faulted at EL1 {line} HOPOS_HOP_FAULT");
        return true;
    }
    if st.cage.app == abi::hopabi::AppStatus::Exited as u64 {
        println!(
            "slot {slot}: Hop exited with {} HOPOS_HOP_EXIT",
            st.cage.exit_code
        );
        return true;
    }
    false
}

/// De haak van HOP_RESTART (`Hooks::hop_restart`, main.rs): toetst het
/// verzoek en het nieuwe image, kopieert het in de kern-heap en geeft het
/// de supervisor. Synchroon: na `Ok` gaat het slot van het image terug in
/// de pool. Elke weigering is één regel `HOPOS_HOP_RESTART_REFUSED`.
pub(crate) fn hop_restart(
    image: Option<&kern::system::HopImage>,
    grace: Duration,
) -> Result<(), Fail> {
    ask_restart(image, grace).map_err(|(why, e)| {
        println!("slots: Hop restart refused: {e} HOPOS_HOP_RESTART_REFUSED {why}");
        e
    })
}

fn ask_restart(
    image: Option<&kern::system::HopImage>,
    grace: Duration,
) -> Result<(), (&'static str, Fail)> {
    let Some(same) = SAME.get().get() else {
        return Err(("not restartable now", Fail::Kern(kern::Error::NoEnt)));
    };
    let pending = ASKED.take();
    let busy = pending.is_some();
    ASKED.set(pending);
    if busy {
        return Err(("restart already pending", Fail::Kern(kern::Error::Busy)));
    }
    let image = match image {
        Some(img) => Some(take_image(img)?),
        None if same == Source::Staged && staged_hop().is_none() => {
            return Err(("no image", Fail::Kern(kern::Error::NoEnt)));
        }
        None => None,
    };
    ASKED.set(Some(Asked { image, grace }));
    ASK_BELL.set();
    Ok(())
}

/// Het nieuwe image uit zijn partitie: de som, de ELF-toets van de
/// plaatsing (`kern::system::check_image`, tegen de partitie van Hop) en
/// een kopie in de kern-heap.
///
/// Waarom de heap: het slot gaat na de haak terug in de pool (slots zijn
/// schaars, één per app-core), de staging is per board anders (het venster
/// van QEMU, de initrd van de Pi, het image zelf op de Mac mini) en houdt
/// de nieuwe kern van een flip, en de heap is ruim: 40 MB kern-RAM op de
/// LicheeRV tot 256 MB op de Mac mini, tegen 2,4 MB voor een gestripte Hop
/// (arm64, 22-10). Er ligt er één: het image van de Hop die draait (voor
/// een volgende herstart met hetzelfde image); tijdens de haak even twee.
/// Het overleeft een kern-flip niet; dan plaatst een herstart het
/// gestagede (`HOPOS_HOP_IMAGE_STAGED`).
fn take_image(img: &kern::system::HopImage) -> Result<Vec<u8>, (&'static str, Fail)> {
    let len = match usize::try_from(img.len) {
        Ok(n) if img.len <= HOP_MEM => n,
        _ => {
            let e = kern::Error::TooLarge {
                len: usize::try_from(img.len).unwrap_or(usize::MAX),
                max: HOP_MEM as usize,
            };
            return Err(("image larger than Hop's partition", Fail::Kern(e)));
        }
    };
    // SAFETY: `[base, base+len)` is de partitie van een stroom die de
    // system-API tijdens deze aanroep in handen houdt (de grant is buiten
    // de actor: er draait niets in en niemand anders schrijft erin), en de
    // lengte is wat er gestroomd is, binnen de partitie (`Placer::raw`).
    // Het geheugen is pool-RAM, Normal gemapt in de identity map. Alleen
    // lezen, en de slice leeft niet langer dan deze functie (zoals
    // `flip::prepare`).
    let bytes = unsafe { core::slice::from_raw_parts(img.base as usize as *const u8, len) };
    let sum = abi::sha256::digest(bytes);
    if sum != img.sha256 {
        let e = kern::Error::Version {
            have: kern::kernflip::sum64(&sum),
            want: kern::kernflip::sum64(&img.sha256),
        };
        return Err(("sha256 mismatch", Fail::Kern(e)));
    }
    kern::system::check_image(bytes, HOP_MEM).map_err(|e| ("not a placeable ELF", e))?;
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| {
        (
            "no heap",
            Fail::Kern(kern::Error::OutOfMemory { bytes: len }),
        )
    })?;
    v.extend_from_slice(bytes);
    Ok(v)
}

/// Het gestagede image van Hop zolang het nog een ELF is: na een warme
/// flip die er zelf overheen moest (`HOPOS_FLIP_STAGE_SHARED`) is het dat
/// niet meer, en is er geen Hop om te herstarten met hetzelfde image. De
/// supervisor vraagt het bij elke plaatsing opnieuw.
pub(crate) fn staged_hop() -> Option<&'static [u8]> {
    let img = vslots::staged_image()?;
    (img.get(..4) == Some(b"\x7fELF".as_slice())).then_some(img)
}

/// Wacht tot het net luistert (`net::NET_READY`: de lease, de uplink, de
/// stack en de system-listener). Zo is de plaatsing van Hop de stap na het
/// net in de bootreeks: zijn eerste call vindt de listener. Zonder termijn:
/// komt het net niet (geen kabel, geen DHCP), dan reset de ene guard van de
/// watchdog de node (docs/sequence-hopos.md §14.1 O1), en de koude boot is
/// de nieuwe poging. Zonder net (geen NIC, of een NIC die niet opkwam)
/// start Hop meteen, met zijn slot-adres als endpoint (luid).
async fn wait_uplink() -> Option<core::net::Ipv4Addr> {
    if !crate::net::switch_up() {
        println!("slots: no net on this boot, Hop announces its slot address HOPOS_HOP_NO_UPLINK");
        return None;
    }
    crate::net::NET_READY.wait().await;
    crate::net::uplink_ip()
}

/// `Placement::hop`: de sharegroup van Hop, op de OS-core met de kern als
/// die groep `system` is (de klasse van die core koos `hopos.oscore`), en
/// anders een eigen app-core, met `hopos.hop.core-class` als voorkeur voor
/// de eerste core van de groep (`Placement::prefer`).
fn hop_placement(plan: &abi::layout::Plan, cfg: &NodeCfg<'_>) -> kern::Result<Placement> {
    let mut at = Placement::hop()?;
    let class = kern::nodecfg::hop_core_class(cfg);
    let want = cfg.one("hopos.hop.core-class");
    if hop_on_os() {
        println!(
            "slots: Hop shares the OS core (cpu {}) with the kern, {} app core(s) stay free HOPOS_HOP_OS_CORE",
            plan.os_core(),
            plan.app_cores()
        );
        if !want.is_empty() {
            println!(
                "slots: hopos.hop.core-class={want} not used: Hop is on the OS core (hopos.oscore picks it) HOPOS_HOP_CLASS"
            );
        }
        return Ok(at);
    }
    match class {
        Ok(Some(c)) => {
            at.prefer = Some(c);
            println!(
                "slots: Hop prefers a {want} core for his group (hopos.hop.core-class) HOPOS_HOP_CLASS"
            );
        }
        Ok(None) => {}
        Err(v) => println!(
            "slots: hopos.hop.core-class={v} is not small, mid or big, ignored HOPOS_HOP_CLASS"
        ),
    }
    Ok(at)
}

/// Het plan van deze node: de kern-core is de OS-core, en dat is de core
/// waar dit draait (na de verhuizing bij boot, `main`). Het board-contract
/// hiervoor (`plan(cores, os_core)`, `this_core`, `os_bell`, `kick_self`)
/// volgen alle boards die deze lijm delen.
pub(crate) fn os_plan() -> abi::Result<abi::layout::Plan> {
    let board = &crate::BOARD;
    vslots::plan(board.cores(), board.this_core())
}

/// Woont Hop op de OS-core? Ja als zijn groep `system` is
/// (`hopos.hop.sharegroup`, [`init_hop_group`]).
fn hop_on_os() -> bool {
    kern::pool::hop_group().as_slice() == kern::pool::SYSTEM_GROUP
}

/// De sharegroup van Hop uit de config (`hopos.hop.sharegroup`): `system`
/// is de OS-core bij de kern, elke andere naam een eigen app-core die jobs
/// met dezelfde tag delen. Zonder sleutel: `system` waar de kern zijn core
/// deelt, anders `hop`. Vraagt de config `system` op een board dat niet
/// deelt (Apple), dan `hop`, luid. Eén keer, vóór de pool en de plaatsing.
///
/// FLIP: een warme flip verplaatst Hop niet (`carried`, zijn staat uit het
/// blob). Dan is zijn groep de waarheid: de bootregel zegt waar hij zit,
/// jobs met zijn tag en een flipbundel komen naast hem, en de core-reclaim
/// spaart hem. Vraagt de config iets anders, dan zegt één regel dat die
/// pas bij een koude start geldt.
fn init_hop_group(cfg: &NodeCfg<'_>, carried: Option<&kern::slots::SlotState>) {
    let want = cfg.one("hopos.hop.sharegroup");
    let (cfg_name, cfg_why): (&[u8], &str) = match want {
        "" if arch::SHARES_OS_CORE => (kern::pool::SYSTEM_GROUP, "default"),
        "" => (
            kern::pool::HOP_GROUP,
            "default, this board does not share the OS core",
        ),
        "system" if !arch::SHARES_OS_CORE => (
            kern::pool::HOP_GROUP,
            "hopos.hop.sharegroup=system refused: this board does not share the OS core",
        ),
        other => (other.as_bytes(), "hopos.hop.sharegroup"),
    };
    let (name, why) = carried.map_or((cfg_name, cfg_why), |st| {
        (st.share_group.as_slice(), "carried over the flip")
    });
    match kern::pool::set_hop_group(name) {
        Ok(()) => println!(
            "slots: Hop in the sharegroup {} ({why}) HOPOS_HOP_GROUP",
            core::str::from_utf8(name).unwrap_or("?")
        ),
        Err(e) => {
            println!("slots: hopos.hop.sharegroup: {e}, Hop in the group hop HOPOS_HOP_GROUP")
        }
    }
    if name != cfg_name {
        println!(
            "slots: the config puts Hop in the sharegroup {} ({cfg_why}) from the next cold start, a warm flip does not move him HOPOS_HOP_GROUP_COLD",
            core::str::from_utf8(cfg_name).unwrap_or("?")
        );
    }
}

/// De core-plaatsing van deze node: de OS-core is een deelbare core als
/// groep `system` (de kern is er de vaste bewoner, PORT.md beslissing 2);
/// Hop zit erbij als zijn groep `system` is. Kan de architectuur niet delen
/// (Apple's EL2-smaak), dan krijgt Hop een app-core zoals vóór 30-09.
fn os_places() -> Places {
    let mut pool = Places::new();
    let groups: &[&[u8]] = if arch::SHARES_OS_CORE {
        &[kern::pool::SYSTEM_GROUP]
    } else {
        &[]
    };
    for g in groups {
        if let Err(e) = pool.share_os_core(g) {
            println!(
                "slots: group {} may not share the OS core: {e} HOPOS_OS_CORE_FAIL",
                core::str::from_utf8(g).unwrap_or("?")
            );
        }
    }
    pool
}

/// De rotatie van de OS-core voor de slaap van de executor: de bewoners van
/// sched-blok 0 en de kick van de app-cores, na de zelftest van de
/// overgang (timer, yield, kick). Aanroepen op de OS-core, ná [`start`]
/// (die de kooi-regio opzette) en ná de interrupts (de zelftest wacht op
/// de CNTHP en de kick-SGI).
pub(crate) fn os_core() -> Result<arch::OsCore, el2::Error> {
    let plan = os_plan().map_err(el2::Error::Plan)?;
    arch::os_core(&plan)
}

/// Het rapport van een exception die de app op EL1 zelf ving (de
/// vectortabel van applib, `CTRL_APP_FAULT_*`), als stuk van een
/// fault-regel; `None` als er geen is. Sinds 30-09: daarvoor sprong zo'n
/// fault naar een lege VBAR_EL1 en zag de kern alleen de instructie-abort
/// op `VBAR + 0x200` (de eerste Pi 5-boot, `esr=0x82000005 far=0x200`).
fn el1_fault(st: &kern::cage::Status) -> Option<alloc::string::String> {
    use core::fmt::Write;
    if st.app_fault_vec == 0 || st.exit_code != abi::hopabi::EXIT_APP_FAULT {
        return None;
    }
    let mut line = alloc::string::String::new();
    let _ = write!(
        line,
        "vec={} esr={:#x} ({}) elr={:#x} far={:#x}",
        st.app_fault_vec - 1,
        st.app_fault_esr,
        kern::cage::esr_class(st.app_fault_esr),
        st.app_fault_elr,
        st.app_fault_far
    );
    Some(line)
}

/// De volumes als `kern::slots::Mount`, faalbaar gealloceerd.
fn kern_mounts(list: &[(&[u8], &[u8])]) -> kern::Result<Vec<Mount>> {
    let copy = |b: &[u8]| -> kern::Result<Vec<u8>> {
        let mut v = Vec::new();
        v.try_reserve_exact(b.len())
            .map_err(|_| kern::Error::OutOfMemory { bytes: b.len() })?;
        v.extend_from_slice(b);
        Ok(v)
    };
    let mut out = Vec::new();
    out.try_reserve_exact(list.len())
        .map_err(|_| kern::Error::OutOfMemory {
            bytes: list.len() * core::mem::size_of::<Mount>(),
        })?;
    for (local, shared) in list {
        out.push(Mount {
            local: copy(local)?,
            shared: copy(shared)?,
        });
    }
    Ok(out)
}

/// Plaatst `img` in `slot` met `mem` bytes, de core-vraag `at`, de env en
/// de volumes `mounts`, langs de plaatsing van elke start
/// (`kern::system::place`). Geeft de entry.
async fn place(
    slot: Slot,
    img: &[u8],
    mem: u64,
    at: Placement,
    env: &[u8],
    mounts: Vec<Mount>,
    ports: Vec<u16>,
) -> Result<u64, Fail> {
    let mut spec = StartSpec::new(slot, mem, at);
    spec.mounts = mounts;
    spec.ports = ports;
    kern::system::place(INBOX, &BOOT_REPLY, &mut DevMem, spec, img, env).await
}

/// Eén meetregel: app-status, ctx-staat, mailbox en heartbeat.
fn probe(plan: &abi::layout::Plan, slot: Slot, core: usize, st: &SlotStatus, dt: u64) {
    let ctx = abi::layout::Slot::new(slot.get())
        .and_then(|s| plan.ctx_pa(s).ok())
        .and_then(el2::ctx_state);
    let mbox = abi::layout::Core::new(core).and_then(|c| arch::core_state(plan, c).ok());
    println!(
        "slot {slot}: +{} ms app={} ctx={:?} mbox={:?} beat={} core_on={}",
        dt / 1_000_000,
        st.cage.app,
        ctx.map(CtxState::raw),
        mbox.map(|m| match m {
            CoreState::Cold => 0,
            CoreState::Parked => 1,
            CoreState::Running(x) => x,
        }),
        st.cage.heartbeat,
        st.cage.core_on
    );
}

/// De stop met `timeout` als coöperatieve kans: bevestigd stil, dus
/// partitie en core terug. Geeft `true` als de actor de stop bevestigde
/// (ook als er niets meer stond).
async fn stop(slot: Slot, timeout: Duration) -> bool {
    match ask(Request::Stop { slot, timeout }).await {
        // De regel `HOPOS_SLOT_STOPPED` schrijft de actor zelf, bij elke stop.
        Ok(_) => true,
        Err(e) => {
            println!("slot {slot}: stop: {e} HOPOS_SLOT_STOP_FAIL");
            false
        }
    }
}
