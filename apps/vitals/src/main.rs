//! vitals: de board-dokter. Eén app die de vitale functies van een node
//! meet en benchmarkt: idle-gedrag, rekenen (ook volgehouden, met de
//! temperatuur ernaast), geheugen, netwerk, schijf en timers. Voor bring-up
//! en diagnose van een nieuw board: plaats vitals, open de pagina, druk op
//! **Run all**, en vergelijk het rapport met een gezond board.
//!
//! De port van `OLD/apps/vitals` (Go). De tabel van tests staat in
//! [`run::Test`] en in de README; wat wegviel (sqlite, gc, ramp, push,
//! syscall) staat daar met het waarom.
//!
//! Eén HTTP-server (leanhttp over `appnet::TcpListener`) op de poort uit
//! `ER_PORT_HTTP` (standaard 8080):
//!
//! ```text
//! GET  /               de pagina: Run all, per test een knop, de cijfers
//! GET  /api/state      alles als JSON (node, idle, temperatuur, resultaten)
//! GET  /api/run?test=  een test starten, of test=all; 409 als er een loopt
//! GET  /ping           "pong": het doelwit van de storm-test
//! GET  /blob?mb=N      N MB naar de client, de zendkant ("tx")
//! PUT  /sink           een body tot 1 MiB, de ontvangkant ("up")
//! GET  /health         "ok"
//! ```
//!
//! De vorm is die van welcome (handboek §2): `applibhttp`, één acceptor met
//! een park voor stille keep-alive-verbindingen en een vaste pool van
//! [`WORKERS`] werkers, elk voor één verzoek tegelijk. Een test draait als
//! eigen taak ([`run::start`]), één tegelijk; de rekentests over meer cores
//! zetten hun werk met `smp::spawn_on` op de andere cores van de app.
//!
//! Markers: `HOPOS_VITALS_UP port=<n>` als de listener staat, en per test
//! `HOPOS_VITALS_<TEST>` met de meetwaarden als `key=value` (docs/measurements.md,
//! tabel Vitals).

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

mod cpu;
mod disk;
mod idle;
mod mem;
mod net;
mod page;
mod report;
mod run;

use alloc::boxed::Box;
use alloc::vec::Vec;
use applib::app::port_of;
use applib::appnet::{self, Net, TcpListener};
use applib::rt::Exec;
use applib::tcp::TcpConn;
use applib::{App, EXEC, clock, log};
use applibhttp::Http;
use leanhttp::{Exchange, Found, Mux};
use sync::{Either, select};

applib::main!(vitals);

/// Op de host bestaat dit image niet: daar is dit een lege binary, zodat de
/// host-poort de rekenkant kan toetsen en clippy de rest kan lezen.
#[cfg(not(target_os = "none"))]
fn main() {}

/// De poort zonder `ER_PORT_HTTP`, zoals de Go-vitals.
const DEFAULT_PORT: u16 = 8080;

/// De werkers: zoveel verzoeken tegelijk. Vier voor een browser, en ruimte
/// voor de storm-test, die zijn eigen poort vanuit hetzelfde slot bestormt
/// (client én server hier). Een stille keep-alive-verbinding telt hier
/// niet mee: die wacht in het park.
const WORKERS: usize = 8;

/// Hoeveel stille keep-alive-verbindingen het park hoogstens houdt (elk
/// hoogstens [`applibhttp::IDLE`]). Een verbinding kost minstens 20 KiB
/// netbudget; een vitals van 128 MB heeft het grootste budget (16 MiB,
/// [`appnet::BUDGET_MAX`]), ruim 800 verbindingen. 512 laat daarvan ruimte
/// voor de clients van de tests (storm, rx) en verse verbindingen; op een
/// kleiner budget is de stack de grens, niet het park. Het park groeit pas
/// als er verbindingen zijn.
const PARK: usize = 512;

/// De maat van het patroonblok dat `/blob` en de disk-test herhalen.
const BLOB: usize = 256 << 10;

/// De versie van dit image (die van de werkruimte).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// De architectuur, voor de pagina.
const ARCH: &str = if cfg!(target_arch = "riscv64") {
    "riscv64"
} else {
    "arm64"
};

/// De server (zie welcome): met alle werkers bezet wacht het volgende
/// verzoek in de acceptor op de eerste die vrijkomt.
static HTTP: Http<WORKERS, PARK> = Http::new();

/// De paden van deze server.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Route {
    /// `GET /{$}`: precies `/`.
    Page,
    /// `GET /health`.
    Health,
    /// `GET /api/state`.
    State,
    /// `GET /api/run`.
    Run,
    /// `GET /ping`.
    Ping,
    /// `GET /blob`.
    Blob,
    /// `PUT /sink`, `POST /sink`.
    Sink,
}

/// De routetabel.
const ROUTES: [(&str, Route); 9] = [
    ("GET /{$}", Route::Page),
    ("GET /health", Route::Health),
    ("GET /api/state", Route::State),
    ("GET /api/run", Route::Run),
    ("GET /ping", Route::Ping),
    ("GET /blob", Route::Blob),
    ("PUT /sink", Route::Sink),
    ("POST /sink", Route::Sink),
    ("GET /sink", Route::Sink),
];

/// Wat elke werker en elke test deelt: de feiten van de start. Gezet in
/// [`vitals`] vóór de eerste spawn en daarna alleen gelezen.
pub(crate) struct Shared {
    /// De executor van core 0.
    pub(crate) exec: &'static Exec,
    /// Het handvat van de app.
    pub(crate) app: &'static App,
    /// De netstack.
    pub(crate) net: &'static Net,
    /// De naam van de node (`ER_ATTR_NODE_ID` of `HOPOS_NODE`).
    pub(crate) node: &'static str,
    /// Het slot-IP.
    pub(crate) ip: [u8; 4],
    /// De gepubliceerde poort.
    pub(crate) port: u16,
    /// De bron van de rx-test.
    pub(crate) rx_url: &'static str,
    /// Het doel van de storm-test als `ip:poort`, of `None` voor de eigen
    /// poort op het eigen adres.
    pub(crate) storm: Option<&'static str>,
    /// De start van de app, in ns.
    pub(crate) started_ns: u64,
    /// Het patroonblok van [`BLOB`] bytes.
    pub(crate) blob: &'static [u8],
    mux: Mux<Route>,
}

impl Shared {
    /// Nanoseconden sinds de start van de app.
    pub(crate) fn uptime_ns(&self) -> u64 {
        clock::now_ns().saturating_sub(self.started_ns)
    }
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "de start van de bin: zonder netstack, poort, patroonblok of werker is er niets te meten, en een luide paniek met reden is het goede einde"
)]
async fn vitals(app: &'static App) {
    let exec: &'static Exec = EXEC.get();
    let port = port_of(app.env("ER_PORT_HTTP"), DEFAULT_PORT);
    let net = appnet::up(app).expect("vitals: network stack");
    let listener = TcpListener::bind(port).expect("vitals: listen on ER_PORT_HTTP");
    let mut mux = Mux::default();
    for (pattern, route) in ROUTES {
        mux.handle(pattern, route).expect("vitals: route");
    }
    let node = app
        .env("ER_ATTR_NODE_ID")
        .or_else(|| app.env("HOPOS_NODE"))
        .unwrap_or("this node");
    let mut blob = Vec::new();
    blob.try_reserve_exact(BLOB).expect("vitals: pattern block");
    blob.resize(BLOB, 0);
    fill_pattern(&mut blob);
    // Eén keer bij de start, voor de levensduur van de app: de werkers en
    // de tests lenen het, niemand schrijft het nog.
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        exec,
        app,
        net,
        node,
        ip: net.ip(),
        port,
        rx_url: app
            .env("VITALS_RX_URL")
            .filter(|u| !u.is_empty())
            .unwrap_or(net::DEFAULT_RX_URL),
        storm: app.env("VITALS_STORM_ADDR").filter(|a| !a.is_empty()),
        started_ns: clock::now_ns(),
        blob: Box::leak(blob.into_boxed_slice()),
        mux,
    }));
    exec.spawn(idle::sample(app))
        .expect("vitals: spawn idle sampler");
    // De werkers lenen de handler voor de levensduur van de app. Een
    // verbinding die eindigt met een termijn of een reset is een client die
    // wegging; applibhttp sluit haar zonder logregel.
    let handler: &'static _ = Box::leak(Box::new(
        async move |ex: &mut Exchange<'_, TcpConn>| -> leanhttp::Result {
            handle(ex, shared).await
        },
    ));
    let [a, b, c, d] = shared.ip;
    log!(
        "vitals {VERSION}: serving http on {a}.{b}.{c}.{d}:{port} for node {node}, slot {}, {ARCH}, {} core(s) HOPOS_VITALS_UP port={port}",
        app.slot(),
        app.cores()
    );
    // Tot de kern vraagt te stoppen; de main-schil doet dan het
    // net-afscheid (elke verbinding dicht) en de exit. `run` keert alleen
    // terug als er geen enkele werker kwam (de takenlijst vol).
    if let Either::Right(_) = select(app.stopped(), HTTP.run(exec, listener, handler)).await {
        panic!("vitals: spawn worker");
    }
}

/// Vult `buf` met een xorshift-patroon: de inhoud doet er niet toe (niemand
/// pakt hem uit), maar hij comprimeert niet en een verschoven lees valt op.
fn fill_pattern(buf: &mut [u8]) {
    let mut r: u64 = 2_463_534_242;
    for b in buf {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        *b = r.to_le_bytes()[0];
    }
}

/// Eén verzoek.
async fn handle(ex: &mut Exchange<'_, TcpConn>, shared: &'static Shared) -> leanhttp::Result {
    match shared.mux.find(&mut ex.req)? {
        Found::Route(Route::Page) => {
            ex.header_mut()
                .set("Content-Type", "text/html; charset=utf-8")?;
            ex.header_mut().set("Cache-Control", "no-store")?;
            ex.write(page::HTML.as_bytes()).await?;
            Ok(())
        }
        Found::Route(Route::Health) => text(ex, "ok\n").await,
        Found::Route(Route::Ping) => text(ex, "pong\n").await,
        Found::Route(Route::State) => state(ex, shared).await,
        Found::Route(Route::Run) => start(ex, shared).await,
        Found::Route(Route::Blob) => net::serve_blob(ex, shared).await,
        Found::Route(Route::Sink) if ex.req.method != "GET" && ex.req.method != "HEAD" => {
            net::serve_sink(ex, shared).await
        }
        Found::Route(Route::Sink) => {
            ex.header_mut().set("Allow", "PUT, POST")?;
            ex.error(
                405,
                "send bodies of up to 1 MiB here: apps/vitals/perf.sh, or PUT /sink",
            )
            .await
        }
        Found::NotFound => ex.error(404, "not found").await,
        Found::MethodNotAllowed(allow) => {
            ex.header_mut().set("Allow", &allow)?;
            ex.error(405, "method not allowed").await
        }
    }
}

/// Een kort antwoord in platte tekst.
async fn text(ex: &mut Exchange<'_, TcpConn>, body: &str) -> leanhttp::Result {
    ex.header_mut()
        .set("Content-Type", "text/plain; charset=utf-8")?;
    ex.write(body.as_bytes()).await?;
    Ok(())
}

/// `GET /api/state`: alles als JSON.
async fn state(ex: &mut Exchange<'_, TcpConn>, shared: &'static Shared) -> leanhttp::Result {
    let Some(body) = run::state_json(shared) else {
        return ex.error(503, "state does not fit").await;
    };
    ex.header_mut().set("Content-Type", "application/json")?;
    ex.header_mut().set("Cache-Control", "no-store")?;
    ex.write(&body).await?;
    Ok(())
}

/// `GET /api/run?test=<naam>|all`: start een test, één tegelijk.
async fn start(ex: &mut Exchange<'_, TcpConn>, shared: &'static Shared) -> leanhttp::Result {
    let params = run::Params::parse(|k| ex.req.query(k).ok().flatten());
    let name = ex.req.query("test").ok().flatten().unwrap_or_default();
    match run::start(shared, &name, params) {
        Ok(started) => {
            ex.header_mut().set("Content-Type", "application/json")?;
            let mut out = bounded::Text::<64>::new();
            let _ = core::fmt::Write::write_fmt(
                &mut out,
                format_args!("{{\"started\":\"{started}\"}}\n"),
            );
            ex.write(out.as_str().as_bytes()).await?;
            Ok(())
        }
        Err(run::StartError::Unknown) => ex.error(404, "unknown test").await,
        Err(run::StartError::Busy(running)) => {
            let mut msg = bounded::Text::<64>::new();
            let _ = core::fmt::Write::write_fmt(
                &mut msg,
                format_args!("test {running:?} is still running"),
            );
            ex.error(409, msg.as_str()).await
        }
        Err(run::StartError::Spawn) => ex.error(503, "no room for the test task").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mux_routes_every_path() {
        let mut mux = Mux::default();
        for (pattern, route) in ROUTES {
            mux.handle(pattern, route).unwrap();
        }
        for (path, want) in [
            ("/", Route::Page),
            ("/api/state", Route::State),
            ("/api/run", Route::Run),
            ("/ping", Route::Ping),
            ("/blob", Route::Blob),
            ("/health", Route::Health),
        ] {
            let mut req = leanhttp::Request::new("GET", path).unwrap();
            assert_eq!(mux.find(&mut req).unwrap(), Found::Route(&want), "{path}");
        }
        let mut req = leanhttp::Request::new("PUT", "/sink").unwrap();
        assert_eq!(mux.find(&mut req).unwrap(), Found::Route(&Route::Sink));
        let mut req = leanhttp::Request::new("DELETE", "/api/state").unwrap();
        assert!(matches!(
            mux.find(&mut req).unwrap(),
            Found::MethodNotAllowed(_)
        ));
        let mut req = leanhttp::Request::new("POST", "/").unwrap();
        assert_eq!(
            mux.find(&mut req).unwrap(),
            Found::MethodNotAllowed("GET, HEAD".into())
        );
        // `{$}` is precies `/`: een onbekend pad is 404, ook met een andere
        // methode (tot 06-11-2026 was `/` een subtree en dat een 405).
        for (method, path) in [("GET", "/nope"), ("POST", "/foo"), ("GET", "/ping/")] {
            let mut req = leanhttp::Request::new(method, path).unwrap();
            assert_eq!(
                mux.find(&mut req).unwrap(),
                Found::NotFound,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn the_pattern_does_not_repeat_within_a_block() {
        let mut b = vec![0u8; 4096];
        fill_pattern(&mut b);
        assert_ne!(b[..2048], b[2048..]);
        assert!(b.iter().any(|&x| x != 0));
    }
}
