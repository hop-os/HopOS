//! De kooi als beleid: de traits van `kern::cage` één keer, voor elke
//! architectuur. Per ISA alleen de registers en de tabellen ([`Isa`]):
//! `slots/cage.rs` (stage-2 en de EL2-switcher) en `slots/cage_riscv.rs`
//! (PMP plus Sv39 en de M-mode-switcher).
//!
//! Dit bezit de per-slot wetenschap die de kern niet heeft en de switcher
//! niet mag hebben: waar de control-page van een levende kooi staat, op
//! welke core hij draait en hoe breed hij is ([`Built`]). De boekhouding
//! (wie welke partitie en core heeft) is van de lifecycle-actor; die krijgt
//! [`Kooi`] en [`KooiCores`] als waarde en is dus de enige aanroeper
//! (`&mut self`, handboek §1). Daarnaast de vier bevestigingen van de
//! switch (attach, detach, publish, unpublish).
//!
//! Het beleid, op beide architecturen hetzelfde:
//!
//! - **Bouwen**: eerst uit elke bewonerslijst (`el2::roster::forget`, de
//!   OS-core inbegrepen) en het ctx-blok leeg, dan de vertaling van de ISA,
//!   de control-page, het zaad, de ringen, de ctx-woorden, en de ringen aan
//!   de switch.
//! - **Starten**: op de OS-core de rotatie van de kern (`el2::host`: eerst
//!   de lijst, dan boot-pending); op een app-core het startschot van de
//!   ISA. Daarna weet elke bewoner van die core of hij hem deelt.
//! - **Stoppen**: van de switch af, de kill-vlag, en de kick van zijn core.
//! - **Intrekken**: `CTX_REVOKE`, en de ISA trekt de vertaling in. Op de
//!   OS-core plus de kick (`el2::recall`): de rotatie van de kern doodt hem
//!   vóór zijn volgende beurt, op één plek (`el2::next`).
//! - **Stil** is dood of leeg, of een app-core die geparkeerd staat.

use abi::hopabi::{
    AppStatus, CTRL_APP_FAULT_ELR, CTRL_APP_FAULT_ESR, CTRL_APP_FAULT_FAR, CTRL_APP_FAULT_VEC,
    CTRL_CORES, CTRL_ENTRY, CTRL_EXIT_CODE, CTRL_FAULT_ESR, CTRL_FAULT_FAR, CTRL_FAULT_VEC,
    CTRL_HART, CTRL_HEARTBEAT, CTRL_IDLE, CTRL_KILL, CTRL_MEM_SYS, CTRL_RAM_SIZE, CTRL_SHARED,
    CTRL_SLOT, CTRL_SMP_REQ, CTRL_STATUS, CTRL_WAKES, CTRL_WALL_OFF, KILL_STOP, hart_word,
};
use abi::layout::{
    self, ABI_TAIL, CTRL_STRIDE, CTX_CTRL_PA, CTX_LEN, CTX_REVOKE, CTX_RING_HEAD_PA, CTX_UNIT_SLOT,
    CtxState, LINK_BASE, NET_RING_DATA_CAP, Plan, RING_DATA_CAP, SCHED_CURRENT, Tail,
};
use abi::ring;
use core::future::Future;
use core::marker::PhantomData;
use core::time::Duration;
use cpu::el2::{self, CoreState, roster};
use cpu::println;
use dev::Pa;
use kern::cage::{Cage, CageError, CoreClass, Cores, PortError, Power, Status};
use kern::{Core, Region, SLOT_CAP, Slot};
use net::ring::AbiTx;
use net::switch::{Ack, Command};

/// Wat een architectuur aan de kooi bijdraagt: de vertaling en de
/// bescherming (stage-2, of Sv39 plus PMP), de woorden die haar switcher
/// en trampoline lezen, het startschot van een app-core, en hoe een core
/// gewekt of ingetrokken wordt. Al het andere is [`Kooi`].
pub(crate) trait Isa {
    /// De breedste SMP-eenheid die deze kooi bouwt.
    const MAX_CORES: usize;
    /// Heeft de switcher van een gedeelde app-core een tijdschijf? Dan wacht
    /// een nieuwe bewoner nooit lang op een buur die rekent, en hoeft de kern
    /// niet te offeren (`Cage::pending`).
    const TIME_SLICE: bool;
    /// Gaat een app-core zonder bewoner de parkeerlus in? Dan is een
    /// geparkeerde core stil. Een switcher zonder parkeerlus draait zijn
    /// rotatie altijd, ook zonder bewoner: daar zegt alleen het ctx-blok
    /// of een kooi stil is.
    const PARKS: bool;
    /// Eerst de regels van de vorige huurder uit de cache (clean +
    /// invalidate) vóór een partitie gewist wordt (`Cage::clear`)? Waar de
    /// kern de pool Device ziet, gaan de nullen langs de cache, en een vuile
    /// regel die later evict klobbert de verse bytes (Go `slots.Scrub`).
    const SCRUB: bool;

    /// Bouwt de vertaling en de bescherming van `b` en zet wat de switcher
    /// en de trampoline van deze ISA lezen in de control-page en het (net
    /// gewiste) ctx-blok: de idle-modus, de eerste beurt op de OS-core, en
    /// de contexten van een SMP-eenheid. Geeft de wortel van de vertaling,
    /// voor de bouwregel.
    fn build(&self, plan: &Plan, b: &Built) -> Result<u64, CageError>;
    /// Het startschot van `b` op zijn app-core.
    fn start(&self, plan: &Plan, b: &Built) -> Result<(), CageError>;
    /// Het startschot van een secundaire van `b` op app-core `c`, binnen
    /// zijn span. Zonder SMP (`MAX_CORES` 1) is er geen span om in te vallen.
    fn start_secondary(&self, plan: &Plan, b: &Built, c: layout::Core) -> Result<(), CageError> {
        let _ = (plan, b, c);
        Err(err(code::SPAN))
    }
    /// De intrekking door de ISA, na `CTX_REVOKE`: de vertaling van kooi
    /// `s` in, en op een app-core de bewoner van zijn core af. `b` is `None`
    /// voor een kooi die deze kern niet bouwde of adopteerde.
    fn revoke(&self, plan: &Plan, s: layout::Slot, b: Option<&Built>);
    /// De toestand van app-core `c` in de vorm van `el2::core_state`.
    fn core_state(plan: &Plan, c: layout::Core) -> Result<CoreState, el2::Error>;
    /// Wekt de switcher van app-core `c`.
    fn kick(plan: &Plan, c: layout::Core);
    /// De klasse van fysieke core `phys`.
    fn class(phys: usize) -> Option<CoreClass>;
}

/// De foutcodes van [`CageError`] van de kooi. De tekst met de getallen
/// staat op de console (één regel met marker); de code gaat de kern in.
pub(crate) mod code {
    /// Het plan weigerde een slot- of core-index.
    pub(crate) const PLAN: u32 = 1;
    /// De partitie geeft geen geldige ABI-staart.
    pub(crate) const TAIL: u32 = 2;
    /// De kooi weigerde: de stage-2-bouw (arm64), of de Sv39-vertaling of
    /// de PMP-whitelist (riscv64).
    pub(crate) const CAGE: u32 = 3;
    /// Een ring kon niet klaargezet worden.
    pub(crate) const RING: u32 = 4;
    /// Dispatch zonder build.
    pub(crate) const NOT_BUILT: u32 = 5;
    /// Het startschot weigerde (de mailbox, de OS-core, een vol rooster).
    pub(crate) const DISPATCH: u32 = 6;
    /// Een secundaire buiten de span van de kooi, of op de OS-core; op
    /// riscv64 één core per slot.
    pub(crate) const SPAN: u32 = 7;
    /// De bewonerslijst van een core weigerde de kooi.
    pub(crate) const ROSTER: u32 = 8;
}

/// Een [`CageError`] met `code`.
pub(crate) const fn err(code: u32) -> CageError {
    CageError { code }
}

/// Het app-RAM van een partitie: alles onder de ABI-staart.
pub(crate) fn app_ram(part: Region) -> Option<u64> {
    part.size.checked_sub(ABI_TAIL).filter(|n| *n > 0)
}

/// De staart van een partitie, in fysieke adressen.
pub(crate) fn tail_of(part: Region) -> Option<Tail> {
    Tail::new(part.base, app_ram(part)?)
}

/// Wat de kooi per gebouwde kooi onthoudt.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Built {
    /// Het slot.
    pub(crate) s: layout::Slot,
    /// Zijn ctx-blok (de primaire context).
    pub(crate) ctx: Pa,
    /// De control-page (fysiek).
    pub(crate) ctrl: Pa,
    /// De primaire core (logisch; 0 is de OS-core).
    pub(crate) core: layout::Core,
    /// De entry uit de ELF.
    pub(crate) entry: u64,
    /// De partitie van deze levensduur.
    pub(crate) part: Region,
    /// De vertrouwde SMP-breedte (1 = geen SMP).
    pub(crate) cores: usize,
}

/// Hoeveel bewoners van één gedeelde core [`Kooi::refresh_shared`]
/// hoogstens bijwerkt.
const SHARE_SCAN: usize = SLOT_CAP;

/// De kooi van deze node over architectuur `I`.
pub(crate) struct Kooi<I: Isa> {
    plan: Plan,
    isa: I,
    built: [Option<Built>; SLOT_CAP + 1],
}

impl<I: Isa> Kooi<I> {
    /// De kooi over `plan` met de ISA-kant `isa`, nog zonder bewoners.
    pub(crate) fn new(plan: Plan, isa: I) -> Kooi<I> {
        Kooi {
            plan,
            isa,
            built: [None; SLOT_CAP + 1],
        }
    }

    /// De ISA-kant.
    pub(crate) fn isa(&self) -> &I {
        &self.isa
    }

    fn slot(&self, slot: Slot) -> Result<layout::Slot, CageError> {
        layout::Slot::new(slot.get()).ok_or(err(code::PLAN))
    }

    fn ctx(&self, slot: Slot) -> Option<Pa> {
        self.plan.ctx_pa(layout::Slot::new(slot.get())?).ok()
    }

    fn built(&self, slot: Slot) -> Option<Built> {
        self.built.get(slot.get()).copied().flatten()
    }

    /// De ctx-staat van `slot`, vers uit het geheugen.
    fn ctx_state(&self, slot: Slot) -> Option<CtxState> {
        el2::ctx_state(self.ctx(slot)?)
    }

    /// Is de context van `slot` dood of leeg?
    fn dead(&self, slot: Slot) -> bool {
        matches!(self.ctx_state(slot), Some(CtxState::Empty | CtxState::Dead))
    }

    /// Leest woord `off` van de control-page van `slot`, vers uit DRAM: de
    /// app schrijft hem met zijn eigen cache, of met de MMU uit.
    fn ctrl_read(&self, slot: Slot, off: u64) -> u64 {
        self.built(slot).map_or(0, |b| {
            dev::pull(b.ctrl.add(off), 8);
            dev::read64(b.ctrl.add(off))
        })
    }

    fn ctrl_write(&self, slot: Slot, off: u64, v: u64) {
        if let Some(b) = self.built(slot) {
            dev::write64(b.ctrl.add(off), v);
            dev::push(b.ctrl.add(off), 8);
        }
    }

    /// De control-page en de ringen van een verse bewoner: de woorden van
    /// het beleid, het zaad en de drie ringen. Geen veeg
    /// van de page: de claim wiste de partitie al (E3), en een stream via
    /// de system-API legde de env (CTRL_ENV_LEN, CTRL_ENV_DATA) er vóór de
    /// Arm al op.
    fn arm_page(&self, b: &Built, tail: Tail) -> Result<(), CageError> {
        let (s, ctrl) = (b.s, b.ctrl);
        for (off, v) in [
            (CTRL_ENTRY, b.entry),
            (CTRL_SLOT, s.get() as u64),
            (CTRL_CORES, b.cores as u64),
            (CTRL_STATUS, AppStatus::Booting as u64),
            // De wandklok vóór de start, zoals de Go-kern: Hop stempelt
            // zijn taken ermee vanaf zijn eerste regel (clock.rs).
            (CTRL_WALL_OFF, crate::clock::offset()),
            // Op het hart van de kern: de app mag zijn ringen dan zonder
            // onderhoud beloven (abi `CTRL_HART`, [`attach`]).
            (CTRL_HART, hart_word(b.core.get() == 0)),
        ] {
            dev::write64(ctrl.add(off), v);
        }
        // Het zaad vóór de start: de app mengt het bij zijn eerste
        // willekeur (applib::rand, seed.rs).
        crate::seed::plant(ctrl);
        // De hele verse page naar DRAM: de app leest hem vanaf een andere
        // core, of met de MMU uit.
        dev::push(ctrl, CTRL_STRIDE as usize);
        for (base, cap) in [
            (tail.outbox(), RING_DATA_CAP),
            (tail.net_tx(), NET_RING_DATA_CAP),
            (tail.net_rx(), NET_RING_DATA_CAP),
        ] {
            ring::init(base, cap).map_err(|e| {
                println!("cage: slot {s}: ring at {:#x}: {e} HOPOS_CAGE_RING", base.0);
                err(code::RING)
            })?;
        }
        Ok(())
    }

    /// Zet `CTRL_SHARED` van elke levende bewoner van app-core `c`: 1 als er
    /// twee of meer zijn (hun idle yieldt dan, zodat de buren draaien),
    /// anders 0 (share.go `refreshShared`). `leaving` telt niet meer mee:
    /// die is gevraagd te stoppen. De kern is de enige schrijver van dit
    /// woord; de app leest het alleen.
    fn refresh_shared(&self, c: layout::Core, leaving: Option<Slot>) {
        if c.get() == 0 {
            return;
        }
        let mut live = [0u8; SHARE_SCAN];
        let mut n = 0;
        let _ = roster::residents(&self.plan, c, |id| {
            let Some(slot) =
                Slot::new(usize::from(id)).filter(|s| s.get() <= SLOT_CAP && Some(*s) != leaving)
            else {
                return;
            };
            if self.live(slot)
                && let Some(x) = live.get_mut(n)
            {
                *x = id;
                n += 1;
            }
        });
        let shared = u64::from(n >= 2);
        for id in live.iter().take(n) {
            if let Some(slot) = Slot::new(usize::from(*id)) {
                self.ctrl_write(slot, CTRL_SHARED, shared);
            }
        }
    }

    /// Maakt `slot` bewoner van de OS-core (fysiek `cpu`): de rotatie van de
    /// kern (`el2::next`) geeft hem de core in de idle van de executor.
    fn host(&self, slot: Slot, b: &Built, cpu: usize) -> Result<(), CageError> {
        el2::host(&self.plan, b.ctx).map_err(|e| {
            println!("cage: slot {slot} on the OS core: {e} HOPOS_CAGE_DISPATCH");
            err(code::DISPATCH)
        })?;
        println!(
            "cage: slot {slot} hosted on the OS core (cpu {cpu}), next to the kern HOPOS_OS_HOST"
        );
        Ok(())
    }
}

// FLIP: de adoptie na een kern-flip (hopos/src/flip.rs). Alleen arm64 flipt
// warm; op riscv64 weigert de constructor van de ISA al.
impl<I: Isa> Kooi<I> {
    /// Geeft een levende bewoner zijn plek in de kooi terug: de
    /// control-page, de core en de SMP-breedte `cores` (voor status, stop,
    /// klok en de secundaires bij een revoke), en zijn frame-ringen aan de
    /// switch van deze kern. Zonder ring-init: de indexen staan in de ring
    /// zelf, en de app schrijft er nog in. De breedte komt uit het
    /// handoff-blob (de kern schreef het, niet de app), dus hij is even
    /// vertrouwd als die van de bouw.
    pub(crate) fn adopt_slot(
        &mut self,
        slot: Slot,
        part: Region,
        core: usize,
        cores: usize,
    ) -> Result<(), CageError> {
        let s = self.slot(slot)?;
        let c = layout::Core::new(core).ok_or(err(code::PLAN))?;
        let tail = tail_of(part).ok_or(err(code::TAIL))?;
        let ctrl = tail.ctrl_page();
        let ctx = self.plan.ctx_pa(s).map_err(|_| err(code::PLAN))?;
        dev::pull(ctrl.add(CTRL_ENTRY), 8);
        let entry = dev::read64(ctrl.add(CTRL_ENTRY));
        if let Some(b) = self.built.get_mut(slot.get()) {
            *b = Some(Built {
                s,
                ctx,
                ctrl,
                core: c,
                entry,
                part,
                cores: cores.max(1),
            });
        }
        // De OS-core: de nieuwe kern geeft Hop de core weer in zijn idle.
        // Het plan (en dus de bewonerslijst van sched-blok 0) overleefde de
        // flip ongeschreven; staat hij er toch niet meer in, dan terug, met
        // zijn bewaarde staat.
        if core == 0 {
            match el2::rehost(&self.plan, ctx) {
                Ok(again) => println!(
                    "cage: slot {slot} back in the OS core rotation (re-added: {again}) HOPOS_OS_HOST"
                ),
                Err(e) => {
                    println!(
                        "cage: slot {slot} not back on the OS core: {e} HOPOS_FLIP_ADOPT_FAIL"
                    );
                    return Err(err(code::DISPATCH));
                }
            }
        }
        attach(s, tail, core == 0);
        crate::clock::attach(slot, ctrl);
        // Vers zaad van déze kern; de generatie telt door vanaf die van de
        // vorige, dus de bewoner ziet hem als nieuw (seed.rs).
        crate::seed::plant(ctrl);
        println!(
            "cage: slot {slot} adopted: part {:#x}+{:#x}, ctrl {:#x}, core {core}, entry {entry:#x}",
            part.base, part.size, ctrl.0
        );
        Ok(())
    }
}

impl<I: Isa> Cage for Kooi<I> {
    fn clear(&mut self, base: u64, len: u64) {
        let Ok(n) = usize::try_from(len) else { return };
        // Eerst de regels van de vorige huurder weg, waar de ISA dat vraagt
        // (`Isa::SCRUB`); dan de nullen, en naar DRAM: de nieuwe eigenaar
        // leest zijn partitie vanaf een andere core, of ongecached.
        if I::SCRUB {
            dev::pull(Pa(base), n);
        }
        dev::clear(Pa(base), n);
        dev::push(Pa(base), n);
    }

    fn build(
        &mut self,
        slot: Slot,
        part: Region,
        entry: u64,
        first: Core,
        cores: usize,
    ) -> Result<(), CageError> {
        let s = self.slot(slot)?;
        if !(1..=I::MAX_CORES).contains(&cores) {
            println!(
                "cage: slot {slot}: {cores} cores asked, this cage carries 1..={} HOPOS_CAGE_SMP",
                I::MAX_CORES
            );
            return Err(err(code::SPAN));
        }
        let core = layout::Core::new(first.get()).ok_or(err(code::PLAN))?;
        let tail = tail_of(part).ok_or(err(code::TAIL))?;
        let ctx = self.plan.ctx_pa(s).map_err(|_| err(code::PLAN))?;
        // Eerst uit elke oude bewonerslijst (een vorige levensduur op een
        // gedeelde core laat een dode byte achter), dan het ctx-blok leeg
        // (staat Empty: een rotatie die hem nog ziet, slaat hem over), dan
        // pas de nieuwe levensduur. Een reboot liet anders `CtxRunning` van
        // een vorige boot staan (Go, 17-08).
        roster::forget(&self.plan, s).map_err(|e| {
            println!("cage: slot {slot}: resident lists: {e} HOPOS_CAGE_ROSTER");
            err(code::ROSTER)
        })?;
        dev::clear(ctx, CTX_LEN as usize);
        let b = Built {
            s,
            ctx,
            ctrl: tail.ctrl_page(),
            core,
            entry,
            part,
            cores,
        };
        let root = self.isa.build(&self.plan, &b)?;
        self.arm_page(&b, tail)?;
        for (off, v) in [
            (CTX_CTRL_PA, b.ctrl.0),
            (CTX_UNIT_SLOT, s.get() as u64),
            (CTX_RING_HEAD_PA, tail.net_rx().0 + ring::HEAD_OFF),
        ] {
            dev::write64(ctx.add(off), v);
        }
        dev::push(ctx, CTX_LEN as usize);
        dev::mb();
        attach(s, tail, b.core.get() == 0);
        if let Some(x) = self.built.get_mut(slot.get()) {
            *x = Some(b);
        }
        crate::clock::attach(slot, b.ctrl);
        println!(
            "cage: slot {slot} built: part {:#x}+{:#x} -> {LINK_BASE:#x}, root {root:#x}, ctrl {:#x}, core {core}",
            part.base, part.size, b.ctrl.0
        );
        Ok(())
    }

    fn dispatch(&mut self, slot: Slot, core: Core) -> Result<(), CageError> {
        let b = self.built(slot).ok_or(err(code::NOT_BUILT))?;
        let c = layout::Core::new(core.get())
            .filter(|c| *c == b.core)
            .ok_or(err(code::DISPATCH))?;
        // `core` is logisch (0 = de OS-core), `cpu` fysiek.
        let cpu = self.plan.phys_core(c);
        if core == Core::OS {
            self.host(slot, &b, cpu)?;
        } else {
            self.isa.start(&self.plan, &b)?;
            self.refresh_shared(c, None);
        }
        // De startregel van elke levensduur, welke weg hij ook kwam (de
        // boot-plaatsing of `STREAM_IMAGE` van Hop): hier, want dit is de
        // ene plek waar elke start langskomt.
        println!(
            "HOPOS_SLOT_START slot={slot} core={core} cpu={cpu} entry={:#x} part={:#x}+{:#x}",
            b.entry, b.part.base, b.part.size
        );
        Ok(())
    }

    fn dispatch_secondary(&mut self, slot: Slot, core: Core) -> Result<(), CageError> {
        let b = self.built(slot).ok_or(err(code::NOT_BUILT))?;
        let c = layout::Core::new(core.get()).ok_or(err(code::PLAN))?;
        // De kern gaf de breedte al tegen zijn eigen boekhouding (nooit de
        // page); hier alleen nog de vorm: een secundaire is een andere core
        // van de span, nooit de OS-core.
        let span = b.core.get()..b.core.get() + b.cores;
        if core == Core::OS || c == b.core || !span.contains(&c.get()) {
            println!(
                "cage: slot {slot}: SMP core {core} outside span {}..{} HOPOS_SMP_DISPATCH_FAIL",
                span.start, span.end
            );
            return Err(err(code::SPAN));
        }
        self.isa.start_secondary(&self.plan, &b, c)
    }

    fn request_exit(&mut self, slot: Slot, grace: Duration) {
        // De termijn in ms op de vlag, nooit onder KILL_STOP: 0 is geen
        // verzoek, en een app die alleen op "niet 0" kijkt (de Go-SDK) stopt
        // bij elke waarde meteen.
        let ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        self.ctrl_write(slot, CTRL_KILL, ms.max(KILL_STOP));
        let Some(b) = self.built(slot) else { return };
        // Een buur die alleen overblijft, hoeft niet meer te yielden; en de
        // kick, zodat een slaper de vlag nu ziet en niet op zijn wektijd.
        // Op de OS-core geeft de kern zelf de beurten: geen kick.
        if b.core.get() != 0 {
            self.refresh_shared(b.core, Some(slot));
            I::kick(&self.plan, b.core);
        }
    }

    fn quiet(&self, slot: Slot, core: Core) -> bool {
        let dead = self.dead(slot);
        if core == Core::OS {
            // De OS-core parkeert nooit (de kern draait er); stil is dood,
            // of niet in zijn rotatie.
            return dead || !self.ctx(slot).is_some_and(|c| el2::hosts(&self.plan, c));
        }
        let Some(c) = layout::Core::new(core.get()) else {
            return false;
        };
        let parked = I::PARKS
            && matches!(
                I::core_state(&self.plan, c),
                Ok(CoreState::Cold | CoreState::Parked)
            );
        // De primaire is de context van de kooi; elke andere core van de
        // span is een secundaire met een eigen ctx-blok. Tot 30-09 las dit
        // voor elke core de staat van de primaire: een SMP-app waarvan de
        // primaire al dood was, telde zo een secundaire die nog draaide als
        // stil (E9).
        if self.built(slot).is_none_or(|b| b.core == c) {
            return dead || parked;
        }
        let sec_dead = self
            .plan
            .smp_ctx_pa(c)
            .is_ok_and(|x| matches!(el2::ctx_state(x), Some(CtxState::Empty | CtxState::Dead)));
        sec_dead || parked
    }

    fn live(&self, slot: Slot) -> bool {
        matches!(
            self.ctx_state(slot),
            Some(CtxState::Running | CtxState::Saved | CtxState::BootPending)
        )
    }

    fn revoke(&mut self, slot: Slot) {
        crate::clock::detach(slot);
        let (Ok(s), Some(ctx)) = (self.slot(slot), self.ctx(slot)) else {
            return;
        };
        let b = self.built(slot);
        if b.is_some_and(|b| b.core.get() == 0) {
            // De OS-core: de aanvraag en de kick. Hij draait nu niet (de
            // kern draait, deze code); de rotatie doodt hem vóór zijn
            // volgende beurt (`el2::next`), ook een slaper met een verre
            // wektijd.
            el2::recall(ctx);
            println!(
                "cage: slot {slot}: revoked on the OS core, the rotation ends it before its next turn HOPOS_OS_UNHOST"
            );
        } else {
            // Het woord dat een switcher bij elke yield, tick en ronde leest
            // (riscv64); de kern schrijft het na de start als enige.
            el2::ctx_write(ctx, CTX_REVOKE, 1);
        }
        self.isa.revoke(&self.plan, s, b.as_ref());
        if let Some(b) = b {
            self.refresh_shared(b.core, Some(slot));
        }
    }

    fn pending(&self, slot: Slot) -> bool {
        !I::TIME_SLICE
            && self.built(slot).is_some_and(|b| b.core.get() != 0)
            && self.ctx_state(slot) == Some(CtxState::BootPending)
    }

    fn holder(&self, core: Core) -> Option<Slot> {
        // Een secundaire SMP-context (een id boven SLOT_CAP) is geen kooi
        // om te offeren.
        let c = layout::Core::new(core.get()).filter(|_| core != Core::OS)?;
        let at = self.plan.park_mbox_pa(c).ok()?.add(SCHED_CURRENT);
        dev::pull(at, 8);
        Slot::new(usize::try_from(dev::read64(at)).ok()?).filter(|s| s.get() <= SLOT_CAP)
    }

    fn smp_request(&self, slot: Slot) -> u64 {
        self.ctrl_read(slot, CTRL_SMP_REQ)
    }

    fn clear_smp_request(&mut self, slot: Slot) {
        self.ctrl_write(slot, CTRL_SMP_REQ, 0);
    }

    fn status(&self, slot: Slot) -> Status {
        Status {
            // De ctx-staat van DEZE kooi, op elke core: een gedeelde core
            // draait door zolang er een buur leeft (Hop las een lid dat al
            // exit deed als draaiend, 30-09), en hij slaapt zodra elk lid
            // yieldt. Dat slapen is geen exit: met de core-toestand erbij las
            // Hop een slapende controller in een sharegroup als gestopt en
            // herplaatste hem acht keer per boot (02-10, qemu-controller met
            // STULP_QEMU_BUNDLE=1).
            core_on: self.built(slot).is_some() && self.live(slot),
            app: self.ctrl_read(slot, CTRL_STATUS),
            exit_code: self.ctrl_read(slot, CTRL_EXIT_CODE),
            heartbeat: self.ctrl_read(slot, CTRL_HEARTBEAT),
            ram_size: self.ctrl_read(slot, CTRL_RAM_SIZE),
            mem_sys: self.ctrl_read(slot, CTRL_MEM_SYS),
            idle_ns: idle_ns(self.ctrl_read(slot, CTRL_IDLE)),
            wakes: self.ctrl_read(slot, CTRL_WAKES),
            cores: self.ctrl_read(slot, CTRL_CORES),
            at_ns: cpu::idle::now(),
            fault_vec: self.ctrl_read(slot, CTRL_FAULT_VEC),
            fault_esr: self.ctrl_read(slot, CTRL_FAULT_ESR),
            fault_far: self.ctrl_read(slot, CTRL_FAULT_FAR),
            // De vectortabel van EL1 is ARM (applib::mmu); op RISC-V is een
            // trap van de app altijd een fault van de switcher, en blijven
            // deze woorden nul.
            app_fault_vec: self.ctrl_read(slot, CTRL_APP_FAULT_VEC),
            app_fault_esr: self.ctrl_read(slot, CTRL_APP_FAULT_ESR),
            app_fault_elr: self.ctrl_read(slot, CTRL_APP_FAULT_ELR),
            app_fault_far: self.ctrl_read(slot, CTRL_APP_FAULT_FAR),
        }
    }

    fn publish(
        &mut self,
        slot: Slot,
        ports: &[u16],
    ) -> impl Future<Output = Result<(), PortError>> {
        publish_ports(slot, ports)
    }

    fn unpublish(&mut self, slot: Slot) {
        unpublish_ports(slot);
    }

    fn detach(&mut self, slot: Slot) -> impl Future<Output = ()> {
        // De control-page uit de tabel van de klok, nu en niet pas in de
        // future: de lifecycle roept dit bij elke stop (stil of ingetrokken)
        // en op de faalpaden na de bouw, vóór de partitie vrijkomt. Tot 24-10
        // stond het alleen in `revoke`, dus na een stille stop schreven de
        // telemetrie (temperatuur, zaad) en `clock::set` elke seconde in een
        // vrijgegeven partitie, ook als die al van een ander was (F1).
        crate::clock::detach(slot);
        detach(slot)
    }
}

/// De cores van deze node over architectuur `I`: logische core 0 is de
/// OS-core, app-core i de i-de andere fysieke core (`Plan::phys_core`).
pub(crate) struct KooiCores<I: Isa> {
    plan: Plan,
    isa: PhantomData<I>,
}

impl<I: Isa> KooiCores<I> {
    /// De cores van dit plan.
    pub(crate) fn new(plan: Plan) -> KooiCores<I> {
        KooiCores {
            plan,
            isa: PhantomData,
        }
    }

    /// App-core `core`, als hij in het plan staat (niet de OS-core).
    fn app(&self, core: Core) -> Option<layout::Core> {
        layout::Core::new(core.get()).filter(|_| core != Core::OS && core.get() <= self.app_cores())
    }
}

impl<I: Isa> Cores for KooiCores<I> {
    fn app_cores(&self) -> usize {
        self.plan.app_cores()
    }

    fn class(&self, core: Core) -> Option<CoreClass> {
        let c = layout::Core::new(core.get()).filter(|_| core.get() <= self.app_cores())?;
        I::class(self.plan.phys_core(c))
    }

    fn power(&self, core: Core) -> Power {
        if core == Core::OS {
            return Power::On; // de kern zelf
        }
        match self.app(core).map(|c| I::core_state(&self.plan, c)) {
            Some(Ok(CoreState::Running(_))) => Power::On,
            _ => Power::Off,
        }
    }

    fn kick(&mut self, core: Core) {
        // De OS-core hoeft geen kick: de kern geeft zijn bewoners zelf de
        // core, in elke idle-ronde.
        if let Some(c) = self.app(core) {
            I::kick(&self.plan, c);
        }
    }
}

/// De idle-ticks van de architectuurteller in nanoseconden.
fn idle_ns(ticks: u64) -> u64 {
    let f = cpu::idle::freq().max(1);
    u64::try_from(u128::from(ticks) * 1_000_000_000 / u128::from(f)).unwrap_or(u64::MAX)
}

// De vier bevestigingen van de switch voor de kooi.

/// De bevestiging van de `Attach` van een verse kooi aan de switch. Niemand
/// wacht erop (de kooi-trait is synchroon); het resultaat wordt bij de
/// volgende attach opgehaald en gemeld als het een weigering was.
static ATTACH_ACK: Ack = Ack::new();
/// De bevestiging van de `Detach` bij een stop; de lifecycle wacht erop.
static DETACH_ACK: Ack = Ack::new();
/// De bevestiging van elke `Publish` van de poorten van een start. De
/// lifecycle-actor is de enige zender en wacht elke bevestiging af.
static PUBLISH_ACK: Ack = Ack::new();
/// De bevestiging van de `UnpublishSlot` bij een stop. Niemand wacht erop:
/// de brievenbus is een rij, dus een publicatie van een volgende start komt
/// altijd ná deze intrekking aan de beurt.
static UNPUBLISH_ACK: Ack = Ack::new();

/// Hangt de frame-ringen van een verse kooi aan de switch (`hopswitch.
/// Attach` in `armSlot`): ná de ring-init, vóór het startschot. De switch
/// wordt eigenaar van de handvatten; een oude poort op dit slot vervalt.
/// Een volle brievenbus of een switch die er niet is (geen NIC) laat de app
/// zonder slot-LAN draaien: één regel, geen weigering van de start.
///
/// `os`: het slot woont op de OS-core, het hart van de kern. Op riscv64
/// belooft de kern dan [`ring::Coherence::Hardware`]: app en kern delen één
/// cache, dus onderhoud is aan geen van beide kanten nodig (de app belooft
/// hetzelfde zodra hij `CTRL_HART` leest; een oude app belooft niets en dan
/// doet de kern per record toch het onderhoud). Op arm64 belooft de kern
/// al wat [`tail_rings`] zegt.
fn attach(s: layout::Slot, tail: Tail, os: bool) {
    if let Some(Err(e)) = ATTACH_ACK.try_take() {
        println!("cage: an earlier attach was refused: {e} HOPOS_CAGE_ATTACH");
    }
    let rings = if os && cfg!(target_arch = "riscv64") {
        ring::Coherence::Hardware
    } else {
        tail_rings::promise(s, tail.base())
    };
    let (Ok(tx), Ok(rx)) = (
        AbiTx::open(tail.net_tx(), NET_RING_DATA_CAP, rings),
        ring::Writer::open_with(tail.net_rx(), NET_RING_DATA_CAP, rings),
    ) else {
        println!("cage: slot {s}: frame rings do not open HOPOS_CAGE_ATTACH");
        return;
    };
    let cmd = Command::Attach {
        slot: s.get(),
        tx,
        rx,
        ack: &ATTACH_ACK,
    };
    if crate::net::COMMANDS.try_send(cmd).is_err() {
        println!("cage: slot {s}: switch mailbox full, no slot LAN HOPOS_CAGE_ATTACH");
    }
}

/// De belofte van de kern voor de frame-ringen in de staart van een slot.
/// Op een board dat de pool Device mapt (Apple, de Radxa) eerst de staart
/// Normal write-back in de kernmap (`Board::map_tail_normal`, Go:
/// `mapTailNormal`, slot-ABI 7), en alleen dan belooft de kern zijn kant
/// zonder onderhoud. Weigert de remap, dan blijft het onderhoud: traag maar
/// correct. GEMETEN 01-10: app naar app op de M4 van 52 naar duizenden
/// MB/s (M8), op de Radxa van 29,83 met een corrupte RX-ring (Device tegen
/// de cache van de app) naar 257 tot 262 (RX1, ook 40 GiB foutloos). Elders
/// is de pool al gemapt zoals [`crate::net::RINGS`] zegt (op riscv64
/// `Maintained`: de harts van de C906 zijn niet coherent).
mod tail_rings {
    use super::{ABI_TAIL, layout, ring};
    use board::Board;
    use cpu::println;
    use dev::Pa;

    pub(super) fn promise(s: layout::Slot, base: Pa) -> ring::Coherence {
        match crate::BOARD.map_tail_normal(base.0, ABI_TAIL) {
            None => crate::net::RINGS,
            Some(Ok(())) => ring::Coherence::Hardware,
            Some(Err(why)) => {
                println!(
                    "cage: slot {s}: tail {:#x} stays device-mapped ({why}), rings with maintenance HOPOS_CAGE_TAIL",
                    base.0
                );
                ring::Coherence::Maintained
            }
        }
    }
}

/// Haalt de ringen van `slot` weer van de switch en wacht op de
/// bevestiging (`Cage::detach`, door de lifecycle bij elke stop): daarna
/// raakt de switch de staart van de partitie niet meer aan. Zonder switch
/// (geen NIC) hing er niets en leest niemand de brievenbus.
async fn detach(slot: Slot) {
    if !crate::net::switch_up() {
        return;
    }
    let cmd = Command::Detach {
        slot: slot.get(),
        ack: &DETACH_ACK,
    };
    match sync::oneshot::call(&crate::net::COMMANDS, &DETACH_ACK, cmd).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => println!("cage: slot {slot}: detach refused: {e} HOPOS_CAGE_DETACH"),
        Err(_) => {
            println!("cage: slot {slot}: switch mailbox full, detach not sent HOPOS_CAGE_DETACH")
        }
    }
}

/// Zet de poorten van een start door (Hop of een jobspec), elk voor tcp en
/// udp (Go's `armSlot`: de jobspec kent geen protocol, en een app die er één
/// bedient laat de ander onbeantwoord). Stopt bij de eerste weigering; wat er al
/// open stond, trekt de lifecycle in (`Cage::unpublish`).
async fn publish_ports(slot: Slot, ports: &[u16]) -> Result<(), PortError> {
    use net::nat::Proto;
    if !crate::net::switch_up() {
        let port = ports.first().copied().unwrap_or(0);
        println!(
            "cage: slot {slot}: no switch on this node, port {port} not published HOPOS_CAGE_PUBLISH"
        );
        return Err(PortError::Refused { port });
    }
    for &port in ports {
        for proto in [Proto::Tcp, Proto::Udp] {
            match crate::net::publish(&PUBLISH_ACK, proto, slot.get(), port).await {
                Ok(()) => {}
                Err(net::Error::AlreadyPublished { port, slot: owner }) => {
                    return Err(PortError::Taken { port, owner });
                }
                Err(e) => {
                    println!("cage: slot {slot}: port {port}: {e} HOPOS_CAGE_PUBLISH");
                    return Err(PortError::Refused { port });
                }
            }
        }
    }
    Ok(())
}

/// Trekt de publicaties (en flows) van `slot` in, zonder te wachten.
fn unpublish_ports(slot: Slot) {
    let _ = UNPUBLISH_ACK.try_take();
    let cmd = Command::UnpublishSlot {
        slot: slot.get(),
        ack: &UNPUBLISH_ACK,
    };
    if crate::net::COMMANDS.try_send(cmd).is_err() {
        println!("cage: slot {slot}: switch mailbox full, unpublish not sent HOPOS_CAGE_PUBLISH");
    }
}
