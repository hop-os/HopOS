//! De ABI-toets van HopOS v3: een app die elk contract van de kern aanraakt
//! en per toets een marker op zijn log zet.
//!
//! Wat de Go-appspike over veel rollen verspreidde (READY, heartbeat, logs,
//! net, bestanden, isolatie), doet deze in één doorloop over wat applib nu
//! draagt: de control-page lezen en terugschrijven, de env, een reeks
//! logregels, een frame op de TX-ring, de klok, de heap, de heartbeat, een
//! system-call over de eigen netstack en de bestandscalls in de eigen root
//! (hopfs op de schijf van de kern). Elke
//! toets is één regel `HOPOS_APPSPIKE_<TOETS> ok|FAIL ...` met de getallen
//! erbij; de laatste regel is `HOPOS_APPSPIKE_DONE pass=N fail=M`, en de
//! exitcode is het aantal mislukte toetsen. De soak-scripts greppen erop.
//!
//! Met `HOLD=1` in de env blijft de app daarna leven (de heartbeat loopt
//! door), voor wie de kill-vlag wil toetsen.
//!
//! Met `FAULT=1` in de env leest de app na zijn eerste regel het woord net
//! onder zijn RAM-declaratie: buiten zijn stage-1, dus een vertaalfout op
//! EL1 die de vectortabel van applib vangt (`applib::mmu`). De kern drukt
//! dan `fault at EL1 vec=4 esr=... (data abort, translation fault) ...
//! HOPOS_SLOT_FAULT` met de échte ESR, ELR en FAR (30-09; daarvoor sprong
//! zo'n fault naar een lege VBAR_EL1). Geen toets in de doorloop: de app
//! stopt hier.
//!
//! Twee rollen (`ROLE` in de env) voegen een toets toe:
//!
//! - `SMP`: de app draait met `cores: 2` of meer. Een taak op core 1 hoogt
//!   een teller op en alloceert, terwijl core 0 de outbox schrijft; de
//!   teller moet lopen terwijl core 0 bezig is, en de heap moet heel zijn
//!   (`HOPOS_APPSPIKE_SMP`).
//! - `SHARE`: de app is lid van een sharegroup met een buur op dezelfde
//!   app-core. Hij wacht tot de kern `CTRL_SHARED` zet (de buur is er) en
//!   telt dan zijn beurten: elke idle-ronde is een yield naar de switcher,
//!   die de core aan de buur geeft (`HOPOS_APPSPIKE_SHARE`).
//! - `FLIPCONN`: één UITGAANDE TCP-verbinding over de kern-flip
//!   (`HOPOS_APPSPIKE_FLIPCONN`, `docs/flip.md`). De app verbindt met
//!   `FLIPCONN=<ip>:<poort>` (een echo op de host, door de masquerade van
//!   de kern en de slirp van QEMU), stuurt elke seconde een regel en telt
//!   de antwoorden. De echo zet er een fase voor (`A` vóór de landing van
//!   de nieuwe kern, `B` erna; het toets-script zet hem om), dus de app
//!   weet zonder klok van de kern welke antwoorden over de flip heen kwamen.
//!   De verbinding mag over de flip breken (01-11: de conntrack gaat niet
//!   mee): de app krijgt dan een fout van zijn stack, geen hang
//!   (`HOPOS_APPSPIKE_FLIPCONN_BROKE`), en verbindt opnieuw. Groen bij drie
//!   `B`-antwoorden binnen de termijn.
//!   Alleen deze toets draait dan: de rest bewijzen de andere rollen.
//!
//! - `STORE`: de object-store (Go: `store_demo.go`), met een S3 achter Hop
//!   (`tools/qemu-test-store.sh`): een pull van iets dat er nooit was is een
//!   nette fout, dan push, list ziet de naam, pull naar een ander (vuil,
//!   langer) pad dat vervangen wordt, vergelijk, drop, en list is leeg
//!   (`HOPOS_APPSPIKE_STORE`).
//!
//! En één toets naast de rollen: met `VOLUME=<pad>` in de env (een pad
//! onder een volume van de jobspec, `"volumes":{"/volumes/demo":"/data"}`)
//! bewijst `HOPOS_APPSPIKE_VOLUME` dat een volume een levensduur overleeft
//! en de eigen root niet. De eerste levensduur vindt niets en schrijft
//! (`wrote`); de volgende (Hop herstart de service na de exit) vindt het
//! bestand terug met dezelfde bytes (`found`), in een root die weer leeg
//! is.
//!
//! Canoniek gelinkt (applib/link.ld): de stage-2-map van de kern legt het
//! image op de partitie van elk slot, en de kern patcht RamStart en RamSize
//! bij plaatsing.

#![cfg_attr(target_os = "none", no_std, no_main)]

extern crate alloc;

use alloc::vec::Vec;
use applib::appnet::{self, TcpStream};
use applib::heap::Heap;
use applib::net::{self, Nic};
use applib::{App, AppStatus, EXEC, clock, heap::HEAP, log, smp, sys};
use core::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use core::time::Duration;
use netdev::Device;

applib::main!(spike);

/// Op de host bestaat dit image niet: daar is dit alleen een lege binary,
/// zodat de host-poort (clippy `--all-targets`) hem kan typechecken zonder
/// allocator en paniekhaak van het slot.
#[cfg(not(target_os = "none"))]
fn main() {}

/// Het resultaat van de doorloop.
#[derive(Default)]
struct Score {
    pass: u32,
    fail: u32,
}

impl Score {
    /// Telt één toets en zet zijn marker op het log.
    fn check(&mut self, name: &str, ok: bool, detail: core::fmt::Arguments<'_>) {
        if ok {
            self.pass += 1;
            log!("HOPOS_APPSPIKE_{name} ok {detail}");
        } else {
            self.fail += 1;
            log!("HOPOS_APPSPIKE_{name} FAIL {detail}");
        }
    }
}

async fn spike(app: &'static App) {
    log!(
        "HOPOS_APPSPIKE_UP slot={} ram={:#x}+{:#x} tail={:#x}",
        app.slot(),
        app.ram_start(),
        app.ram_size(),
        app.tail().base().0
    );
    if app.env("FAULT") == Some("1") {
        let below = app.ram_start().saturating_sub(0x1000);
        log!("HOPOS_APPSPIKE_FAULT reading {below:#x}, outside the stage-1 map");
        // Een vertaalfout op EL1: keert niet terug.
        let v = dev::read64(dev::Pa(below));
        log!("HOPOS_APPSPIKE_FAULT FAIL: read {v:#x} without a fault");
        app.exit(1);
    }
    let mut s = Score::default();
    ctrl_page(app, &mut s);
    env(app, &mut s);
    if app.env("ROLE") == Some("FLIPCONN") {
        flipconn_role(app, &mut s).await;
        log!("HOPOS_APPSPIKE_DONE pass={} fail={}", s.pass, s.fail);
        app.shutdown(u64::from(s.fail)).await;
        return;
    }
    match app.env("ROLE") {
        Some("SMP") => smp_role(app, &mut s).await,
        Some("SHARE") => share_role(app, &mut s).await,
        _ => {}
    }
    logs(&mut s).await;
    frame(app, &mut s).await;
    timer(&mut s).await;
    heartbeat(app, &mut s).await;
    let mut client = network(app, &mut s).await;
    files(client.as_mut(), &mut s).await;
    if let Some(path) = app.env("VOLUME") {
        volume(path, client.as_mut(), &mut s).await;
    }
    if app.env("ROLE") == Some("STORE") {
        store(client.as_mut(), &mut s).await;
    }
    heap(app, &mut s);

    log!("HOPOS_APPSPIKE_DONE pass={} fail={}", s.pass, s.fail);
    if app.env("HOLD") == Some("1") {
        applib::park().await;
    }
    // Netjes: eerst het net-afscheid (elke FIN bevestigd), dan de exit.
    app.shutdown(u64::from(s.fail)).await;
}

/// De control-page: READY staat er, de RAM-maat is wat de app van zichzelf
/// weet, en een woord dat de app schrijft leest hij terug.
fn ctrl_page(app: &App, s: &mut Score) {
    let c = app.ctrl();
    let status = c.status();
    let ram = c.ram_size();
    c.set_mem_sys(0x5a5a_0001);
    let back = c.mem_sys();
    let ok = status == Some(AppStatus::Ready) && ram == app.ram_size() && back == 0x5a5a_0001;
    s.check(
        "CTRL",
        ok,
        format_args!(
            "status={status:?} ram={ram:#x} readback={back:#x} cores={} shared={} yield={} wall={}",
            c.cores(),
            c.is_shared(),
            c.is_yield_mode(),
            c.wall_offset()
        ),
    );
}

/// De env: gelezen van de pagina. Een lege env is geen fout (de kern geeft
/// er niet altijd een mee), een onleesbare wel.
fn env(app: &App, s: &mut Score) {
    let role = app.env("ROLE").unwrap_or("-");
    let port = app.env("ER_PORT_HTTP").unwrap_or("-");
    s.check("ENV", true, format_args!("ROLE={role} ER_PORT_HTTP={port}"));
}

/// Hoe lang de SMP-toets op de opgang van de andere cores wacht.
const SMP_WAIT: Duration = Duration::from_secs(5);

/// De teller van de taak op core 1.
static SMP_TICKS: AtomicU64 = AtomicU64::new(0);
/// Allocaties die de taak op core 1 deed en weer vrijgaf.
static SMP_ALLOCS: AtomicU64 = AtomicU64::new(0);
/// De core waarop de taak draaide, zoals hij het zelf zag.
static SMP_WHERE: AtomicU64 = AtomicU64::new(u64::MAX);
/// TTBR0 van core 1 zoals die hem zag (0 = MMU uit). Sinds 30-09 zet de
/// stub van een secundaire dezelfde stage-1 aan als de primaire
/// (`applib::mmu`); met de MMU uit zou core 1 de heap van core 0 uit het
/// geheugen lezen terwijl die nog in zijn cache staat.
static SMP_TTBR: AtomicU64 = AtomicU64::new(u64::MAX);
/// Core 0 vraagt de taak te stoppen.
static SMP_STOP: AtomicBool = AtomicBool::new(false);
/// Het antwoord van core 1, als taak terug op core 0.
static SMP_REPLIED: AtomicU64 = AtomicU64::new(0);

/// De taak op core 1: tellen en alloceren tot core 0 stop zegt, dan het
/// eindgetal als taak terug naar core 0 (de wek de andere kant op).
async fn smp_counter() {
    SMP_WHERE.store(smp::current() as u64, Release);
    SMP_TTBR.store(applib::mmu::regime().0, Release);
    while !SMP_STOP.load(Acquire) {
        SMP_TICKS.fetch_add(1, Relaxed);
        if SMP_TICKS.load(Relaxed).is_multiple_of(64) {
            // Een blok van deze core, vrijgegeven op deze core, terwijl core
            // 0 ook alloceert: het slot van de heap.
            let mut v: Vec<u8> = Vec::new();
            if v.try_reserve(96).is_ok() {
                SMP_ALLOCS.fetch_add(1, Relaxed);
            }
        }
    }
    let n = SMP_TICKS.load(Relaxed);
    let _ = smp::spawn_on(0, async move {
        SMP_REPLIED.store(n.max(1), Release);
    });
}

/// De SMP-toets: `cores: 2` of meer, een taak op core 1 die telt terwijl
/// core 0 zijn outbox schrijft. Bewijst dat de tweede core in de kooi van
/// deze app draait (hij ziet dezelfde statics), parallel (de teller loopt
/// terwijl core 0 niet yieldt), dat werk heen en terug gaat (`spawn_on`
/// met een wek per kant), en dat de heap het samen overleeft.
async fn smp_role(app: &'static App, s: &mut Score) {
    let cores = app.cores();
    let t0 = clock::now_ns();
    let limit = SMP_WAIT.as_nanos() as u64;
    while !smp::is_up(1) && clock::now_ns().saturating_sub(t0) < limit {
        EXEC.after(Duration::from_millis(5)).await;
    }
    let up_ms = clock::now_ns().saturating_sub(t0) / 1_000_000;
    if cores < 2 || !smp::is_up(1) {
        s.check(
            "SMP",
            false,
            format_args!("cores={cores} online={} after {up_ms} ms", smp::online()),
        );
        return;
    }
    if let Err(e) = smp::spawn_on(1, smp_counter()) {
        s.check("SMP", false, format_args!("cores={cores} spawn_on(1): {e}"));
        return;
    }
    // Wacht tot de taak op core 1 loopt.
    let t1 = clock::now_ns();
    while SMP_TICKS.load(Relaxed) == 0 && clock::now_ns().saturating_sub(t1) < limit {
        EXEC.after(Duration::from_millis(1)).await;
    }
    // Core 0 schrijft zijn outbox zonder te yielden, en alloceert ook: loopt
    // de teller intussen door, dan draait core 1 echt naast ons.
    let before = SMP_TICKS.load(Relaxed);
    let mut mine = 0u32;
    for i in 1..=16u32 {
        log!(
            "HOPOS_APPSPIKE_SMP_LINE {i}/16 from core {}",
            smp::current()
        );
        let mut v: Vec<u8> = Vec::new();
        if v.try_reserve(80).is_ok() {
            mine += 1;
        }
    }
    let during = SMP_TICKS.load(Relaxed).wrapping_sub(before);
    SMP_STOP.store(true, Release);
    let t2 = clock::now_ns();
    while SMP_REPLIED.load(Acquire) == 0 && clock::now_ns().saturating_sub(t2) < limit {
        EXEC.after(Duration::from_millis(1)).await;
    }
    let ticks = SMP_REPLIED.load(Acquire);
    let on = SMP_WHERE.load(Acquire);
    // Dezelfde tabellen op beide cores (of op beide geen).
    let ttbr = SMP_TTBR.load(Acquire);
    let same_map = ttbr == applib::mmu::regime().0;
    let heap = HEAP.check();
    let lock = Heap::lock_stats();
    s.check(
        "SMP",
        on == 1 && during > 0 && ticks > 0 && heap.is_ok() && mine == 16 && same_map,
        format_args!(
            "cores={cores} online={} up_ms={up_ms} task_core={on} ticks={ticks} during_log={during} allocs={} remote_spawns={} kicks={} heap_ok={} lock_taken={} lock_contended={} lock_spin_max={} core1_ttbr={ttbr:#x} same_map={same_map}",
            smp::online(),
            SMP_ALLOCS.load(Relaxed),
            smp::REMOTE_SPAWNS.load(Relaxed),
            smp::KICKS.load(Relaxed),
            heap.is_ok(),
            lock.taken,
            lock.contended,
            lock.longest_spin
        ),
    );
}

/// Hoe lang de SHARE-toets op zijn buur wacht.
const SHARE_WAIT: Duration = Duration::from_secs(30);

/// Hoeveel slaapjes van 1 ms de SHARE-toets telt.
const SHARE_NAPS: u32 = 100;

/// De SHARE-toets: wacht tot de kern `CTRL_SHARED` zet (een tweede lid
/// van de groep woont op deze core), en slaap dan honderd keer 1 ms. Elke
/// slaap is een idle-ronde, en op een gedeelde core is die een yield naar
/// de switcher: de beurt gaat naar de buur. Groen als het slot gedeeld was
/// en er beurten waren, en de klok liep zoals beloofd (de buur hield de
/// core niet vast).
async fn share_role(app: &'static App, s: &mut Score) {
    let c = app.ctrl();
    let t0 = clock::now_ns();
    let limit = SHARE_WAIT.as_nanos() as u64;
    while !c.is_shared() && clock::now_ns().saturating_sub(t0) < limit {
        EXEC.after(Duration::from_millis(10)).await;
    }
    let waited_ms = clock::now_ns().saturating_sub(t0) / 1_000_000;
    let shared = c.is_shared();
    let rounds = c.idle_rounds();
    let t1 = clock::now_ns();
    for _ in 0..SHARE_NAPS {
        EXEC.after(Duration::from_millis(1)).await;
    }
    let ms = clock::now_ns().saturating_sub(t1) / 1_000_000;
    let yields = c.idle_rounds().wrapping_sub(rounds);
    s.check(
        "SHARE",
        shared && yields >= u64::from(SHARE_NAPS) && ms < 2000,
        format_args!(
            "yields={yields} naps={SHARE_NAPS} ms={ms} shared={} waited_ms={waited_ms} yield_mode={} idle_ticks={}",
            u8::from(shared),
            c.is_yield_mode(),
            c.idle_ticks()
        ),
    );
}

/// Hoeveel antwoorden na de landing de FLIPCONN-toets eist.
const FLIPCONN_AFTER: u32 = 3;

/// Hoe lang de FLIPCONN-toets hooguit loopt: de flip moet binnen deze tijd
/// komen en landen (op QEMU: een halve minuut tot de flip, een paar
/// seconden sprong).
const FLIPCONN_LIMIT: Duration = Duration::from_secs(240);

/// De telling van de FLIPCONN-toets.
#[derive(Default)]
struct Echoes {
    /// Regels gestuurd.
    sent: u32,
    /// Antwoorden van vóór de landing (`A`).
    before: u32,
    /// Antwoorden van na de landing (`B`).
    after: u32,
    /// Een antwoord dat geen van beide was.
    odd: u32,
}

impl Echoes {
    /// Telt de complete regels in `buf[..*len]` en schuift de rest naar
    /// voren.
    fn take(&mut self, buf: &mut [u8], len: &mut usize) {
        let filled = (*len).min(buf.len());
        let mut used = 0;
        while let Some(nl) = buf
            .get(used..filled)
            .and_then(|b| b.iter().position(|c| *c == b'\n'))
        {
            match buf.get(used) {
                Some(b'A') => self.before += 1,
                Some(b'B') => self.after += 1,
                _ => self.odd += 1,
            }
            used += nl + 1;
        }
        buf.copy_within(used..filled, 0);
        *len = filled - used;
    }
}

/// De FLIPCONN-toets: één uitgaande verbinding die de kern-flip overleeft
/// (zie de moduledoc). De lus schrijft elke seconde een regel en leest tot
/// de volgende seconde; een time-out van de read is gewoon "nog niets".
async fn flipconn_role(app: &'static App, s: &mut Score) {
    let target = app.env("FLIPCONN").unwrap_or("");
    let Some((ip, port)) = target
        .split_once(':')
        .and_then(|(h, p)| Some((appnet::parse_ip4(h)?, p.parse::<u16>().ok()?)))
    else {
        s.check(
            "FLIPCONN",
            false,
            format_args!("FLIPCONN={target:?} is not ip:port"),
        );
        return;
    };
    if let Err(e) = appnet::up(app) {
        s.check("FLIPCONN", false, format_args!("up: {e}"));
        return;
    }
    let t0 = clock::now_ns();
    let limit = FLIPCONN_LIMIT.as_nanos() as u64;
    let mut n = Echoes::default();
    let mut buf = [0u8; 256];
    let mut len = 0usize;
    let mut conn: Option<TcpStream> = None;
    let mut breaks = 0u32;
    let mut local = 0;
    let failed = loop {
        if n.after >= FLIPCONN_AFTER {
            break None;
        }
        if clock::now_ns().saturating_sub(t0) > limit {
            break Some("no answers after the landing in time");
        }
        let c = match conn.as_mut() {
            Some(c) => c,
            None => match TcpStream::connect_timeout(ip, port, NET_DIAL).await {
                Ok(c) => {
                    local = c.local().map(|e| e.port).unwrap_or(0);
                    len = 0;
                    log!("HOPOS_APPSPIKE_FLIPCONN_UP to={target} local_port={local}");
                    conn.insert(c)
                }
                Err(e) => {
                    log!("HOPOS_APPSPIKE_FLIPCONN_BROKE reconnect: {e}");
                    break Some("reconnect failed");
                }
            },
        };
        n.sent += 1;
        let line = alloc::format!("ping {}\n", n.sent);
        let r = match c.write_all(line.as_bytes()).await {
            Ok(()) => read_round(c, &mut buf, &mut len, &mut n).await,
            Err(e) => {
                log!("HOPOS_APPSPIKE_FLIPCONN_BROKE write: {e}");
                Err("write failed")
            }
        };
        if let Err(why) = r {
            // Gebroken: een fout van de stack, geen hang. Opnieuw.
            breaks += 1;
            log!("HOPOS_APPSPIKE_FLIPCONN_BROKE {why}, break {breaks}, reconnecting");
            if let Some(c) = conn.take() {
                let _ = c.close();
            }
        }
        if n.sent.is_multiple_of(5) {
            log!(
                "HOPOS_APPSPIKE_FLIPCONN_TICK sent={} before={} after={} odd={}",
                n.sent,
                n.before,
                n.after,
                n.odd
            );
        }
    };
    let total = n.before + n.after;
    match failed {
        None => s.check(
            "FLIPCONN",
            n.before > 0 && n.odd == 0,
            format_args!(
                "before={} after={total} sent={} odd={} breaks={breaks} local_port={local}",
                n.before, n.sent, n.odd
            ),
        ),
        Some(why) => s.check(
            "FLIPCONN",
            false,
            format_args!(
                "{why}: before={} after={total} sent={} odd={} breaks={breaks} local_port={local}",
                n.before, n.sent, n.odd
            ),
        ),
    }
    if let Some(c) = conn {
        let _ = c.close();
    }
}

/// Leest één seconde lang wat de echo terugstuurt en telt het. Een
/// time-out is "nog niets" (tijdens de sprong antwoordt niemand); een EOF
/// of een fout van de stack (een reset) is het einde van de verbinding.
async fn read_round(
    conn: &mut TcpStream,
    buf: &mut [u8; 256],
    len: &mut usize,
    n: &mut Echoes,
) -> Result<(), &'static str> {
    conn.set_timeout(Some(Duration::from_secs(1)));
    let r = loop {
        // Een regel langer dan de buffer is geen echo: weg ermee, geteld.
        if *len >= buf.len() {
            *len = 0;
            n.odd += 1;
        }
        match conn.read(buf.get_mut(*len..).unwrap_or(&mut [])).await {
            Ok(0) => break Err("the peer closed the connection"),
            Ok(k) => {
                *len += k;
                n.take(buf, len);
            }
            Err(appnet::NetError::Timeout) => break Ok(()),
            Err(e) => {
                log!("HOPOS_APPSPIKE_FLIPCONN_BROKE read: {e}");
                break Err("the connection broke");
            }
        }
    };
    conn.set_timeout(None);
    r
}

/// Aantal logregels van de logtoets.
const LOG_LINES: u32 = 64;

/// N logregels op de outbox. Elke 16 regels een milliseconde, zodat de
/// servicer van de kern de ring kan leegtrekken; gedropt is gedropt, en dat
/// telt als mislukt.
async fn logs(s: &mut Score) {
    let dropped = applib::log::DROPPED.load(core::sync::atomic::Ordering::Relaxed);
    for i in 1..=LOG_LINES {
        log!("HOPOS_APPSPIKE_LINE {i}/{LOG_LINES}");
        if i % 16 == 0 {
            EXEC.after(Duration::from_millis(1)).await;
        }
    }
    let lost = applib::log::DROPPED
        .load(core::sync::atomic::Ordering::Relaxed)
        .wrapping_sub(dropped);
    s.check(
        "LOG",
        lost == 0,
        format_args!("lines={LOG_LINES} dropped={lost}"),
    );
}

/// De Ethertype van het toetsframe: 0x88B5, "local experimental" (IEEE
/// 802), dus nooit iets dat een echte stack verkeerd leest.
const ETHERTYPE_LOCAL: [u8; 2] = [0x88, 0xb5];

/// Eén frame op de TX-ring, van dit slot naar de kern.
async fn frame(app: &App, s: &mut Score) {
    let mut nic = match Nic::open(app) {
        Ok(n) => n,
        Err(e) => {
            s.check("FRAME", false, format_args!("open: {e}"));
            return;
        }
    };
    let mut f = [0u8; 64];
    f[0..6].copy_from_slice(&net::mac_of(0).0);
    f[6..12].copy_from_slice(&nic.mac().0);
    f[12..14].copy_from_slice(&ETHERTYPE_LOCAL);
    f[14..28].copy_from_slice(b"HOPOS_APPSPIKE");
    let r = nic.transmit_wait(&f, clock::now_ns).await;
    s.check(
        "FRAME",
        r.is_ok(),
        format_args!(
            "len={} src={} dst={} result={r:?}",
            f.len(),
            nic.mac(),
            net::mac_of(0)
        ),
    );
}

/// De klok en het timerwiel: een slaap van 10 ms duurt minstens 10 ms.
async fn timer(s: &mut Score) {
    let t0 = clock::now_ns();
    EXEC.after(Duration::from_millis(10)).await;
    let dt = clock::now_ns().wrapping_sub(t0);
    s.check(
        "TIMER",
        (10_000_000..1_000_000_000).contains(&dt),
        format_args!("slept_ns={dt}"),
    );
}

/// De heartbeat loopt als taak: na 300 ms moet de teller op de pagina
/// minstens vijf slagen verder staan.
async fn heartbeat(app: &App, s: &mut Score) {
    let before = app.ctrl().heartbeat();
    EXEC.after(Duration::from_millis(300)).await;
    let after = app.ctrl().heartbeat();
    let beats = after.wrapping_sub(before);
    s.check(
        "BEAT",
        beats >= 5,
        format_args!(
            "before={before} after={after} idle_ticks={}",
            app.ctrl().idle_ticks()
        ),
    );
}

/// Hoe lang de NET-toets op de handshake met de kern wacht.
const NET_DIAL: Duration = Duration::from_secs(3);

/// De netstack: `appnet::up`, een TCP-verbinding naar de system-listener van
/// de kern (10.100.0.1:10100) en één `stat` over die verbinding via
/// `sys::Client`. Zonder listener aan de kern-kant faalt hij met de reden
/// (refused, of een timeout als er niemand antwoordt).
async fn network(app: &'static App, s: &mut Score) -> Option<appnet::SystemClient> {
    let n = match appnet::up(app) {
        Ok(n) => n,
        Err(e) => {
            s.check("NET", false, format_args!("up: {e}"));
            return None;
        }
    };
    let [a, b, c, d] = n.ip();
    let (ip, port) = sys::ADDRESS;
    let t0 = clock::now_ns();
    let conn = match TcpStream::connect_timeout(ip, port, NET_DIAL).await {
        Ok(conn) => conn,
        Err(e) => {
            s.check(
                "NET",
                false,
                format_args!("ip={a}.{b}.{c}.{d} connect {ip:?}:{port}: {e}"),
            );
            return None;
        }
    };
    let dial_us = clock::now_ns().wrapping_sub(t0) / 1000;
    let mut client = n.system_client_over(conn);
    // Eén logregel over de system-verbinding (KindLog): listener, admit,
    // servicer en LogTee. De kern zet de regel als `slot N: ...` op zijn
    // console, en dát is het bewijs van buitenaf: app, switch, kern-stack en
    // system-API in één lijn. De bestandscalls volgen over dezelfde
    // verbinding (`files`).
    let t1 = clock::now_ns();
    let r = client
        .log(b"HOPOS_APPSPIKE_NETLOG via the system connection")
        .await;
    let log_us = clock::now_ns().wrapping_sub(t1) / 1000;
    // `log` keert terug zodra de regel in de TCP-zendbuffer staat, niet als
    // hij aankwam (gemeten 29-09: log_us=320, en zonder wachten zag de kern
    // niets). `flush` wacht tot de kern hem bevestigde; zo meet de toets
    // wat hij belooft, en niet een pauze die toevallig lang genoeg is.
    let t2 = clock::now_ns();
    let flushed = match client.conn_mut() {
        Some(conn) => Some(conn.flush().await),
        None => None,
    };
    let flush_us = clock::now_ns().wrapping_sub(t2) / 1000;
    let kicks = net::TX_KICKS.load(core::sync::atomic::Ordering::Relaxed);
    let ip = format_args!("ip={a}.{b}.{c}.{d} dial_us={dial_us} tx_kicks={kicks}");
    match (r, flushed) {
        (Ok(()), Some(Ok(()))) => s.check(
            "NET",
            true,
            format_args!("{ip} log_us={log_us} flush_us={flush_us}"),
        ),
        (Err(e), _) => s.check("NET", false, format_args!("{ip} log: {e}")),
        (Ok(()), Some(Err(e))) => s.check(
            "NET",
            false,
            format_args!("{ip} flush after {flush_us}us: {e}"),
        ),
        (Ok(()), None) => s.check("NET", false, format_args!("{ip} no connection to flush")),
    }
    Some(client)
}

/// Het bestand van de FS-toets, in de eigen root (`/.tasks/slot<N>/`).
const FS_FILE: &str = "hallo.txt";

/// De inhoud: langer dan één hopfs-blok niet nodig, wel met een staart die
/// een truncate zichtbaar zou maken.
const FS_DATA: &[u8] = b"hallo van appspike, via de system-API naar hopfs op de schijf\n";

/// De bestandscalls over dezelfde verbinding: schrijven (truncate plus
/// write), `stat`, teruglezen (ook als bundel: drie lezingen in één call,
/// de laatste voorbij het einde), de lijst van de eigen root en weer weg. De
/// root is bij elke start leeg (de kern veegt hem), dus de lijst is precies
/// dit ene bestand.
async fn files(client: Option<&mut appnet::SystemClient>, s: &mut Score) {
    let Some(c) = client else {
        s.check("FS", false, format_args!("no system connection"));
        return;
    };
    let t0 = clock::now_ns();
    let r = fs_round(c).await;
    let us = clock::now_ns().wrapping_sub(t0) / 1000;
    match r {
        Ok((size, names)) => s.check(
            "FS",
            true,
            format_args!("file={FS_FILE} size={size} list={names} us={us}"),
        ),
        Err(why) => s.check("FS", false, format_args!("{why} after {us} us")),
    }
}

/// Wat er in de FS-toets misging, met de call of het getal erbij.
enum Why {
    /// Een call faalde.
    Sys(&'static str, sys::Error),
    /// Een call gaf iets anders dan verwacht.
    Num(&'static str, u64),
}

impl Why {
    fn sys(what: &'static str, e: sys::Error) -> Why {
        Why::Sys(what, e)
    }
    fn num(what: &'static str, n: u64) -> Why {
        Why::Num(what, n)
    }
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Sys(what, e) => write!(f, "{what}: {e}"),
            Why::Num(what, n) => write!(f, "{what} {n}"),
        }
    }
}

/// Eén ronde; geeft de maat en het aantal namen, of wat er misging.
async fn fs_round(c: &mut appnet::SystemClient) -> Result<(u64, usize), Why> {
    c.write_file(FS_FILE, FS_DATA)
        .await
        .map_err(|e| Why::sys("write_file", e))?;
    let size = c.stat(FS_FILE).await.map_err(|e| Why::sys("stat", e))?;
    if size != FS_DATA.len() as u64 {
        return Err(Why::num("stat size", size));
    }
    let mut buf = [0u8; 128];
    let n = c
        .read_into(FS_FILE, 0, &mut buf)
        .await
        .map_err(|e| Why::sys("read_into", e))?;
    if buf.get(..n) != Some(FS_DATA) {
        return Err(Why::num("read back bytes", n as u64));
    }
    let (mut a, mut b, mut z) = ([0u8; 5], [0u8; 4], [0u8; 8]);
    let mut ops = [
        sys::ReadOp::new(0, &mut a),
        sys::ReadOp::new(6, &mut b),
        sys::ReadOp::new(FS_DATA.len() as u64 + 100, &mut z),
    ];
    let n = c
        .read_many(FS_FILE, &mut ops)
        .await
        .map_err(|e| Why::sys("read_many", e))?;
    let got = [ops[0].got, ops[1].got, ops[2].got];
    if got != [Ok(5), Ok(4), Ok(0)] || n != 9 || (&a, &b) != (b"hallo", b"van ") {
        return Err(Why::num("read_many bytes", n as u64));
    }
    let mut list = [0u8; 256];
    let n = c
        .list("/", &mut list)
        .await
        .map_err(|e| Why::sys("list", e))?;
    let names = sys::names(list.get(..n).unwrap_or(&[])).count();
    if !sys::names(list.get(..n).unwrap_or(&[])).any(|x| x == FS_FILE) {
        return Err(Why::num("list without the file, names", names as u64));
    }
    c.remove(FS_FILE).await.map_err(|e| Why::sys("remove", e))?;
    match c.stat(FS_FILE).await {
        Err(sys::Error::NotFound { .. }) => Ok((size, names)),
        Ok(n) => Err(Why::num("still there after remove, size", n)),
        Err(e) => Err(Why::sys("stat after remove", e)),
    }
}

/// Wat de VOLUME-toets in het volume zet.
const VOLUME_DATA: &[u8] = b"appspike was here: this file lives in a volume, not in the root\n";

/// Het bestand in de eigen root dat een nieuwe levensduur NIET mag zien.
const ROOT_MARK: &str = "/life.txt";

/// De VOLUME-toets: de eigen root is leeg (een vorige levensduur liet er
/// [`ROOT_MARK`] achter), en het volume houdt `path` over een herstart.
async fn volume(path: &str, client: Option<&mut appnet::SystemClient>, s: &mut Score) {
    let Some(c) = client else {
        s.check("VOLUME", false, format_args!("no system connection"));
        return;
    };
    match volume_round(c, path).await {
        Ok((what, n)) => s.check(
            "VOLUME",
            true,
            format_args!("{what} path={path} bytes={n} root=fresh"),
        ),
        Err(why) => s.check("VOLUME", false, format_args!("path={path}: {why}")),
    }
}

/// Eén ronde: `("wrote" | "found", bytes)`, of wat er misging.
async fn volume_round(
    c: &mut appnet::SystemClient,
    path: &str,
) -> Result<(&'static str, usize), Why> {
    // De root eerst: wat een vorige levensduur daar schreef, is weg.
    match c.stat(ROOT_MARK).await {
        Err(sys::Error::NotFound { .. }) => {}
        Ok(n) => return Err(Why::num("the root kept a file of a previous life, size", n)),
        Err(e) => return Err(Why::sys("stat root mark", e)),
    }
    c.write_file(ROOT_MARK, b"1")
        .await
        .map_err(|e| Why::sys("write root mark", e))?;
    match c.stat(path).await {
        Err(sys::Error::NotFound { .. }) => {
            c.write_file(path, VOLUME_DATA)
                .await
                .map_err(|e| Why::sys("write", e))?;
            Ok(("wrote", VOLUME_DATA.len()))
        }
        Ok(size) if size == VOLUME_DATA.len() as u64 => {
            let mut buf = [0u8; 128];
            let n = c
                .read_into(path, 0, &mut buf)
                .await
                .map_err(|e| Why::sys("read_into", e))?;
            if buf.get(..n) != Some(VOLUME_DATA) {
                return Err(Why::num("found other bytes, n", n as u64));
            }
            Ok(("found", n))
        }
        Ok(size) => Err(Why::num("found a file of the wrong size", size)),
        Err(e) => Err(Why::sys("stat", e)),
    }
}

/// Wat de STORE-toets pusht.
const STORE_DATA: &[u8] = b"leven 1: geboren om te pushen\n";

/// De STORE-toets: de hele keten van app via kern en Hop naar de bucket en
/// terug.
async fn store(client: Option<&mut appnet::SystemClient>, s: &mut Score) {
    let Some(c) = client else {
        s.check("STORE", false, format_args!("no system connection"));
        return;
    };
    let t0 = clock::now_ns();
    let r = store_round(c).await;
    let us = clock::now_ns().wrapping_sub(t0) / 1000;
    match r {
        Ok(n) => s.check(
            "STORE",
            true,
            format_args!("push/list/pull/drop round-trip verified ({n} bytes) us={us}"),
        ),
        Err(why) => s.check("STORE", false, format_args!("{why} after {us} us")),
    }
}

/// Eén ronde; geeft de maat van het object.
async fn store_round(c: &mut appnet::SystemClient) -> Result<u64, Why> {
    use applib::store;
    // 1. Een verse map: een pull van iets dat er nooit was is een nette fout.
    match store::pull(c, "never-pushed.json").await {
        Err(sys::Error::NotFound { .. }) => {}
        Ok(n) => return Err(Why::num("pull of a never-pushed object gave", n)),
        Err(e) => return Err(Why::sys("pull of a never-pushed object", e)),
    }
    // 2. Schrijven en pushen: vanaf nu is het persistent.
    c.write_file("state.json", STORE_DATA)
        .await
        .map_err(|e| Why::sys("write", e))?;
    let n = store::push(c, "state.json")
        .await
        .map_err(|e| Why::sys("push", e))?;
    if n != STORE_DATA.len() as u64 {
        return Err(Why::num("push size", n));
    }
    // 3. List is relatief aan de eigen map.
    let mut buf = [0u8; 256];
    let mut names = store::list(c, "", &mut buf)
        .await
        .map_err(|e| Why::sys("list", e))?;
    let (first, more) = (names.next() == Some("state.json"), names.count());
    if !first || more != 0 {
        return Err(Why::num(
            "list is not [state.json], extra names",
            more as u64,
        ));
    }
    // 4. Pull naar een ander pad dat al vuil en langer is: de bucket wint,
    //    en er blijft geen staart staan.
    c.write_file(
        "copy.json",
        b"vervuild, en met opzet veel langer dan het origineel dat terugkomt",
    )
    .await
    .map_err(|e| Why::sys("dirty write", e))?;
    let got = store::pull_to(c, "state.json", "copy.json")
        .await
        .map_err(|e| Why::sys("pull", e))?;
    let mut back = [0u8; 128];
    let len = c
        .read_into("copy.json", 0, &mut back)
        .await
        .map_err(|e| Why::sys("read back", e))?;
    if got != n || back.get(..len) != Some(STORE_DATA) {
        return Err(Why::num("after pull the copy has bytes", len as u64));
    }
    // 5. Drop, en de map is weer leeg.
    store::drop(c, "state.json")
        .await
        .map_err(|e| Why::sys("drop", e))?;
    let left = store::list(c, "", &mut buf)
        .await
        .map_err(|e| Why::sys("list after drop", e))?
        .count();
    if left != 0 {
        return Err(Why::num("names left after drop", left as u64));
    }
    Ok(n)
}

/// De heap: de executor alloceerde de taken, dus er is iets in gebruik, het
/// plafond ligt onder de stack, en de wandeling vindt geen gebroken
/// invariant. De piek en de weigeringen gaan mee als meetlat.
fn heap(app: &App, s: &mut Score) {
    let st = HEAP.stats();
    let walk = HEAP.check();
    let (used, cap) = (st.used, st.capacity);
    match walk {
        Ok(w) => s.check(
            "HEAP",
            used > 0 && used <= cap && cap < app.ram_size(),
            format_args!(
                "used={used} peak={} capacity={cap} allocs={} frees={} failed={} blocks={} free_blocks={}",
                st.peak, st.allocs, st.frees, st.failed, w.blocks, w.free_blocks
            ),
        ),
        Err(e) => s.check("HEAP", false, format_args!("used={used} {e}")),
    }
}
