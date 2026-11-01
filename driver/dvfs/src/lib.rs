//! Het klokbeleid van HopOS: een OS-taak, geen Hop-taak. De orchestrator
//! is er volledig blind voor, zoals bij SMP. Vastgelegd met Derek in
//! `docs/archief/plan-p2b-soak.md` (2026-07-11), geport uit
//! `OLD/metal/driver/dvfs`:
//!
//! - het signaal is de idle-teller: idle-tijd in generic-timer-tikken. Een
//!   idle core telt ~CNTFRQ per seconde op, een drukke staat stil. Apps
//!   publiceren hem op hun control-page, de kern telt zelf (de slaap van
//!   zijn executor);
//! - de last van de node is de kern plus elke bewoner: de wachter sampelt
//!   elke ~10 ms en oordeelt over de laatste 50 ms. Mist een bewoner meer
//!   dan 30% van één core aan idle (de kern: 70%), dan de klok vol (~20 ms
//!   bij aanhoudende last); is álles ~30 s stil, dan de klok laag;
//! - de knop ([`Knob`]) alleen op de flank: op de Pi de firmware-mailbox,
//!   op de O6N het `_CPC`-fastchannel per domein; de firmware-throttle
//!   blijft het vangnet.
//!
//! Dit crate is het beleid als rekenwerk: [`Governor::step`] krijgt de
//! tellers van één sample en zegt of de knop om moet. De taak eromheen is
//! [`run`]: een lus op de slaap van de executor die de tellers leest (de
//! binary geeft ze), de flanken en elke 10 s een meetregel logt. De knop is
//! van het board. Zo is het hele beleid op de host te toetsen, ook de lus.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
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

use alloc::vec::Vec;
use core::fmt;
use core::future::Future;

/// De sample-tijd.
pub const SAMPLE_NS: u64 = 10_000_000;
/// Samples per oordeel (50 ms). Eén sample van 10 ms was het oordeel tot
/// 23-09, en dat hield de O6N voorgoed vol: een app die 99,7% idle is,
/// heeft toch losse samples van 0-60% idle (klusjes van ~1 ms die op de
/// stille klok 3,25 keer langer duren, en een teller die pas bij terugkomst
/// uit de yield bijloopt en dus in klonters binnenkomt). Over 50 ms middelt
/// dat weg; echte aanhoudende last haalt de grens na twee volle samples.
pub const WINDOW: usize = 5;
/// De hysterese omlaag.
pub const COOLDOWN_NS: u64 = 30_000_000_000;
/// Een bron is druk als hij over het venster meer dan 30% van één core aan
/// idle mist: ruim boven de jitter van de event-stream, ruim onder "half
/// werk". Per core, niet per bron (Linux' schedutil kijkt ook naar de
/// drukste CPU van een domein, niet naar het gemiddelde): een rekenaar op
/// één core van een app met vier cores mist een kwart van het geheel, en
/// telde onder de oude drempel (70% van de hele bron) nooit. Voor een bron
/// met één core is het dezelfde regel: onder 70% idle.
const SHORT_NUM: u64 = 3;
const SHORT_DEN: u64 = 10;
/// De kern (bron 0) is pas druk boven 70% van zijn core: zijn console is een
/// UART die hij pollt (115200 baud, 87 us per teken), en vijf regels
/// van 70 tekens (de meetregels van toen) zijn ~23 ms in één venster van
/// 50 ms. Onder
/// de drempel van de bewoners hield dat de klok met de cooldown van 30 s
/// voorgoed vol. Een OS-core die het net, de switch of de system-API
/// draagt, haalt 70% wel.
const KERN_SHORT_NUM: u64 = 7;
/// Een klonter van een lange slaap telt hoogstens voor vier samples.
const CLAMP: u64 = 4;

/// Eén stand van de knop, voor de logregel ("2600 MHz", "8192 perf").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// De waarde.
    pub value: u32,
    /// De eenheid.
    pub unit: &'static str,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.value, self.unit)
    }
}

/// De klok van één board, teruggebracht tot wat het beleid kent: twee
/// standen. `None` is mislukt: het beleid blijft dan staan waar het stond.
pub trait Knob {
    /// Naar het plafond.
    fn full(&mut self) -> Option<Level>;
    /// Naar de stil-stand.
    fn quiet(&mut self) -> Option<Level>;
}

impl<K: Knob + ?Sized> Knob for &mut K {
    fn full(&mut self) -> Option<Level> {
        (**self).full()
    }
    fn quiet(&mut self) -> Option<Level> {
        (**self).quiet()
    }
}

/// Een console-pin: `clock full|quiet|auto`, zodat één kern beide standen
/// kan meten zonder flip. Geen config-sleutel: een pin hoort niet in een
/// node die draait zonder dat iemand kijkt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Hold {
    /// Het beleid beslist.
    #[default]
    Auto,
    /// Vast vol.
    Full,
    /// Vast stil.
    Quiet,
}

/// Eén bron in één sample. Bron 0 is de kern; bron n is slot n.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Draait er iets (een slot met status Ready en cores > 0)?
    pub live: bool,
    /// De idle-teller (monotoon, in timer-tikken).
    pub idle: u64,
    /// Het aantal cores van de bron: het verwachte tempo schaalt mee.
    pub cores: u64,
    /// Wil de bron de core (hij rekent, of hij staat klaar: onderbroken, of
    /// zijn wektijd is voorbij)? Een teller die niet steeg, telt alleen als
    /// druk als dit waar is: een app die in zijn yield slaapt, publiceert
    /// zijn idle pas bij terugkomst, en zonder deze vraag las elke slapende
    /// app als 100% bezig en zakte de klok nooit (23-09).
    pub running: bool,
}

/// Wat de governor deed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// Naar vol (`true`) of stil.
    pub full: bool,
    /// Waarom ("boot", "busy", "idle 30s", "held").
    pub why: &'static str,
    /// Wat de knop meldde; `None` = de knop weigerde, de stand bleef.
    pub level: Option<Level>,
}

/// De laatste drukke bron, voor de console: een governor die niet zakt,
/// moet kunnen zeggen wie hem wakker houdt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy {
    /// De bron (0 = de kern, n = slot n).
    pub source: usize,
    /// Wanneer.
    pub at_ns: u64,
    /// Idle in promille van het verwachte tempo (alle cores van de bron).
    pub idle_permille: u64,
    /// De cores van de bron.
    pub cores: u64,
}

/// Het glijdende venster van één bron: idle-tikken en verwacht tempo van
/// de laatste [`WINDOW`] samples.
#[derive(Clone, Copy, Debug, Default)]
struct History {
    d: [u64; WINDOW],
    want: [u64; WINDOW],
    k: usize,
    full: bool,
}

impl History {
    /// Schuift een sample in het venster en geeft de sommen. Tot het venster
    /// vol is, telt alleen wat er is: een verse app wordt meteen beoordeeld.
    fn add(&mut self, d: u64, want: u64) -> (u64, u64) {
        if let (Some(sd), Some(sw)) = (self.d.get_mut(self.k), self.want.get_mut(self.k)) {
            *sd = d.min(want.saturating_mul(CLAMP));
            *sw = want;
        }
        self.k = (self.k + 1) % WINDOW;
        if self.k == 0 {
            self.full = true;
        }
        let n = if self.full { WINDOW } else { self.k };
        let sd = self.d.iter().take(n).sum();
        let sw = self.want.iter().take(n).sum();
        (sd, sw)
    }
}

/// Wat de governor van één bron onthoudt.
#[derive(Clone, Copy, Debug, Default)]
struct Src {
    hist: History,
    last: u64,
    seen: bool,
}

/// Het beleid over de bronnen van een sample (bron 0 is de kern-core,
/// bron n is slot n). Er is geen vast aantal: de governor onthoudt zoveel
/// bronnen als het langste sample ooit had, zoals er geen maximum aantal
/// slots is.
pub struct Governor {
    srcs: Vec<Src>,
    high: bool,
    quiet_since: u64,
    /// De console-pin.
    pub hold: Hold,
    /// De laatste drukke bron.
    pub busy: Option<Busy>,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new()
    }
}

impl Governor {
    /// Een governor die nog niets weet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            srcs: Vec::new(),
            high: false,
            quiet_since: 0,
            hold: Hold::Auto,
            busy: None,
        }
    }

    /// Staat de klok vol?
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.high
    }

    /// De eerste stap: de toestand niet aannemen maar zetten. Gemeten
    /// 2026-07-11: met een `arm_freq_min`-vloer boot de firmware op de vloer,
    /// niet op vol, en de hele P1-acceptatie draaide per ongeluk op 800 MHz.
    /// Boot-werk verdient de volle klok; daarna regeert het beleid.
    pub fn boot(&mut self, now: u64, knob: &mut impl Knob) -> Change {
        let level = knob.full();
        self.high = level.is_some();
        self.quiet_since = now;
        Change {
            full: true,
            why: "boot",
            level,
        }
    }

    /// Eén sample: `expect` is het verwachte idle-tempo per core per sample
    /// (CNTFRQ maal [`SAMPLE_NS`]), `samples[i]` de tellers van bron `i`.
    /// Geeft de flank als de knop om moest.
    pub fn step(
        &mut self,
        now: u64,
        expect: u64,
        samples: &[Sample],
        knob: &mut impl Knob,
    ) -> Option<Change> {
        let busy = self.measure(now, expect, samples);
        match self.hold {
            Hold::Auto => {}
            h => {
                // Loslaten begint niet meteen met "idle 30s".
                self.quiet_since = now;
                let want = h == Hold::Full;
                return (want != self.high).then(|| self.set(want, "held", knob));
            }
        }
        if busy {
            // Anders valt de klok één stil sample later alweer terug ("idle
            // 30s" één tel na "busy", gemeten 19-07).
            self.quiet_since = now;
            return (!self.high).then(|| self.set(true, "busy", knob));
        }
        if self.high && now.saturating_sub(self.quiet_since) > COOLDOWN_NS {
            return Some(self.set(false, "idle 30s", knob));
        }
        None
    }

    fn set(&mut self, full: bool, why: &'static str, knob: &mut impl Knob) -> Change {
        let level = if full { knob.full() } else { knob.quiet() };
        if level.is_some() {
            self.high = full;
        }
        Change { full, why, level }
    }

    /// Leest de bronnen in de vensters; `true` als er één druk is. De
    /// drukste (minste idle) wordt [`Governor::busy`].
    fn measure(&mut self, now: u64, expect: u64, samples: &[Sample]) -> bool {
        let mut worst: Option<Busy> = None;
        // Een langer sample dan ooit: plaats voor de nieuwe bronnen. Lukt
        // dat niet, dan tellen zij dit sample niet mee.
        if let Some(more) = samples.len().checked_sub(self.srcs.len())
            && more > 0
            && self.srcs.try_reserve(more).is_ok()
        {
            self.srcs.resize(samples.len(), Src::default());
        }
        for (i, s) in samples.iter().enumerate() {
            let Some(Src {
                hist: h,
                last,
                seen,
            }) = self.srcs.get_mut(i)
            else {
                continue;
            };
            if !s.live || s.cores == 0 {
                *seen = false;
                continue;
            }
            let want = expect.saturating_mul(s.cores);
            // Het eerste sample na een start ijkt alleen; daarna telt óók een
            // teller die op nul blijft (een app die vanaf seconde één 100%
            // brandt) als druk.
            if !*seen {
                *h = History::default();
            } else {
                let mut d = s.idle.wrapping_sub(*last);
                if !s.running {
                    d = d.max(want); // slaapt in zijn yield: dit sample was idle
                }
                let (sd, sw) = h.add(d, want);
                let short = sw.saturating_sub(sd);
                let one = sw / s.cores; // het venster van één core
                let num = if i == 0 { KERN_SHORT_NUM } else { SHORT_NUM };
                if short.saturating_mul(SHORT_DEN) > one.saturating_mul(num) {
                    let idle_permille = sd.saturating_mul(1000) / sw.max(1);
                    if worst.is_none_or(|b| idle_permille < b.idle_permille) {
                        worst = Some(Busy {
                            source: i,
                            at_ns: now,
                            idle_permille,
                            cores: s.cores,
                        });
                    }
                }
            }
            *seen = true;
            *last = s.idle;
        }
        if worst.is_some() {
            self.busy = worst;
        }
        worst.is_some()
    }
}

/// Om de hoeveel tijd [`run`] zijn meetregel logt.
pub const REPORT_NS: u64 = 10_000_000_000;

/// Wat [`run`] van de binary nodig heeft buiten de knop: de klok, de slaap
/// en de tellers.
pub trait Host {
    /// Monotone nanoseconden.
    fn now(&self) -> u64;
    /// Slaapt `ns` nanoseconden (de timer van de executor).
    fn sleep(&self, ns: u64) -> impl Future<Output = ()>;
    /// Het verwachte idle-tempo per core per sample (CNTFRQ maal
    /// [`SAMPLE_NS`]).
    fn expect(&self) -> u64;
    /// Vult de tellers van dit sample: `out` is leeg, en bron `i` komt op
    /// plek `i` (een gat is een `Sample` die niet `live` is).
    fn sample(&mut self, out: &mut Vec<Sample>);
    /// De temperatuur voor de meetregel, in milligraden; 0 = geen.
    fn temp_milli_c(&mut self) -> i32;
    /// Eén Engelse logregel.
    fn log(&self, args: fmt::Arguments<'_>);
}

/// De governor als taak: eerst de klok vol ("boot"), dan elke
/// [`SAMPLE_NS`] een sample en een oordeel over het venster van 50 ms, de
/// flank naar stil na [`COOLDOWN_NS`], en elke [`REPORT_NS`] één
/// meetregel (`HOPOS_CLOCK`). `hold` is de pin uit de config
/// (`hopos.clock`). Keert nooit terug.
pub async fn run(mut knob: impl Knob, hold: Hold, host: &mut impl Host) {
    let mut g = Governor::new();
    g.hold = hold;
    let c = g.boot(host.now(), &mut knob);
    report_change(host, &c, None);
    run_governor(g, knob, host, c.level).await;
}

/// Als [`run`], maar de boot-flank is al gezet: de aanroeper zette de klok
/// vol met `Knob::full` vóór het net (de Pi's, hopos/src/telemetry.rs), en
/// `level` is wat de knop toen meldde. Zo boot een geflipte kern zijn NIC
/// op de volle klok, zoals een koude boot, ook als de vorige kern stil
/// stond (30-09: twee flips vanuit 800 MHz met een NIC die nooit meer
/// meldde).
pub async fn run_after_boot(
    knob: impl Knob,
    hold: Hold,
    host: &mut impl Host,
    level: Option<Level>,
) {
    let mut g = Governor::new();
    g.hold = hold;
    g.high = level.is_some();
    g.quiet_since = host.now();
    run_governor(g, knob, host, level).await;
}

/// De lus van de governor na de boot-flank.
async fn run_governor(
    mut g: Governor,
    mut knob: impl Knob,
    host: &mut impl Host,
    level: Option<Level>,
) {
    let mut samples = Vec::new();
    let mut last_report = host.now();
    let mut level = level;
    loop {
        host.sleep(SAMPLE_NS).await;
        let now = host.now();
        samples.clear();
        host.sample(&mut samples);
        if let Some(c) = g.step(now, host.expect(), &samples, &mut knob) {
            report_change(host, &c, g.busy);
            if c.level.is_some() {
                level = c.level;
            }
        }
        if now.saturating_sub(last_report) >= REPORT_NS {
            last_report = now;
            let t = host.temp_milli_c();
            host.log(format_args!(
                "dvfs: clock {} ({}), temp {}.{} C, busy {} HOPOS_CLOCK",
                level.map_or(
                    Level {
                        value: 0,
                        unit: "?"
                    },
                    |l| l
                ),
                if g.is_full() { "full" } else { "quiet" },
                t / 1000,
                (t % 1000).abs() / 100,
                BusyText { last: g.busy, now },
            ));
        }
    }
}

/// De flank, met de drukke bron erbij als hij de reden is
/// (`(full, busy: slot 3 (0 permille idle))`).
fn report_change(host: &impl Host, c: &Change, busy: Option<Busy>) {
    let by = busy.filter(|_| c.why == "busy");
    match c.level {
        Some(l) => host.log(format_args!(
            "dvfs: -> {l} ({}, {}{}{}) HOPOS_CLOCK_EDGE",
            if c.full { "full" } else { "quiet" },
            c.why,
            if by.is_some() { ": " } else { "" },
            Who(by)
        )),
        None => host.log(format_args!(
            "dvfs: clock change to {} ({}) failed, the policy keeps its state",
            if c.full { "full" } else { "quiet" },
            c.why
        )),
    }
}

/// Een drukke bron als tekst: `kern (40 permille idle)`, `slot 3 (0
/// permille idle)`, `slot 4 (750 permille idle over 4 cores)`; leeg zonder.
struct Who(Option<Busy>);

impl fmt::Display for Who {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(b) = self.0 else {
            return Ok(());
        };
        match b.source {
            0 => f.write_str("kern")?,
            n => write!(f, "slot {n}")?,
        }
        write!(f, " ({} permille idle", b.idle_permille)?;
        if b.cores > 1 {
            write!(f, " over {} cores", b.cores)?;
        }
        f.write_str(")")
    }
}

/// Het `busy`-veld van de meetregel: de drukke bron van de laatste
/// [`REPORT_NS`], anders `none`, met de laatste drukke bron erachter als er
/// een was (een volle klok zonder drukke bron koelt af: wie tilde hem?).
struct BusyText {
    last: Option<Busy>,
    now: u64,
}

impl fmt::Display for BusyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.last {
            Some(b) if self.now.saturating_sub(b.at_ns) < REPORT_NS => Who(Some(b)).fmt(f),
            Some(b) => {
                let ago = self.now.saturating_sub(b.at_ns) / 1_000_000_000;
                match b.source {
                    0 => write!(f, "none (last kern, {ago} s ago)"),
                    n => write!(f, "none (last slot {n}, {ago} s ago)"),
                }
            }
            None => f.write_str("none"),
        }
    }
}

/// De pin uit `hopos.clock`: `dvfs` (of niets) is het beleid, `max` vast
/// vol, `quiet` vast stil, `firmware` geen governor (de boot-OPP blijft).
/// `None` = firmware; een onbekende waarde is het beleid (de aanroeper
/// meldt hem).
#[must_use]
pub fn hold_of(v: &str) -> (Option<Hold>, bool) {
    match v {
        "" | "dvfs" | "auto" => (Some(Hold::Auto), true),
        "max" | "full" => (Some(Hold::Full), true),
        "quiet" | "min" => (Some(Hold::Quiet), true),
        "firmware" | "off" => (None, true),
        _ => (Some(Hold::Auto), false),
    }
}

#[cfg(test)]
mod tests;
