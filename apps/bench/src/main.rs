//! bench: de meetbank in het slot. De server-kant van `tools/netmeter` en
//! de meetrollen uit de Go-appspike, gekozen via de env.
//!
//! | Env | Rol | Marker |
//! | --- | --- | --- |
//! | (niets), of `BENCH=serve` | TCP echo, sink en source plus UDP-echo op `ER_PORT_HTTP`, `BENCH_PORT` of 9000 ([`serve`]) | `HOPOS_BENCH_UP role=serve` |
//! | `BENCH=ping BENCH_PEER=ip:poort` | rtt tegen een andere bench in de node, warm en koud ([`client`]) | `HOPOS_BENCH_RTT`, `HOPOS_BENCH_COLD` |
//! | `BENCH=pull`, `BENCH=push` (met `BENCH_PEER`, `BENCH_BYTES`) | doorvoer app naar app door de switch | `HOPOS_BENCH_PULL`, `HOPOS_BENCH_PUSH` |
//! | `BENCH=disk` | hopfs door de system-API: schrijven, lezen, willekeurig 4 KiB ([`disk`]) | `HOPOS_BENCH_DISK` |
//! | `BURN=1` (`BURN_WORK`, `BURN_REST`) | rekenen in een werk/rust-ritme ([`load::burn`]) | `HOPOS_BENCH_BURN` |
//! | `BURN=1 BENCH=serve` | rekenen náást de server, op dezelfde core: de rtt van een bezette app (`BENCH=ping` ertegen) | `HOPOS_BENCH_BURN`, `HOPOS_BENCH_UP role=serve` |
//! | `THRASH=1` | de heap tot 3/5 vullen en churnen ([`load::thrash`]) | `HOPOS_BENCH_THRASH` |
//! | `MCAST=send` of `MCAST=listen` | mDNS-groep 224.0.0.251:5353 ([`load::mcast`]); `listen` joint de groep | `HOPOS_BENCH_MCAST`, bij `listen` met `recv=N` |
//! | `NETDEMO=out` (`NETDEMO_NAME`) | één DNS-vraag door de NAT naar buiten | `HOPOS_BENCH_NETDEMO` |
//!
//! Van de Go-rollen vielen weg: `NETDEMO=listen` en `dial` (dat zijn nu
//! `serve` en `ping`), `outkeep` (de NAT-mapping leeft in de kern; de soak
//! toetst hem), `HANG` (een test van de kill, niet van een meting) en de
//! SMP-bench (applib heeft één app-core; die rol komt met de SMP-app).
//!
//! Een client-rol blijft na zijn meting staan (`HOPOS_BENCH_HOLD`), zodat
//! Hop hem niet herstart; met `BENCH_EXIT=1` stopt hij met code 0 als de
//! meting slaagde, anders 1. Een server- of lastrol stopt nooit.

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

mod client;
mod disk;
mod load;
mod proto;
mod serve;

applib::main!(bench);

/// Op de host bestaat dit image niet: daar is dit een lege binary, zodat de
/// host-poort het protocol kan toetsen en clippy de rest kan lezen.
#[cfg(not(target_os = "none"))]
fn main() {}

/// De poort zonder `ER_PORT_HTTP` en `BENCH_PORT`.
const DEFAULT_PORT: u16 = 9000;

/// De rol uit de env en de rol draaien.
async fn bench(app: &'static applib::App) {
    use applib::log;
    if app.env("BURN").is_some_and(|v| !v.is_empty()) {
        if app.env("BENCH") != Some("serve") {
            load::burn(app).await;
        }
        // Naast de server: deze core slaapt dan niet, en de pomp ziet RX
        // zonder deurbel (applib appnet, de ronde van `RxPoll::lo`).
        if applib::EXEC
            .spawn(async move { load::burn(app).await })
            .is_err()
        {
            log!("bench: no task for BURN next to serve HOPOS_BENCH_FAIL");
        }
    }
    if app.env("THRASH").is_some_and(|v| !v.is_empty()) {
        load::thrash(app).await;
    }
    if let Some(role) = app.env("MCAST") {
        load::mcast(app, role).await;
    }
    let code = match (app.env("NETDEMO"), app.env("BENCH")) {
        (Some("out"), _) => load::netdemo_out(app).await,
        (_, Some("ping")) => client::run(app, client::Role::Ping).await,
        (_, Some("pull")) => client::run(app, client::Role::Pull).await,
        (_, Some("push")) => client::run(app, client::Role::Push).await,
        (_, Some("disk")) => disk::run(app).await,
        (_, None | Some("serve" | "")) => {
            let env = app.env("ER_PORT_HTTP").or_else(|| app.env("BENCH_PORT"));
            let port = applib::app::port_of(env, DEFAULT_PORT);
            sync::select(app.stopped(), serve::run(app, port)).await;
            0
        }
        (_, Some(other)) => {
            log!("bench: BENCH={other:?} is not serve, ping, pull, push or disk HOPOS_BENCH_FAIL");
            1
        }
    };
    // Een meting die klaar is, blijft staan: Hop herstart een service die
    // stopt, en een pull die elke paar seconden opnieuw loopt, meet vooral
    // zichzelf (en vult de console). `BENCH_EXIT=1` stopt wel, met de code.
    if app.env("BENCH_EXIT") != Some("1") {
        log!("bench: done with code {code}, holding until the job is stopped HOPOS_BENCH_HOLD");
        app.stopped().await;
    }
    app.shutdown(code).await;
}
