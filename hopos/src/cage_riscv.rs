//! De kooi op riscv64: wat `crate::kooi` van de architectuur vraagt
//! ([`Isa`]), over `cpu::riscv::{pmp, sv39, switch, boot}` en `dev`. Het
//! beleid (bouwen, starten, stoppen, intrekken, stil) is van `kooi.rs` en
//! hetzelfde als op arm64 (`cage.rs`); hier alleen andere letters (Go,
//! `OLD/metal/kern/slots/cage_riscv64.go`):
//!
//! ```text
//! ARM (cage.rs)                     RISC-V (dit bestand)
//! stage-2-tabel + VMID              PMP-whitelist (TOR) + Sv39 in de staart
//! trampoline, x0 = control-page     koude boot door de switcher, a0 = page
//! mailbox + SEV, koud PSCI CPU_ON   BootPending in de lijst + msip
//! revoke: tabel nul + TLBI          CTX_REVOKE: de switcher (yield, tick,
//!                                   rotatie) zet hem dood; of het resetblok
//! OS-core (Hop naast de kern)       dezelfde vorm: `cpu::riscv::oscore`
//! SMP-eenheden, sharegroepen        één core per kooi, kooien delen een hart
//! ```
//!
//! Wie wat schrijft (de regel van de cachelines, `abi::layout::SchedBlock`):
//! de kern schrijft van het sched-blok alleen regel 1..3 (lijst, lengte, de
//! wekker, de bel, de slaap- en tickperiode) en van het ctx-blok alles vóór
//! BootPending, daarna alleen `CTX_REVOKE` (eigen regel); de switcher de
//! rest. Op de C906 zijn de harts niet coherent (30-07), dus elke schrijf
//! van hier gaat met `dev::push` naar DRAM en elke lees van een
//! switcher-woord met `dev::pull`.

use crate::kooi::{Built, Isa, Kooi, KooiCores, app_ram, code, err, tail_of};
use abi::checksum::Fnv64;
use abi::hopabi::{CTRL_IDLE_MODE, CTRL_TIMEBASE_HZ, IDLE_KICK, IDLE_YIELD};
use abi::layout::{
    self, ABI_CTRL_OFF, ABI_MAP_PAGES, ABI_TAIL, CTX_ASID, CTX_BOOT_ARG, CTX_BOOT_PC, CTX_REGIME,
    CTX_STATE, CtxState, LINK_BASE, PARK_MBOX_LEN, Plan, SCHED_CLINT_PA, SCHED_CURRENT,
    SCHED_LIST_PA, SCHED_MSIP_PA, SCHED_OS_BELL, SCHED_S2_PA, SCHED_SLEEP_CAP, SCHED_TICK_TICKS,
    Tail,
};
use board::Board;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use cpu::el2::{self, CoreState, roster};
use cpu::println;
use cpu::riscv::switch::{self, AppHart, REGIME_PMPADDR0, REGIME_PMPCFG0, REGIME_SATP};
use cpu::riscv::{boot, pmp, sv39};
use dev::Pa;
use kern::Region;
use kern::cage::{CageError, CoreClass};

/// De kooi van deze node, en zijn cores.
pub(crate) type SlotCage = Kooi<Rv>;
/// Zie [`SlotCage`].
pub(crate) type SlotCores = KooiCores<Rv>;

/// De bel (`msip`-PA) van elk app-hart, per logische core; 0 = geen bel.
/// Eén schrijver ([`new`], bij de boot), daarna alleen lezers: de
/// kick na elke RX-schrijf van de switch ([`wake_all`]) hoeft zo geen plan
/// te bouwen.
static BELLS: [AtomicU64; BELL_CAP] = [const { AtomicU64::new(0) }; BELL_CAP];

/// Hoeveel app-harts [`BELLS`] draagt: de parkeerlus kent er niet meer.
const BELL_CAP: usize = boot::MAX_HARTS;

/// Het hoogste linkadres dat één Sv39-niveau-1-tabel dekt: de gigabyte van
/// [`LINK_BASE`] (0x4000_0000..0x8000_0000). De staart heeft plek voor de
/// wortel en één tabel (`ABI_MAP_PAGES`), dus het venster eindigt hier.
const LINK_GIGA_END: u64 = 0x8000_0000;

const _: () = assert!(LINK_BASE >= 0x4000_0000 && LINK_BASE < LINK_GIGA_END);
const _: () = assert!(ABI_MAP_PAGES == 2);

/// Het app-adresvenster van een partitie van `size` bytes: vanaf
/// [`LINK_BASE`] tot het einde van zijn gigabyte (768 MB), want de
/// Sv39-tabel in de staart is een wortel plus één tabel.
pub(crate) fn link_window(size: u64) -> u64 {
    size.min(LINK_GIGA_END - LINK_BASE)
}

/// Extra vertaalopslag achter de partitie: geen. De Sv39-tabel woont in de
/// ABI-staart (`ABI_MAP_OFF`), binnen de kooi, want de walker is zelf aan
/// de whitelist onderworpen (Go, `kern/cage/relocate.go`).
pub(crate) fn reserve(_size: u64) -> u64 {
    0
}

/// Waarom de kooi niet op kon.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Error {
    /// Het plan weigerde een index.
    Plan(abi::Error),
    /// Een app-hart kwam niet in de parkeerlus.
    Start(boot::StartError),
    /// Een warme flip bestaat op riscv64 niet: er is niets te adopteren.
    NoFlip,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Plan(e) => write!(f, "plan: {e}"),
            Self::Start(e) => write!(f, "app hart: {e}"),
            Self::NoFlip => f.write_str("riscv64 does not flip, nothing to adopt"),
        }
    }
}

/// De switch-code zoals `slots::start` hem meldt: waar hij staat en zijn
/// som. Op riscv64 draait een app-hart hem uit het kern-image (hij is niet
/// naar de plan-regio gekopieerd zoals op arm64: dat is alleen nodig voor
/// de flip), dus `entry` is `mentry` in het image en `tramp` de
/// parkeer-ingang.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Installed {
    /// De trap-ingang van de switcher (`mentry`).
    pub(crate) entry: Pa,
    /// De maat van de switch-code.
    pub(crate) len: u64,
    /// De parkeer-ingang (`parkenter`), waar een app-hart binnenkomt.
    pub(crate) tramp: Pa,
    /// FNV-1a over de code: dezelfde som op elke boot van hetzelfde image.
    pub(crate) hash: u64,
}

/// De riscv64-kant van de kooi: de switch-code in het kern-image.
pub(crate) struct Rv {
    installed: Installed,
    /// De ASID-bits van het hart van de kern (`oscore::asid_bits`), als de
    /// rotatie daar ASID's gebruikt (`hopos.system.asid`); 0 = een flush per
    /// wissel. Alleen een bewoner van de OS-core krijgt er een: een app-hart
    /// flusht bij elke wissel.
    asid_bits: u32,
}

impl Rv {
    /// De switch-code.
    pub(crate) fn installed(&self) -> &Installed {
        &self.installed
    }
}

/// Zet elk app-hart klaar: de kern-regels van zijn sched-blok (de
/// kooi-regio, de wekker, de bel, de slaap en de kill-tick), elk ctx-blok
/// leeg, en het hart de switcher in (`parkenter`). Eén keer bij boot, vóór
/// de eerste dispatch.
///
/// Het ctx-blok van een kooi begint de bouw schoon, uit het kooi-geheugen
/// (de les van 17-08, Go boot 3: een reboot liet `CtxRunning` van een vorige
/// boot staan, en elke plaatsing weigerde met "still live").
pub(crate) fn new(plan: Plan) -> Result<SlotCage, Error> {
    let base = plan.vec_base_pa();
    // Het sched-blok van de OS-core (core 0) ook: zijn rotatie leest de
    // bewonerslijst (`SCHED_LIST_PA`) en de lengte, en verse DRAM is geen
    // nul, zoals arm64 alle sched-blokken wist (`cpu::el2::dispatch`). De
    // LicheeRV (07-11): rommel van een vorige boot als lijst, en de kern
    // viel in zijn eerste slaap op een load van 0x3_4044_9505.
    let os = plan
        .park_mbox_pa(layout::Core::new(0).ok_or(Error::Plan(abi::Error::TooMany {
            what: "app cores",
            cap: layout::CORES_MAX - 1,
        }))?)
        .map_err(Error::Plan)?;
    dev::clear(os, PARK_MBOX_LEN as usize);
    dev::push(os, PARK_MBOX_LEN as usize);
    for c in 1..=plan.app_cores() {
        let core = layout::Core::new(c).ok_or(Error::Plan(abi::Error::TooMany {
            what: "app cores",
            cap: layout::CORES_MAX - 1,
        }))?;
        let hart = plan.phys_core(core);
        let sched = plan.park_mbox_pa(core).map_err(Error::Plan)?;
        let t = crate::BOARD.app_hart(hart);
        if let Some(b) = BELLS.get(c) {
            b.store(t.msip.0, Relaxed);
        }
        let fresh = !boot::is_started(hart);
        arm_sched(sched, base.0, &t, fresh);
        if fresh {
            boot::start_hart(hart, switch::park_pc(), sched.0, t.msip).map_err(Error::Start)?;
            crate::BOARD.start_app_hart(hart);
        } else {
            // Al in de switcher (de zelftest van het board): de bel,
            // zodat hij de verse regels leest.
            ring_bell(&t);
        }
        println!(
            "cage: hart {hart} (core {core}) in the switcher: wake {:#x}, bell {:#x}, kick {:#x}, sleep cap {} ticks, kill tick {} ticks, reset {} HOPOS_RV_HART_UP",
            t.mtimecmp.0, t.msip.0, t.kick.0, t.sleep_cap, t.tick, t.resettable
        );
        if !t.resettable && (t.mtimecmp.0 == 0 || t.tick == 0) {
            // Go, `HOPOS_CORE_NO_KILL`: geen stille stand.
            println!(
                "cage: hart {hart} has no reset and no kill tick: a resident that stops cooperating is only cleared by rebooting the node HOPOS_CORE_NO_KILL"
            );
        }
    }
    let (begin, end) = switch::code_range();
    let installed = Installed {
        entry: Pa(switch::entry_pc()),
        len: end.saturating_sub(begin),
        tramp: Pa(switch::park_pc()),
        hash: code_hash(begin, end),
    };
    let asid_bits = if crate::OS_ASID.load(Relaxed) {
        cpu::riscv::oscore::asid_bits()
    } else {
        0
    };
    Ok(Kooi::new(
        plan,
        Rv {
            installed,
            asid_bits,
        },
    ))
}

/// Een flip bestaat op riscv64 niet (de switch-code draait uit het
/// kern-image): er valt niets over te nemen. De haak weigert al vóór de 202
/// (`flip::FLIPS`).
pub(crate) fn adopt(_plan: Plan) -> Result<SlotCage, Error> {
    Err(Error::NoFlip)
}

impl Isa for Rv {
    const MAX_CORES: usize = 1;
    // De kill-tick is op een gedeeld hart ook de tijdschijf (sinds 02-10):
    // een buur die rekent, houdt het hart hooguit één tick.
    const TIME_SLICE: bool = true;
    const PARKS: bool = false;
    // Geen `dev::pull` vooraf: `th.dcache.cipa` raakt alleen de cache van
    // dit hart, en wat de vorige huurder vuil achterliet, veegde zijn eigen
    // switcher al vóór hij Dead meldde (`HOPOS_RV_CIALL` in de teardown).
    // Een regel in de cache van de kern zelf overschrijft `dev::clear`.
    const SCRUB: bool = false;

    /// De Sv39-tabel in de staart, de PMP-whitelist en de koude start in
    /// het ctx-blok (de switcher, of op de OS-core de overgang van de kern,
    /// leest ze daar), en de idle-modus en de timebase op de control-page.
    fn build(&self, plan: &Plan, b: &Built) -> Result<u64, CageError> {
        let (s, ctx, part) = (b.s, b.ctx, b.part);
        let ram = app_ram(part).ok_or(err(code::TAIL))?;
        let tail = tail_of(part).ok_or(err(code::TAIL))?;
        let t = crate::BOARD.app_hart(plan.phys_core(b.core));
        let satp = relocate(s, part, ram, tail, &t, b.core.get() == 0)?;
        // De whitelist: één TOR-venster over de hele claim (app-RAM plus
        // staart), dan de deny-all. De switcher schrijft hem en leest
        // pmpcfg0 terug vóór hij de bewoner binnenlaat.
        let window = pmp::Window {
            base: part.base,
            size: part.size,
            r: true,
            w: true,
            x: true,
        };
        let enc = pmp::encode(&[window], t.pmp).map_err(|e| {
            println!("cage: slot {s}: {e} HOPOS_CAGE_PMP");
            err(code::CAGE)
        })?;
        let regime = ctx.add(CTX_REGIME);
        dev::write64(regime.add(REGIME_PMPCFG0), enc.cfg);
        for (k, a) in enc.addr.iter().enumerate() {
            dev::write64(regime.add(REGIME_PMPADDR0 + 8 * k as u64), *a);
        }
        for (at, v) in [
            (regime.add(REGIME_SATP), satp),
            // De ASID op het hart van de kern (`oscore::OsCore::turn`); 0
            // elders.
            (ctx.add(CTX_ASID), u64::from(b.vmid)),
            (ctx.add(CTX_BOOT_PC), b.entry),
            // a0 van de koude boot: de control-page zoals de app hem ziet.
            (ctx.add(CTX_BOOT_ARG), LINK_BASE + ram + ABI_CTRL_OFF),
            // Een bewoner idlet met de yield (a0 = wektijd): een `wfi` van
            // een bewoner wekt nooit (hij draait met mie = 0), en de switcher
            // slaapt dan voor hem op de CLINT (Go, idle_riscv64.go: "de
            // ecall is zijn enige route naar een wfi"). Met IDLE_KICK belt
            // hij de kern na een publicatie (ecall, a7 = 2): op de OS-core een
            // yield naar nu (de rotatie van de kern), op een app-hart alleen
            // met een bel naar de kern.
            (
                b.ctrl.add(CTRL_IDLE_MODE),
                IDLE_YIELD
                    | if b.core.get() == 0 || t.kick.0 != 0 {
                        IDLE_KICK
                    } else {
                        0
                    },
            ),
            // De timebase van de TIME-CSR: RISC-V heeft geen CNTFRQ.
            (b.ctrl.add(CTRL_TIMEBASE_HZ), cpu::riscv::idle::hz()),
        ] {
            dev::write64(at, v);
        }
        Ok(satp)
    }

    /// De lijst, dan de staat (`roster::enlist`), dan de bel: de switcher
    /// leest de lijst, dan de staat, en bij een koude boot de lijst nog
    /// eens.
    fn start(&self, plan: &Plan, b: &Built) -> Result<(), CageError> {
        let (slot, hart) = (b.s, plan.phys_core(b.core));
        let sched = plan.park_mbox_pa(b.core).map_err(|_| err(code::PLAN))?;
        if roster::enlist(sched, b.ctx).is_err() {
            println!("cage: slot {slot}: resident list of hart {hart} is full HOPOS_CAGE_DISPATCH");
            return Err(err(code::ROSTER));
        }
        let t = crate::BOARD.app_hart(hart);
        ring_bell(&t);
        println!(
            "cage: slot {slot} dispatched to hart {hart} ({}) HOPOS_RV_DISPATCH",
            if t.msip.0 != 0 {
                "BootPending + msip"
            } else {
                "BootPending, picked up by the spinning switcher"
            }
        );
        Ok(())
    }

    /// De bel, zodat een slaper het woord nu leest; op een hart met een
    /// resetblok en zonder kill-tick is de reset het mes.
    fn revoke(&self, plan: &Plan, _s: layout::Slot, b: Option<&Built>) {
        let Some(b) = b.filter(|b| b.core.get() != 0) else {
            return;
        };
        let t = crate::BOARD.app_hart(plan.phys_core(b.core));
        ring_bell(&t);
        let dead = matches!(
            el2::ctx_state(b.ctx),
            Some(CtxState::Empty | CtxState::Dead)
        );
        if t.resettable && t.tick == 0 && !dead {
            Rv::reset_hart(plan, b);
        }
    }

    fn core_state(plan: &Plan, c: layout::Core) -> Result<CoreState, el2::Error> {
        core_state(plan, c)
    }

    fn kick(plan: &Plan, c: layout::Core) {
        ring_bell(&crate::BOARD.app_hart(plan.phys_core(c)));
    }

    fn class(_phys: usize) -> Option<CoreClass> {
        // De C906B en C906L zijn hetzelfde ontwerp; QEMU virt homogeen.
        None
    }

    /// De ASID, alleen op het hart van de kern: daar wisselt de rotatie
    /// zonder flush. Een app-hart flusht bij elke wissel.
    fn id_bits(&self, c: layout::Core) -> u32 {
        if c.get() == 0 { self.asid_bits } else { 0 }
    }
}

impl Rv {
    /// De harde intrekking op een hart met een resetblok en zonder
    /// kill-tick (de C906L van de LicheeRV): reset vast (dat wist ook zijn
    /// PMP en zijn caches, gemeten 30-07), de bewoner dood en uit de lijst,
    /// en het hart opnieuw de switcher in. Er draait daarna niets meer van
    /// deze bewoner: het hart kwam uit reset.
    fn reset_hart(plan: &Plan, b: &Built) {
        let (slot, core, ctx, hart) = (b.s, b.core, b.ctx, plan.phys_core(b.core));
        if !crate::BOARD.hold_app_hart(hart) {
            return;
        }
        let Ok(sched) = plan.park_mbox_pa(core) else {
            return;
        };
        let t = crate::BOARD.app_hart(hart);
        el2::ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
        // Alle andere bewoners van dit hart gingen met de reset mee; op
        // riscv64 is er één per hart, dus de lijst mag leeg. De lijst zelf
        // (in het kooi-geheugen) blijft van dit hart.
        let list = dev::read64(sched.add(SCHED_LIST_PA));
        arm_sched(sched, plan.vec_base_pa().0, &t, true);
        dev::write64(sched.add(SCHED_LIST_PA), list);
        dev::push(sched.add(SCHED_LIST_PA), 8);
        match boot::start_hart(hart, switch::park_pc(), sched.0, t.msip) {
            Ok(()) => {
                crate::BOARD.start_app_hart(hart);
                println!(
                    "cage: slot {slot}: hart {hart} reset and back in the switcher HOPOS_CAGE_REVOKE"
                );
            }
            Err(e) => println!(
                "cage: slot {slot}: hart {hart} held in reset, restart refused: {e} HOPOS_CAGE_REVOKE"
            ),
        }
    }
}

/// Legt het linkvenster op de partitie: het app-RAM als normaal geheugen
/// (rwx), de staart erboven (rw) als device, zonder de cachebits van de
/// C906, behalve voor een bewoner op het hart van de kern (`own`). De kern
/// leest en schrijft de staart, en de harts zijn niet coherent; op zijn
/// eigen hart deelt de bewoner de L1 met hem, en dan is gecachet coherent
/// (linkadres en partitie liggen op 2 MB, dus VA en PA vallen in dezelfde
/// cacheset). Device kost daar alles: leannet leest elk frame in de
/// RX-ring zelf, en zijn checksum over één segment uit device duurt 245 tot
/// 258 us tegen 10 us uit RAM (03-10, LicheeRV, de pull van 100 MiB over de
/// draad op 3,9 MB/s met de bewoner 78 % van de C906B bezig). De tabel gaat
/// naar `ABI_MAP_OFF` van de staart.
fn relocate(
    slot: layout::Slot,
    part: Region,
    ram: u64,
    tail: Tail,
    t: &AppHart,
    own: bool,
) -> Result<u64, CageError> {
    let map = [
        sv39::MapWindow {
            link: LINK_BASE,
            phys: part.base,
            size: ram,
            r: true,
            w: true,
            x: true,
            device: false,
        },
        sv39::MapWindow {
            link: LINK_BASE + ram,
            phys: tail.base().0,
            size: ABI_TAIL,
            r: true,
            w: true,
            x: false,
            device: !own,
        },
    ];
    let mut tables = sv39::Tables::new();
    let satp = sv39::relocate(tail.map().0, &map, t.attrs, &mut tables).map_err(|e| {
        println!("cage: slot {slot}: {e} HOPOS_CAGE_SV39");
        err(code::CAGE)
    })?;
    if tables.used as u64 > ABI_MAP_PAGES {
        println!(
            "cage: slot {slot}: the map needs {} table pages, the tail holds {ABI_MAP_PAGES} HOPOS_CAGE_SV39",
            tables.used
        );
        return Err(err(code::CAGE));
    }
    for (i, page) in tables.pages.iter().take(tables.used).enumerate() {
        for (j, e) in page.iter().enumerate() {
            dev::write64(tail.map().add((i * 4096 + j * 8) as u64), *e);
        }
    }
    // De walker van het app-hart leest de tabel uit DRAM.
    dev::push(tail.map(), tables.len() as usize);
    Ok(satp)
}

/// Schrijft de kern-regels van sched-blok `sched`; `fresh` = het hart staat
/// nog niet in de switcher, dus ook regel 0 (lopend slot, rotor) mag nul.
fn arm_sched(sched: Pa, cage: u64, t: &AppHart, fresh: bool) {
    if fresh {
        dev::clear(sched, PARK_MBOX_LEN as usize);
    }
    let _ = roster::reset(sched, None);
    for (off, v) in [
        (SCHED_S2_PA, cage),
        (SCHED_CLINT_PA, t.mtimecmp.0),
        (SCHED_MSIP_PA, t.msip.0),
        (SCHED_OS_BELL, t.kick.0),
        (SCHED_SLEEP_CAP, t.sleep_cap),
        (SCHED_TICK_TICKS, t.tick),
    ] {
        dev::write64(sched.add(off), v);
    }
    dev::push(sched, PARK_MBOX_LEN as usize);
}

/// De bel van een app-hart (`msip`), als hij er een heeft.
fn ring_bell(t: &AppHart) {
    if t.msip.0 != 0 {
        dev::mb();
        dev::write32(t.msip, 1);
    }
}

/// De som van de switch-code (`abi::checksum::Fnv64`, zoals op arm64)
/// over `[begin, end)` van het image, woordgewijs: vanaf `begin` naar
/// beneden op 8, het woord waarin `end` valt nog mee.
fn code_hash(begin: u64, end: u64) -> u64 {
    let mut h = Fnv64::new();
    let mut a = begin & !7;
    while a < end {
        h.write(&dev::read64(Pa(a)).to_le_bytes());
        a += 8;
    }
    h.sum()
}

// De architectuur-naad van `slots.rs`: wat daar op arm64 `cpu::el2` is.

/// De toestand van app-core `core` in de vorm van `cpu::el2::core_state`:
/// koud (nooit gestart), geparkeerd (de switcher draait niemand) of het
/// slot dat hij draait (`SCHED_CURRENT`, vers uit DRAM).
pub(crate) fn core_state(plan: &Plan, core: layout::Core) -> Result<CoreState, el2::Error> {
    let sched = plan.park_mbox_pa(core).map_err(el2::Error::Plan)?;
    if !boot::is_started(plan.phys_core(core)) {
        return Ok(CoreState::Cold);
    }
    dev::pull(sched.add(SCHED_CURRENT), 8);
    Ok(match dev::read64(sched.add(SCHED_CURRENT)) {
        0 => CoreState::Parked,
        s => CoreState::Running(s),
    })
}

/// De kick na een schrijf in een RX-ring van een slot (`slot_wake` van de
/// switch): de bel van elk app-hart. Het slot kiest geen doel, want de
/// lijm van de switch weet niet op welk hart het woont; een bel te veel is
/// een lege ronde van de switcher.
pub(crate) fn wake_all() {
    let mut rung = false;
    for b in BELLS.iter() {
        let pa = b.load(Relaxed);
        if pa != 0 {
            if !rung {
                dev::mb();
                rung = true;
            }
            dev::write32(Pa(pa), 1);
        }
    }
}

/// Kan de kern zijn core delen (de groep `system`, en Hop als het board
/// dat wil: `hopos.hop.sharegroup=system`)? Ja: de kern geeft zijn bewoners het
/// hart in zijn idle (`cpu::riscv::oscore`, PORT.md beslissing 2).
pub(crate) const SHARES_OS_CORE: bool = true;

/// De rotatie van de OS-core, zoals `slots.rs` hem aan de slaap van de
/// executor geeft.
pub(crate) use cpu::riscv::oscore::OsCore;

/// De rotatie van de OS-core voor de slaap van de executor, na de zelftest
/// van de overgang: de wekker (een spinner komt terug op de deadline), de
/// yield en de kick (een spinner komt terug op de `msip` van het eigen
/// hart), en de exit. Elke proef na de dispatch-ronde over wat al bij de
/// PLIC wacht, en opnieuw als een device-lijn hem onderbrak
/// (`el2::selftest_tries`, zoals op arm64). Aanroepen op het hart van de
/// kern, ná de interrupts.
pub(crate) fn os_core(plan: &Plan) -> Result<OsCore, el2::Error> {
    use cpu::riscv::oscore::Probe;
    let board = &crate::BOARD;
    let hart = board.this_core();
    // De CLINT-index van dit hart: op de LicheeRV altijd 0 (één CLINT per
    // core), op virt het hart-id.
    let mut os = OsCore::new(
        plan,
        board.clint(),
        board.clint_hart(),
        board.app_hart(hart).pmp,
        crate::OS_ASID.load(Relaxed),
    )?;
    let ms = cpu::riscv::idle::hz() / 1000;
    let mut probe = |p: Probe, ticks: u64, kick: &dyn Fn()| {
        let (seen, tries) = el2::selftest_tries(
            crate::drain_interrupts,
            || os.selftest(p, ticks, kick),
            |s| s.back,
        );
        Shown { seen, tries, ms }
    };
    let t = probe(Probe::Spin, ms, &|| {});
    let y = probe(Probe::Yield, 100 * ms, &|| {});
    let k = probe(Probe::Spin, 100 * ms, &|| board.kick_self());
    let x = probe(Probe::Exit, 100 * ms, &|| {});
    let ok = t.back() == Some(el2::Back::Timer)
        && y.back() == Some(el2::Back::Yield)
        && k.back() == Some(el2::Back::Ipi)
        && x.back() == Some(el2::Back::Exit);
    println!(
        "oscore: hart {hart} self-test timer={t} yield={y} kick={k} exit={x} {}",
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

/// Een proef voor de zelftest-regel: `(Timer, 1645 us)`, en bij een andere
/// terugkeer de `mcause` (`mcause irq 11` is de PLIC) en het aantal
/// pogingen.
struct Shown {
    seen: Option<cpu::riscv::oscore::Seen>,
    tries: u32,
    ms: u64,
}

impl Shown {
    fn back(&self) -> Option<el2::Back> {
        self.seen.map(|s| s.back)
    }
}

impl core::fmt::Display for Shown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Some(s) = self.seen else {
            return f.write_str("(none: no cage for the stub)");
        };
        let us = s.ticks.saturating_mul(1000) / self.ms.max(1);
        write!(f, "({:?}, {us} us", s.back)?;
        if !matches!(
            s.back,
            el2::Back::Timer | el2::Back::Yield | el2::Back::Ipi | el2::Back::Exit
        ) {
            let code = s.cause & !(1 << 63);
            if s.cause >> 63 != 0 {
                write!(f, ", mcause irq {code}")?;
            } else {
                write!(f, ", mcause {code}")?;
            }
        }
        if self.tries > 1 {
            write!(f, ", try {}", self.tries)?;
        }
        f.write_str(")")
    }
}
