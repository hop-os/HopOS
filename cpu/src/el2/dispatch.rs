//! De Rust-kant van de switcher: wat de kern met de EL2-code van de
//! app-cores doet.
//!
//! Dit bezit: de switch-code-kopie in de plan-regio (de descriptor met magic
//! `HOPSWTC1` en de FNV-som, `stage2_tamago.go` `installSwitchCode`), de
//! gedeelde vectortabel van de app-cores (dunne thunks naar de switcher),
//! de parkeerlus en de sched-blokken (`stage2.go` `InitVectors`), het
//! startschot via de park-mailbox (`cage_arm64.go` `cageDispatch` plus
//! `share.go` `residentReset`), de kick, de SMP-handoff (`smp_context.go`)
//! en de lezers van het ctx-blok die de wekker gebruikt (`waker.go`).
//!
//! Wat hier NIET staat: welk slot op welke core komt, de bewonerslijst van
//! een gedeelde core, de wekker-lus zelf en het PSCI-startschot van een
//! koude core. Dat is beleid van de kern en het board; deze module levert
//! de primitieven en rekent alleen met adressen uit het [`Plan`].
//!
//! # De vectortabel van een app-core
//!
//! `plan.vec_base_pa()` draagt zestien ingangen van 0x80 bytes (VBAR_EL2
//! van elke app-core, via `CTRL_VEC_PA`). Elke ingang is dezelfde thunk:
//! `stp x2, x3, [sp, #16]`, `x2` = de vectorindex, en een sprong naar de
//! switcher. Daar kiest de index het pad: 8 (0x400, synchroon uit EL1) met
//! EC = HVC kiest op de immediate ([`HVC_EXIT`], [`HVC_YIELD`],
//! [`HVC_WAKE`], [`HVC_DOOR_ACK`], [`HVC_KICK_OS`]); 10 (0x500, FIQ uit EL1) is op Apple de
//! kick; al het andere is een fault-rapport op de control-page van de
//! bewoner, waarna die dood is en de core doorroteert.

use super::layout::{
    CTX_BOOT_ARG, CTX_BOOT_PC, CTX_CTRL_PA, CTX_KICK_PENDING, CTX_KICK_TARGET, CTX_LEN,
    CTX_NEXT_PA, CTX_RING_HEAD_PA, CTX_STATE, CTX_VTTBR, CTX_WAKE, Core, CtxState, PARK_CODE_OFF,
    PARK_COLD, PARK_MBOX_LEN, PARK_MBOX_OFF, PARK_PARKED, Plan, SCHED_CURRENT, SCHED_CURSOR,
    SCHED_MBOX_CTX, SCHED_MBOX_PC, SCHED_ROTOR, SCHED_S2_PA, SWITCH_CODE_MAX,
};
use super::{Error, roster, stage2, switch};
use abi::checksum::Fnv64;
use abi::hopabi::{
    CTRL_RX_DOOR, CTRL_S2_TABLE, CTRL_SMP_FN, CTRL_SMP_G0, CTRL_SMP_MAIR, CTRL_SMP_MBOX,
    CTRL_SMP_MP, CTRL_SMP_SP, CTRL_SMP_STUB, CTRL_SMP_TCR, CTRL_SMP_TTBR0, CTRL_SMP_VBAR,
    CTRL_VEC_PA, CTRL_VMID, RX_DOOR_ARMED,
};
use dev::Pa;

// ---------------------------------------------------------------------------
// Het contract met de assembly.
// ---------------------------------------------------------------------------

/// HVC #0: de coöperatieve exit (de app zette zijn status al).
pub(crate) const HVC_EXIT: u64 = 0;
/// HVC #1: de idle-yield, met de wektijd in x1 (applib `arch::hvc_yield`).
/// Elke immediate die hieronder niet staat, is ook een yield.
pub(crate) const HVC_YIELD: u64 = 1;
/// HVC #4: wek een sibling-core van dezelfde app (x0 = zijn affiniteit).
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code) // alleen de switcher-asm leest dit
)]
pub(crate) const HVC_WAKE: u64 = 4;
/// HVC #5: de doorbell-interrupt is afgehandeld (alleen Apple).
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code) // alleen de switcher-asm leest dit
)]
pub(crate) const HVC_DOOR_ACK: u64 = 5;
/// HVC #6: bel de kern op de OS-core (een frame op de TX-ring, een
/// system-call): de switcher stuurt zijn kick-SGI als de kern op dat moment
/// geen SEV hoort (`oscore::SCHED_OS_KICK`), en keert meteen terug. Een
/// yield (HVC #1) belt ook, want een app die idle gaat na een publicatie
/// wacht meestal op het antwoord.
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code) // alleen de switcher-asm leest dit
)]
pub(crate) const HVC_KICK_OS: u64 = 6;

/// De vectorindex van een synchrone exception uit een lagere EL (AArch64).
pub(crate) const VEC_SYNC_LOWER: u64 = 8;
/// De vectorindex van een FIQ uit een lagere EL (AArch64): de Apple-kick.
pub(crate) const VEC_FIQ_LOWER: u64 = 10;
/// De afstand tussen twee vectoringangen.
pub(crate) const VEC_STRIDE: u64 = 0x80;
/// Het aantal vectoringangen.
pub(crate) const VEC_COUNT: u64 = 16;
/// De maat van de vectortabel: 2 KB, ook de uitlijning die VBAR_EL2 eist.
pub(crate) const VEC_TABLE_LEN: u64 = VEC_STRIDE * VEC_COUNT;

/// Waar de thunk x2/x3 op de sched-scratch zet (SP_EL2 = scratch); de
/// switcher legt er zelf x0/x1 op +0/+8 bij.
pub(super) const SCRATCH_X2: u64 = 16;

/// "HOPSWTC1" little-endian: de magic van de switch-code-descriptor.
pub(crate) const SWITCH_MAGIC: u64 = 0x3143_5457_5350_4F48;
/// Descriptor +8: de totale lengte (descriptor plus blobs).
pub(crate) const SW_LEN: u64 = 8;
/// Descriptor +16: de FNV-1a-64 over de blobs, in kopieervolgorde.
pub(crate) const SW_HASH: u64 = 16;
/// Descriptor +24: de offset van de switcher (el2entry).
pub(crate) const SW_ENTRY: u64 = 24;
/// Descriptor +32: de offset van de stage-2-trampoline.
pub(crate) const SW_TRAMP: u64 = 32;
/// Descriptor +40: de offset van de SMP-trampoline.
pub(crate) const SW_SMP: u64 = 40;
/// De maat van de descriptor: de blobs beginnen hier.
pub(crate) const SW_HEAD: u64 = 0x40;
/// De uitlijning van elke blob in de kopie: een cacheline.
const SW_ALIGN: u64 = 64;

/// De bovengrens per blob (blobs.go `MaxBlobSize`). Een groter bereik
/// tussen een symbool en zijn eindmarker betekent een verschoven marker.
pub const MAX_BLOB: usize = 0x2000;

/// Welke EL2-code het board draait. Het BOARD kiest, niet een vlag in een
/// bouwscript (Derek, 20-09), want het verschil is silicium:
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Flavor {
    /// E2H=0: QEMU virt, de Pi's, RK3566, Altra. De switcher slaapt in WFE.
    Nvhe,
    /// E2H=1, de EL1-registers als `_EL12`: de O6N (17-09: nVHE-EL1 stierf
    /// er binnen 0,5 s). De switcher slaapt in WFE.
    Vhe,
    /// E2H=1 met Apple's fast IPI: WFE slaapt op de M4 niet (CYC_OVRD), dus
    /// WFI plus een geackte FIQ als kick (gemeten 02-09).
    AppleVhe,
}

// ---------------------------------------------------------------------------
// De switch-code-kopie.
// ---------------------------------------------------------------------------

/// De geïnstalleerde switch-code: waar de drie blobs in de plan-regio staan.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Installed {
    /// De switcher: het sprongdoel van de thunks.
    pub entry: Pa,
    /// De stage-2-trampoline: het startadres van een app-core
    /// (`CTX_BOOT_PC`, mailbox-woord 1, PSCI CPU_ON), x0 = de control-page.
    pub tramp: Pa,
    /// De SMP-trampoline: het startadres van een secundaire of node-core,
    /// x0 = de handoff van [`prepare_smp`].
    pub smp_tramp: Pa,
    /// De totale lengte van de kopie.
    pub len: u64,
    /// De som over de blobs.
    pub hash: u64,
}

/// Rekent de indeling van de kopie uit: offsets per blob (op een
/// cacheline), de totale lengte en de som. Eén pass voor install én adoptie.
fn place(blobs: &[&[u8]; 3]) -> Result<([u64; 3], u64, u64), Error> {
    let mut sum = Fnv64::new();
    let mut offs = [0u64; 3];
    let mut off = SW_HEAD;
    for (slot, b) in offs.iter_mut().zip(blobs) {
        sum.write(b);
        *slot = off;
        let len = b.len() as u64;
        let need = len
            .checked_add(SW_ALIGN - 1)
            .map(|l| l & !(SW_ALIGN - 1))
            .and_then(|l| off.checked_add(l))
            .unwrap_or(u64::MAX);
        if need > SWITCH_CODE_MAX {
            return Err(Error::SwitchCodeFull {
                need,
                max: SWITCH_CODE_MAX,
            });
        }
        off = need;
    }
    Ok((offs, off, sum.sum()))
}

/// Kopieert de blobs naar `base` en schrijft de descriptor, de magic als
/// laatste. Eerst gaat de magic op nul: een koude boot die over een oude
/// kopie heen schrijft, laat zo nooit een geldige kop op halve code staan.
fn install_blobs(base: Pa, blobs: &[&[u8]; 3]) -> Result<Installed, Error> {
    let (offs, len, hash) = place(blobs)?;
    dev::write64(base, 0);
    for (off, b) in offs.iter().zip(blobs) {
        dev::copy_in(base.add(*off), b);
    }
    dev::write64(base.add(SW_LEN), len);
    dev::write64(base.add(SW_HASH), hash);
    dev::write64(base.add(SW_ENTRY), offs[0]);
    dev::write64(base.add(SW_TRAMP), offs[1]);
    dev::write64(base.add(SW_SMP), offs[2]);
    dev::write64(base, SWITCH_MAGIC);
    Ok(Installed {
        entry: base.add(offs[0]),
        tramp: base.add(offs[1]),
        smp_tramp: base.add(offs[2]),
        len,
        hash,
    })
}

/// Neemt een zittende kopie over zonder één byte te schrijven: er draaien
/// cores in. Dat mag alleen als de som gelijk is aan die van onze blobs.
fn adopt_blobs(base: Pa, blobs: &[&[u8]; 3]) -> Result<Installed, Error> {
    let (_, _, ours) = place(blobs)?;
    let resident = dev::read64(base.add(SW_HASH));
    if dev::read64(base) != SWITCH_MAGIC || resident != ours {
        return Err(Error::SwitchCodeMismatch { resident, ours });
    }
    let len = dev::read64(base.add(SW_LEN));
    let at = |field: u64| -> Result<Pa, Error> {
        let off = dev::read64(base.add(field));
        if off < SW_HEAD || off >= len || len > SWITCH_CODE_MAX {
            return Err(Error::SwitchCodeFull {
                need: len.max(off),
                max: SWITCH_CODE_MAX,
            });
        }
        Ok(base.add(off))
    };
    Ok(Installed {
        entry: at(SW_ENTRY)?,
        tramp: at(SW_TRAMP)?,
        smp_tramp: at(SW_SMP)?,
        len,
        hash: resident,
    })
}

/// De som over de blobs van `flavor` in dít image: wat een kern-flip tegen
/// [`installed_hash`] houdt voordat hij met levende bewoners vertrekt.
pub fn image_hash(flavor: Flavor) -> Result<u64, Error> {
    let (_, _, hash) = place(&switch::blobs(flavor)?)?;
    Ok(hash)
}

/// De som in de descriptor van de geïnstalleerde kopie, of `None` als er
/// geen geldige kopie staat (`SwitchCodeHash`).
#[must_use]
pub fn installed_hash(plan: &Plan) -> Option<u64> {
    let base = plan.switch_code_pa();
    (dev::read64(base) == SWITCH_MAGIC).then(|| dev::read64(base.add(SW_HASH)))
}

/// Kopieert de drie EL2-blobs uit het kern-image naar de plan-regio
/// (`SWITCH_CODE_OFF`): vanaf hier voert een app-core nooit meer
/// kern-image-bytes uit, en kan een kern-flip het oude venster verlaten
/// terwijl geyielde en geparkeerde cores doordraaien (docs/kern-flip.md).
///
/// Anders dan in Go gebeurt dit bij elke boot, niet alleen op een
/// flip-capabele node: de kern van v3 staat op EL2 en garandeert niet dat
/// zijn symbooladres een fysiek adres is, dus een image-adres als sprongdoel
/// voor een app-core bestaat niet meer.
///
/// Niet aanroepen als er bewoners leven: dan is het [`adopt`].
pub fn install_switch_code(plan: &Plan, flavor: Flavor) -> Result<Installed, Error> {
    let base = plan.switch_code_pa();
    let installed = install_blobs(base, &switch::blobs(flavor)?)?;
    // Zelfde fetch-contract als de thunks: geschreven door de kern, maar een
    // cacheable instructie-fetch moet ze vers uit DRAM halen.
    dev::pull(base, installed.len as usize);
    dev::mb();
    switch::publish_code();
    Ok(installed)
}

/// Neemt de zittende switch-code over bij een geadopteerde kern-flip: er
/// draaien al cores in de kopie, op hun sched- en ctx-blokken. Er wordt
/// niets geschreven; alleen de adressen komen terug.
///
/// Een andere som is een harde weigering: de oude cores kunnen nog draaien,
/// en een mismatch geeft geen toestemming hun code, contexten of geheugen
/// te overschrijven of opnieuw uit te geven. De boot stopt dan vóór enige
/// koude init (`HOPOS_FLIP_SWITCHCODE_MISMATCH` in Go).
pub fn adopt(plan: &Plan, flavor: Flavor) -> Result<Installed, Error> {
    adopt_blobs(plan.switch_code_pa(), &switch::blobs(flavor)?)
}

// ---------------------------------------------------------------------------
// De vectortabel, de parkeerlus en de sched-blokken.
// ---------------------------------------------------------------------------

/// `stp x2, x3, [sp, #off]` (64-bit, signed offset).
const fn stp_x2_x3_sp(off: u64) -> u32 {
    0xA900_0000 | (((off / 8) as u32 & 0x7F) << 15) | (3 << 10) | (31 << 5) | 2
}

/// `movz xd, #imm16, lsl #shift`.
const fn movz(rd: u32, imm16: u64, shift: u32) -> u32 {
    0xD280_0000 | ((shift / 16) << 21) | ((imm16 as u32 & 0xFFFF) << 5) | (rd & 0x1F)
}

/// `movk xd, #imm16, lsl #shift`.
const fn movk(rd: u32, imm16: u64, shift: u32) -> u32 {
    0xF280_0000 | ((shift / 16) << 21) | ((imm16 as u32 & 0xFFFF) << 5) | (rd & 0x1F)
}

/// `br x3`.
const BR_X3: u32 = 0xD61F_0060;

/// De thunk van vectoringang `v`: x2/x3 naar de scratch, `x2 = v`, en een
/// absolute sprong naar de switcher (movz plus een movk per niet-nul
/// halfwoord, zoals `a64.Mov64`). Absoluut, want de tabel en de kopie
/// liggen elk op hun eigen plan-adres. Geeft de woorden en hun aantal.
fn thunk(v: u64, entry: Pa) -> ([u32; 7], usize) {
    let mut w = [0u32; 7];
    w[0] = stp_x2_x3_sp(SCRATCH_X2);
    w[1] = movz(2, v, 0);
    w[2] = movz(3, entry.0 & 0xFFFF, 0);
    let mut n = 3;
    for sh in [16u32, 32, 48] {
        let part = (entry.0 >> sh) & 0xFFFF;
        if part != 0
            && let Some(slot) = w.get_mut(n)
        {
            *slot = movk(3, part, sh);
            n += 1;
        }
    }
    if let Some(slot) = w.get_mut(n) {
        *slot = BR_X3;
        n += 1;
    }
    (w, n)
}

const _: () = assert!(7 * 4 <= VEC_STRIDE);

/// Faalt als een app-core niet in de parkeerlus staat: een geldige lege
/// flip kan cores in de lus hebben, maar een core met een dispatch in zijn
/// mailbox draait, en dan is "niemand leeft" gelogen.
pub(crate) fn check_parked(plan: &Plan) -> Result<(), Error> {
    for c in 1..=plan.app_cores() {
        let core = Core::new(c).ok_or(Error::Plan(abi::Error::OutOfPlan {
            index: c,
            max: plan.app_cores(),
        }))?;
        let mbox = dev::read64(plan.park_mbox_pa(core).map_err(Error::Plan)?);
        if mbox > PARK_PARKED {
            return Err(Error::UnparkedCore { core: c, mbox });
        }
    }
    Ok(())
}

/// Zet alles klaar wat een app-core aanraakt: de zestien thunks, de
/// parkeerlus, de sched-blokken en de ctx-staten (`initAppCoreRegion`).
///
/// `keep_parked`: een lege flip heeft misschien cores in de parkeerlus
/// staan. Dan blijven de lus en mailbox-woord 0 staan (de lus leest alles
/// behalve 1 als dispatch), en moet elke core aantoonbaar geparkeerd zijn.
/// Bij een geadopteerde flip met levende bewoners wordt deze functie niet
/// aangeroepen: de clears zouden gaten maken waar een trappende core in
/// nullen springt, en de lege ctx-staten zouden elke bewoner uit de rotatie
/// schrijven.
pub fn init_app_cores(plan: &Plan, installed: &Installed, keep_parked: bool) -> Result<(), Error> {
    let park = if keep_parked {
        check_parked(plan)?;
        None
    } else {
        Some(switch::park_code()?)
    };
    init_region(plan, installed.entry, park)?;
    dev::pull(plan.vec_base_pa(), VEC_TABLE_LEN as usize);
    if let Some(p) = park {
        dev::pull(plan.park_code_pa(), p.len());
    }
    dev::mb();
    switch::publish_code();
    Ok(())
}

/// Het schrijfwerk van [`init_app_cores`], zonder cache-onderhoud.
fn init_region(plan: &Plan, entry: Pa, park: Option<&[u8]>) -> Result<(), Error> {
    let vecs = plan.vec_base_pa();
    dev::clear(vecs, VEC_TABLE_LEN as usize);
    for v in 0..VEC_COUNT {
        let (w, n) = thunk(v, entry);
        for (i, ins) in w.iter().take(n).enumerate() {
            dev::write32(vecs.add(v * VEC_STRIDE + i as u64 * 4), *ins);
        }
    }
    if let Some(p) = park {
        if p.len() as u64 > PARK_MBOX_OFF - PARK_CODE_OFF {
            return Err(Error::Blob {
                index: 3,
                len: p.len(),
            });
        }
        dev::copy_in(plan.park_code_pa(), p);
    }
    // Sched-blokken per CORE: verse DRAM is geen nul (Pi-meting). Woord 0 =
    // 0 is "koud"; bij een lege flip blijft het staan. Daarna de kooi-basis,
    // zodat de switcher volledig SP-relatief blijft.
    for c in 0..=plan.app_cores() {
        let core = Core::new(c).ok_or(Error::Plan(abi::Error::OutOfPlan {
            index: c,
            max: plan.app_cores(),
        }))?;
        let mb = plan.park_mbox_pa(core).map_err(Error::Plan)?;
        if park.is_none() {
            dev::clear(mb.add(8), PARK_MBOX_LEN as usize - 8);
        } else {
            dev::clear(mb, PARK_MBOX_LEN as usize);
        }
        dev::write64(mb.add(SCHED_S2_PA), vecs.0);
    }
    // De ctx-staat van elke kooi expliciet Empty: de kern leest dit woord al
    // vóór de eerste build van dat slot, en verse DRAM is geen nul.
    // De ctx-blokken van de secundaire cores; die van de slots begint de
    // bouw schoon (`stage2::build`), uit het kooi-geheugen.
    for c in 2..=plan.app_cores() {
        if let Some(ctx) = Core::new(c).and_then(|c| plan.smp_ctx_pa(c).ok()) {
            dev::write64(ctx.add(CTX_STATE), CtxState::Empty.raw());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Contexten en cores.
// ---------------------------------------------------------------------------

/// Wat mailbox-woord 0 van een core zegt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CoreState {
    /// Nooit geparkeerd: de eerste start is PSCI CPU_ON (of het board-pad).
    Cold,
    /// In de parkeerlus, wachtend op een dispatch.
    Parked,
    /// Gedispatcht; het woord is het x0 van de trampoline.
    Running(u64),
}

/// De toestand van `core` volgens zijn mailbox. Op ARM is het sched-blok
/// device-gemapt, dus coherent: geen veeg nodig.
pub fn core_state(plan: &Plan, core: Core) -> Result<CoreState, Error> {
    let w = dev::read64(
        plan.park_mbox_pa(core)
            .map_err(Error::Plan)?
            .add(SCHED_MBOX_CTX),
    );
    Ok(match w {
        PARK_COLD => CoreState::Cold,
        PARK_PARKED => CoreState::Parked,
        _ => CoreState::Running(w),
    })
}

/// Hoe het startschot aankwam.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Start {
    /// De core was koud: de mailbox staat klaar, maar de aanroeper moet hem
    /// nu eenmalig starten (PSCI CPU_ON of het board-pad) met `entry` en
    /// `arg`. Daarna leeft hij in de parkeerlus van HopOS.
    Cold,
    /// De core stond geparkeerd en is met een SEV gewekt.
    Woken,
}

/// Zet de bewoner met ctx-blok `ctx` als enige op `core` en geeft het
/// startschot via de park-mailbox: `entry` is de trampoline
/// ([`Installed::tramp`] of [`Installed::smp_tramp`]), `arg` zijn x0 (de
/// control-page of de SMP-handoff).
///
/// Eerst de rotatie-staat (`residentReset`): cursor 0, de bewonerslijst is
/// [deze context], SCHED_CURRENT = deze context, en de ctx-staat Running.
/// Dat mag alleen hier, want de core staat stil: er leest geen switcher
/// mee. Zonder SCHED_CURRENT zou de eerste yield het ctx-blok van context 0
/// aanwijzen. Daarna de mailbox: eerst het doel, dan het startschot.
///
/// Een core die al draait, wordt geweigerd: dat zou een app een core midden
/// in de uitvoering laten kapen. Een bewoner erbij op een draaiende core
/// gaat via de rotatie (boot-pending), niet hier.
pub fn dispatch(plan: &Plan, core: Core, ctx: Pa, entry: Pa, arg: u64) -> Result<Start, Error> {
    if arg <= PARK_PARKED {
        return Err(Error::BadArg { arg });
    }
    let mb = plan.park_mbox_pa(core).map_err(Error::Plan)?;
    let was = dev::read64(mb.add(SCHED_MBOX_CTX));
    if was > PARK_PARKED {
        return Err(Error::CoreRunning {
            core: core.get(),
            mbox: was,
        });
    }
    dev::write64(mb.add(SCHED_CURSOR), 0);
    dev::write64(mb.add(SCHED_ROTOR), 0);
    dev::write64(mb.add(SCHED_CURRENT), ctx.0);
    roster::reset(mb, Some(ctx))?;
    dev::push(mb, PARK_MBOX_LEN as usize);
    ctx_write(ctx, CTX_STATE, CtxState::Running.raw());
    // Het doel vóór het startschot: woord 0 = arg maakt de core meteen
    // "running" voor elke lezer, en de lus leest woord 1 pas daarna.
    dev::write64(mb.add(SCHED_MBOX_PC), entry.0);
    dev::write64(mb.add(SCHED_MBOX_CTX), arg);
    dev::mb();
    if was == PARK_COLD {
        return Ok(Start::Cold);
    }
    dev::notify();
    Ok(Start::Woken)
}

// ---------------------------------------------------------------------------
// De rotatie van een app-core: bewoners erbij en eraf (share.go).
// ---------------------------------------------------------------------------

/// Hoe een bewoner bij een app-core kwam ([`join`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Join {
    /// De core draaide: de bewoner staat boot-pending in zijn lijst, en de
    /// rotatie start hem bij de eerstvolgende yield van een buur.
    Joined,
    /// De core staat stil (koud of geparkeerd): er is geen rotatie die hem
    /// oppikt, dus het is een gewoon startschot ([`dispatch`]).
    Idle,
}

/// Zet de bewoner met ctx-blok `ctx` erbij op de DRAAIENDE app-core `core`
/// (`bootPendingDispatch` in share.go): `entry` en `arg` zoals bij
/// [`dispatch`], de staat boot-pending, en hij in de bewonerslijst. De
/// rotatie start hem bij de eerstvolgende yield van een buur, exact het
/// mailbox-pad maar EL2 naar EL2.
///
/// Staat de core stil, dan [`Join::Idle`] en geen enkele schrijf: dan is
/// het een gewoon startschot. De aanroeper moet daarna ook kijken of de
/// core tussen zijn lijstlezing en onze append parkeerde (de park-race uit
/// share.go): dan pikt niemand de bewoner meer op, en is het alsnog een
/// [`dispatch`]. Zo'n core staat in de parkeerlus en leest geen lijst meer,
/// dus dat startschot is het enige.
///
/// De volgorde is die van [`roster::enlist`]: eerst de ctx, dan de lijst,
/// dan de staat. De rotatie leest byte, staat en de byte nog eens; wie de
/// nieuwe staat ziet, ziet ook de lijst die erbij hoort.
pub fn join(plan: &Plan, core: Core, ctx: Pa, entry: Pa, arg: u64) -> Result<Join, Error> {
    if arg <= PARK_PARKED {
        return Err(Error::BadArg { arg });
    }
    let mb = roster::sched(plan, core)?;
    if dev::read64(mb.add(SCHED_MBOX_CTX)) <= PARK_PARKED {
        return Ok(Join::Idle);
    }
    ctx_write(ctx, CTX_BOOT_PC, entry.0);
    ctx_write(ctx, CTX_BOOT_ARG, arg);
    ctx_write(ctx, CTX_WAKE, 0);
    ctx_write(ctx, CTX_KICK_PENDING, 0);
    dev::mb();
    roster::enlist(mb, ctx)?;
    Ok(Join::Joined)
}

/// Haalt de bewoner met ctx-blok `ctx` uit de rotatie van `core`, voor de
/// intrekking: uit de lijst, en is hij niet aan het draaien (geyield of nog
/// nooit gestart), dan Dead. Een geyielde bewoner met een verre wektijd
/// hervat anders pas bij die wektijd, en voelt de intrekking zo lang niet;
/// uit de lijst is hij nooit meer aan de beurt, en dat is een bevestigd
/// einde. Een draaiende bewoner faultt op de ingetrokken tabel en meldt
/// zichzelf dood. Geeft of hij in de lijst stond.
///
/// Een rotatie die hem nét vóór de verwijdering las, hervat hem nog één
/// keer: op de ingetrokken tabel faultt hij bij zijn eerste instructie, en
/// is hij opnieuw dood.
pub fn evict(plan: &Plan, core: Core, ctx: Pa) -> Result<bool, Error> {
    let was = roster::remove(roster::sched(plan, core)?, ctx);
    if matches!(
        ctx_state(ctx),
        Some(CtxState::BootPending | CtxState::Saved)
    ) {
        ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
    }
    Ok(was)
}

/// Maakt het ctx-blok van de secundaire op `core` klaar voor een nieuwe
/// levensduur van de eenheid met VTTBR `vttbr` (`prepareSMPContexts`):
/// gewist, de control-page van de eenheid (het fault-rapport en de
/// wek-keten vinden hem hier), de eenheid zelf (tabel en VMID bij een
/// hervatting), GEEN
/// RX-peek (alleen de primaire leest de ring; een secundaire op elk frame
/// hervatten is alleen maar een pingpong), en het wekdoel van zijn core.
///
/// Het wekdoel staat er meteen, niet pas na de eerste yield: een wek (HVC
/// #4) die komt vóórdat de secundaire ooit yieldde, landde anders nergens,
/// en zijn eerste yield sliep dan tot zijn wektijd. Nu zet de wek de latch,
/// en ziet de eerste yield hem.
pub fn prepare_secondary(
    plan: &Plan,
    core: Core,
    vttbr: u64,
    ctrl: Pa,
    kick_target: u64,
) -> Result<Pa, Error> {
    let ctx = plan.smp_ctx_pa(core).map_err(Error::Plan)?;
    dev::clear(ctx, CTX_LEN as usize);
    ctx_write(ctx, CTX_CTRL_PA, ctrl.0);
    ctx_write(ctx, CTX_VTTBR, vttbr);
    ctx_write(ctx, CTX_RING_HEAD_PA, 0);
    ctx_write(ctx, CTX_KICK_TARGET, kick_target);
    ctx_write(ctx, CTX_STATE, CtxState::Empty.raw());
    Ok(ctx)
}

/// Sluit de vertrouwde wek-keten over de contexten van één eenheid:
/// elk ctx-blok wijst naar het volgende, het laatste terug naar het eerste
/// (`CTX_NEXT_PA`, de keten die HVC #4 afloopt). Eén context is een keten
/// van nul: geen schakel, want een oude schakel van een vorige SMP-
/// levensduur van dit slot wees anders naar contexten die er niet meer bij
/// horen.
pub fn chain(ctxs: &[Pa]) {
    let n = ctxs.len();
    for (i, c) in ctxs.iter().enumerate() {
        let next = match ctxs.get((i + 1) % n.max(1)) {
            Some(x) if n > 1 => x.0,
            _ => 0,
        };
        ctx_write(*c, CTX_NEXT_PA, next);
    }
}

/// Het wekdoel van Apple's fast IPI voor een core met affiniteit `mpidr`:
/// core | cluster << 16 uit aff0 en aff1 (m1n1 smp.c).
#[must_use]
pub const fn apple_ipi_target(mpidr: u64) -> u64 {
    (mpidr & 0xFF) | (((mpidr >> 8) & 0xFF) << 16)
}

/// Wekt een app-core die in de switcher slaapt. Een kick te veel is een
/// geackte FIQ of een lege WFE-ronde op EL2, en verder niets.
///
/// Op Apple de fast IPI naar `mpidr` (de switcher slaapt daar in WFI); op
/// de WFE-smaken een SEV, die elke WFE-slaper wekt: de switcher, de
/// parkeerlus en een idle-governor op EL1. Een board met een GIC-SGI als
/// kick doet dat in zijn eigen `irq`-pad.
pub fn kick(flavor: Flavor, mpidr: u64) {
    match flavor {
        Flavor::AppleVhe => switch::apple_ipi(apple_ipi_target(mpidr)),
        Flavor::Nvhe | Flavor::Vhe => dev::notify(),
    }
}

/// De hard-kill van het slot met kooi-blok `block`: nult zijn
/// stage-2-tabellen en invalideert de
/// TLB's ([`stage2::revoke`]). Elke core van het slot (een SMP-app deelt
/// tabel en VMID) faultt op zijn volgende vertaalde toegang naar de
/// switcher, die hem dood meldt en doorroteert; de SEV aan het eind wekt
/// ook de WFE-slapers (19-07).
pub fn revoke(block: Pa) {
    stage2::revoke(block);
}

/// Publiceert een node-owned SMP-startcontext op `dst` (`PrepareSMP`).
///
/// Alleen de EL1-staat komt van de aanroepende control-page `src`; al het
/// EL2-gezag (tabel, VMID, mailbox, vectoren) komt van de kern. Na de
/// kopie kan de draaiende app de handoff niet meer veranderen. `src` en
/// `dst` mogen samenvallen voor een node-core, wiens page al vertrouwd is.
/// `table` = 0 kiest het node-profiel (geen kooi) in de SMP-trampoline.
pub fn prepare_smp(dst: Pa, src: Pa, table: u64, vmid: u64, mailbox: Pa, vectors: Pa) {
    for off in [
        CTRL_SMP_SP,
        CTRL_SMP_MP,
        CTRL_SMP_G0,
        CTRL_SMP_FN,
        CTRL_SMP_TTBR0,
        CTRL_SMP_STUB,
        CTRL_SMP_MAIR,
        CTRL_SMP_TCR,
        CTRL_SMP_VBAR,
    ] {
        dev::pull(src.add(off), 8);
        dev::write64(dst.add(off), dev::read64(src.add(off)));
    }
    dev::write64(dst.add(CTRL_S2_TABLE), table);
    dev::write64(dst.add(CTRL_VMID), vmid);
    dev::write64(dst.add(CTRL_SMP_MBOX), mailbox.0);
    dev::write64(dst.add(CTRL_VEC_PA), vectors.0);
    dev::push(dst, 256);
    dev::mb();
}

// ---------------------------------------------------------------------------
// De lezers van het ctx-blok.
// ---------------------------------------------------------------------------

/// Leest woord `off` van ctx-blok `ctx`, vers uit het geheugen: dit blok
/// heeft twee schrijvers op twee cores (op ARM device-gemapt en dus
/// gratis; op RISC-V het verschil tussen de staatswissel zien en eeuwig op
/// een oude nul pollen).
#[must_use]
pub fn ctx_read(ctx: Pa, off: u64) -> u64 {
    debug_assert!(off < CTX_LEN && off.is_multiple_of(8));
    dev::pull(ctx.add(off), 8);
    dev::read64(ctx.add(off))
}

/// Schrijft woord `off` van ctx-blok `ctx` en publiceert het.
pub fn ctx_write(ctx: Pa, off: u64, v: u64) {
    debug_assert!(off < CTX_LEN && off.is_multiple_of(8));
    dev::write64(ctx.add(off), v);
    dev::push(ctx.add(off), 8);
}

/// Een woord van het ctx-blok van een bewoner van de OS-core, gelezen door
/// de rotatie (`el2::next`, `due`, `rx_due`). Op riscv64 schrijft alleen het
/// hart van de kern die woorden (de kern en de overgang van de OS-core; de
/// switcher van een app-hart kent alleen zijn eigen bewoners, en niemand
/// anders zet daar een kick), dus heeft zijn cache de laatste waarde en is
/// de `th.dcache.cipa` met `th.sync.is` van [`ctx_read`] loos: per hop een
/// stuk of zeven op de C906 (03-10). Op arm64 kan een switcher met de MMU
/// uit er een kick in zetten: daar [`ctx_read`].
#[must_use]
pub fn os_ctx_read(ctx: Pa, off: u64) -> u64 {
    if cfg!(target_arch = "riscv64") {
        debug_assert!(off < CTX_LEN && off.is_multiple_of(8));
        dev::read64(ctx.add(off))
    } else {
        ctx_read(ctx, off)
    }
}

/// Schrijft een woord van het ctx-blok van een bewoner van de OS-core vanuit
/// de beurt (`begin`, `settle`), om dezelfde reden als [`os_ctx_read`]: op
/// riscv64 zonder `th.dcache.cpa` en `th.sync.is` (een lezer elders in de
/// kern doet zelf een `pull`, en die cleant eerst; de boot-stub doet
/// `th.dcache.ciall`), op arm64 [`ctx_write`].
pub fn os_ctx_write(ctx: Pa, off: u64, v: u64) {
    if cfg!(target_arch = "riscv64") {
        debug_assert!(off < CTX_LEN && off.is_multiple_of(8));
        dev::write64(ctx.add(off), v);
    } else {
        ctx_write(ctx, off, v);
    }
}

/// De staat van ctx-blok `ctx`, of `None` voor een onbekend woord.
#[must_use]
pub fn ctx_state(ctx: Pa) -> Option<CtxState> {
    CtxState::from_raw(ctx_read(ctx, CTX_STATE))
}

/// Ligt er RX voor de bewoner van `ctx`: is zijn doorbell gewapend en groeide
/// de kop van zijn RX-ring voorbij de drempel? Voorbij, niet ongelijk: de
/// kop mag achterlopen op wat de bewoner in zijn cache zag (04-09).
#[must_use]
pub fn rx_due(ctx: Pa) -> bool {
    // De control-page via het ctx-blok: een secundaire heeft geen eigen
    // partitie, wel een ctx-blok met de gedeelde page.
    let cp = os_ctx_read(ctx, CTX_CTRL_PA);
    if cp == 0 {
        return false;
    }
    let door_pa = Pa(cp).add(CTRL_RX_DOOR);
    // De deurbel zet de bewoner zelf; op riscv64 vraagt alleen de rotatie
    // van de OS-core dit, over een bewoner op hetzelfde hart (zie
    // [`os_ctx_read`]).
    if !cfg!(target_arch = "riscv64") {
        dev::pull(door_pa, 8);
    }
    let door = dev::read64(door_pa);
    if door & RX_DOOR_ARMED == 0 {
        return false;
    }
    let head_pa = os_ctx_read(ctx, CTX_RING_HEAD_PA);
    if head_pa == 0 {
        return false;
    }
    // De kop van een RX-ring schrijft alleen de kern zelf (de switch is de
    // enige producer), dus zijn eigen cache heeft de laatste waarde: geen
    // `dc civac` en twee `dsb sy` per blik (03-10, O6N: `el2::next` kijkt
    // zo bij elke beurt).
    dev::read64(Pa(head_pa)) > door & !RX_DOOR_ARMED
}

#[cfg(test)]
mod tests;
