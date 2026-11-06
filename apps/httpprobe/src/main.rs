//! Meetapp: dezelfde routes en dezelfde 16 werkers op twee manieren.
//!
//! - `MODE=pool`: zoals welcome en Meet tot 06-11-2026: `Doors`, een werker
//!   houdt zijn keep-alive-verbinding vast, met een leeskap van 5 s;
//! - `MODE=park` (standaard): `applibhttp`, stille verbindingen in het park.
//!
//! De poort is `ER_PORT_HTTP` (standaard 9200). Elke 10 s een regel
//! `HTTPPROBE_STATS` met de tellers van het park en de timer-overflows.

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

use alloc::boxed::Box;
use applib::app::port_of;
use applib::appnet::{self, TcpListener, TcpStream};
use applib::rt::Exec;
use applib::tcp::TcpConn;
use applib::{App, EXEC, log};
use core::sync::atomic::Ordering::Relaxed;
use core::time::Duration;
use leanhttp::{Exchange, Found, Mux, Outcome};
use sync::{Doors, select};

applib::main!(httpprobe);

#[cfg(not(target_os = "none"))]
fn main() {}

const WORKERS: usize = 16;
const PARK: usize = 4096;
const READ_CAP: Duration = Duration::from_secs(5);

static HTTP: applibhttp::Http<WORKERS, PARK> = applibhttp::Http::new();
static DOORS: Doors<TcpStream, WORKERS> = Doors::new();

/// De routes van de bench op de host (scratchpad/muxbench): de echte tabel
/// uit leanhttp's muxtest plus een API-achtige.
fn routes() -> Mux<u16> {
    let mut m = Mux::new();
    let small = [
        "/health",
        "/logs/",
        "GET /api/devices",
        "POST /api/devices",
        "DELETE /api/devices/",
        "GET /api/devices/{id}",
        "GET /api/devices/{id}/history",
        "GET /app-ui/{app}/settings/{path...}",
        "GET /app-ui/{app}/pair/{driver}/{path...}",
    ];
    let mut n = 0u16;
    for p in small {
        let _ = m.handle(p, n);
        n += 1;
    }
    for r in [
        "users", "repos", "orgs", "teams", "issues", "pulls", "gists", "apps", "hooks", "keys",
        "events", "projects",
    ] {
        for (method, tail) in [
            ("GET", ""),
            ("POST", ""),
            ("GET", "/{id}"),
            ("DELETE", "/{id}"),
            ("GET", "/{id}/events"),
        ] {
            let _ = m.handle(&alloc::format!("{method} /v1/{r}{tail}"), n);
            n += 1;
        }
    }
    m
}

async fn handle(ex: &mut Exchange<'_, TcpConn>, mux: &Mux<u16>) -> leanhttp::Result {
    match mux.find(&mut ex.req)? {
        Found::Route(_) => {
            ex.header_mut().set("Content-Type", "text/plain")?;
            ex.write(b"ok").await.map(|_| ())
        }
        Found::NotFound => ex.error(404, "not found").await,
        Found::MethodNotAllowed(_) => ex.error(405, "method not allowed").await,
    }
}

#[expect(
    clippy::expect_used,
    reason = "de start van een meetapp: zonder netstack of poort is er niets te meten"
)]
async fn httpprobe(app: &'static App) {
    let exec: &'static Exec = EXEC.get();
    let port = port_of(app.env("ER_PORT_HTTP"), 9200);
    let _net = appnet::up(app).expect("httpprobe: network stack");
    let listener = TcpListener::bind(port).expect("httpprobe: listen");
    let mux: &'static Mux<u16> = Box::leak(Box::new(routes()));
    let handler: &'static _ = Box::leak(Box::new(
        async move |ex: &mut Exchange<'_, TcpConn>| -> leanhttp::Result { handle(ex, mux).await },
    ));
    let pool = app.env("MODE") == Some("pool");
    let _ = exec.spawn(report(exec, pool));
    if pool {
        for i in 0..WORKERS {
            exec.spawn(worker(i, exec, handler))
                .expect("httpprobe: spawn worker");
        }
        log!("httpprobe: pool, {WORKERS} workers, port {port} HTTPPROBE_UP mode=pool port={port}");
        select(app.stopped(), accept(listener, exec)).await;
    } else {
        log!(
            "httpprobe: park, {WORKERS} workers, park {PARK}, port {port} HTTPPROBE_UP mode=park port={port}"
        );
        select(app.stopped(), HTTP.run(exec, listener, handler)).await;
    }
}

/// De pool zoals welcome en Meet: de acceptor wacht op een vrije werker.
async fn accept(listener: TcpListener, exec: &'static Exec) {
    loop {
        match listener.accept().await {
            Ok(stream) => DOORS.place(stream).await,
            Err(_) => exec.after(Duration::from_millis(100)).await,
        }
    }
}

/// Een werker die zijn keep-alive-verbinding vasthoudt (met de leeskap).
async fn worker<H>(i: usize, exec: &'static Exec, handler: &'static H)
where
    H: AsyncFn(&mut Exchange<'_, TcpConn>) -> leanhttp::Result,
{
    loop {
        let stream = DOORS.take(i).await;
        let conn = TcpConn::new(stream, exec).with_read_cap(READ_CAP);
        let mut out = leanhttp::serve(conn, handler).await;
        while let Ok(Outcome::Kept(k)) = out {
            out = k.serve(handler).await;
        }
        DOORS.free(i);
    }
}

async fn report(exec: &'static Exec, pool: bool) {
    loop {
        exec.after(Duration::from_secs(10)).await;
        let s = HTTP.stats();
        log!(
            "httpprobe: HTTPPROBE_STATS mode={} parked={} idle_closed={} park_full={} calls={} timer_overflows={}",
            if pool { "pool" } else { "park" },
            s.parked,
            s.idle_closed,
            s.park_full,
            s.calls,
            exec.stats.timer_overflows.load(Relaxed)
        );
    }
}
