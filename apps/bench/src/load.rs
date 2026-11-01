//! De last-rollen: rekenen (BURN), geheugen (THRASH), multicast (MCAST) en
//! de uitgaande DNS-toets (NETDEMO=out), uit de Go-appspike.
//!
//! BURN en THRASH keren nooit terug: een HOP-app is een service, en de
//! kill van de kern of een stop van Hop beëindigt ze.

use alloc::vec::Vec;
use applib::appnet::{self, Endpoint, UdpSocket};
use applib::burn::{BURST, lcg};
use applib::heap::HEAP;
use applib::{App, EXEC, clock, log, park};
use bounded::Text;
use core::fmt::Write as _;
use core::time::Duration;
use sync::yield_now;

/// De standaard-cyclus van BURN: 10 minuten werk, 5 rust (Go, 11-07: zo
/// toetst een soak ook het terugklokken, niet alleen het opklokken).
const BURN_WORK: u64 = 600;
const BURN_REST: u64 = 300;

/// Om de zoveel seconden één BURN-regel.
const BURN_EVERY: Duration = Duration::from_secs(10);

/// Een getal uit de env, of `default`.
fn env_u64(app: &App, key: &str, default: u64) -> u64 {
    app.env(key)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

/// BURN: rekenen in een ritme van werk en rust (`BURN_WORK`, `BURN_REST`
/// in seconden). Tijdens werk rekent de core in bursts met een yield
/// ertussen (heartbeat en kill-vlag krijgen de core); tijdens rust slaapt
/// hij. De meetlat: bursts per seconde in deze app, en de idle van het
/// slot (`CTRL_IDLE` via `SLOT_STATUS`, bij Hop), die tijdens werk hoort te
/// dalen.
pub(crate) async fn burn(app: &'static App) -> ! {
    let work = env_u64(app, "BURN_WORK", BURN_WORK).max(1);
    let rest = env_u64(app, "BURN_REST", BURN_REST);
    let cycle = work + rest;
    log!(
        "BURN: soak load on 1 core, RAM {} MB, {work}s work / {rest}s rest cycle HOPOS_BENCH_UP role=burn",
        app.ram_size() >> 20
    );
    let t0 = clock::now_ns();
    let (mut acc, mut bursts, mut last_bursts) = (0u64, 0u64, 0u64);
    // De idle van deze core zoals de slaper hem op de control-page zet: in
    // tikken van de teller, dus met `clock::hz` naar nanoseconden.
    let hz = clock::hz();
    let mut last_idle = app.ctrl().idle_ticks();
    let mut next = t0.saturating_add(nanos(BURN_EVERY));
    let mut in_work = true;
    loop {
        let now = clock::now_ns();
        let secs = now.saturating_sub(t0) / 1_000_000_000;
        let working = secs % cycle < work;
        if working != in_work {
            in_work = working;
            let what = if working {
                "work phase, cores busy HOPOS_BENCH_BURN_WORK"
            } else {
                "rest phase, cores idle HOPOS_BENCH_BURN_REST"
            };
            log!("BURN: {what}");
        }
        if working {
            acc = lcg(acc, BURST);
            bursts += 1;
            yield_now().await;
        } else {
            EXEC.after(Duration::from_millis(200)).await;
        }
        if now >= next {
            let dt = now
                .saturating_sub(next.saturating_sub(nanos(BURN_EVERY)))
                .max(1);
            let rate = (bursts - last_bursts) * 1_000_000_000 / dt;
            let idle_ticks = app.ctrl().idle_ticks();
            // Tijdens werk hoort dit bij 0 te liggen (de burst geeft de core
            // alleen met een yield af, nooit aan de slaper), tijdens rust bij
            // 100; ertussenin is de heartbeat, of een core die lekt.
            let idle = clock::idle_pct(idle_ticks.wrapping_sub(last_idle), hz, dt) as u64;
            log!(
                "BURN: {bursts} bursts, {rate} bursts/s, idle={idle}%, phase={}, heap={} KB HOPOS_BENCH_BURN",
                if in_work { "work" } else { "rest" },
                HEAP.stats().used >> 10
            );
            last_bursts = bursts;
            last_idle = idle_ticks;
            next = now.saturating_add(nanos(BURN_EVERY));
        }
    }
}

/// De maat van één THRASH-knoop: 48 bytes, zoals de `ball` van Go (een
/// pointer en 40 bytes).
const NODE: usize = 48;

/// THRASH: de v3-vorm van de GC-doodspiraal. Er is geen GC meer; wat blijft
/// is de vraag of een app die zijn heap tot tegen het plafond vult en dan
/// door-churnt, (1) blijft lopen zonder dat de allocator versnippert tot
/// hij faalt, (2) zijn falen als `Err` ziet en niet als paniek, en (3) de
/// kern niet raakt. De live set is 3/5 van de heap (dezelfde verhouding als
/// Go, om dezelfde reden: dichterbij is het een OOM-toets, geen
/// churn-toets). Elke 2 s een regel met allocaties per seconde en de
/// grootste vrije brok (de fragmentatie).
pub(crate) async fn thrash(app: &'static App) -> ! {
    let cap = HEAP.stats().capacity;
    let target = cap / 5 * 3;
    log!(
        "THRASH: filling a live set of {} MB against a {} MB heap, then churning HOPOS_BENCH_UP role=thrash",
        target >> 20,
        cap >> 20
    );
    let slots = usize::try_from(target / NODE as u64).unwrap_or(0);
    let mut live: Vec<Option<Vec<u8>>> = Vec::new();
    if live.try_reserve_exact(slots).is_err() {
        log!("THRASH: no room for the table of {slots} nodes HOPOS_BENCH_FAIL role=thrash");
        park().await;
    }
    let mut fill_fail = 0u64;
    for _ in 0..slots {
        if HEAP.stats().used >= target {
            break;
        }
        live.push(node());
        if live.last().is_some_and(Option::is_none) {
            fill_fail += 1;
            break;
        }
    }
    let st = HEAP.stats();
    let walk = HEAP.check().map(|w| w.largest_free).unwrap_or(0);
    log!(
        "THRASH: pinned {} nodes, used {} MB, largest free {} KB, fill failures {fill_fail} HOPOS_BENCH_THRASH_PINNED",
        live.len(),
        st.used >> 20,
        walk >> 10
    );
    // De rand: één verzoek van twee keer de heap moet een `Err` zijn, geen
    // paniek en geen kapotte heap.
    let mut probe: Vec<u8> = Vec::new();
    let refused = probe
        .try_reserve_exact(usize::try_from(cap.saturating_mul(2)).unwrap_or(usize::MAX))
        .is_err();
    log!(
        "THRASH: an allocation of twice the heap was {} HOPOS_BENCH_THRASH_OOM",
        if refused {
            "refused cleanly"
        } else {
            "GRANTED (the ceiling does not hold)"
        }
    );
    drop(probe);
    churn(app, live).await
}

/// Eén knoop, of `None` als de heap nee zei.
fn node() -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(NODE).ok()?;
    v.resize(NODE, 0xa5);
    Some(v)
}

/// De churn: telkens een knoop vrijgeven en een nieuwe maken, op een
/// pseudo-willekeurige plek, met een yield per batch.
async fn churn(_app: &'static App, mut live: Vec<Option<Vec<u8>>>) -> ! {
    let n = live.len().max(1);
    let (mut x, mut churned, mut failed) = (0x9e37_79b9_7f4a_7c15u64, 0u64, 0u64);
    let mut last = clock::now_ns();
    let mut last_churned = 0u64;
    loop {
        for _ in 0..512 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = usize::try_from(x % n as u64).unwrap_or(0);
            if let Some(slot) = live.get_mut(i) {
                *slot = None;
                *slot = node();
                if slot.is_none() {
                    failed += 1;
                }
            }
            churned += 1;
        }
        yield_now().await;
        let now = clock::now_ns();
        if now.saturating_sub(last) >= 2_000_000_000 {
            let st = HEAP.stats();
            let rate = (churned - last_churned) * 1_000_000_000 / now.saturating_sub(last).max(1);
            // De wandeling is ook de integriteitstoets: een heap die onder
            // de churn kapot ging, zegt het hier.
            let (largest, blocks, ok) = match HEAP.check() {
                Ok(w) => (w.largest_free, w.blocks, "ok"),
                Err(_) => (0, 0, "CORRUPT"),
            };
            log!(
                "THRASH: {rate} churns/s, used {} MB, peak {} MB, largest free {} KB, blocks {blocks}, heap {ok}, failed {failed}+{} HOPOS_BENCH_THRASH",
                st.used >> 20,
                st.peak >> 20,
                largest >> 10,
                st.failed
            );
            last = now;
            last_churned = churned;
        }
    }
}

/// De mDNS-groep en -poort (het matter-pad).
const MDNS: Endpoint = Endpoint {
    ip: [224, 0, 0, 251],
    port: 5353,
};

/// MCAST: `send` stuurt elke seconde een datagram naar de mDNS-groep;
/// `listen` joint de groep, bindt 5353 en telt wat er binnenkomt.
///
/// Twee benches in één node bewijzen zo de keten: de switch van de kern
/// floodt het frame van de zender naar elk slot, en de stack van de
/// luisteraar laat het door omdat hij de groep joinde
/// (`appnet::join_group`). Zonder join blijft `recv` op 0.
pub(crate) async fn mcast(app: &'static App, role: &str) -> ! {
    if let Err(e) = appnet::up(app) {
        log!("MCAST {role}: network stack: {e} HOPOS_BENCH_FAIL role=mcast");
        park().await;
    }
    match role {
        "send" => mcast_send().await,
        _ => mcast_listen().await,
    }
}

/// Elke seconde één probe naar de groep.
async fn mcast_send() -> ! {
    let udp = match UdpSocket::bind(5354) {
        Ok(u) => u,
        Err(e) => {
            log!("MCAST send: bind: {e} HOPOS_BENCH_FAIL role=mcast");
            park().await;
        }
    };
    log!("MCAST send: probing 224.0.0.251:5353 every second HOPOS_BENCH_UP role=mcast-send");
    let mut i = 0u64;
    loop {
        let mut msg = Text::<32>::new();
        let _ = write!(msg, "mdns-probe {i}");
        match udp.send_to(MDNS, msg.as_bytes()).await {
            Err(e) => log!("MCAST send: {e} HOPOS_BENCH_MCAST_FAIL"),
            Ok(_) if i.is_multiple_of(5) => {
                log!("MCAST send: probe {i} sent to 224.0.0.251:5353 HOPOS_BENCH_MCAST");
            }
            Ok(_) => {}
        }
        i += 1;
        EXEC.after(Duration::from_secs(1)).await;
    }
}

/// De eerste zoveel datagrammen krijgen elk een regel, daarna één per
/// tiende: een mDNS-rijk LAN hoort de console niet te vullen.
const MCAST_LOG_FIRST: u64 = 5;

/// Joint de groep, bindt 5353 en telt elk datagram (`recv=N`).
async fn mcast_listen() -> ! {
    let udp = match UdpSocket::bind(MDNS.port) {
        Ok(u) => u,
        Err(e) => {
            log!("MCAST listen: bind: {e} HOPOS_BENCH_FAIL role=mcast");
            park().await;
        }
    };
    if let Err(e) = appnet::join_group(MDNS.ip) {
        log!("MCAST listen: join 224.0.0.251: {e} HOPOS_BENCH_FAIL role=mcast");
        park().await;
    }
    log!("MCAST listen: joined 224.0.0.251, port 5353 open HOPOS_BENCH_UP role=mcast-listen");
    let mut buf = [0u8; 512];
    let mut recv = 0u64;
    loop {
        match udp.recv_from(&mut buf).await {
            Ok((n, from)) => {
                recv += 1;
                if recv > MCAST_LOG_FIRST && !recv.is_multiple_of(10) {
                    continue;
                }
                let text =
                    core::str::from_utf8(buf.get(..n).unwrap_or_default()).unwrap_or("<bin>");
                let [a, b, c, d] = from.ip;
                log!(
                    "MCAST listen: {text:?} from {a}.{b}.{c}.{d}:{} HOPOS_BENCH_MCAST recv={recv}",
                    from.port
                );
            }
            Err(e) => {
                log!("MCAST listen: {e}");
                EXEC.after(Duration::from_secs(1)).await;
            }
        }
    }
}

/// NETDEMO=out: één naam resolven over de DNS-server uit de env, door de
/// NAT van de kern naar buiten. De exitcode: 0 bij een adres.
pub(crate) async fn netdemo_out(app: &'static App) -> u64 {
    let name = app.env("NETDEMO_NAME").unwrap_or("github.com");
    if let Err(e) = appnet::up(app) {
        log!("NETDEMO out: network stack: {e} HOPOS_BENCH_FAIL role=netdemo");
        return 1;
    }
    let t0 = clock::now_ns();
    match appnet::resolve(name).await {
        Ok([a, b, c, d]) => {
            log!(
                "NETDEMO out: {name} is {a}.{b}.{c}.{d} in {} us, outgoing masquerade works HOPOS_BENCH_NETDEMO",
                clock::now_ns().saturating_sub(t0) / 1000
            );
            0
        }
        Err(e) => {
            log!("NETDEMO out: {name}: {e} HOPOS_BENCH_FAIL role=netdemo");
            1
        }
    }
}

/// Een duur in nanoseconden, geklemd.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}
