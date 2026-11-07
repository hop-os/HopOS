//! Meetapp: HTTP-belasting vanaf een node, over de kabel.
//!
//! `TARGET` (`a.b.c.d:poort`), `PATH` (standaard `/health`), `PHASES`
//! (verbindingen per fase, standaard `1,8,16,32,48`), `SECS` per fase
//! (standaard 8). Elke verbinding is een eigen taak met één keep-alive-
//! verbinding die verzoek na verzoek stuurt; geen timer per taak. Per fase
//! een regel `HTTPLOAD_RESULT` met req/s en de latency (buckets van 10 µs).

#![cfg_attr(target_os = "none", no_std, no_main)]
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
use applib::appnet::{self, TcpStream};
use applib::rt::Exec;
use applib::{App, EXEC, log};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use sync::Signal;

applib::main!(httpload);

#[cfg(not(target_os = "none"))]
fn main() {}

/// Hoogstens zoveel verbindingen (taken; een app-core heeft er 64).
const MAX: usize = 48;
/// Buckets van 10 µs tot 40 ms; de laatste vangt de rest.
const BUCKETS: usize = 4096;
const BUCKET_NS: u64 = 10_000;

static GO: [Signal; MAX] = [const { Signal::new() }; MAX];
static HIST: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
static ERRORS: AtomicU64 = AtomicU64::new(0);
static CONNECT_RETRIES: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);

struct Cfg {
    ip: [u8; 4],
    port: u16,
    req: Vec<u8>,
}

#[expect(
    clippy::expect_used,
    reason = "een meetapp zonder netstack of doel heeft niets te meten"
)]
async fn httpload(app: &'static App) {
    let exec: &'static Exec = EXEC.get();
    let _net = appnet::up(app).expect("httpload: network stack");
    let (ip, port) = appnet::parse_addr(app.env("TARGET").unwrap_or("192.168.1.122:9202"))
        .expect("httpload: TARGET a.b.c.d:port");
    let path = app.env("PATH").unwrap_or("/health");
    let secs: u64 = app.env("SECS").and_then(|s| s.parse().ok()).unwrap_or(8);
    let phases: Vec<usize> = app
        .env("PHASES")
        .unwrap_or("1,8,16,32,48")
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .map(|c: usize| c.min(MAX))
        .collect();
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: load\r\n\r\n");
    let cfg: &'static Cfg = alloc::boxed::Box::leak(alloc::boxed::Box::new(Cfg { ip, port, req }));
    for i in 0..MAX {
        exec.spawn(worker(i, exec, cfg)).expect("httpload: spawn");
    }
    log!("httpload: {MAX} workers, target {ip:?}:{port}{path}, {secs} s per phase HTTPLOAD_UP");
    exec.after(Duration::from_secs(2)).await;
    for c in phases {
        for b in &HIST {
            b.store(0, Relaxed);
        }
        ERRORS.store(0, Relaxed);
        CONNECT_RETRIES.store(0, Relaxed);
        let t0 = exec.now();
        END.store(t0 + secs * 1_000_000_000, Relaxed);
        for g in GO.iter().take(c) {
            g.set();
        }
        exec.after(Duration::from_secs(secs) + Duration::from_millis(500))
            .await;
        let elapsed = exec
            .now()
            .saturating_sub(t0)
            .min(secs * 1_000_000_000)
            .max(1);
        report(c, elapsed);
        exec.after(Duration::from_secs(1)).await;
    }
    log!("httpload: klaar HTTPLOAD_DONE");
    app.stopped().await;
}

/// Verbinding `i`: wacht op zijn bel, stuurt dan verzoeken tot het einde
/// van de fase, en sluit.
async fn worker(i: usize, exec: &'static Exec, cfg: &'static Cfg) {
    let Some(go) = GO.get(i) else { return };
    let mut buf = alloc::vec![0u8; 4096];
    loop {
        go.wait().await;
        // Een mislukte connect (een volle backlog bij een storm van
        // verbindingen) probeert het binnen de fase opnieuw; anders doet
        // deze verbinding de hele fase niet mee.
        let mut conn = None;
        while conn.is_none() && exec.now() < END.load(Relaxed) {
            match TcpStream::connect_timeout(cfg.ip, cfg.port, Duration::from_secs(2)).await {
                Ok(s) => conn = Some(s),
                Err(_) => {
                    CONNECT_RETRIES.fetch_add(1, Relaxed);
                }
            }
        }
        let Some(mut s) = conn else { continue };
        while exec.now() < END.load(Relaxed) {
            let t = exec.now();
            if s.write(&cfg.req).await.is_err() || !response(&mut s, &mut buf).await {
                ERRORS.fetch_add(1, Relaxed);
                break;
            }
            let ns = exec.now().saturating_sub(t);
            let b = usize::try_from(ns / BUCKET_NS)
                .unwrap_or(BUCKETS - 1)
                .min(BUCKETS - 1);
            if let Some(h) = HIST.get(b) {
                h.fetch_add(1, Relaxed);
            }
        }
        let _ = s.close();
    }
}

/// Leest één antwoord: de kop tot de lege regel, dan `Content-Length`
/// bytes body.
async fn response(s: &mut TcpStream, buf: &mut [u8]) -> bool {
    let mut have = 0;
    loop {
        let Some(free) = buf.get_mut(have..) else {
            return false;
        };
        match s.read(free).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => have += n,
        }
        let got = buf.get(..have).unwrap_or(&[]);
        let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = core::str::from_utf8(got.get(..end).unwrap_or(&[])).unwrap_or("");
        let len: usize = head
            .split("\r\n")
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        if have >= end + 4 + len {
            return true;
        }
    }
}

fn report(c: usize, elapsed_ns: u64) {
    let total: u64 = HIST.iter().map(|b| b.load(Relaxed)).sum();
    let at = |q: u64| -> u64 {
        let want = (total * q).div_ceil(1000).max(1);
        let mut seen = 0;
        for (i, b) in HIST.iter().enumerate() {
            seen += b.load(Relaxed);
            if seen >= want {
                return (u64::try_from(i).unwrap_or(0) + 1) * BUCKET_NS / 1000;
            }
        }
        0
    };
    let rps = total * 1_000_000_000 / elapsed_ns;
    log!(
        "httpload: HTTPLOAD_RESULT c={c} req={total} rps={rps} p50_us<={} p90_us<={} p99_us<={} errors={} connect_retries={}",
        at(500),
        at(900),
        at(990),
        ERRORS.load(Relaxed),
        CONNECT_RETRIES.load(Relaxed)
    );
}
