//! De testdispatcher: welke tests er zijn, één tegelijk, en de tabel met
//! de uitkomsten.
//!
//! Eén test tegelijk, want twee benchmarks door elkaar meten allebei niets.
//! De claim is een veld in [`BOARD`] dat de handler van `/api/run` zet en de
//! test-taak wist; alle taken die hem raken draaien op de executor van core
//! 0, dus een `Local` is genoeg (handboek §1.1, de leesbare tabel: elke
//! lening is één blok zonder `.await`). De test zelf is een eigen taak
//! ([`start`]); een tweede start krijgt [`StartError::Busy`], de 409.
//!
//! Dit module bezit [`BOARD`]: de lopende test, de voortgangsregel, de
//! rapporten en de lopende uploadreeks van `/sink`.

use crate::report::{Report, json_num};
use crate::{ARCH, Shared, VERSION, cpu, disk, idle, mem, net};
use alloc::string::String;
use alloc::vec::Vec;
use applib::heap::HEAP;
use applib::text::{Room, json_str};
use applib::{clock, log, smp};
use bounded::Text;
use core::cell::RefCell;
use core::fmt::{self, Write};
use sync::Local;

/// De tests. De volgorde van [`RUN_ALL`] is die van de pagina en van
/// `test=all`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Test {
    /// Het idle-venster van nu (passief; de sampler loopt altijd).
    Idle,
    /// LCG-stappen per seconde op één core.
    Cpu,
    /// Serieel tegen parallel over alle cores, met de gedeelde heap.
    Smp,
    /// Volgehouden last op alle cores, met de temperatuur.
    Burn,
    /// STREAM-achtig: copy en triad.
    MemBw,
    /// Pointer-chase over oplopende working-sets.
    MemLat,
    /// Het allocatietempo van de heap (de plaats van Go's gc).
    Alloc,
    /// Download van een externe bron.
    Rx,
    /// Schrijven en lezen door de system-API.
    Disk,
    /// Korte verbindingen naar de eigen poort.
    Storm,
    /// TCP-handshakes naar de gateway.
    Rtt,
    /// Overslaap bij 1, 5 en 20 ms.
    Timer,
    /// De zendkant, gedreven door een client op `/blob`.
    Tx,
    /// De ontvangkant, gedreven door een client op `/sink`.
    Up,
}

/// Het aantal plaatsen in de tabel.
const COUNT: usize = 14;

/// Alle tests, in de volgorde van de tabel.
pub(crate) const TESTS: [Test; COUNT] = [
    Test::Idle,
    Test::Cpu,
    Test::Smp,
    Test::Burn,
    Test::MemBw,
    Test::MemLat,
    Test::Alloc,
    Test::Rx,
    Test::Disk,
    Test::Storm,
    Test::Rtt,
    Test::Timer,
    Test::Tx,
    Test::Up,
];

/// Wat `test=all` draait, in deze volgorde. Tx en up zijn client-gedreven
/// en staan er niet in.
pub(crate) const RUN_ALL: [Test; 12] = [
    Test::Idle,
    Test::Cpu,
    Test::Smp,
    Test::Burn,
    Test::MemBw,
    Test::MemLat,
    Test::Alloc,
    Test::Rx,
    Test::Disk,
    Test::Storm,
    Test::Rtt,
    Test::Timer,
];

impl Test {
    /// De naam, in de API en de JSON.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Test::Idle => "idle",
            Test::Cpu => "cpu",
            Test::Smp => "smp",
            Test::Burn => "burn",
            Test::MemBw => "membw",
            Test::MemLat => "memlat",
            Test::Alloc => "alloc",
            Test::Rx => "rx",
            Test::Disk => "disk",
            Test::Storm => "storm",
            Test::Rtt => "rtt",
            Test::Timer => "timer",
            Test::Tx => "tx",
            Test::Up => "up",
        }
    }

    /// De marker op de console.
    pub(crate) const fn marker(self) -> &'static str {
        match self {
            Test::Idle => "HOPOS_VITALS_IDLE",
            Test::Cpu => "HOPOS_VITALS_CPU",
            Test::Smp => "HOPOS_VITALS_SMP",
            Test::Burn => "HOPOS_VITALS_BURN",
            Test::MemBw => "HOPOS_VITALS_MEMBW",
            Test::MemLat => "HOPOS_VITALS_MEMLAT",
            Test::Alloc => "HOPOS_VITALS_ALLOC",
            Test::Rx => "HOPOS_VITALS_RX",
            Test::Disk => "HOPOS_VITALS_DISK",
            Test::Storm => "HOPOS_VITALS_STORM",
            Test::Rtt => "HOPOS_VITALS_RTT",
            Test::Timer => "HOPOS_VITALS_TIMER",
            Test::Tx => "HOPOS_VITALS_TX",
            Test::Up => "HOPOS_VITALS_UPLOAD",
        }
    }

    /// Eén regel uitleg, voor de pagina.
    pub(crate) const fn desc(self) -> &'static str {
        match self {
            Test::Idle => "idle window (passive, always sampling)",
            Test::Cpu => "single-core throughput",
            Test::Smp => "multi-core scaling + shared heap",
            Test::Burn => "sustained load + temperature",
            Test::MemBw => "memory bandwidth",
            Test::MemLat => "memory latency",
            Test::Alloc => "heap allocation rate",
            Test::Rx => "download throughput",
            Test::Disk => "storage through the system-call path",
            Test::Storm => "connection storm",
            Test::Rtt => "TCP handshakes to the gateway",
            Test::Timer => "sleep jitter",
            Test::Tx => "client-driven: curl /blob?mb=64",
            Test::Up => "client-driven: PUT /sink (perf.sh)",
        }
    }

    /// De plaats in de tabel.
    const fn index(self) -> usize {
        self as usize
    }

    /// De test die `/api/run` onder `name` kent (tx en up niet: die drijft
    /// een client).
    pub(crate) fn runnable(name: &str) -> Option<Test> {
        RUN_ALL.into_iter().find(|t| t.name() == name)
    }
}

/// De query van `/api/run`, één keer gelezen. Elke test kiest zijn eigen
/// default en grenzen ([`Params::int`]); dezelfde params gelden in `all`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Params {
    /// `secs`: de duur van cpu, burn en alloc.
    pub(crate) secs: Option<i64>,
    /// `mb`: de maat van rx en disk.
    pub(crate) mb: Option<i64>,
    /// `kb`: de chunk van disk.
    pub(crate) kb: Option<i64>,
    /// `n`: verbindingen (rx), werkers (storm), handshakes (rtt).
    pub(crate) n: Option<i64>,
    /// `reqs`: het aantal verzoeken van storm.
    pub(crate) reqs: Option<i64>,
    /// `url`: de bron van rx.
    pub(crate) url: Option<String>,
    /// `path`: het bestand van disk.
    pub(crate) path: Option<String>,
    /// `addr`: het doel van storm en rtt, `ip:poort`.
    pub(crate) addr: Option<String>,
    /// `hole=1`: disk leest gaten in plaats van geschreven data.
    pub(crate) hole: bool,
    /// `rand`: disk doet zoveel willekeurige 4 KiB-lezingen (en houdt het
    /// bestand voor de volgende run).
    pub(crate) rand: Option<i64>,
    /// `depth`: zoveel lezingen van rand per call (een bundel,
    /// `OP_READ_MANY`); 1 is de gewone lees.
    pub(crate) depth: Option<i64>,
    /// `bundles`: zoveel bundels van rand tegelijk in de lucht, elk over
    /// een eigen verbinding.
    pub(crate) bundles: Option<i64>,
}

impl Params {
    /// Leest de params via `get` (de query van het verzoek).
    pub(crate) fn parse(get: impl Fn(&str) -> Option<String>) -> Self {
        let int = |k: &str| get(k).and_then(|v| v.trim().parse::<i64>().ok());
        let text = |k: &str| get(k).filter(|v| !v.is_empty());
        Self {
            secs: int("secs"),
            mb: int("mb"),
            kb: int("kb"),
            n: int("n"),
            reqs: int("reqs"),
            url: text("url"),
            path: text("path"),
            addr: text("addr"),
            hole: get("hole").as_deref() == Some("1"),
            rand: int("rand"),
            depth: int("depth"),
            bundles: int("bundles"),
        }
    }

    /// Een getal: `v` geklemd op `min..=max`, of `def` zonder, zoals
    /// Go's `qInt`.
    pub(crate) fn int(v: Option<i64>, def: u64, min: u64, max: u64) -> u64 {
        match v {
            None => def,
            Some(x) if x < 0 => min,
            Some(x) => u64::try_from(x).unwrap_or(max).clamp(min, max),
        }
    }
}

/// De uploadreeks van `/sink`: requests die elkaar binnen [`BURST_GAP_NS`]
/// opvolgen, horen bij één meting (leanhttp begrenst een body op 1 MiB, dus
/// een upload van betekenis is een reeks PUTs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Burst {
    /// De start van de eerste request, ns sinds de start van de app.
    pub(crate) start_ns: u64,
    /// Het einde van de laatste.
    pub(crate) end_ns: u64,
    /// Bytes in de reeks.
    pub(crate) bytes: u64,
    /// Requests in de reeks.
    pub(crate) reqs: u64,
}

/// Een gat van meer dan twee seconden begint een nieuwe reeks.
pub(crate) const BURST_GAP_NS: u64 = 2_000_000_000;

impl Burst {
    /// Telt een request van `bytes` van `t0` tot `t1` erbij; begint een
    /// nieuwe reeks na een gat.
    pub(crate) fn add(&mut self, t0: u64, t1: u64, bytes: u64) {
        if self.reqs == 0 || t0.saturating_sub(self.end_ns) > BURST_GAP_NS {
            *self = Burst {
                start_ns: t0,
                ..Burst::default()
            };
        }
        self.end_ns = t1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.reqs += 1;
    }
}

/// De tabel van de app.
pub(crate) struct Board {
    /// De lopende test (`all` of een naam).
    running: Option<&'static str>,
    /// Eén regel voortgang.
    note: Text<96>,
    /// Per test het laatste rapport.
    results: [Option<Report>; COUNT],
    /// De lopende uploadreeks.
    pub(crate) up: Burst,
}

/// De tabel: geschreven door de test-taak en de handlers van `/blob` en
/// `/sink`, gelezen door `/api/state`. Alles op core 0.
pub(crate) static BOARD: Local<RefCell<Board>> = Local::new(RefCell::new(Board {
    running: None,
    note: Text::new(),
    results: [const { None }; COUNT],
    up: Burst {
        start_ns: 0,
        end_ns: 0,
        bytes: 0,
        reqs: 0,
    },
}));

/// Zet de voortgangsregel.
pub(crate) fn note(args: fmt::Arguments<'_>) {
    if let Ok(mut b) = BOARD.get().try_borrow_mut() {
        b.note = Text::new();
        let _ = b.note.write_fmt(args);
    }
}

/// Zet `r` in de tabel en zegt het op de console.
pub(crate) fn store(test: Test, r: Report) {
    announce(test, &r);
    keep(test, r);
}

/// Zet `r` in de tabel zonder regel op de console (de tussenstanden van
/// een uploadreeks).
pub(crate) fn keep(test: Test, r: Report) {
    if let Ok(mut b) = BOARD.get().try_borrow_mut()
        && let Some(slot) = b.results.get_mut(test.index())
    {
        *slot = Some(r);
    }
}

/// De regel op de console: klaar met de marker en de meetwaarden, of de
/// fout, of de reden van het overslaan.
fn announce(test: Test, r: &Report) {
    let name = test.name();
    let ms = r.duration_ns / 1_000_000;
    if let Some(e) = r.error() {
        log!("vitals: {name} failed after {ms} ms: {e} HOPOS_VITALS_FAIL test={name}");
    } else if let Some(why) = r.skipped() {
        log!("vitals: {name} skipped: {why} HOPOS_VITALS_SKIP test={name}");
    } else {
        log!(
            "vitals: {name} done in {ms} ms {}{}",
            test.marker(),
            r.kv().as_str()
        );
    }
}

/// Waarom een test niet startte.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartError {
    /// Geen test met die naam.
    Unknown,
    /// Er loopt er al een (de 409).
    Busy(&'static str),
    /// De executor nam de taak niet.
    Spawn,
}

/// Wat er draait: één test of allemaal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Plan {
    One(Test),
    All,
}

/// Start test `name` (of `all`) als eigen taak; de naam die loopt.
pub(crate) fn start(
    sh: &'static Shared,
    name: &str,
    p: Params,
) -> Result<&'static str, StartError> {
    let (plan, label) = match name {
        "all" => (Plan::All, "all"),
        _ => {
            let t = Test::runnable(name).ok_or(StartError::Unknown)?;
            (Plan::One(t), t.name())
        }
    };
    {
        let mut b = BOARD
            .get()
            .try_borrow_mut()
            .map_err(|_| StartError::Spawn)?;
        if let Some(running) = b.running {
            return Err(StartError::Busy(running));
        }
        b.running = Some(label);
        b.note = Text::new();
        let _ = b.note.write_str("starting");
    }
    if sh.exec.spawn(runner(sh, plan, p)).is_err() {
        release();
        log!("vitals: no room for the test task HOPOS_VITALS_SPAWN");
        return Err(StartError::Spawn);
    }
    Ok(label)
}

/// Geeft de claim terug.
fn release() {
    if let Ok(mut b) = BOARD.get().try_borrow_mut() {
        b.running = None;
        b.note = Text::new();
    }
}

/// De test-taak: één test of de hele rij, elk rapport in de tabel, en dan
/// de claim terug.
async fn runner(sh: &'static Shared, plan: Plan, p: Params) {
    match plan {
        Plan::One(t) => {
            note(format_args!("running {}", t.name()));
            let r = run_one(sh, t, &p).await;
            store(t, r);
        }
        Plan::All => {
            let t0 = clock::now_ns();
            let (mut failed, mut skipped) = (0u32, 0u32);
            for t in RUN_ALL {
                note(format_args!("running {}", t.name()));
                let r = run_one(sh, t, &p).await;
                failed += u32::from(r.error().is_some());
                skipped += u32::from(r.skipped().is_some());
                store(t, r);
            }
            let s = clock::now_ns().saturating_sub(t0) / 1_000_000_000;
            log!(
                "vitals: all {} tests done in {s} s HOPOS_VITALS_ALL failed={failed} skipped={skipped}",
                RUN_ALL.len()
            );
        }
    }
    release();
}

/// Draait één test en geeft zijn rapport.
async fn run_one(sh: &'static Shared, t: Test, p: &Params) -> Report {
    let t0 = clock::now_ns();
    let mut r = Report::new(t.name(), sh.uptime_ns());
    log!("vitals: test {} started", t.name());
    match t {
        Test::Idle => idle_report(&mut r),
        Test::Cpu => cpu::cpu(&mut r, p).await,
        Test::Smp => cpu::smp(sh, &mut r).await,
        Test::Burn => cpu::burn(sh, &mut r, p).await,
        Test::MemBw => mem::bandwidth(sh, &mut r).await,
        Test::MemLat => mem::latency(sh, &mut r).await,
        Test::Alloc => mem::alloc_rate(&mut r, p).await,
        Test::Rx => net::rx(sh, &mut r, p).await,
        Test::Disk => disk::disk(sh, &mut r, p).await,
        Test::Storm => net::storm(sh, &mut r, p).await,
        Test::Rtt => net::rtt(sh, &mut r, p).await,
        Test::Timer => mem::timer(&mut r).await,
        Test::Tx | Test::Up => r.skip("client-driven, see the README"),
    }
    r.duration_ns = clock::now_ns().saturating_sub(t0);
    r
}

/// Het idle-venster van nu als rapport.
fn idle_report(r: &mut Report) {
    let w = idle::now();
    if !w.ok {
        r.skip("no idle window yet (the sampler needs a few seconds)");
        return;
    }
    r.add("span", w.span_s, "s");
    r.add("wakes_per_s", w.wakes_per_s, "1/s");
    if let Some(p) = w.idle_pct {
        r.add("idle_pct", p, "%");
    }
    if let Some(c) = w.wake_cost_us {
        r.add("wake_cost", c, "us");
    }
    if let Some(n) = w.note {
        r.line(format_args!("{n}"));
    }
}

/// De grens van de state-JSON. De rapporten zijn samen hooguit
/// [`COUNT`] maal [`crate::report::LINES_CAP`] plus hun meetwaarden.
const STATE_CAP: usize = 160 << 10;

/// `GET /api/state` als bytes; `None` als hij niet past of de heap nee zei.
pub(crate) fn state_json(sh: &Shared) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(STATE_CAP).ok()?;
    let mut w = Room(&mut out);
    let b = BOARD.get().try_borrow().ok()?;
    write_state(&mut w, sh, &b).ok()?;
    drop(b);
    Some(out)
}

/// De hele staat.
fn write_state(w: &mut impl Write, sh: &Shared, b: &Board) -> fmt::Result {
    write_node(w, sh)?;
    let win = idle::now();
    write!(
        w,
        ",\"idle\":{{\"ok\":{},\"span_s\":{:.1},\"wakes_per_s\":",
        win.ok, win.span_s
    )?;
    json_num(w, win.wakes_per_s)?;
    w.write_str(",\"idle_pct\":")?;
    opt_num(w, win.idle_pct)?;
    w.write_str(",\"wake_cost_us\":")?;
    opt_num(w, win.wake_cost_us)?;
    if let Some(n) = win.note {
        w.write_str(",\"idle_note\":")?;
        json_str(w, n)?;
    }
    write!(
        w,
        "}},\"temp_milli_c\":{},\"running\":",
        sh.app.ctrl().temp_milli_c()
    )?;
    json_str(w, b.running.unwrap_or(""))?;
    w.write_str(",\"note\":")?;
    json_str(w, b.note.as_str())?;
    w.write_str(",\"tests\":[")?;
    for (i, t) in TESTS.iter().enumerate() {
        if i > 0 {
            w.write_str(",")?;
        }
        w.write_str("{\"name\":")?;
        json_str(w, t.name())?;
        w.write_str(",\"desc\":")?;
        json_str(w, t.desc())?;
        write!(w, ",\"runnable\":{}}}", Test::runnable(t.name()).is_some())?;
    }
    w.write_str("],\"results\":{")?;
    let mut first = true;
    for (t, r) in TESTS.iter().zip(b.results.iter()) {
        let Some(r) = r else { continue };
        if !first {
            w.write_str(",")?;
        }
        first = false;
        json_str(w, t.name())?;
        w.write_str(":")?;
        r.json(w)?;
    }
    w.write_str("}}")
}

/// Een getal of `null`.
fn opt_num(w: &mut impl Write, v: Option<f64>) -> fmt::Result {
    match v {
        Some(v) => json_num(w, v),
        None => w.write_str("null"),
    }
}

/// Het node-blok: wat de app van zichzelf en zijn slot weet.
fn write_node(w: &mut impl Write, sh: &Shared) -> fmt::Result {
    let app = sh.app;
    let st = HEAP.stats();
    let [a, b, c, d] = sh.ip;
    w.write_str("{\"node\":{\"name\":")?;
    json_str(w, sh.node)?;
    w.write_str(",\"version\":")?;
    json_str(w, VERSION)?;
    write!(
        w,
        ",\"arch\":\"{ARCH}\",\"runtime\":\"rust\",\"slot\":{},\"ram_mb\":{},\"ip\":\"{a}.{b}.{c}.{d}\",\"port\":{},\"cores\":{},\"online\":{},\"shared\":{},\"counter_hz\":{},\"uptime_s\":{}",
        app.slot(),
        app.ram_size() >> 20,
        sh.port,
        app.cores(),
        smp::online(),
        app.ctrl().is_shared(),
        clock::hz(),
        sh.uptime_ns() / 1_000_000_000
    )?;
    write!(
        w,
        ",\"heap_used_kb\":{},\"heap_peak_kb\":{},\"heap_capacity_kb\":{},\"heap_allocs\":{},\"heap_failed\":{},\"timer_overflows\":{},\"timers_peak\":{},\"tasks_peak\":{},\"smp_kicks\":{}}}",
        st.used >> 10,
        st.peak >> 10,
        st.capacity >> 10,
        st.allocs,
        st.failed,
        sh.exec
            .stats
            .timer_overflows
            .load(core::sync::atomic::Ordering::Relaxed),
        sh.exec
            .stats
            .timers_peak
            .load(core::sync::atomic::Ordering::Relaxed),
        sh.exec
            .stats
            .tasks_peak
            .load(core::sync::atomic::Ordering::Relaxed),
        smp::KICKS.load(core::sync::atomic::Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn query(target: &str) -> Params {
        let req = leanhttp::Request::new("GET", target).unwrap();
        Params::parse(|k| req.query(k).ok().flatten())
    }

    #[test]
    fn params_come_from_the_query_and_clamp_like_go() {
        let p = query("/api/run?test=disk&mb=4096&kb=2&hole=1&path=/data/x.bin&n=-3");
        assert_eq!(Params::int(p.mb, 64, 1, 1024), 1024);
        assert_eq!(Params::int(p.kb, 1024, 4, 1024), 4);
        assert_eq!(Params::int(p.n, 100, 10, 2000), 10);
        assert_eq!(Params::int(p.secs, 5, 1, 60), 5);
        assert!(p.hole);
        assert_eq!(p.path.as_deref(), Some("/data/x.bin"));
        assert_eq!(p.url, None);
        let p = query("/api/run?test=rx&url=http%3A%2F%2F10.0.0.2%3A8000%2Fbig&secs=x");
        assert_eq!(p.url.as_deref(), Some("http://10.0.0.2:8000/big"));
        assert_eq!(p.secs, None);
        assert!(!p.hole);
    }

    #[test]
    fn only_the_runnable_tests_start() {
        assert_eq!(Test::runnable("cpu"), Some(Test::Cpu));
        assert_eq!(Test::runnable("timer"), Some(Test::Timer));
        assert_eq!(Test::runnable("tx"), None);
        assert_eq!(Test::runnable("sqlite"), None);
        assert_eq!(Test::runnable("gc"), None);
    }

    #[test]
    fn the_table_is_in_index_order_with_unique_names_and_markers() {
        for (i, t) in TESTS.iter().enumerate() {
            assert_eq!(t.index(), i);
            assert!(t.marker().starts_with("HOPOS_VITALS_"));
        }
        let mut names: Vec<String> = TESTS.iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), COUNT);
        let mut markers: Vec<&str> = TESTS.iter().map(|t| t.marker()).collect();
        markers.sort_unstable();
        markers.dedup();
        assert_eq!(markers.len(), COUNT);
        // De startmarker van de app is geen testmarker.
        assert!(!markers.contains(&"HOPOS_VITALS_UP"));
    }

    #[test]
    fn a_burst_of_puts_is_one_measurement_until_a_gap() {
        let mut b = Burst::default();
        b.add(1_000, 2_000, 100);
        b.add(2_500, 3_000, 100);
        assert_eq!(
            (b.start_ns, b.end_ns, b.bytes, b.reqs),
            (1_000, 3_000, 200, 2)
        );
        b.add(3_000 + BURST_GAP_NS + 1, 3_000 + BURST_GAP_NS + 10, 7);
        assert_eq!(b.reqs, 1);
        assert_eq!(b.bytes, 7);
        assert_eq!(b.start_ns, 3_000 + BURST_GAP_NS + 1);
    }
}
