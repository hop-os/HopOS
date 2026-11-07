//! De OS-core als deelbare core (PORT.md beslissing 2, 30-09): de kern is
//! er de eerste bewoner van, en zijn idle is de rotatie over de andere
//! bewoners (Hop, en de groepen die de kern het toestaat).
//!
//! Dit bezit: de overgang van de kern (EL2) naar een bewoner (EL1 met
//! stage-2) en terug, de rotatie over de bewonerslijst van logische core 0
//! (sched-blok 0 van het plan), de EL2-timer (CNTHP) die de deadline van de
//! executor bewaakt terwijl een bewoner draait, en de meetlat van de
//! overgangen. Niet van hier: wie er bewoner wordt (de plaatsing van de
//! kern) en wat de kern doet als hij terug is (de executor).
//!
//! Waarom de kern hier de rotor is en niet een lid van de switcher van de
//! app-cores: de kern draait zelf op EL2, met zijn eigen vectoren, stack en
//! MMU. De switcher in de plan-regio kent alleen EL1-bewoners en slaapt op
//! EL2 in WFE; een kern daarin hervatten zou zijn hele EL2-regime door die
//! blob laten dragen. Hier is het omgekeerd en klein: de kern roept
//! [`OsCore::run`] als zijn executor niets te doen heeft, de bewoner draait
//! tot hij yieldt, exit doet of faultt, of tot een interrupt (de NIC, de
//! kick van een app-core, de CNTHP op de deadline) naar EL2 trapt; dan keert
//! `run` terug en draait de executor zijn ronde. De ctx-blokken zijn die van
//! de switcher, byte voor byte (zelfde bewaarvolgorde, zelfde staten), zodat
//! alles wat de kern van een bewoner leest (`ctx_state`, `rx_due`, het
//! fault-rapport) hier ook klopt.
//!
//! De verloren-wek-race blijft dicht zoals in `cpu::idle`: de executor
//! toetst `ready()` met I en F gemaskeerd vóór [`OsCore::run`], en een
//! interrupt die daarna binnenkomt staat pending bij de GIC. Met HCR_EL2.IMO
//! op 1 is een fysieke IRQ voor EL1 nooit gemaskeerd, dus hij trapt direct
//! na de ERET weer naar EL2.
//!
//! Wat de kern tijdens de beurt van een bewoner NIET hoort, is een SEV: die
//! wekt een WFE, geen draaiende core. Daarom zet de kern tijdens zo'n beurt
//! (en in zijn WFI-slaap, die een SEV evenmin wekt) in sched-blok 0 de
//! ICC_SGI1R-waarde van zijn kick ([`SCHED_OS_KICK`]); de switcher van een
//! app-core die yieldt of HVC #6 doet, stuurt dan die SGI.
//!
//! Op Apple (`AppleVhe`) is er geen GIC: de kick is de fast IPI
//! (IPI_RR_GLOBAL_EL1, [`Bell::apple`]) en komt aan als FIQ, net als de
//! CNTHP. Met HCR_EL2.FMO op 1 trapt die FIQ tijdens een beurt naar de
//! vectoren hieronder (`fiq/lower-a64`, index 10), en de kern ackt de IPI
//! zelf op EL2 voor hij terugkeert (les 04-09: een ongeackte fast IPI blijft
//! staan en de core komt nooit meer tot slapen).
//!
//! FP: een beurt kan asynchroon eindigen (IRQ, kick, CNTHP), en daarna kan
//! een ándere bewoner aan de beurt komen. Daarom bewaart de terugweg q0..q31,
//! FPCR en FPSR in de FP-kier achter het ctx-blok (`CTX_FPRS`) bij elke
//! terugkeer behalve HVC #1, en zet de overgang ze terug als `CTX_FP_LIVE`
//! dat zegt. Bij HVC #1 bewaart de app zelf wat de ABI hem opdraagt (d8..d15,
//! FPCR, FPSR; applib `hvc_yield`, Go `hvcYield`), en een yield kost dan
//! niets extra. Een HVC #4 of #6 bewaart de app niet, en hier is dat een
//! yield naar nu: die telt als onderbreking. Via GP-registers (`fmov`), want
//! het ctx-blok is op sommige borden Device (rk3566) en een SIMD-store
//! daarheen is niet te vertrouwen. Een verse bewoner begint met nullen. De
//! kern zelf is softfloat; CPTR_EL2 trapt FP niet ([`OsCore::new`]).

extern crate alloc;

#[cfg(test)]
use super::dispatch::ctx_state;
use super::dispatch::{
    Flavor, HVC_EXIT, HVC_YIELD, VEC_FIQ_LOWER, VEC_SYNC_LOWER, ctx_read, ctx_write, os_ctx_read,
    os_ctx_write, rx_due,
};
use super::layout::{
    CTX_CTRL_PA, CTX_FP_BITS, CTX_FP_END, CTX_FP_LIVE, CTX_FPRS, CTX_FPRS_ARM_WORDS, CTX_GPRS,
    CTX_KICK_PENDING, CTX_REGIME, CTX_REGIME_ARM_WORDS, CTX_RESUME, CTX_REVOKE, CTX_SP, CTX_STATE,
    CTX_VTTBR, CTX_WAKE, CTX_WAKE_NO_PEEK, Core, CtxState, Plan, SCHED_CLINT_PA, SCHED_CURRENT,
    SCHED_CURSOR, SCHED_MSIP_PA,
};
use super::{Error, roster};
use abi::hopabi::{CTRL_FAULT_ESR, CTRL_FAULT_FAR, CTRL_FAULT_VEC};
use core::sync::atomic::{
    AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use dev::Pa;

/// Het woord in sched-blok 0 (de OS-core) met de ICC_SGI1R_EL1-waarde die
/// de kern wekt; 0 = hij hoort een SEV, niet kicken.
///
/// Op ARM ongebruikt veld van het sched-blok: `SCHED_MSIP_PA` is de
/// RISC-V-kick (de PA van `msip`), en in blok 0 is dat hier dezelfde rol,
/// "het wek-IPI van de kern". Regel 3 van het blok: de kern schrijft, de
/// switcher leest.
pub(crate) const SCHED_OS_KICK: u64 = SCHED_MSIP_PA;

/// Het woord in sched-blok 0 met de PA van GICD_SGIR als de kick een
/// MMIO-schrijf is (GICv2, de GIC-400 van de Pi's); 0 = de kick is een
/// ICC_SGI1R-systeemregister (GICv3). Op ARM ongebruikt veld: op RISC-V is
/// het de PA van `mtimecmp`. Eén keer gezet door [`OsCore::new`].
pub(crate) const SCHED_OS_KICK_PA: u64 = SCHED_CLINT_PA;

/// SCTLR_EL1 bij een koude start: de RES1-bits, M/C/I/A/WXN uit. Nooit
/// erven (de Pi 5-les van 10-07). Eén bron voor de drop van de switcher en
/// de eerste beurt van een bewoner hier.
pub(super) const SCTLR_EL1_CLEAN: u64 = 0x30d0_0800;
/// SPSR voor de eerste ERET: EL1h, DAIF gemaskeerd.
pub(super) const SPSR_EL1H_MASKED: u64 = 0x3c5;

/// HCR_EL2-bits van een bewoner op de OS-core bovenop die van de kern: VM
/// (stage-2 aan), TSC (een SMC is een ontsnappingspoging), en IMO/FMO/AMO
/// (elke fysieke interrupt landt op EL2, bij de kern; de kern zelf draait
/// al zo, `cpu::boot`). TGE moet uit, anders draait er geen EL1.
const HCR_VM: u64 = 1 << 0;
const HCR_FMO: u64 = 1 << 3;
const HCR_IMO: u64 = 1 << 4;
const HCR_AMO: u64 = 1 << 5;
const HCR_TSC: u64 = 1 << 19;
const HCR_TGE: u64 = 1 << 27;
const HCR_GUEST: u64 = HCR_VM | HCR_FMO | HCR_IMO | HCR_AMO | HCR_TSC;

/// De vectorindex van een IRQ uit een lagere EL: met [`VEC_FIQ_LOWER`] de
/// ingangen die niet [`VEC_SYNC_LOWER`] zijn en toch geen fault.
const VEC_IRQ_LOWER: u64 = 9;

/// Bit 63 van het kick-woord op Apple: "scherp". Het doel van de fast IPI
/// (core | cluster << 16) is voor E-core 0 in cluster 0 gewoon 0, en 0 is
/// in [`SCHED_OS_KICK`] "niet kicken" (dezelfde val als de brievenbus van
/// Go 31-08, waar E-core 0 nooit werk kon aannemen). De switcher wist het
/// bit voor hij IPI_RR_GLOBAL_EL1 schrijft (bits 29:28 zijn het type, 0 =
/// meteen; bit 63 is er niet).
pub(super) const APPLE_KICK_ARMED: u64 = 1 << 63;

/// Het kick-woord in [`SCHED_OS_KICK`] voor de fast IPI naar de core met
/// affiniteit `mpidr`: het IPI_RR-doel plus [`APPLE_KICK_ARMED`].
#[must_use]
pub(super) const fn apple_kick_word(mpidr: u64) -> u64 {
    APPLE_KICK_ARMED | super::dispatch::apple_ipi_target(mpidr)
}

/// Wat de switcher van een kick-woord in IPI_RR_GLOBAL_EL1 schrijft: zijn
/// masker is `apple_kick_target(u64::MAX)`, als `const` in de assembly
/// (switch.rs), zodat de test hier en de switcher één regel delen.
#[must_use]
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code) // op de host is er geen switcher, alleen de test
)]
pub(super) const fn apple_kick_target(word: u64) -> u64 {
    word & !APPLE_KICK_ARMED
}

/// Waar een beurt eindigde, naar de vectorindex waarmee de bewoner
/// terugkwam.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Exit {
    /// Synchroon (index 8): een HVC, of een fault die de ESR beschrijft.
    Sync,
    /// Een IRQ of FIQ uit EL1 (index 9, 10): de kern wil zijn core terug.
    Interrupt,
    /// SError of AArch32 (11..15): altijd een fault.
    Fault,
}

/// De afbeelding van vectorindex naar [`Exit`]. Op elke smaak dezelfde:
/// de GIC levert alles als IRQ, Apple de timer en de kick als FIQ en de AIC
/// als IRQ.
const fn exit_of(vec: u64) -> Exit {
    match vec {
        VEC_SYNC_LOWER => Exit::Sync,
        VEC_IRQ_LOWER | VEC_FIQ_LOWER => Exit::Interrupt,
        _ => Exit::Fault,
    }
}

/// Waardoor een onderbroken beurt terugkwam: de CNTHP als hij afging
/// (ISTATUS), anders de kick als die wacht, anders een device.
const fn interrupted(fired: bool, kick: bool) -> Back {
    if fired {
        Back::Timer
    } else if kick {
        Back::Ipi
    } else {
        Back::Irq
    }
}

/// De vangrail van de zelftest-spinner bovenop twee keer zijn termijn: 50
/// ms.
///
/// Les van 30-09 (de soak van de hoplb-kring, `tools/qemu-soak-hop.sh`,
/// vier QEMU's naast elkaar, load 20 op 14 host-cores): met alleen twee
/// keer de termijn (2 ms bij de timer-toets van 1 ms) gaf de spinner op
/// 2042 us zelf op, vóór de CNTHP van een QEMU-vCPU die even geen host-tijd
/// kreeg, en de boot werd rood (HOPOS_OS_SELFTEST_FAIL, 1 van 25 runs met
/// OSCORE=1). Laat is geen fout: een timer die nooit komt wel. 50 ms is
/// ruim boven elke gemeten vertraging en houdt een board zonder routering
/// nog steeds uit een hangende boot.
const SELFTEST_GRACE_NS: u64 = 50_000_000;

/// De tellerstand waarop de zelftest-spinner zelf opgeeft: `now` plus twee
/// keer `ticks` plus [`SELFTEST_GRACE_NS`] op een teller van `hz`.
fn spin_limit(now: u64, ticks: u64, hz: u64) -> u64 {
    let grace = crate::idle::ns_to_ticks(SELFTEST_GRACE_NS, hz);
    now.wrapping_add(ticks.saturating_mul(2).saturating_add(grace))
}

/// Het ctx-blok met zijn FP-kier in woorden, voor de scratch van de
/// zelftest.
const CTX_WORDS: usize = (CTX_FP_END / 8) as usize;

/// De langste beurt van een bewoner van de OS-core: 10 ms. "Geen deadline"
/// als oneindig lezen is geen zuinigheid maar een hang.
pub const TURN_CAP_NS: u64 = 10_000_000;

/// Waardoor de kern zijn core terugkreeg.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Back {
    /// De bewoner yieldde (HVC #1, of een andere HVC die geen exit is).
    Yield,
    /// De bewoner deed exit (HVC #0) en is dood.
    Exit,
    /// Een interrupt van een device (de NIC, de UART, de CNTP van de kern).
    Irq,
    /// De kick van een app-core: de SGI van [`Bell`].
    Ipi,
    /// De CNTHP op de deadline van de executor.
    Timer,
    /// Een fault, een getrapte SMC, of een intrekking: de bewoner is dood en
    /// het rapport staat op zijn control-page.
    Fault,
}

impl Back {
    /// Het getal van deze uitkomst in [`Stats::last`] (1..=6; 0 = nog geen
    /// beurt).
    #[must_use]
    pub const fn code(self) -> u64 {
        match self {
            Back::Yield => 1,
            Back::Exit => 2,
            Back::Irq => 3,
            Back::Ipi => 4,
            Back::Timer => 5,
            Back::Fault => 6,
        }
    }

    /// De naam bij [`Back::code`], voor de tik-regel.
    #[must_use]
    pub const fn name_of(code: u64) -> &'static str {
        match code {
            1 => "yield",
            2 => "exit",
            3 => "irq",
            4 => "ipi",
            5 => "timer",
            6 => "fault",
            _ => "-",
        }
    }
}

/// Wat één proef van [`OsCore::selftest`] zag: waardoor de kern terugkwam,
/// na hoeveel ticks, met welke vectorindex, en wat de controller op dat
/// moment als hoogste pending liet zien, vóór de overgang en erna (de peek
/// van [`Bell::pending`], zonder claim).
///
/// Waarom zo veel: op de eerste Pi 5-boot (30-09) gaf de zelftest drie keer
/// `Irq` na 0 us, en de regel zei niet wélke lijn. Een lijn die al vóór de
/// overgang pending stond (een NIC-interrupt uit de boot die nog niemand
/// claimde) is een andere fout dan een lijn die tijdens de beurt komt. Met
/// de INTID erbij is dezelfde klasse fout op de O6N en de Altra in één
/// regel te lezen.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Probe {
    /// Waardoor de kern terugkwam.
    pub back: Back,
    /// Na hoeveel ticks van de teller.
    pub ticks: u64,
    /// De vectorindex van de terugkeer (8 synchroon, 9 IRQ, 10 FIQ).
    pub vec: u64,
    /// De hoogste pending INTID vlak vóór de overgang (1023 = niets; zonder
    /// bel [`Probe::NONE`]).
    pub before: u32,
    /// De hoogste pending INTID direct na de terugkeer.
    pub after: u32,
}

impl Probe {
    /// "Geen peek": een OS-core zonder bel.
    pub const NONE: u32 = u32::MAX;

    /// Stond er vóór de overgang al een lijn pending die niet de kick
    /// `kick` is, dan die INTID. De GIC zegt 1023 (en hoger) voor niets.
    #[must_use]
    pub const fn stale(&self, kick: u32) -> Option<u32> {
        if self.before >= 1020 || self.before == kick {
            None
        } else {
            Some(self.before)
        }
    }
}

/// Hoe vaak één proef van de zelftest het opnieuw doet als een device-lijn
/// hem onderbrak.
const SELFTEST_TRIES: u32 = 3;

/// Eén proef van de zelftest van de OS-core, op beide architecturen: eerst
/// afhandelen wat al bij de interrupt-controller wacht (`drain`, de ronde
/// van de dispatch-taak), dan de proef (`once`), en opnieuw zolang een
/// device-lijn hem onderbrak (`back` zegt [`Back::Irq`]), hoogstens
/// `SELFTEST_TRIES` keer. Geeft de laatste uitkomst en het aantal
/// pogingen.
///
/// Waarom (30-09, de eerste Pi 5-boot: drie keer `Irq` na 0 us; 04-10 op
/// de LicheeRV elke boot alle vier de proeven `Irq` na 5 us): de zelftest
/// draait in de boot, vóór de executor. Een lijn die al eerder scherp stond
/// (de NIC, sinds `probe_nic`) en daarna één keer vuurde, liet de ingang
/// de vlag zetten en gemaskeerd terugkeren, maar de dispatch-taak draait
/// pas als de executor loopt. De lijn stond dus nog pending bij de
/// controller (de GIC, de PLIC), en elke proef kwam meteen terug op een
/// interrupt die niets met de overgang te maken had. Een lijn die tijdens
/// de proef komt (een frame op het LAN) is net zo min een oordeel. Komt hij
/// drie keer, dan zegt de regel welke lijn het was.
pub fn selftest_tries<T>(
    mut drain: impl FnMut(),
    mut once: impl FnMut() -> Option<T>,
    back: impl Fn(&T) -> Back,
) -> (Option<T>, u32) {
    let mut last = (None, 0);
    for n in 1..=SELFTEST_TRIES {
        drain();
        let seen = once();
        let again = seen.as_ref().is_some_and(|s| back(s) == Back::Irq);
        last = (seen, n);
        if !again {
            break;
        }
    }
    last
}

/// Wat één beurt van de rotatie deed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Turn {
    /// Een bewoner draaide; zo kwam de core terug.
    Ran(Back),
    /// Niemand was aan de beurt. `wake` is de vroegste wektijd (CNTPCT) van
    /// een geyielde bewoner, zodat de kern niet langer slaapt dan nodig.
    Idle {
        /// De vroegste wektijd, of `None` als er niemand wacht.
        wake: Option<u64>,
    },
}

/// De kick van de OS-core: wat de switcher van een app-core schrijft om hem
/// te wekken, en hoe de kern na een IRQ ziet of het de kick was. Het board
/// kiest de weg: GICv3 een systeemregister, GICv2 een MMIO-schrijf, Apple
/// de fast IPI ([`Bell::apple`]).
#[derive(Copy, Clone, Debug)]
pub struct Bell {
    /// De SGI naar de OS-core, als woord: met `sgir == 0` de
    /// ICC_SGI1R_EL1-waarde (GICv3, `driver_gicv3::sgi1r`), anders de
    /// 32-bit GICD_SGIR-waarde (GICv2: CPUTargetList in bits 23:16, de
    /// INTID in 3:0). Op Apple het kick-woord van [`apple_kick_word`]. 0 =
    /// geen kick.
    pub sgi1r: u64,
    /// De PA van GICD_SGIR (distributor + 0xF00) op een GICv2, 0 op een
    /// GICv3. De switcher draait met de MMU uit, dus dit is een fysiek adres.
    pub sgir: u64,
    /// De INTID van de kick.
    pub intid: u32,
    /// De hoogste pending INTID (ICC_HPPIR1_EL1), zonder hem te claimen.
    pub pending: fn() -> u32,
}

impl Bell {
    /// De "INTID" van de fast IPI: Apple heeft geen GIC, dus een waarde die
    /// geen GIC-INTID kan zijn. [`Bell::pending`] geeft hem als IPI_SR_EL1
    /// bit 0 staat.
    pub const APPLE_INTID: u32 = u32::MAX;

    /// De kick van Apple silicium naar de OS-core met affiniteit `mpidr`:
    /// de switcher schrijft het doel in IPI_RR_GLOBAL_EL1 (m1n1 `smp.c`,
    /// GEMETEN 02-09 als wek van de switcher), en de kern leest IPI_SR_EL1
    /// bit 0 zonder te acken; de ack doet hij zelf, op EL2, bij de terugkeer
    /// van de beurt (les 04-09).
    #[must_use]
    pub const fn apple(mpidr: u64) -> Bell {
        Bell {
            sgi1r: apple_kick_word(mpidr),
            sgir: 0,
            intid: Self::APPLE_INTID,
            pending: arch::apple_ipi_pending,
        }
    }
}

/// Ackt een wachtende fast IPI op deze Apple-core (IPI_SR_EL1); `true` = er
/// stond er een. Voor de dispatch van een kern zonder switcher eronder: een
/// IPI die niemand ackt, blijft staan en de core slaapt nooit meer (04-09).
pub fn apple_ipi_ack() -> bool {
    let up = arch::apple_ipi_pending() != 0;
    if up {
        arch::apple_ipi_ack();
    }
    up
}

/// De meetlat van de OS-core: overgangen naar een bewoner en waardoor de
/// kern terugkwam. Zonder deze getallen is "Hop krijgt tijd" niet te
/// onderscheiden van "de kern spint".
#[derive(Debug)]
pub struct Stats {
    /// Overgangen naar een bewoner.
    pub entries: AtomicU64,
    /// Terug op een device-interrupt.
    pub irq: AtomicU64,
    /// Terug op de kick van een app-core.
    pub ipi: AtomicU64,
    /// Terug op de CNTHP (de deadline van de executor).
    pub timer: AtomicU64,
    /// Terug op een yield.
    pub yields: AtomicU64,
    /// Terug op een exit.
    pub exits: AtomicU64,
    /// Terug op een fault.
    pub faults: AtomicU64,
    /// De tijd die bewoners kregen, in ticks.
    pub ticks: AtomicU64,
    /// Idle-rondes waarin niemand aan de beurt was.
    pub idle: AtomicU64,
    /// Kicks die de interrupt-dispatch van de kern claimde (ook die de kern
    /// uit zijn WFI haalden, niet alleen uit een beurt). Het board telt.
    pub kicks: AtomicU64,
    /// De laatste beurt: de PA van het ctx-blok van de bewoner, met
    /// [`Back::code`] in bits 7:0 (een ctx-blok ligt op 1 KB). Staat de
    /// console stil, dan zegt dit woord (via de monitor gelezen) wie de core
    /// het laatst had en hoe hij hem teruggaf.
    pub last: AtomicU64,
    /// De langste beurt sinds de vorige lezer hem nulde, in ticks.
    pub longest: AtomicU64,
}

/// De meetlat van deze node (één OS-core).
pub static STATS: Stats = Stats {
    entries: AtomicU64::new(0),
    irq: AtomicU64::new(0),
    ipi: AtomicU64::new(0),
    timer: AtomicU64::new(0),
    yields: AtomicU64::new(0),
    exits: AtomicU64::new(0),
    faults: AtomicU64::new(0),
    ticks: AtomicU64::new(0),
    idle: AtomicU64::new(0),
    kicks: AtomicU64::new(0),
    last: AtomicU64::new(0),
    longest: AtomicU64::new(0),
};

/// De device-ack van de kick-SGI bij de dispatcher (`cpu::irq`): er is geen
/// device om los te laten (de core is al terug bij de kern), alleen tellen
/// in [`Stats::kicks`].
pub fn count_kick() {
    STATS.kicks.fetch_add(1, Relaxed);
}

/// De rotatie van de OS-core. Eén, eigendom van de slaap van de executor
/// op die core (`cpu::idle::ArmSleeper::host`).
///
/// De bewonerslijst zelf staat in sched-blok 0 van het plan: de plaatsing
/// schrijft hem ([`host`], `roster::forget`), de rotatie leest hem. Dat is
/// dezelfde tabel als die van een gedeelde app-core, en beide schrijvers
/// draaien op deze ene core tussen twee rondes van de executor.
pub struct OsCore {
    sched: Pa,
    flavor: Flavor,
    hcr: u64,
    bell: Option<Bell>,
}

/// Het laatste fault-rapport van een beurt: ESR, FAR, de PC van de
/// bewoner en de vector (1 synchroon, 2 IRQ, 3 FIQ), voor een proef die
/// geen control-page heeft (de voorproef op Apple, 30-09).
static LAST_FAULT: [core::sync::atomic::AtomicU64; 4] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Het laatste fault-rapport: `(esr, far, pc, vec)`; alles nul als er
/// nog geen beurt op een fault eindigde.
#[must_use]
pub fn last_fault() -> (u64, u64, u64, u64) {
    (
        LAST_FAULT[0].load(Relaxed),
        LAST_FAULT[1].load(Relaxed),
        LAST_FAULT[2].load(Relaxed),
        LAST_FAULT[3].load(Relaxed),
    )
}

impl OsCore {
    /// De rotatie over het plan `plan`, met de EL2-code van `flavor` en de
    /// kick `bell` (of geen, dan hoort de kern een app-core alleen op zijn
    /// eigen deadline). Zet eenmalig het EL2-regime dat een bewoner nodig
    /// heeft en de kern niet raakt: VTCR, CPTR zonder FP-trap, de
    /// timertoegang van EL1 (CNTHCTL) en CNTVOFF 0.
    ///
    /// Aanroepen op de OS-core zelf. Op Apple (`AppleVhe`) hoort de kick
    /// [`Bell::apple`] te zijn: een GIC-bel zou de switcher daar nooit
    /// sturen (hij kent alleen de fast IPI).
    pub fn new(plan: &Plan, flavor: Flavor, bell: Option<Bell>) -> Result<OsCore, Error> {
        let sched = os_sched(plan)?;
        arch::prepare(flavor);
        dev::write64(sched.add(SCHED_OS_KICK), 0);
        dev::write64(sched.add(SCHED_OS_KICK_PA), bell.map_or(0, |b| b.sgir));
        Ok(OsCore {
            sched,
            flavor,
            hcr: (arch::hcr() & !HCR_TGE) | HCR_GUEST,
            bell,
        })
    }

    /// Zet de kick scherp (`true`) of uit: scherp zolang de kern een SEV niet
    /// hoort (in een beurt van een bewoner, en in een WFI-slaap).
    pub fn listen(&self, on: bool) {
        let v = match (on, self.bell) {
            (true, Some(b)) => b.sgi1r,
            _ => 0,
        };
        dev::write64(self.sched.add(SCHED_OS_KICK), v);
        dev::mb();
    }

    /// Eén beurt: de bewoner die [`next`] aanwijst draait tot hij de core
    /// teruggeeft of tot `deadline` (CNTPCT). Aanroepen met I en F
    /// gemaskeerd, ná de laatste `ready()`-toets van de executor.
    pub fn run(&mut self, deadline: u64) -> Turn {
        let sched = self.sched;
        round(sched, arch::counter(), |i, ctx, fresh| {
            self.turn(i, ctx, deadline, fresh)
        })
    }

    /// De beurt van de bewoner met ctx-blok `ctx` op lijstplek `i`.
    fn turn(&mut self, i: usize, ctx: Pa, deadline: u64, fresh: bool) -> Back {
        begin(self.sched, i, ctx);
        // VTTBR naar de EENHEID van de bewoner (zoals de switcher): tabel en
        // VMID die de kern bij de bouw in het ctx-blok zette. Geen TLBI bij
        // een hervatting: de entries zijn VMID-getagd. Bij een verse bewoner
        // wel, plus de I-cache: de VMID en de PA's kunnen van een vorige
        // huurder zijn (Altra 15-07).
        arch::set_vttbr(ctx_read(ctx, CTX_VTTBR), fresh);
        arch::timer_arm(deadline);
        self.listen(true);
        let fp = fp_turn(ctx, fresh);
        let t0 = arch::counter();
        let vec = arch::enter(self.flavor, ctx, self.hcr, fp);
        home(self.sched, arch::counter().wrapping_sub(t0));
        self.listen(false);
        let fired = arch::timer_disarm();
        let back = if exit_of(vec) == Exit::Sync && arch::esr() >> 26 == EC_FP {
            // Een FP-instructie met de trap aan: meteen weer aan de beurt op
            // dezelfde instructie, nu met FP (de FP-staat in zijn ctx-blok
            // zet de volgende overgang terug), zie [`FP_USER`].
            if fp_trapped(ctx) {
                crate::println!(
                    "oscore: resident {:#x} uses FP, its FP state goes along from now on HOPOS_OS_FP",
                    ctx.0
                );
            }
            ctx_write(ctx, CTX_WAKE, 0);
            ctx_write(ctx, CTX_STATE, CtxState::Saved.raw());
            Back::Yield
        } else {
            self.settle(ctx, vec, fired)
        };
        end(ctx, back)
    }

    /// Zet de staat van een bewoner na zijn beurt, en zegt waardoor de kern
    /// terug is.
    fn settle(&self, ctx: Pa, vec: u64, fired: bool) -> Back {
        let back = match exit_of(vec) {
            Exit::Sync => {
                let esr = arch::esr();
                if esr >> 26 != crate::vectors::EC_HVC64 {
                    return self.fault(ctx, vec, esr);
                }
                let imm = esr & 0xffff;
                if imm == HVC_EXIT {
                    ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
                    STATS.exits.fetch_add(1, Relaxed);
                    return Back::Exit;
                }
                // x1 van de bewoner is zijn wektijd (alleen bij HVC #1; een
                // andere HVC, zoals een sibling-wek zonder siblings, is een
                // yield naar nu). Een kick die tijdens de beurt kwam, wint.
                let mut wake = if imm == HVC_YIELD {
                    ctx_read(ctx, CTX_GPRS + 8)
                } else {
                    0
                };
                if ctx_read(ctx, CTX_KICK_PENDING) != 0 {
                    ctx_write(ctx, CTX_KICK_PENDING, 0);
                    wake = 0;
                }
                ctx_write(ctx, CTX_WAKE, wake);
                STATS.yields.fetch_add(1, Relaxed);
                Back::Yield
            }
            Exit::Interrupt => {
                // Onderbroken midden in zijn werk: meteen weer aan de beurt.
                ctx_write(ctx, CTX_WAKE, 0);
                let kick = self.bell.is_some_and(|b| (b.pending)() == b.intid);
                let back = interrupted(fired, kick);
                match back {
                    Back::Timer => STATS.timer.fetch_add(1, Relaxed),
                    Back::Ipi => STATS.ipi.fetch_add(1, Relaxed),
                    _ => STATS.irq.fetch_add(1, Relaxed),
                };
                // Apple: de fast IPI hier acken, op EL2, vóór de kern zijn
                // maskers opent. Een GIC-SGI blijft pending tot de dispatch
                // van de kern hem claimt; een fast IPI die niemand ackt,
                // blijft staan en de core komt nooit meer tot slapen (04-09).
                // Een FIQ die geen van beide is (een timer van de bewoner
                // zelf, een PMC) telt als device: de kern kijkt in zijn
                // eigen dispatch.
                if back == Back::Ipi && self.flavor == Flavor::AppleVhe {
                    arch::apple_ipi_ack();
                }
                back
            }
            Exit::Fault => return self.fault(ctx, vec, arch::esr()),
        };
        ctx_write(ctx, CTX_STATE, CtxState::Saved.raw());
        back
    }

    /// Het fault-rapport op de control-page (vec+1, ESR, FAR, zoals de
    /// switcher), en de bewoner dood. De fault-PC staat al in zijn
    /// resume-woord.
    fn fault(&self, ctx: Pa, vec: u64, esr: u64) -> Back {
        // Ook zonder control-page (de zelftest, de voorproef van een board)
        // blijft het rapport leesbaar: [`last_fault`].
        LAST_FAULT[0].store(esr, Relaxed);
        LAST_FAULT[1].store(arch::far(), Relaxed);
        LAST_FAULT[2].store(ctx_read(ctx, CTX_RESUME), Relaxed);
        LAST_FAULT[3].store(vec + 1, Relaxed);
        let cp = ctx_read(ctx, CTX_CTRL_PA);
        if cp != 0 {
            for (off, v) in [
                (CTRL_FAULT_VEC, vec + 1),
                (CTRL_FAULT_ESR, esr),
                (CTRL_FAULT_FAR, arch::far()),
            ] {
                dev::write64(Pa(cp).add(off), v);
                dev::push(Pa(cp).add(off), 8);
            }
        }
        ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
        STATS.faults.fetch_add(1, Relaxed);
        Back::Fault
    }

    /// De zelftest bij boot: een bewoner zonder kooi (VM=0, MMU uit, een
    /// stub in het kern-image) die spint (`yield_ = false`) of meteen HVC #1
    /// doet, met `kick` vlak vóór de overgang (bijvoorbeeld een SGI naar
    /// deze core, om het IPI-pad te bewijzen), en de CNTHP op `ticks` na
    /// nu. Geeft wat de proef zag ([`Probe`]); `None` als er geen heap was
    /// voor het scratch-ctx-blok.
    pub fn selftest(&mut self, yield_: bool, ticks: u64, kick: &dyn Fn()) -> Option<Probe> {
        let mut block: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        block.try_reserve_exact(CTX_WORDS).ok()?;
        block.resize(CTX_WORDS, 0);
        let ctx = Pa(block.as_mut_ptr().addr() as u64);
        // De spinner geeft na twee keer de termijn plus een vangrail zelf op
        // (een yield): een board dat de CNTHP of de kick niet naar deze core
        // routeert, geeft zo een rode zelftest in plaats van een hangende
        // boot. Zie [`spin_limit`] voor de vangrail.
        let limit = spin_limit(arch::counter(), ticks, arch::freq());
        prepare(ctx, arch::stub(yield_), limit);
        ctx_write(ctx, CTX_STATE, CtxState::Running.raw());
        let hcr = self.hcr & !HCR_VM;
        let daif = arch::mask();
        let peek = || self.bell.map_or(Probe::NONE, |b| (b.pending)());
        let before = peek();
        arch::timer_arm(arch::counter().wrapping_add(ticks));
        kick();
        let t0 = arch::counter();
        let vec = arch::enter(self.flavor, ctx, hcr, true);
        let dt = arch::counter().wrapping_sub(t0);
        let after = peek();
        let fired = arch::timer_disarm();
        let back = self.settle(ctx, vec, fired);
        arch::restore(daif);
        // De zelftest telt niet mee in de meetlat van de bewoners.
        let counter = match back {
            Back::Yield => &STATS.yields,
            Back::Exit => &STATS.exits,
            Back::Irq => &STATS.irq,
            Back::Ipi => &STATS.ipi,
            Back::Timer => &STATS.timer,
            Back::Fault => &STATS.faults,
        };
        counter.fetch_sub(1, Relaxed);
        drop(block);
        Some(Probe {
            back,
            ticks: dt,
            vec,
            before,
            after,
        })
    }
}

// ---------------------------------------------------------------------------
// De verhuizing: de oude boot-core wordt een app-core.
// ---------------------------------------------------------------------------

/// De MPIDR van de core die in [`hold`] wacht, met bit 63 gezet (0 = geen:
/// MPIDR 0 is een geldige core).
static HELD: AtomicU64 = AtomicU64::new(0);
/// De parkeerlus van het plan, 0 zolang de kooi-regio er nog niet is.
static HOLD_PARK: AtomicU64 = AtomicU64::new(0);
/// De park-mailbox van de wachtende core: TPIDR_EL2 van de parkeerlus.
static HOLD_MBOX: AtomicU64 = AtomicU64::new(0);

/// Hoe lang [`release_held`] op de parkeerlus wacht: 100 ms. Op QEMU is het
/// een paar microseconden; een core die het in 100 ms niet haalt, is een
/// fout, geen trage core.
const HOLD_GRACE_NS: u64 = 100_000_000;

/// Geeft deze core op nadat de kern naar de OS-core verhuisde (Go:
/// `parkenter`): hij wacht met de maskers dicht tot de kooi-regio er is
/// ([`release_held`]), zet dan zijn EL2-MMU en caches uit, zoals elke
/// app-core na PSCI CPU_ON draait, en springt de parkeerlus van het plan
/// in. Vanaf daar is hij een geparkeerde app-core: een dispatch is mailbox
/// plus SEV, geen CPU_ON (dat gaf op een levende core ALREADY_ON). Geen
/// CPU_OFF: dat was op de Pi 5-stockfirmware een deur zonder terugweg
/// (10-07).
pub fn hold() -> ! {
    HELD.store((crate::mpidr() & 0xff_00ff_ffff) | (1 << 63), Release);
    dev::notify();
    loop {
        let (mbox, park) = (HOLD_MBOX.load(Acquire), HOLD_PARK.load(Acquire));
        if mbox != 0 && park != 0 {
            arch::park(mbox, park);
        }
        arch::wfe();
    }
}

/// De MPIDR van de core die in [`hold`] wacht, als er een is.
#[must_use]
pub fn held() -> Option<u64> {
    let v = HELD.load(Acquire);
    (v != 0).then_some(v & !(1 << 63))
}

/// Haalt de wachtende boot-core de parkeerlus van `plan` in, als logische
/// app-core `core`: zijn park-mailbox, dan het startschot voor [`hold`], en
/// wachten tot de mailbox "geparkeerd" zegt. Aanroepen na
/// [`super::init_app_cores`] en vóór de eerste dispatch.
pub fn release_held(plan: &Plan, core: Core) -> Result<(), Error> {
    let mbox = plan.park_mbox_pa(core).map_err(Error::Plan)?;
    HOLD_MBOX.store(mbox.0, Release);
    HOLD_PARK.store(plan.park_code_pa().0, Release);
    dev::notify();
    let grace = crate::idle::ns_to_ticks(HOLD_GRACE_NS, arch::freq());
    let t0 = arch::counter();
    loop {
        let w = dev::read64(mbox.add(super::layout::SCHED_MBOX_CTX));
        if w == super::layout::PARK_PARKED {
            return Ok(());
        }
        if arch::counter().wrapping_sub(t0) > grace {
            return Err(Error::UnparkedCore {
                core: core.get(),
                mbox: w,
            });
        }
        core::hint::spin_loop();
    }
}

/// De EC van een getrapte FP/SIMD-instructie (CPTR_EL2.TFP, of FPEN = 0
/// onder E2H = 1).
const EC_FP: u64 = 0x07;

/// Lazy FP zoals KVM (`kvm_hyp_handle_fpsimd`), per bewoner: drie bits in
/// zijn ctx-blok ([`CTX_FP_BITS`]): [`FP_USER`], [`FP_SEEN`] en [`FP_ONCE`].
///
/// Een bewoner die geen FP-bewoner is, draait met de FP-trap van CPTR_EL2
/// aan, en dan gaat er bij zijn beurt geen FP-woord heen of terug. Zonder
/// dit kostte elke kick (HVC #6) en elke interrupt 66 stores en de beurt
/// erna 66 loads in het ctx-blok, op de O6N Device-geheugen (03-10). Een
/// Rust-app is softfloat; zijn enige FP-instructies zijn de `msr fpcr` en
/// `msr fpsr` van `_start` (applib). Daarom maakt de eerste trap hem nog
/// geen FP-bewoner: hij krijgt die ene beurt FP (zijn staat uit het ctx-blok
/// heen, en terug), en pas een tweede trap maakt hem FP-bewoner voor de
/// rest van dit kern-leven (een Go-app, een hardfloat-app). Een koude start
/// wist alle drie.
///
/// Zijn FP-staat gaat elke beurt mee (eager, zoals tot 03-10).
const FP_USER: u64 = 1;
/// Hij trapte al een keer op FP.
const FP_SEEN: u64 = 2;
/// Zijn volgende beurt met FP aan (na zijn eerste trap).
const FP_ONCE: u64 = 4;

/// Gaat de FP-staat van de bewoner van `ctx` deze beurt mee? Een koude start
/// (`fresh`) begint opnieuw.
fn fp_turn(ctx: Pa, fresh: bool) -> bool {
    let bits = if fresh { 0 } else { ctx_read(ctx, CTX_FP_BITS) };
    ctx_write(ctx, CTX_FP_BITS, bits & !FP_ONCE);
    bits & (FP_ONCE | FP_USER) != 0
}

/// De bewoner van `ctx` trapte op FP: deze keer één beurt FP, de tweede keer
/// voor altijd. `true` = vanaf nu FP-bewoner.
fn fp_trapped(ctx: Pa) -> bool {
    let bits = ctx_read(ctx, CTX_FP_BITS);
    if bits & FP_SEEN != 0 {
        ctx_write(ctx, CTX_FP_BITS, bits | FP_USER);
        return true;
    }
    ctx_write(ctx, CTX_FP_BITS, bits | FP_SEEN | FP_ONCE);
    false
}

/// Wat [`next`] besluit.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Next {
    /// De bewoner met ctx-blok `ctx` op lijstplek `i`; `fresh` = zijn
    /// eerste beurt (boot-pending).
    Turn {
        /// De lijstplek: de nieuwe cursor.
        i: usize,
        /// Zijn ctx-blok.
        ctx: Pa,
        /// Koud starten.
        fresh: bool,
    },
    /// Niemand heeft werk; de vroegste wektijd, als er een wacht.
    Idle {
        /// De vroegste wektijd (tellerstand).
        wake: Option<u64>,
    },
}

/// Wie nu op de OS-core: het besluitpunt van de rotatie op beide
/// architecturen (`OsCore::run` hier en in `cpu::riscv::oscore`), met de
/// regel van de switcher van een gedeelde app-core. Round-robin vanaf de
/// plek ná de cursor; aan de beurt is de eerste die boot-pending is, of
/// geyield en [`due`] (zijn wektijd, een kick, of RX achter zijn deurbel).
/// Niemand: de vroegste wektijd, en dan pas mag de kern slapen.
///
/// Hier, en alleen hier, gaat een ingetrokken bewoner dood: wie aan de
/// beurt zou zijn en `CTX_REVOKE` draagt, wordt Dead zonder nog één
/// instructie, en de ronde gaat door. Zoals KVM: de intrekking is een
/// aanvraag plus een kick (`kvm_make_all_cpus_request` met
/// `KVM_REQ_VM_DEAD`), en de vCPU leest zijn aanvragen op één plek, vlak
/// vóór de ingang (`vcpu_enter_guest`), niet bij zijn exit. De kick is
/// [`recall`]: een slaper met een verre wektijd is dan meteen aan de beurt
/// en komt zo hier langs. Zo leest de rotatie `CTX_REVOKE` één keer per
/// beurt, niet voor elke slaper in elke ronde.
///
/// Waar de kern afwijkt, staat niet hier maar bij de aanroeper: hij is
/// zelf geen bewoner en gaat altijd voor (de rotatie draait alleen als zijn
/// executor niets klaar heeft, en hij neemt de core terug op zijn deadline,
/// een interrupt of de kick).
#[must_use]
pub fn next(sched: Pa, now: u64) -> Next {
    let count = roster::len(sched);
    let cursor = usize::try_from(dev::read64(sched.add(SCHED_CURSOR))).unwrap_or(0);
    let mut earliest: Option<u64> = None;
    for k in 1..=count {
        let i = (cursor + k) % count;
        let ctx = match roster::get(sched, i) {
            0 => continue,
            pa => Pa(pa),
        };
        let fresh = match CtxState::from_raw(os_ctx_read(ctx, CTX_STATE)) {
            Some(CtxState::BootPending) => true,
            Some(CtxState::Saved) => match due(ctx, now) {
                None => false,
                Some(t) => {
                    earliest = Some(earliest.map_or(t, |e| e.min(t)));
                    continue;
                }
            },
            _ => continue,
        };
        if os_ctx_read(ctx, CTX_REVOKE) != 0 {
            ctx_write(ctx, CTX_STATE, CtxState::Dead.raw());
            continue;
        }
        return Next::Turn { i, ctx, fresh };
    }
    Next::Idle { wake: earliest }
}

/// Eén ronde van de rotatie, op elke architectuur: [`next`] kiest, `turn`
/// draait de gekozen bewoner (lijstplek, ctx-blok, koud) en zegt hoe de
/// kern terugkwam.
pub(crate) fn round(sched: Pa, now: u64, turn: impl FnOnce(usize, Pa, bool) -> Back) -> Turn {
    match next(sched, now) {
        Next::Turn { i, ctx, fresh } => Turn::Ran(turn(i, ctx, fresh)),
        Next::Idle { wake } => {
            STATS.idle.fetch_add(1, Relaxed);
            Turn::Idle { wake }
        }
    }
}

/// Het begin van de beurt van de bewoner met ctx-blok `ctx` op lijstplek
/// `i`, op elke architectuur: de cursor, wie er draait, een kick die nu
/// vervalt, en zijn staat.
pub(crate) fn begin(sched: Pa, i: usize, ctx: Pa) {
    dev::write64(sched.add(SCHED_CURSOR), i as u64);
    dev::write64(sched.add(SCHED_CURRENT), ctx.0);
    os_ctx_write(ctx, CTX_KICK_PENDING, 0);
    os_ctx_write(ctx, CTX_STATE, CtxState::Running.raw());
    STATS.entries.fetch_add(1, Relaxed);
}

/// De kern is terug na een beurt van `dt` tikken: de meetlat, en niemand
/// draait meer.
pub(crate) fn home(sched: Pa, dt: u64) {
    STATS.ticks.fetch_add(dt, Relaxed);
    STATS.longest.fetch_max(dt, Relaxed);
    dev::write64(sched.add(SCHED_CURRENT), 0);
}

/// Het einde van de beurt van de bewoner van `ctx`, na zijn `settle`: de
/// laatste beurt in de meetlat.
pub(crate) fn end(ctx: Pa, back: Back) -> Back {
    STATS.last.store(ctx.0 | back.code(), Relaxed);
    back
}

/// `None` als de geyielde bewoner van `ctx` aan de beurt is op
/// counterstand `now`, anders zijn wektijd. Dezelfde vraag als de switcher
/// stelt: de wektijd (0 = meteen), een kick, of RX voorbij de gewapende
/// drempel (niet bij bit 63, een wachter zonder peek). Ook de vraag van de
/// wekker van de app-cores op Apple (`waker.go` `wakeDue`, hopos/src/cage.rs).
#[must_use]
pub fn due(ctx: Pa, now: u64) -> Option<u64> {
    let w = os_ctx_read(ctx, CTX_WAKE);
    let t = w & !CTX_WAKE_NO_PEEK;
    if t == 0 || now >= t || os_ctx_read(ctx, CTX_KICK_PENDING) != 0 {
        return None;
    }
    if w & CTX_WAKE_NO_PEEK == 0 && rx_due(ctx) {
        return None;
    }
    Some(t)
}

/// Legt de eerste beurt van een bewoner van de OS-core klaar: x0 = `arg`
/// (de control-page, zoals de trampoline hem doorgeeft), de rest nul,
/// hervatten op `entry` in EL1h met DAIF dicht, en een schoon EL1-regime
/// (SCTLR zonder MMU, de rest nul), en lege FP-registers die de overgang
/// terugzet. Wat de trampoline van een app-core verder zet (VTCR, CPTR,
/// CNTHCTL, CNTVOFF) deed [`OsCore::new`] één keer voor de hele core. Vóór
/// [`host`]; op riscv64 staat de koude start al in het ctx-blok.
pub fn prepare(ctx: Pa, entry: u64, arg: u64) {
    for r in 0..31 {
        ctx_write(ctx, CTX_GPRS + 8 * r, 0);
    }
    ctx_write(ctx, CTX_GPRS, arg);
    ctx_write(ctx, CTX_SP, 0);
    ctx_write(ctx, CTX_SP + 8, 0);
    ctx_write(ctx, CTX_RESUME, entry);
    ctx_write(ctx, CTX_RESUME + 8, SPSR_EL1H_MASKED);
    for w in 0..CTX_REGIME_ARM_WORDS {
        ctx_write(ctx, CTX_REGIME + 8 * w, 0);
    }
    ctx_write(ctx, CTX_REGIME, SCTLR_EL1_CLEAN);
    ctx_write(ctx, CTX_WAKE, 0);
    ctx_write(ctx, CTX_KICK_PENDING, 0);
    dev::clear(ctx.add(CTX_FPRS), (8 * CTX_FPRS_ARM_WORDS) as usize);
    dev::write64(ctx.add(CTX_FP_LIVE), 1);
    dev::push(ctx.add(CTX_FPRS), (CTX_FP_END - CTX_FPRS) as usize);
}

/// Sched-blok 0: de bewonerslijst van de OS-core.
fn os_sched(plan: &Plan) -> Result<Pa, Error> {
    roster::sched(plan, Core::new(0).ok_or(Error::BadContext { pa: 0 })?)
}

/// Maakt de bewoner met ctx-blok `ctx` bewoner van de OS-core: de rotatie
/// neemt hem mee zodra de executor idle is ([`roster::enlist`]: eerst de
/// lijst, dan boot-pending). Het tegenstuk van [`super::dispatch`] voor een
/// app-core, zonder mailbox: de kern ís de core.
pub fn host(plan: &Plan, ctx: Pa) -> Result<(), Error> {
    roster::enlist(os_sched(plan)?, ctx)
}

/// Zet de bewoner met ctx-blok `ctx` terug in de rotatie van de OS-core
/// zonder één woord van zijn ctx te raken: na een kern-flip draagt het blok
/// zijn bewaarde staat nog, en de nieuwe kern hervat hem waar de oude hem
/// liet. Geeft `true` als hij er niet meer in stond.
pub fn rehost(plan: &Plan, ctx: Pa) -> Result<bool, Error> {
    roster::add(os_sched(plan)?, ctx)
}

/// Staat de bewoner met ctx-blok `ctx` in de rotatie van de OS-core?
#[must_use]
pub fn hosts(plan: &Plan, ctx: Pa) -> bool {
    os_sched(plan).is_ok_and(|sched| roster::find(sched, ctx).is_some())
}

/// Trekt de bewoner van de OS-core met ctx-blok `ctx` in: de aanvraag
/// (`CTX_REVOKE`) en de kick (`CTX_KICK_PENDING`), zodat hij aan de beurt
/// is en [`next`] hem doodt zonder dat hij nog draait. Op de OS-core draait
/// hij nu niet (de kern draait, deze code), dus er is niets te onderbreken.
pub fn recall(ctx: Pa) {
    ctx_write(ctx, CTX_REVOKE, 1);
    ctx_write(ctx, CTX_KICK_PENDING, 1);
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    //! De instructies van de overgang: de EL2-kant van de OS-core. Elk blok
    //! raakt alleen registers van deze core, en de enige sprong naar een
    //! lagere EL is [`enter`].
    use super::super::dispatch::{HVC_YIELD, VEC_SYNC_LOWER};
    use super::super::layout::{
        CTX_FP_LIVE, CTX_FPRS, CTX_FPRS_ARM_WORDS, CTX_GPRS, CTX_REGIME, CTX_RESUME, CTX_SP,
    };
    use super::Flavor;
    use crate::vectors::EC_HVC64;
    use core::arch::{asm, global_asm};
    use dev::Pa;

    /// De teller (CNTPCT_EL0).
    #[inline]
    pub(super) fn counter() -> u64 {
        let v: u64;
        // SAFETY: een lees van de teller heeft geen neveneffect.
        unsafe { asm!("isb", "mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn freq() -> u64 {
        let v: u64;
        // SAFETY: CNTFRQ_EL0 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn wfe() {
        // SAFETY: WFE wacht op een event; geen geheugeneffect.
        unsafe { asm!("wfe", options(nomem, nostack, preserves_flags)) };
    }

    /// De sprong van [`super::hold`] de parkeerlus in: maskers dicht, de
    /// EL2-MMU en de caches uit (de parkeerlus en de trampoline draaien
    /// zoals op elke app-core met de MMU uit, en de plan-regio is in de map
    /// van de kern XN), de eigen I-cache leeg, TPIDR_EL2 = de mailbox.
    pub(super) fn park(mbox: u64, park: u64) -> ! {
        // SAFETY: deze core draait niets meer van de kern (de kern
        // verhuisde, zijn stack wordt niet meer gelezen); de code hier staat
        // identity-gemapt (VA = PA), dus de fetch na het uitzetten van de
        // MMU leest dezelfde bytes. `park` is de parkeerlus van het plan,
        // `mbox` de park-mailbox van deze core: dat is precies wat de lus
        // van TPIDR_EL2 verwacht.
        unsafe {
            asm!(
                "msr daifset, #0xf",
                "mrs {t}, sctlr_el2",
                "bic {t}, {t}, #1",
                "bic {t}, {t}, #(1 << 2)",
                "bic {t}, {t}, #(1 << 12)",
                "dsb sy",
                "msr sctlr_el2, {t}",
                "isb",
                "ic iallu",
                "dsb sy",
                "isb",
                "msr tpidr_el2, {m}",
                "br {p}",
                t = in(reg) 0u64,
                m = in(reg) mbox,
                p = in(reg) park,
                options(noreturn, nostack),
            )
        }
    }

    /// Maskeert I en F en geeft DAIF zoals het stond.
    pub(super) fn mask() -> u64 {
        let v: u64;
        // SAFETY: DAIF lezen en maskeren raakt alleen PSTATE van deze core;
        // geen `nomem`, zodat er geen toegang over het masker schuift.
        unsafe { asm!("mrs {}, daif", "msr daifset, #3", out(reg) v, options(nostack)) };
        v
    }

    /// Zet DAIF terug zoals `mask` hem las.
    pub(super) fn restore(daif: u64) {
        // SAFETY: zie `mask`.
        unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
    }

    pub(super) fn hcr() -> u64 {
        let v: u64;
        // SAFETY: HCR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, hcr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn esr() -> u64 {
        let v: u64;
        // SAFETY: ESR_EL2 lezen heeft geen neveneffect; hij beschrijft de
        // trap waarmee de bewoner net terugkwam (sindsdien geen exception:
        // I en F dicht, en de kern-code ertussen faultt niet).
        unsafe { asm!("mrs {}, esr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn far() -> u64 {
        let v: u64;
        // SAFETY: zie `esr`.
        unsafe { asm!("mrs {}, far_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// Het EL2-regime dat een bewoner nodig heeft en de kern niet raakt,
    /// eenmalig: VTCR (4 KB-granule, 39-bit IPA, PS = min(PARange, 44 bit),
    /// dezelfde waarde als de trampoline), CPTR zonder FP-trap (de kern is
    /// softfloat; de overgang bewaart de FP van een bewoner op EL2),
    /// CNTHCTL met de teller- en timertoegang van EL1 in beide lay-outs, en
    /// CNTVOFF 0: de wektijd van een yield is dan dezelfde stand als CNTPCT.
    pub(super) fn prepare(flavor: Flavor) {
        const VTCR_NO_PS: u64 = 0x8000_3559;
        const PARANGE_44: u64 = 4;
        // VTCR_EL2.VS: zestien VMID-bits als het ijzer ze heeft, zoals de
        // trampoline (`cpu::el2::switch`).
        const VTCR_VS: u64 = 1 << 19;
        let cptr = cptr(flavor, true);
        // SAFETY: registers van het EL2-regime die alleen een lagere EL
        // raken (VTCR, CNTVOFF, de EL1-toegang in CNTHCTL) of een trap
        // weghalen die de softfloat-kern nooit raakt (CPTR); geen geheugen.
        unsafe {
            asm!(
                "mrs {t}, id_aa64mmfr0_el1",
                "and {t}, {t}, #0xf",
                "cmp {t}, {p44}",
                "csel {t}, {t}, {p44}, lo",
                "orr {t}, {vtcr}, {t}, lsl #16",
                "mrs {u}, id_aa64mmfr1_el1",
                "ubfx {u}, {u}, #4, #4",
                "cmp {u}, #2",
                "b.ne 2f",
                "orr {t}, {t}, {vs}",
                "2:",
                "msr vtcr_el2, {t}",
                "msr cptr_el2, {cptr}",
                "mrs {t}, cnthctl_el2",
                "orr {t}, {t}, #0x3",
                "orr {t}, {t}, #(0x3 << 10)",
                "msr cnthctl_el2, {t}",
                "msr cntvoff_el2, xzr",
                "isb",
                t = out(reg) _,
                u = out(reg) _,
                vs = in(reg) VTCR_VS,
                p44 = in(reg) PARANGE_44,
                vtcr = in(reg) VTCR_NO_PS,
                cptr = in(reg) cptr,
                options(nomem, nostack),
            );
        }
    }

    /// CPTR_EL2 zonder FP-trap (`fp`) of met: nVHE TFP (bit 10), onder
    /// E2H = 1 FPEN = 0. Beide trappen ook EL2 zelf; de overgang zet de
    /// trap daarom terug uit vóór de kern weer verder gaat.
    pub(super) fn cptr(flavor: Flavor, fp: bool) -> u64 {
        match (flavor, fp) {
            (Flavor::Nvhe, true) => 0x33FF,
            (Flavor::Nvhe, false) => 0x33FF | 1 << 10,
            (Flavor::Vhe | Flavor::AppleVhe, true) => 0x30_0000,
            (Flavor::Vhe | Flavor::AppleVhe, false) => 0,
        }
    }

    /// VTTBR_EL2 op de eenheid van de bewoner; `fresh`: plus een TLBI van
    /// deze VMID en een lege I-cache (een verse bewoner op hergebruikte PA's
    /// en VMID).
    pub(super) fn set_vttbr(v: u64, fresh: bool) {
        // SAFETY: VTTBR_EL2 vertaalt alleen voor EL1/EL0; de kern op EL2
        // merkt er niets van. De TLBI en IC raken alleen caches van deze
        // core.
        unsafe {
            asm!("msr vttbr_el2, {}", "isb", in(reg) v, options(nostack));
            if fresh {
                asm!(
                    "tlbi vmalls12e1",
                    "dsb nsh",
                    "ic iallu",
                    "dsb nsh",
                    "isb",
                    options(nostack)
                );
            }
        }
    }

    /// De CNTHP (de fysieke EL2-timer, PPI 26) op `deadline`: een bewoner
    /// op EL1 kan hem niet zien of uitzetten, anders dan de CNTP.
    pub(super) fn timer_arm(deadline: u64) {
        // SAFETY: alleen de EL2-timer van deze core.
        unsafe {
            asm!(
                "msr cnthp_cval_el2, {d}",
                "msr cnthp_ctl_el2, {one}",
                "isb",
                d = in(reg) deadline,
                one = in(reg) 1u64,
                options(nomem, nostack),
            );
        }
    }

    /// De CNTHP uit; `true` als hij afging (ISTATUS). Uit laat zijn lijn
    /// vallen, dus de GIC hoeft hem niet te claimen.
    pub(super) fn timer_disarm() -> bool {
        let ctl: u64;
        // SAFETY: alleen de EL2-timer van deze core.
        unsafe {
            asm!(
                "mrs {c}, cnthp_ctl_el2",
                "msr cnthp_ctl_el2, xzr",
                "isb",
                c = out(reg) ctl,
                options(nomem, nostack),
            );
        }
        ctl & (1 << 2) != 0
    }

    /// IPI_SR_EL1 bit 0: wacht er een fast IPI op deze core? Zonder ack
    /// (de peek van [`super::Bell::apple`]); alleen op Apple silicium.
    pub(super) fn apple_ipi_pending() -> u32 {
        let v: u64;
        // SAFETY: IPI_SR_EL1 lezen heeft geen neveneffect; de aanroepers
        // zijn de bel van `Bell::apple` en `apple_ipi_ack`, en die bestaan
        // alleen op een Apple-core, waar het register er is.
        unsafe { asm!("mrs {}, s3_5_c15_c1_1", out(reg) v, options(nomem, nostack)) };
        if v & 1 != 0 {
            super::Bell::APPLE_INTID
        } else {
            0
        }
    }

    /// Ackt de fast IPI van deze core (IPI_SR_EL1, bit 0 is W1C).
    pub(super) fn apple_ipi_ack() {
        // SAFETY: raakt alleen de IPI-status van deze core; alleen geroepen
        // onder `Flavor::AppleVhe` of uit de dispatch van het Apple-board,
        // dus op een Apple-core.
        unsafe { asm!("msr s3_5_c15_c1_1, {}", "isb", in(reg) 1u64, options(nomem, nostack)) };
    }

    unsafe extern "C" {
        fn hopos_os_nvhe_enter(ctx: u64, hcr: u64, vbar: u64, cptr: u64, cptr_fp: u64) -> u64;
        fn hopos_os_vhe_enter(ctx: u64, hcr: u64, vbar: u64, cptr: u64, cptr_fp: u64) -> u64;
        safe static hopos_os_nvhe_vectors: u8;
        safe static hopos_os_vhe_vectors: u8;
        safe static hopos_os_stub_spin: u8;
        safe static hopos_os_stub_yield: u8;
    }

    /// Het adres van een label in het kern-image (identity: ook de PA).
    fn addr(p: *const u8) -> u64 {
        p.addr() as u64
    }

    /// De ingang van de zelftest-bewoner.
    pub(super) fn stub(yield_: bool) -> u64 {
        if yield_ {
            addr(&raw const hopos_os_stub_yield)
        } else {
            addr(&raw const hopos_os_stub_spin)
        }
    }

    /// Eén beurt van de bewoner met ctx-blok `ctx`, onder HCR_EL2 = `hcr`.
    /// Geeft de vectorindex waarmee hij terugkwam (8 synchroon, 9 IRQ, 10
    /// FIQ, 11 SError, 12..15 AArch32). `fp`: de bewoner gebruikt FP (zijn
    /// FP-staat gaat mee); anders draait hij met de FP-trap aan.
    pub(super) fn enter(flavor: Flavor, ctx: Pa, hcr: u64, fp: bool) -> u64 {
        // Apple is VHE (E2H RES1): dezelfde overgang als de O6N. Het verschil
        // (de kick als FIQ, de ack op EL2) zit in `settle`, niet hier.
        type Enter = unsafe extern "C" fn(u64, u64, u64, u64, u64) -> u64;
        let (f, vbar): (Enter, u64) = match flavor {
            Flavor::Nvhe => (hopos_os_nvhe_enter, addr(&raw const hopos_os_nvhe_vectors)),
            Flavor::Vhe | Flavor::AppleVhe => {
                (hopos_os_vhe_enter, addr(&raw const hopos_os_vhe_vectors))
            }
        };
        // SAFETY: `ctx` is een ctx-blok van het plan (of de scratch van de
        // zelftest) met een hervatbaar regime, en de aanroeper heeft I en F
        // gemaskeerd. De stub bewaart de callee-saved registers, HCR en
        // VBAR van de kern op zijn eigen stack en zet ze terug vóór hij
        // terugkeert; de bewoner draait op EL1 onder stage-2 (of, bij de
        // zelftest, zonder MMU op een eigen stub in het kern-image) en kan
        // de EL2-stack niet zien. Elke exception uit de bewoner komt op
        // `vbar` en keert via dezelfde frame terug.
        unsafe { f(ctx.0, hcr, vbar, cptr(flavor, fp), cptr(flavor, true)) }
    }

    // De overgang en de vectoren, twee smaken uit één bron (zoals de
    // switcher): nVHE (op1 = 0) en VHE (op1 = 5, de _EL12-encoderingen).
    // De VHE-smaak eist dat de kern zelf onder E2H = 1 draait: onder E2H = 0
    // zijn de _EL12-encoderingen UNDEFINED (29-09, QEMU neoverse-n1 met een
    // nVHE-kern: EC 0x0 op `hopos_os_vhe_enter` + 0x40 bij de zelftest).
    // Het board kiest die vorm (board/uefi/src/el2.rs), de binary toetst
    // dat die bij de smaak past (hopos/src/cage.rs).
    //
    // De frame op SP_EL2 (144 bytes): +0..+88 x19..x30, +96 x18 en de ctx,
    // +112 HCR en VBAR van de kern, +128 CPTR van deze beurt en die zonder
    // FP-trap (lazy FP: zijn ze gelijk, dan gaat de FP-staat mee). SP_EL2
    // verandert niet door een beurt op EL1, dus de exception komt binnen met
    // SP = deze frame.
    //
    // FP (de moduledoc): q0..q31 per register via x2/x3 naar of uit de
    // FP-kier, laag dan hoog (`fmov d` wist de hoge helft, dus die tweede),
    // dan FPCR en FPSR. De terugweg schrijft `CTX_FP_LIVE` altijd.
    //
    // Veiligheid (als gewone regel: clippy weigert een SAFETY-tag op
    // `global_asm!`): deze code draait alleen via `enter` hierboven, met de
    // maskers dicht; de regime-save en -restore zijn die van de switcher
    // (regs.rs, gedeeld met switch.rs), die ze op ijzer bewees.
    global_asm!(
        concat!(
            r#"
    // Eén q-register via x2/x3 naar of uit [x4], met x4 een register
    // verder. Eigen macro's: in de `.irp` van een smaak eet de smaak de
    // `\()` op, en `\n.d` leest de assembler als één naam. Het doel is
    // softfloat: FP en SIMD gaan alleen rond deze blokken aan.
    .macro hopos_os_qsave n
    fmov x2, d\n
    fmov x3, v\n\().d[1]
    stp x2, x3, [x4], #16
    .endm
    .macro hopos_os_qload n
    ldp x2, x3, [x4], #16
    fmov d\n, x2
    fmov v\n\().d[1], x3
    .endm

    .macro hopos_os_flavor p, op1
    .pushsection .text.hopos_os, "ax"
    .balign 16
    .global \p\()_enter
\p\()_enter:
    sub sp, sp, #144
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    stp x18, x0, [sp, #96]
    mrs x9, hcr_el2
    mrs x10, vbar_el2
    stp x9, x10, [sp, #112]
    stp x3, x4, [sp, #128]
    msr cptr_el2, x3
    msr vbar_el2, x2
    msr hcr_el2, x1
    isb
    mov x1, x0
"#,
            restore_el1!(),
            r#"
    ldr x2, [x1, #{fp_live}]
    cbz x2, 1f
    ldp x2, x3, [sp, #128]
    cmp x2, x3
    b.ne 1f
    .arch_extension fp
    .arch_extension simd
    add x4, x1, #{fprs}
    .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
    hopos_os_qload \n
    .endr
    ldp x2, x3, [x4]
    msr fpcr, x2
    msr fpsr, x3
    .arch_extension nosimd
    .arch_extension nofp
1:
"#,
            restore_gprs_eret!(),
            r#"

// De terugweg, voor elke exception uit de bewoner. x0 = de vectorindex, de
// x0/x1 van de bewoner staan op [sp] (16 bytes onder de frame).
\p\()_back:
    ldr x1, [sp, #(16 + 104)]
    stp x2, x3, [x1, #({gprs} + 2 * 8)]
"#,
            save_gprs!(),
            r#"
    ldp x2, x3, [sp], #16
    stp x2, x3, [x1, #{gprs}]
"#,
            save_el1!(),
            r#"
    // Lazy FP: draaide hij met de trap aan, dan raakte hij FP niet (anders
    // was hij hier op die trap): niets te bewaren, en de trap uit vóór er
    // op EL2 nog FP komt. FP_LIVE blijft zoals hij was.
    ldp x2, x3, [sp, #128]
    cmp x2, x3
    b.eq 5f
    msr cptr_el2, x3
    isb
    b 3f
5:
    cmp x0, #{vsync}
    b.ne 2f
    mrs x2, esr_el2
    ubfx x3, x2, #26, #6
    cmp x3, #{ec_hvc}
    b.ne 2f
    and x3, x2, #0xffff
    cmp x3, #{hvc_yield}
    b.ne 2f
    str xzr, [x1, #{fp_live}]
    b 3f
2:
    .arch_extension fp
    .arch_extension simd
    add x4, x1, #{fprs}
    .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
    hopos_os_qsave \n
    .endr
    mrs x2, fpcr
    mrs x3, fpsr
    .arch_extension nosimd
    .arch_extension nofp
    stp x2, x3, [x4]
    mov x2, #1
    str x2, [x1, #{fp_live}]
3:
    ldp x2, x3, [sp, #112]
    msr hcr_el2, x2
    msr vbar_el2, x3
    isb
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    ldr x18, [sp, #96]
    add sp, sp, #144
    ret

// De vectoren tijdens een beurt. De huidige EL (0..7) is de kern zelf: door
// naar zijn eigen tabel (een fout in de kern blijft een fout van de kern).
// Een lagere EL (8..15) is de bewoner: terug naar de kern.
    .balign 2048
    .global \p\()_vectors
\p\()_vectors:
    .irp i, 0, 1, 2, 3, 4, 5, 6, 7
    .balign 0x80
    b __hopos_vectors + \i * 0x80
    .endr
    .irp i, 8, 9, 10, 11, 12, 13, 14, 15
    .balign 0x80
    stp x0, x1, [sp, #-16]!
    mov x0, #\i
    b \p\()_back
    .endr
    .popsection
    .endm

    hopos_os_flavor hopos_os_nvhe, 0
    hopos_os_flavor hopos_os_vhe, 5

// De zelftest-bewoner: EL1 zonder MMU, uit het kern-image. Spinnen tot de
// kern hem onderbreekt (hooguit tot tellerstand x0, dan toch een yield), of
// meteen yielden (x1 = 0: wek nu).
    .pushsection .text.hopos_os, "ax"
    .balign 16
    .global hopos_os_stub_spin
hopos_os_stub_spin:
    isb
    mrs x2, cntvct_el0
    cmp x2, x0
    b.lo hopos_os_stub_spin
    b hopos_os_stub_yield
    .global hopos_os_stub_yield
hopos_os_stub_yield:
    mov x1, #0
    hvc #1
    b hopos_os_stub_yield
    .popsection
"#
        ),
        gprs = const CTX_GPRS,
        regime = const CTX_REGIME,
        resume = const CTX_RESUME,
        ctx_sp = const CTX_SP,
        fprs = const CTX_FPRS,
        fp_live = const CTX_FP_LIVE,
        vsync = const VEC_SYNC_LOWER,
        ec_hvc = const EC_HVC64,
        hvc_yield = const HVC_YIELD,
    );
    // De twee lussen hierboven lopen 32 keer 16 bytes plus FPCR/FPSR.
    const _: () = assert!(CTX_FPRS_ARM_WORDS == 32 * 2 + 2);
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Host-stubs: geen EL2. De rotatie en de lijst bewijzen de tests; de
    //! overgang zelf bewijst het board.
    use super::Flavor;
    use dev::Pa;

    pub(super) fn counter() -> u64 {
        0
    }
    pub(super) fn mask() -> u64 {
        0
    }
    pub(super) fn restore(_daif: u64) {}
    pub(super) fn freq() -> u64 {
        62_500_000
    }
    pub(super) fn wfe() {
        core::hint::spin_loop();
    }
    pub(super) fn park(_mbox: u64, _park: u64) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
    pub(super) fn hcr() -> u64 {
        0
    }
    pub(super) fn esr() -> u64 {
        0
    }
    pub(super) fn far() -> u64 {
        0
    }
    pub(super) fn prepare(_flavor: Flavor) {}
    pub(super) fn set_vttbr(_v: u64, _fresh: bool) {}
    pub(super) fn timer_arm(_deadline: u64) {}
    pub(super) fn timer_disarm() -> bool {
        false
    }
    pub(super) fn stub(_yield: bool) -> u64 {
        0
    }
    pub(super) fn apple_ipi_pending() -> u32 {
        0
    }
    pub(super) fn apple_ipi_ack() {}
    /// Op de host komt elke beurt meteen terug op een IRQ (vector 9); de
    /// tests toetsen de rotatie, niet de overgang.
    pub(super) fn enter(_flavor: Flavor, _ctx: Pa, _hcr: u64, _fp: bool) -> u64 {
        super::VEC_IRQ_LOWER
    }
}

#[cfg(test)]
mod tests;
