//! De slaap van een arm64-core: de [`Sleeper`] van de executor, en de klok
//! ([`now`]) die de executor als [`executor::Clock`] krijgt.
//!
//! Dit bezit: de keuze van de event-stream (EVNTI per CNTFRQ), de manieren
//! van slapen (WFE met event-stream, WFI op de fysieke timer, en op de
//! OS-core de rotatie over zijn bewoners) en de tellers die zeggen of een
//! core slaapt of spint.
//! Niet van hier: wat er te doen is. De deuren
//! (IRQ-vlag, ringkoppen) zitten in de executor en komen binnen als het
//! `ready`-predicaat; de governor uit de Go-kern, met zijn `nested`-vlaggen,
//! `RunIdleTimers` en `IdleMayReady`, bestaat niet meer (PORT §4).
//!
//! Waarom slapen hier geen detail is (Derek): jobs staan vooral te idlen, en
//! een slapende core is clock-gated en verbruikt vrijwel niets, op elke
//! kloksnelheid. Een core die "idle" spint is dat niet.

use crate::el2::{OsCore, TURN_CAP_NS, Turn};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use executor::Sleeper;

/// De event-stream mag hooguit zo lang zijn: 1,5 ms. De kleinste periode
/// eronder is op elk bekend board ~1 ms (zie [`event_stream`]).
pub const EVENT_STREAM_MAX_NS: u64 = 1_500_000;

/// De vangrail van de WFI-slaap zonder deadline: 10 ms.
///
/// Een SEV van een andere core wekt WFI niet (alleen WFE), en "geen timer"
/// als oneindig lezen is geen zuinigheid maar een hang (idle.go `wakeAt`):
/// zolang niets anders wekt, pollt de core dus op deze korrel.
pub const WFI_CAP_NS: u64 = 10_000_000;

/// Nanoseconden per seconde.
const NS_PER_S: u64 = 1_000_000_000;

/// Omrekenen van counter-ticks naar nanoseconden zonder overloop.
///
/// Seconden en rest apart: zelfs een uur maal 1 GHz loopt over als je eerst
/// vermenigvuldigt en dan deelt (idle.go `wakeAt`). `hz == 0` geeft 0.
#[must_use]
pub const fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    // Een tik van een heel aantal nanoseconden (1 GHz, 62,5, 25 en 10 MHz;
    // niet de 54 en 24 MHz van de Pi's en de RK3566): één vermenigvuldiging
    // en hetzelfde getal. De deling door een u128 is software
    // (`__udivti3`), en de klok wordt per beurt op de OS-core een paar keer
    // gelezen.
    if NS_PER_S.is_multiple_of(hz) {
        return ticks.saturating_mul(NS_PER_S / hz);
    }
    let secs = ticks / hz;
    let rest = ticks % hz;
    // `rest < hz <= u64::MAX`, dus `rest * 1e9` past in een u128.
    let frac = (rest as u128 * 1_000_000_000 / hz as u128) as u64;
    secs.saturating_mul(1_000_000_000).saturating_add(frac)
}

/// Omrekenen van nanoseconden naar counter-ticks, zelfde splitsing.
#[must_use]
pub const fn ns_to_ticks(ns: u64, hz: u64) -> u64 {
    if NS_PER_S.is_multiple_of(hz) {
        return ns / (NS_PER_S / hz);
    }
    let secs = ns / 1_000_000_000;
    let rest = ns % 1_000_000_000;
    let frac = (rest as u128 * hz as u128 / 1_000_000_000) as u64;
    secs.saturating_mul(hz).saturating_add(frac)
}

/// De CNTKCTL_EL1-waarde voor de event-stream: EVNTEN plus de EVNTI-bit
/// met de grootste periode die nog onder `max_period_ns` blijft.
///
/// EVNTI kiest de counterbit waarvan de 0-naar-1-flank het wek-event is;
/// de periode is 2^(EVNTI+1)/CNTFRQ. Bit 15 op de Pi's 54 MHz (1,2 ms) en
/// QEMU's 62,5 MHz (1,05 ms), bit 14 op de Altra's 25 MHz (1,3 ms; een
/// vaste 15 gaf daar 2,6 ms wek-granulariteit).
///
/// EVNTI is 4 bits, dus bit 15 is het plafond, en dat plafond is op een
/// GHz-teller te laag. GEMETEN 29-08 op de Mac mini M4 (CNTFRQ 1 GHz):
/// 2^16 ticks = 65 µs, vijftien keer vaker wakker dan bedoeld. FEAT_ECV
/// (`ecv`, ID_AA64MMFR0_EL1.ECV >= 1) heeft daar een schaalbit voor:
/// CNTKCTL.EVNTIS schuift de gekozen bit 8 posities op (x256), en die
/// zetten we zodra zelfs bit 15 onder een halve milliseconde uitkomt. Op
/// de M4 komt dit uit op EVNTI 11: 2^20 ticks = 1,048 ms = 954 wekken/s,
/// precies wat er gemeten is. Op elk board onder ~131 MHz verandert er
/// niets.
#[must_use]
pub const fn event_stream(hz: u64, ecv: bool, max_period_ns: u64) -> u64 {
    const EVNTEN: u64 = 1 << 2;
    const EVNTIS: u64 = 1 << 17;
    let shift: u32 = if ecv && (1u64 << 16) < hz / 2000 {
        8
    } else {
        0
    };
    let mut i: u32 = 15;
    while i > 4 && (1u128 << (i + 1 + shift)) * 1_000_000_000 > hz as u128 * max_period_ns as u128 {
        i -= 1;
    }
    let mut v = EVNTEN | ((i as u64) << 4);
    if shift != 0 {
        v |= EVNTIS;
    }
    v
}

/// De bits van de event-stream in CNTKCTL_EL1 en CNTHCTL_EL2 (in beide
/// op dezelfde plek): EVNTEN (2), EVNTDIR (3), EVNTI (7:4) en EVNTIS (17).
pub const EVENT_STREAM_BITS: u64 = (0xF << 4) | (1 << 3) | (1 << 2) | (1 << 17);

/// De event-stream `stream` in een CNTHCTL_EL2 die `old` was, met de rest
/// intact.
///
/// Waarom dit bestaat: onder E2H = 1 schrijft `msr cntkctl_el1` vanaf EL2
/// in werkelijkheid CNTHCTL_EL2 (de vondst van de Apple-agent, board/apple
/// `sleeper`, en daarna op de O6N-vorm van board-uefi). Een blinde write
/// zet de event-stream goed maar wist EL1PCTEN en EL1PTEN (10, 11) en de
/// EL0-bits (0, 1, 8, 9), en dan trapt een `mrs cntpct_el0` van elke
/// bewoner. Dus alleen de stream-bits vervangen.
#[must_use]
pub const fn merge_event_stream(old: u64, stream: u64) -> u64 {
    (old & !EVENT_STREAM_BITS) | (stream & EVENT_STREAM_BITS)
}

/// De WFE-slaap: WFE's tot `ready()` of tot de teller `deadline` haalt, en
/// de tijd die ze duurden (ticks). `wfe` en `counter` zijn de instructies;
/// een test geeft een nep-core.
///
/// Een wek die geen werk bracht (de tik van de event-stream, de SEV van een
/// andere core, een verschaald event) gaat niet terug naar de executor maar
/// meteen de volgende WFE in: Linux' `do_idle`, dat blijft liggen zolang
/// `need_resched()` niets zegt. Tot 30-09 keerde de slaap na elke echte WFE
/// terug, en elke event-stream-tik werd een ronde van de executor: ~830
/// rondes per seconde op een stille Pi (de event-stream van 1,2 ms), met
/// daarbovenop elke SEV van de kern (de Radxa-app: 3.003 per seconde).
///
/// Tussen twee WFE's staan alleen kale loads (`ready()` belooft dat, net als
/// de teller): een exclusive zou het event-register weer vullen en de
/// volgende WFE meteen laten terugkeren (Altra 18-07, 4,7M wakes/s). De
/// eerste WFE mag wel meteen terugkeren op een event van vóór de slaap; de
/// tweede slaapt.
pub fn wfe_until(
    deadline: u64,
    ready: &dyn Fn() -> bool,
    wfe: &mut dyn FnMut() -> u64,
    counter: &dyn Fn() -> u64,
) -> u64 {
    let mut slept: u64 = 0;
    while !ready() && counter() < deadline {
        slept = slept.saturating_add(wfe());
    }
    slept
}

/// Monotone nanoseconden sinds het aanzetten van de teller: de
/// [`executor::Clock`] van deze architectuur.
///
/// CNTPCT_EL0 gedeeld door CNTFRQ_EL0. De drop naar EL1 zet CNTVOFF op 0
/// en geeft EL1 toegang tot de fysieke teller (drop.h), dus dit is dezelfde
/// stand als de CNTVCT die de EL2-switcher tegen de wektijden houdt.
#[must_use]
pub fn now() -> u64 {
    ticks_to_ns(arch::counter(), arch::freq())
}

/// De rauwe teller in ticks (CNTPCT_EL0).
#[must_use]
pub fn counter() -> u64 {
    arch::counter()
}

/// De eenheid van de teller: ticks per seconde (CNTFRQ_EL0).
///
/// LET OP QEMU-TCG: WFE is daar een no-op, dus idle-tijd meet er ~0;
/// idle-metingen zijn ijzer-metingen.
#[must_use]
pub fn freq() -> u64 {
    arch::freq()
}

/// De fysieke EL1-timer (CNTP) van deze core uit: zijn lijn valt, tot de
/// slaap hem weer zet.
pub fn timer_off() {
    arch::timer_off();
}

/// De EL2-timer (CNTHP) van deze core uit: zijn lijn valt.
pub fn hyp_timer_off() {
    arch::hyp_timer_off();
}

/// Hoe een core slaapt als hij niets te doen heeft.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// WFE tot werk of de deadline ([`wfe_until`]), met de event-stream
    /// (~1 ms) als klok: de default, want hij wekt op elk silicium dat we
    /// kennen (behalve de M4). Zonder deadline houdt
    /// [`WFI_CAP_NS`] de vangrail.
    Wfe,
    /// WFI met de fysieke timer op de deadline. Alleen kiezen waar het board
    /// bewezen heeft dat de timer-PPI de WFI wekt: een slaap die niet wekt
    /// is een hang, en die hoort niet uit een redenering te komen.
    Wfi,
    /// De OS-core (PORT.md beslissing 2): idle is een beurt voor de volgende
    /// bewoner die aan de beurt is ([`OsCore::run`]), op EL1 onder stage-2,
    /// tot een interrupt, de kick van een app-core, de deadline of zijn
    /// eigen yield de core teruggeeft. Is niemand aan de beurt, dan slaapt
    /// de core in zijn basismodus (WFE of WFI) tot de vroegste wektijd.
    /// Een ronde-modus: de slaper staat erin zodra hij een [`OsCore`] host
    /// ([`ArmSleeper::host`]); als basismodus leest hij als `Wfe`.
    Resident,
}

/// De meetlat van de slaap (handboek §4): zonder deze getallen is "de node
/// slaapt" niet te onderscheiden van "de node spint".
#[derive(Default, Debug)]
pub struct Stats {
    /// Geaccumuleerde idle-TIJD in ticks: de counterstand voor en na elke
    /// slaap, delta erbij. Tijd en geen rondes (herzien 18-07): WFE wekt
    /// ook op SEV's van andere cores en spurious events, en op de drukke
    /// Altra las elke deels-idle app daardoor als "vol idle" (DUTY
    /// 25/50/75 gaf allemaal cpu=0%). Een valse wek telt zo zijn echte
    /// microduur, geen volle tik.
    pub idle_ticks: AtomicU64,
    /// Aantal slaaprondes (de wek-teller van 31-07). Samen met de idle-tijd
    /// is de rekensom rond: tijd per wek = (1 - idle-fractie) / wekken-per-s.
    pub wakes: AtomicU64,
    /// Rondes waarin `ready()` al waar was toen de interrupts dicht waren:
    /// de verloren-wek-race die de deur dichthield.
    pub caught: AtomicU64,
}

/// De [`Sleeper`] van een arm64-core.
///
/// Eén per core, eigendom van de executor-lus van die core (`run` neemt
/// hem als `&mut`). De modus kiest het board, na zijn bewijs op dít
/// silicium.
pub struct ArmSleeper {
    mode: Mode,
    hz: u64,
    os: Option<OsCore>,
    /// De meetlat.
    pub stats: Stats,
}

impl ArmSleeper {
    /// Een sleeper in modus `mode`. Zet op deze core de event-stream aan
    /// (CNTKCTL is per core): zonder die stream wekt een WFE op een stille
    /// core nooit, en ook de EL2-switcher slaapt erop.
    ///
    /// Onder E2H = 1 op EL2 (Apple, de O6N, board-uefi met `vhe`) is dat
    /// register CNTHCTL_EL2, en dan alleen de stream-bits
    /// ([`merge_event_stream`]): de timertoegang van de bewoners blijft
    /// staan. Elders een gewone write, zoals altijd.
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        let hz = arch::freq();
        let stream = event_stream(hz, arch::has_ecv(), EVENT_STREAM_MAX_NS);
        if arch::is_el2_vhe() {
            arch::set_cntkctl(merge_event_stream(arch::cntkctl(), stream));
        } else {
            arch::set_cntkctl(stream);
        }
        Self {
            mode,
            hz,
            os: None,
            stats: Stats::default(),
        }
    }

    /// Maakt deze core de OS-core: vanaf de volgende idle-ronde is idle een
    /// beurt voor zijn bewoners ([`Mode::Resident`]). Alleen op de core van
    /// de kern zelf, en één keer.
    pub fn host(&mut self, os: OsCore) {
        self.os = Some(os);
    }

    /// Welke modus deze ronde geldt: de OS-core geeft zijn idle aan zijn
    /// bewoners.
    fn round_mode(&self) -> Mode {
        if self.os.is_some() {
            Mode::Resident
        } else {
            self.mode
        }
    }

    /// De slaap zelf als niemand de core krijgt: de basismodus. `Resident`
    /// als basis is WFE.
    fn base_mode(&self) -> Mode {
        match self.mode {
            Mode::Wfi => Mode::Wfi,
            Mode::Wfe | Mode::Resident => Mode::Wfe,
        }
    }

    fn account(&self, slept: u64) {
        self.stats.idle_ticks.fetch_add(slept, Relaxed);
        self.stats.wakes.fetch_add(1, Relaxed);
    }
}

impl Sleeper for ArmSleeper {
    /// Maskeert, toetst `ready()`, slaapt, en zet de maskers terug zoals ze
    /// stonden.
    ///
    /// Terugzetten, niet blind openen: de IRQ-vector keert terug met I
    /// gemaskeerd zolang de dispatcher zijn lijn nog niet claimde (zie
    /// [`crate::irq`]); die opent I zelf weer.
    ///
    /// WFI wekt op een pending interrupt óók met I gemaskeerd; WFE niet (hij
    /// wekt alleen op een ongemaskeerde). Daarom opent de WFE-modus de
    /// maskers weer vóór de WFE, en is hij toch vrij van de verloren wek:
    /// een interrupt tussen de toets en de WFE eindigt in een exception
    /// return, en die zet het event-register, net als de SEV van
    /// `dev::notify` bij elke `Signal::set`. De eerstvolgende WFE valt er
    /// dan meteen doorheen.
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        let daif = arch::mask();
        if ready() {
            arch::restore(daif);
            self.stats.caught.fetch_add(1, Relaxed);
            return;
        }
        let slept = match self.round_mode() {
            Mode::Resident => self.resident(now, until, ready, daif),
            mode => self.nap(mode, now, until, ready, daif),
        };
        self.account(slept);
    }
}

impl ArmSleeper {
    /// Eén slaap in `mode`, met de maskers `daif` terug zoals ze stonden.
    fn nap(
        &self,
        mode: Mode,
        now: u64,
        until: Option<u64>,
        ready: &dyn Fn() -> bool,
        daif: u64,
    ) -> u64 {
        match mode {
            Mode::Wfe | Mode::Resident => {
                // WFE wekt alleen op een ongemaskeerde interrupt: open. Een
                // interrupt tussen de toets en de WFE eindigt in een
                // exception return, en die zet het event-register.
                arch::restore(daif);
                let cap = now.saturating_add(WFI_CAP_NS);
                let u = until.map_or(cap, |u| u.min(cap));
                wfe_until(
                    deadline_ticks(now, u, self.hz),
                    ready,
                    &mut arch::wfe,
                    &arch::counter,
                )
            }
            Mode::Wfi => {
                // Nog één toets, nu de kick scherp staat (`resident` zette
                // `listen` net): een app die vóór `listen` publiceerde en
                // kickte, zag nog geen bel en stuurde geen IPI. Die viel tot
                // 30-09 op de failsafe van 1 ms; die wekt nu niet meer.
                if ready() {
                    arch::restore(daif);
                    return 0;
                }
                let cap = now.saturating_add(WFI_CAP_NS);
                let u = until.map_or(cap, |u| u.min(cap));
                let slept = arch::wfi_until(deadline_ticks(now, u, self.hz));
                arch::restore(daif);
                slept
            }
        }
    }

    /// De idle van de OS-core: een beurt voor de volgende bewoner, tot
    /// hooguit de deadline van de executor (en de vangrail van
    /// [`TURN_CAP_NS`]). Is niemand aan de beurt, dan de basisslaap tot de
    /// deadline of de vroegste wektijd van een bewoner, wat eerst komt; in
    /// WFI met de kick scherp, want een SEV wekt WFI niet.
    ///
    /// Een beurt telt niet als slaap van de kern: de tijd staat in de
    /// meetlat van de OS-core (`el2::OS_STATS`).
    fn resident(
        &mut self,
        now: u64,
        until: Option<u64>,
        ready: &dyn Fn() -> bool,
        daif: u64,
    ) -> u64 {
        let cap = now.saturating_add(TURN_CAP_NS);
        let deadline = deadline_ticks(now, until.map_or(cap, |u| u.min(cap)), self.hz);
        let turn = match self.os.as_mut() {
            Some(os) => os.run(deadline),
            None => Turn::Idle { wake: None },
        };
        let wake = match turn {
            Turn::Ran(_) => {
                arch::restore(daif);
                return 0;
            }
            Turn::Idle { wake } => wake,
        };
        let until = match (until, wake) {
            (u, None) => u,
            (u, Some(t)) => {
                let w = now.saturating_add(ticks_to_ns(t.saturating_sub(arch::counter()), self.hz));
                Some(u.map_or(w, |u| u.min(w)))
            }
        };
        let base = self.base_mode();
        let listen = base == Mode::Wfi;
        if listen && let Some(os) = &self.os {
            os.listen(true);
        }
        let slept = self.nap(base, now, until, ready, daif);
        if listen && let Some(os) = &self.os {
            os.listen(false);
        }
        slept
    }
}

/// De counterstand die bij tijdstip `until` (ns) hoort, gerekend vanaf nu.
fn deadline_ticks(now_ns: u64, until_ns: u64, hz: u64) -> u64 {
    arch::counter().saturating_add(ns_to_ticks(until_ns.saturating_sub(now_ns), hz))
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    //! De instructies. Elk blok is één of twee systeeminstructies zonder
    //! geheugeneffect buiten wat er staat.
    use core::arch::asm;

    #[inline]
    pub(super) fn counter() -> u64 {
        let v: u64;
        // SAFETY: CNTPCT_EL0 lezen heeft geen neveneffect; de drop naar EL1
        // zette CNTHCTL.EL1PCTEN, dus hij trapt niet.
        unsafe { asm!("isb", "mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack)) };
        v
    }

    #[inline]
    pub(super) fn freq() -> u64 {
        let v: u64;
        // SAFETY: CNTFRQ_EL0 is een alleen-lezen-register voor EL1.
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
        v
    }

    pub(super) fn has_ecv() -> bool {
        let v: u64;
        // SAFETY: een ID-register lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v, options(nomem, nostack)) };
        (v >> 60) & 0xF != 0
    }

    pub(super) fn set_cntkctl(v: u64) {
        // SAFETY: CNTKCTL_EL1 regelt alleen EL0-toegang en de event-stream
        // van deze core; de ISB maakt de stream meteen actief. Onder E2H = 1
        // op EL2 is het CNTHCTL_EL2, en dan geeft de aanroeper een waarde
        // die de rest van dat register behoudt.
        unsafe { asm!("msr cntkctl_el1, {}", "isb", in(reg) v, options(nostack)) };
    }

    /// CNTKCTL_EL1 zoals hij nu staat (onder E2H = 1 op EL2: CNTHCTL_EL2).
    pub(super) fn cntkctl() -> u64 {
        let v: u64;
        // SAFETY: een lees zonder neveneffect.
        unsafe { asm!("mrs {}, cntkctl_el1", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// Draait deze core op EL2 met HCR_EL2.E2H = 1? HCR_EL2 wordt alleen op
    /// EL2 gelezen: op EL1 (een app) is dat een trap.
    pub(super) fn is_el2_vhe() -> bool {
        let el: u64;
        // SAFETY: CurrentEL lezen heeft geen neveneffect en mag op EL1.
        unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
        if (el >> 2) & 3 != 2 {
            return false;
        }
        let hcr: u64;
        // SAFETY: we staan op EL2 (net getoetst); HCR_EL2 lezen heeft geen
        // neveneffect.
        unsafe { asm!("mrs {}, hcr_el2", out(reg) hcr, options(nomem, nostack)) };
        hcr & (1 << 34) != 0
    }

    /// Maskeert I en F en geeft DAIF zoals het stond.
    #[inline]
    pub(super) fn mask() -> u64 {
        let v: u64;
        // SAFETY: DAIF lezen en maskeren raakt alleen PSTATE van deze core.
        // Geen `nomem`: de compiler mag geen geheugentoegang over het
        // maskeren heen schuiven.
        unsafe { asm!("mrs {}, daif", "msr daifset, #3", out(reg) v, options(nostack)) };
        v
    }

    #[inline]
    pub(super) fn restore(daif: u64) {
        // SAFETY: zet de maskers terug die `mask` las; niets anders.
        unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
    }

    /// Eén WFE, met de counterstand eromheen: de écht in WFE doorgebrachte
    /// tijd in ticks.
    pub(super) fn wfe() -> u64 {
        let (a, b): (u64, u64);
        // SAFETY: WFE wacht op een event en heeft geen ander effect.
        unsafe {
            asm!(
                "mrs {a}, cntpct_el0",
                "wfe",
                "mrs {b}, cntpct_el0",
                a = out(reg) a,
                b = out(reg) b,
                options(nostack),
            );
        }
        b.wrapping_sub(a)
    }

    pub(super) fn timer_off() {
        // SAFETY: CNTP_CTL_EL0 = 0 zet de eigen fysieke timer van deze core
        // uit; geen geheugeneffect.
        unsafe { asm!("msr cntp_ctl_el0, xzr", "isb", options(nomem, nostack)) };
    }

    pub(super) fn hyp_timer_off() {
        // SAFETY: CNTHP_CTL_EL2 = 0 zet de EL2-timer van deze core uit; geen
        // geheugeneffect.
        unsafe { asm!("msr cnthp_ctl_el2, xzr", "isb", options(nomem, nostack)) };
    }

    /// Zet de fysieke EL1-timer op `deadline`, doet één WFI en zet de timer
    /// weer uit (anders blijft hij pending). De interrupt wordt niet genomen
    /// (DAIF dicht) maar wekt WFI wél: dat belooft de architectuur voor
    /// WFI-wake-up events (board/apple wfiTimer, 02-09).
    pub(super) fn wfi_until(deadline: u64) -> u64 {
        let (a, b): (u64, u64);
        // SAFETY: alleen de eigen fysieke timer van deze core en een WFI;
        // de timer staat na afloop weer uit.
        unsafe {
            asm!(
                "mrs {a}, cntpct_el0",
                "msr cntp_cval_el0, {d}",
                "msr cntp_ctl_el0, {one}",
                "isb",
                "wfi",
                "msr cntp_ctl_el0, xzr",
                "isb",
                "mrs {b}, cntpct_el0",
                a = out(reg) a,
                b = out(reg) b,
                d = in(reg) deadline,
                one = in(reg) 1u64,
                options(nostack),
            );
        }
        b.wrapping_sub(a)
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
mod arch {
    //! Host-stubs: geen teller, geen slaap. De rekenkunde hierboven is wat
    //! de host-tests bewijzen; de slaap zelf bewijst het board. Op riscv64
    //! zijn de teller en de timebase wel echt (de TIME-CSR en de hz van het
    //! board, `cpu::riscv::idle`): de kooi stempelt er `at_ns` mee en
    //! rekent er `CTRL_IDLE` mee om, en met de stub (0 en 62,5 MHz) zag Hop
    //! op de LicheeRV nooit een cpu-procent en stond de meetlat 2,5 keer te
    //! laag (03-10).
    pub(super) fn counter() -> u64 {
        #[cfg(all(target_os = "none", target_arch = "riscv64"))]
        {
            crate::riscv::idle::counter()
        }
        #[cfg(not(all(target_os = "none", target_arch = "riscv64")))]
        {
            0
        }
    }
    pub(super) fn freq() -> u64 {
        #[cfg(all(target_os = "none", target_arch = "riscv64"))]
        {
            crate::riscv::idle::hz()
        }
        #[cfg(not(all(target_os = "none", target_arch = "riscv64")))]
        {
            62_500_000
        }
    }
    pub(super) fn has_ecv() -> bool {
        false
    }
    pub(super) fn set_cntkctl(_v: u64) {}
    pub(super) fn cntkctl() -> u64 {
        0
    }
    pub(super) fn is_el2_vhe() -> bool {
        false
    }
    pub(super) fn mask() -> u64 {
        0
    }
    pub(super) fn restore(_daif: u64) {}
    pub(super) fn wfe() -> u64 {
        0
    }
    pub(super) fn wfi_until(_deadline: u64) -> u64 {
        0
    }
    pub(super) fn timer_off() {}
    pub(super) fn hyp_timer_off() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evnti(v: u64) -> u64 {
        (v >> 4) & 0xF
    }

    #[test]
    fn event_stream_per_board() {
        // Pi (54 MHz) en QEMU (62,5 MHz): bit 15, geen EVNTIS.
        let pi = event_stream(54_000_000, false, EVENT_STREAM_MAX_NS);
        assert_eq!((evnti(pi), pi & (1 << 17)), (15, 0));
        assert_eq!(
            evnti(event_stream(62_500_000, true, EVENT_STREAM_MAX_NS)),
            15
        );
        // Altra (25 MHz): bit 14.
        assert_eq!(
            evnti(event_stream(25_000_000, false, EVENT_STREAM_MAX_NS)),
            14
        );
        // M4 (1 GHz, FEAT_ECV): EVNTI 11 met EVNTIS = 2^20 ticks = 1,048 ms.
        let m4 = event_stream(1_000_000_000, true, EVENT_STREAM_MAX_NS);
        assert_eq!(evnti(m4), 11);
        assert_ne!(m4 & (1 << 17), 0);
        assert_ne!(m4 & (1 << 2), 0);
        // Zonder ECV blijft de M4 op het plafond steken: bit 15, 65 µs.
        assert_eq!(
            evnti(event_stream(1_000_000_000, false, EVENT_STREAM_MAX_NS)),
            15
        );
    }

    #[test]
    fn under_vhe_only_the_stream_bits_change() {
        let m4 = event_stream(1_000_000_000, true, EVENT_STREAM_MAX_NS);
        // CNTHCTL_EL2 zoals board/apple hem bij de ingang zet: EL0PCTEN,
        // EL0VCTEN, EL1PCTEN, EL1PTEN (0, 1, 10, 11), plus een oude stream.
        let old = 0x3 | 0xc00 | (1 << 2) | (15 << 4);
        let v = merge_event_stream(old, m4);
        assert_eq!(v & 0xc03, 0xc03, "de timertoegang van de bewoners blijft");
        assert_eq!(v & EVENT_STREAM_BITS, m4);
        // EL0VTEN/EL0PTEN (8, 9) en ECV-bits erboven blijven ook.
        let old = (1 << 8) | (1 << 9) | (1 << 12);
        assert_eq!(merge_event_stream(old, m4) & !EVENT_STREAM_BITS, old);
        // Een waarde van `event_stream` valt altijd binnen het masker.
        for hz in [25_000_000, 54_000_000, 62_500_000, 1_000_000_000] {
            let s = event_stream(hz, true, EVENT_STREAM_MAX_NS);
            assert_eq!(s & !EVENT_STREAM_BITS, 0, "{hz} Hz");
        }
    }

    /// Een event-stream-tik zonder werk is geen ronde van de executor: de
    /// slaap WFE't door tot de deadline, of tot er werk ligt.
    #[test]
    fn a_wake_without_work_sleeps_on() {
        use core::cell::Cell;
        // De Pi: 54 MHz, een event-stream-tik elke 65.536 ticks (1,2 ms).
        let now = Cell::new(0u64);
        let wfes = Cell::new(0u32);
        let mut wfe = || {
            wfes.set(wfes.get() + 1);
            now.set(now.get() + 65_536);
            65_536
        };
        let counter = || now.get();
        // 10 ms (540.000 ticks) zonder werk: negen WFE's, één terugkeer.
        let slept = wfe_until(540_000, &|| false, &mut wfe, &counter);
        assert_eq!(wfes.get(), 9);
        assert_eq!(slept, 9 * 65_536);
        // Werk na de derde wek: daar houdt hij op.
        now.set(0);
        wfes.set(0);
        let ready = || wfes.get() >= 3;
        wfe_until(540_000, &ready, &mut wfe, &counter);
        assert_eq!(wfes.get(), 3);
        // Werk vooraf of een verstreken deadline: geen WFE.
        wfes.set(0);
        assert_eq!(wfe_until(540_000, &|| true, &mut wfe, &counter), 0);
        assert_eq!(wfe_until(0, &|| false, &mut wfe, &counter), 0);
        assert_eq!(wfes.get(), 0);
    }

    #[test]
    fn whole_nanosecond_ticks_take_the_short_way_to_the_same_number() {
        for hz in [
            1_000_000_000,
            62_500_000,
            25_000_000,
            10_000_000,
            54_000_000,
            24_000_000,
        ] {
            for t in [0u64, 1, 7, 999_999, 123_456_789_012, 1 << 40] {
                let want = u64::try_from(u128::from(t) * 1_000_000_000 / u128::from(hz)).unwrap();
                assert_eq!(ticks_to_ns(t, hz), want, "{t} at {hz}");
                let back =
                    u64::try_from(u128::from(want) * u128::from(hz) / 1_000_000_000).unwrap();
                assert_eq!(ns_to_ticks(want, hz), back, "{want} at {hz}");
            }
        }
    }

    #[test]
    fn conversions_do_not_overflow() {
        let hz = 1_000_000_000;
        let hour = 3_600_000_000_000;
        assert_eq!(ns_to_ticks(hour, hz), hour);
        assert_eq!(ticks_to_ns(ns_to_ticks(hour, 62_500_000), 62_500_000), hour);
        assert_eq!(ticks_to_ns(62_500_000, 62_500_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(1, 62_500_000), 16);
        assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(ticks_to_ns(5, 0), 0);
    }

    #[test]
    fn ready_before_sleep_is_caught() {
        let mut s = ArmSleeper::new(Mode::Wfe);
        s.sleep(0, None, &|| true);
        assert_eq!(s.stats.caught.load(Relaxed), 1);
        assert_eq!(s.stats.wakes.load(Relaxed), 0);
        s.sleep(0, Some(10), &|| false);
        assert_eq!(s.stats.wakes.load(Relaxed), 1);
    }
}
