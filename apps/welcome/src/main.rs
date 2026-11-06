//! welcome: de pagina die een node een gezicht geeft, de eerste Rust-app
//! van HopOS v3.
//!
//! Eén HTTP-server (leanhttp over `appnet::TcpListener`) op de poort uit
//! `ER_PORT_HTTP` (standaard 80). `GET /` geeft per verzoek één pagina
//! ([`page`]): de bunny in tekst, de naam van de node en het slot, de
//! uptime, het aantal verzoeken en de heap; `GET /health` geeft 200 met
//! `ok`. De kern zet de poort van de jobspec op de uplink door naar dit slot
//! (DNAT), dus een browser op het LAN ziet de pagina op het adres van de
//! node.
//!
//! De vorm (handboek §2): `applibhttp`, één acceptor met een park voor
//! stille keep-alive-verbindingen en een vaste pool van [`WORKERS`]
//! werkers. Een werker bedient één verzoek en geeft de verbinding dan terug
//! aan het park; een stille browser houdt dus geen werker vast. Geen taak
//! en geen timer per verbinding.
//!
//! Markers: `HOPOS_WELCOME_UP port=<n>` als de listener staat, en om de
//! [`LOG_EVERY`] verzoeken één regel `HOPOS_WELCOME_REQUESTS`. Per verzoek
//! niets: logs over de outbox zijn goedkoop, maar een pagina die elke
//! seconde herladen wordt, hoort de console niet te vullen.

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

mod page;

use alloc::boxed::Box;
use alloc::vec::Vec;
use applib::app::port_of;
use applib::appnet::{self, TcpListener};
use applib::rt::Exec;
use applib::tcp::TcpConn;
use applib::{App, EXEC, clock, heap::HEAP, log};
use applibhttp::Http;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use leanhttp::{Exchange, Found, Mux};
use sync::{Either, select};

applib::main!(welcome);

/// Op de host bestaat dit image niet: daar is dit een lege binary, zodat de
/// host-poort de pagina kan toetsen en clippy de rest kan lezen.
#[cfg(not(target_os = "none"))]
fn main() {}

/// De poort zonder `ER_PORT_HTTP` (een image buiten Hop om).
const DEFAULT_PORT: u16 = 80;

/// De werkers: zoveel verzoeken tegelijk. Een browser opent er twee tot
/// zes naar één host; vier dekt één lezer ruim, en de rest wacht kort op de
/// eerste die vrijkomt. Een stille keep-alive-verbinding telt hier niet
/// mee: die wacht in het park.
const WORKERS: usize = 4;

/// Hoeveel stille keep-alive-verbindingen het park hoogstens houdt (elk
/// hoogstens [`applibhttp::IDLE`]); een verbinding daarboven gaat dicht na
/// haar antwoord. Een verbinding kost minstens 20 KiB netbudget, en het
/// budget van een welcome van 16 tot 32 MB (een achtste, `NET_BUDGET`) is
/// 2 tot 4 MiB: 100 tot 200 verbindingen. 256 laat de stack de grens zijn,
/// niet het park; het park groeit pas als er verbindingen zijn.
const PARK: usize = 256;

/// Om de zoveel verzoeken één logregel.
const LOG_EVERY: u64 = 100;

/// De versie van dit image (die van de werkruimte).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Verzoeken sinds de start, alle paden. Een teller, dus een atomic
/// (handboek §1.3).
static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// De server: de acceptor met het park en de werkers. Zijn alle werkers
/// bezet, dan wacht het volgende verzoek in de acceptor op de eerste die
/// vrijkomt.
static HTTP: Http<WORKERS, PARK> = Http::new();

/// De paden van deze server.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Route {
    /// `GET /health`: 200, `ok`.
    Health,
    /// `GET /{$}`: de pagina, precies op `/`; een ander pad is 404.
    Page,
}

/// Wat elke werker deelt: de feiten van de start en de routetabel. Gezet
/// in [`welcome`] vóór de eerste spawn en daarna alleen gelezen.
struct Shared {
    node: &'static str,
    slot: u64,
    ip: [u8; 4],
    port: u16,
    ram: u64,
    started_ns: u64,
    mux: Mux<Route>,
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "de start van de bin: zonder netstack, poort of werker is er niets te serveren, en een luide paniek met reden is het goede einde"
)]
async fn welcome(app: &'static App) {
    let exec: &'static Exec = EXEC.get();
    let port = port_of(app.env("ER_PORT_HTTP"), DEFAULT_PORT);
    let net = appnet::up(app).expect("welcome: network stack");
    let listener = TcpListener::bind(port).expect("welcome: listen on ER_PORT_HTTP");
    let mut mux = Mux::default();
    mux.handle("GET /health", Route::Health)
        .expect("welcome: route /health");
    mux.handle("GET /{$}", Route::Page)
        .expect("welcome: route /");
    let node = app
        .env("ER_ATTR_NODE_ID")
        .or_else(|| app.env("HOPOS_NODE"))
        .unwrap_or("this node");
    // Eén keer bij de start, en voor de levensduur van de app: de werkers
    // lenen het, niemand schrijft het nog.
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        node,
        slot: app.slot(),
        ip: net.ip(),
        port,
        ram: app.ram_size(),
        started_ns: clock::now_ns(),
        mux,
    }));
    // De werkers lenen de handler voor de levensduur van de app. Een
    // verbinding die eindigt met een termijn of een reset is een browser
    // die wegging; applibhttp sluit haar zonder logregel.
    let handler: &'static _ = Box::leak(Box::new(
        async move |ex: &mut Exchange<'_, TcpConn>| -> leanhttp::Result {
            handle(ex, shared).await
        },
    ));
    let [a, b, c, d] = shared.ip;
    log!(
        "welcome: serving http on {a}.{b}.{c}.{d}:{port} for node {node}, slot {}, {WORKERS} workers HOPOS_WELCOME_UP port={port}",
        shared.slot
    );
    // Tot de kern vraagt te stoppen; de main-schil doet dan het
    // net-afscheid (elke verbinding dicht) en de exit. `run` keert alleen
    // terug als er geen enkele werker kwam (de takenlijst vol).
    if let Either::Right(_) = select(app.stopped(), HTTP.run(exec, listener, handler)).await {
        panic!("welcome: spawn worker");
    }
}

/// Eén verzoek.
async fn handle(ex: &mut Exchange<'_, TcpConn>, shared: &Shared) -> leanhttp::Result {
    let n = REQUESTS.fetch_add(1, Relaxed).wrapping_add(1);
    if n.is_multiple_of(LOG_EVERY) {
        let st = HEAP.stats();
        log!(
            "welcome: requests={n} heap_used={} heap_peak={} HOPOS_WELCOME_REQUESTS",
            st.used,
            st.peak
        );
    }
    match shared.mux.find(&mut ex.req)? {
        Found::Route(Route::Health) => {
            ex.header_mut()
                .set("Content-Type", "text/plain; charset=utf-8")?;
            ex.write(b"ok\n").await?;
            Ok(())
        }
        Found::Route(Route::Page) => serve_page(ex, shared, n).await,
        Found::NotFound => ex.error(404, "not found").await,
        Found::MethodNotAllowed(allow) => {
            ex.header_mut().set("Allow", &allow)?;
            ex.error(405, "method not allowed").await
        }
    }
}

/// De pagina, met de getallen van dit moment.
async fn serve_page(
    ex: &mut Exchange<'_, TcpConn>,
    shared: &Shared,
    requests: u64,
) -> leanhttp::Result {
    let mut out = Vec::new();
    if out.try_reserve_exact(page::PAGE_CAP).is_err() {
        return ex.error(503, "out of memory").await;
    }
    let st = HEAP.stats();
    let facts = page::Facts {
        node: shared.node,
        slot: shared.slot,
        ip: shared.ip,
        port: shared.port,
        ram: shared.ram,
        version: VERSION,
    };
    let live = page::Live {
        host: ex.req.header.get("Host").unwrap_or(""),
        uptime_ns: clock::now_ns().wrapping_sub(shared.started_ns),
        requests,
        heap_used: st.used,
        heap_peak: st.peak,
        heap_capacity: st.capacity,
        allocs: st.allocs,
    };
    if page::render(&facts, &live, &mut out).is_err() {
        return ex.error(500, "page does not fit").await;
    }
    ex.header_mut()
        .set("Content-Type", "text/html; charset=utf-8")?;
    ex.header_mut().set("Cache-Control", "no-store")?;
    ex.write(&out).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mux_routes_health_and_the_page() {
        let mut mux = Mux::default();
        mux.handle("GET /health", Route::Health).unwrap();
        mux.handle("GET /{$}", Route::Page).unwrap();
        let mut req = leanhttp::Request::new("GET", "/health").unwrap();
        assert_eq!(mux.find(&mut req).unwrap(), Found::Route(&Route::Health));
        let mut req = leanhttp::Request::new("GET", "/").unwrap();
        assert_eq!(mux.find(&mut req).unwrap(), Found::Route(&Route::Page));
        let mut req = leanhttp::Request::new("HEAD", "/").unwrap();
        assert_eq!(mux.find(&mut req).unwrap(), Found::Route(&Route::Page));
        let mut req = leanhttp::Request::new("POST", "/").unwrap();
        assert_eq!(
            mux.find(&mut req).unwrap(),
            Found::MethodNotAllowed("GET, HEAD".into())
        );
        // `{$}` is precies `/`: wat eronder hangt is 404, ook met een andere
        // methode (tot 06-11-2026 was `/` een subtree en dat een 405).
        for (method, path) in [("GET", "/x"), ("GET", "/health/"), ("POST", "/foo")] {
            let mut req = leanhttp::Request::new(method, path).unwrap();
            assert_eq!(
                mux.find(&mut req).unwrap(),
                Found::NotFound,
                "{method} {path}"
            );
        }
    }
}
