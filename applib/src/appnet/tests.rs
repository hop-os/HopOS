//! Twee stacks aan één draad: de "kern" op 10.100.0.1 en een app op slot 1,
//! elk met een eigen pomp-taak over een [`Nic`] op gewone buffers, op één
//! executor met een klok die de test zelf verzet. Zo lopen handshake, data,
//! close en de system-client door precies de code die op het slot draait.

use super::*;
use crate::contract::{HOPABI_HDR_LEN, OP_STAT, SYS_HEADER_LEN};
use crate::ring::tests::Backing;
use crate::ring::{Peek, Reader, Writer};
use abi::systemapi::decode_header;
use core::cell::Cell;
use std::boxed::Box;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(1_000_000_000) };
}

/// De klok van de test: staat stil tot de lus hem naar de volgende timer zet.
fn now() -> u64 {
    NOW.with(Cell::get)
}

fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}

const CAP: u64 = NET_RING_DATA_CAP;
const BUDGET: usize = 4 << 20;
/// Een poll-ronde van tien seconden: de tests lopen dan op de bel en op
/// de wekken van de stack, en een gemiste wek is een test die tien
/// (gesimuleerde) seconden duurt in plaats van een die toevallig slaagt.
const POLL: RxPoll = RxPoll {
    lo: Duration::from_secs(10),
    hi: Duration::from_secs(10),
    hold: 0,
};

/// De deurbel zoals de idle van de app-core hem belt: ligt er RX, dan de
/// bel van die pomp.
type Door = (Peek, &'static Signal);

struct Pair {
    exec: &'static Exec,
    kern: &'static Net,
    app: &'static Net,
    doors: [Door; 2],
}

fn nic(tx: &Backing, rx: &Backing, slot: u64) -> Nic {
    Nic::over(
        Writer::open(tx.pa(), CAP).unwrap(),
        Reader::open(rx.pa(), CAP).unwrap(),
        Peek::new(rx.pa(), CAP),
        mac_of(slot),
    )
}

fn spawn_pump(
    exec: &'static Exec,
    net: &'static Net,
    mut nic: Nic,
    rx: &Backing,
    poll: RxPoll,
) -> Door {
    let bell: &'static Signal = leak(Signal::new());
    let mut buf = frame_buf(net.frame_len()).unwrap();
    exec.spawn(async move { net.pump(&mut nic, &mut buf, bell, poll).await })
        .unwrap();
    (Peek::new(rx.pa(), CAP), bell)
}

/// De kern en slot 1 aan één draad, met hun pompen al gespawnd.
fn pair() -> Pair {
    pair_with(POLL)
}

/// Als [`pair`], met de pompen op `poll`.
fn pair_with(poll: RxPoll) -> Pair {
    let exec: &'static Exec = leak(Exec::new());
    exec.set_clock(now);
    let up = leak(Backing::new(CAP));
    let down = leak(Backing::new(CAP));
    let kern = leak(Net::new(slot_config(0, BUDGET), 7, exec, now).unwrap());
    let app = leak(Net::new(slot_config(1, BUDGET), 9, exec, now).unwrap());
    app.seed_neighbor(host_ip(), mac_of(0).0).unwrap();
    let k = spawn_pump(exec, kern, nic(down, up, 0), up, poll);
    let a = spawn_pump(exec, app, nic(up, down, 1), down, poll);
    Pair {
        exec,
        kern,
        app,
        doors: [k, a],
    }
}

impl Pair {
    /// Draait de executor tot `done`. Heeft geen taak iets te doen, dan
    /// eerst de deurbel (zoals de slaper vóór de WFE), en anders de klok
    /// naar de volgende timer.
    fn run_until(&self, done: impl Fn() -> bool) {
        for _ in 0..1_000_000 {
            if done() {
                return;
            }
            if self.exec.step() {
                continue;
            }
            let mut rang = false;
            for (peek, bell) in &self.doors {
                if peek.head_pending().1 {
                    bell.set();
                    rang = true;
                }
            }
            if !rang {
                let next = self
                    .exec
                    .next_deadline()
                    .expect("niets te doen en geen timer");
                NOW.with(|c| c.set(c.get().max(next)));
            }
        }
        panic!("liep niet af");
    }
}

/// Hoeveel gesimuleerde tijd sinds `t0`; ruim onder de poll-ronde bewijst
/// dat bel en stack de pompen wekten, niet de timer.
fn elapsed(t0: u64) -> u64 {
    now() - t0
}

fn slot<T>() -> &'static RefCell<Option<T>> {
    leak(RefCell::new(None))
}

/// Een kern van één call: leest een request-frame, antwoordt op een stat
/// met maat 4096, en leest dan tot EOF.
async fn fake_kern(l: TcpListener, seen: &'static RefCell<Option<(u8, Vec<u8>, usize)>>) {
    let mut c = l.accept().await.unwrap();
    let mut fh = [0u8; SYS_HEADER_LEN];
    c.read_exact(&mut fh).await.unwrap();
    let n = decode_header(&fh).unwrap().len;
    let mut req = vec![0u8; n];
    c.read_exact(&mut req).await.unwrap();
    let op = req[1];
    let seq = u32::from_le_bytes(req[4..8].try_into().unwrap());
    c.write_all(&crate::sys::tests::resp(seq, 0, 4096, &[]))
        .await
        .unwrap();
    // Na de call: de client sluit, en dat is EOF, geen reset.
    let mut rest = [0u8; 16];
    let eof = c.read(&mut rest).await.unwrap();
    *seen.borrow_mut() = Some((op, req[HOPABI_HDR_LEN..].to_vec(), eof));
}

#[test]
fn system_stat_over_a_real_tcp_connection() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let seen = slot();
    let got = slot();
    p.exec.spawn(fake_kern(l, seen)).unwrap();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut client = app.system_client();
            let r = client.stat("/data/db.bin").await;
            let connected = client.is_connected();
            drop(client); // FIN
            *got.borrow_mut() = Some((r, connected));
        })
        .unwrap();
    p.run_until(|| seen.borrow().is_some() && got.borrow().is_some());
    assert_eq!(got.borrow_mut().take(), Some((Ok(4096), true)));
    assert!(elapsed(t0) < 1_000_000, "stat duurde {} ns", elapsed(t0));
    let (op, path, eof) = seen.borrow_mut().take().unwrap();
    assert_eq!(
        (op, path.as_slice(), eof),
        (OP_STAT, &b"/data/db.bin"[..], 0)
    );
}

#[test]
fn an_explicit_connection_carries_the_first_call() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let seen = slot();
    let got = slot();
    p.exec.spawn(fake_kern(l, seen)).unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let c = app.tcp_connect(HOST, sys::ADDRESS.1).await.unwrap();
            let local = c.local().unwrap();
            let mut client = app.system_client_over(c);
            let r = client.stat("/").await;
            drop(client);
            *got.borrow_mut() = Some((r, local.ip));
        })
        .unwrap();
    p.run_until(|| seen.borrow().is_some() && got.borrow().is_some());
    assert_eq!(got.borrow_mut().take(), Some((Ok(4096), slot_ip(1))));
}

/// 40 × 8 KiB heen en weer door een echo, dan close: de client ziet de
/// echo byte voor byte terug en daarna EOF.
#[test]
fn bulk_both_ways_then_close() {
    const CHUNK: usize = 8 << 10;
    const ROUNDS: usize = 40;
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let echoed = leak(Cell::new(0usize));
    let server_done = leak(Cell::new(false));
    let result = slot();
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            loop {
                let n = c.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                c.write_all(&buf[..n]).await.unwrap();
                echoed.set(echoed.get() + n);
            }
            c.close().unwrap();
            server_done.set(true);
        })
        .unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect([10, 100, 0, 1], 7).await.unwrap();
            let mut back = vec![0u8; CHUNK];
            let mut ok = true;
            for r in 0..ROUNDS {
                let out: Vec<u8> = (0..CHUNK).map(|i| (i * 7 + r) as u8).collect();
                c.write_all(&out).await.unwrap();
                c.read_exact(&mut back).await.unwrap();
                ok &= back == out;
            }
            // Half dicht is hier heel dicht: close, en de echo sluit ook.
            let h = c.h;
            let net = c.net;
            c.close().unwrap();
            let after = net.with(|st| st.tcp_state(h)).unwrap();
            *result.borrow_mut() = Some((ok, after.is_err()));
        })
        .unwrap();
    p.run_until(|| result.borrow().is_some());
    assert_eq!(result.borrow_mut().take(), Some((true, true)));
    // De echo zag EOF en sloot.
    p.run_until(|| server_done.get());
    assert_eq!(echoed.get(), CHUNK * ROUNDS);
    assert!(
        elapsed(t0) < 1_000_000_000,
        "bulk duurde {} ns",
        elapsed(t0)
    );
    let st = p.app.stats().unwrap();
    assert_eq!(st.refused_no_budget, 0);
}

#[test]
fn a_small_answer_carries_the_ack_in_the_same_frame() {
    // De rondreis van `bench ping` (03-10, hop-cost5): op de OS-core is elk
    // frame naar de buur een beurt voor die buur. Zond de pomp na een
    // ontvangst meteen, dan ging de ACK alleen (een frame van 54 bytes) en
    // kwam het antwoord in een tweede: drie frames per rondreis en zes hops.
    // Nu wacht hij tot de lezer die hij wekte schreef, en per richting is er
    // per rondreis één frame: kop (8) plus 54 plus 64 bytes, op 8 = 128.
    const ROUNDS: u64 = 20;
    const LEN: usize = 64;
    const RECORD: u64 = 128;
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            let mut buf = [0u8; LEN];
            loop {
                let n = c.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                c.write_all(&buf[..n]).await.unwrap();
            }
        })
        .unwrap();
    let app = p.app;
    let (up, down) = (p.doors[0].0, p.doors[1].0);
    let result = slot();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect([10, 100, 0, 1], 7).await.unwrap();
            let mut msg = [0x5au8; LEN];
            let heads = || (up.head_pending().0, down.head_pending().0);
            // Eén rondreis warm: de handshake en zijn ACK's tellen niet.
            c.write_all(&msg).await.unwrap();
            c.read_exact(&mut msg).await.unwrap();
            let before = heads();
            for _ in 0..ROUNDS {
                c.write_all(&msg).await.unwrap();
                c.read_exact(&mut msg).await.unwrap();
            }
            let after = heads();
            *result.borrow_mut() = Some((after.0 - before.0, after.1 - before.1));
        })
        .unwrap();
    p.run_until(|| result.borrow().is_some());
    let (sent, answered) = result.borrow_mut().take().unwrap();
    assert_eq!(
        sent,
        ROUNDS * RECORD,
        "de vragen: {} bytes per rondreis",
        sent / ROUNDS
    );
    assert_eq!(
        answered,
        ROUNDS * RECORD,
        "de antwoorden: {} bytes per rondreis",
        answered / ROUNDS
    );
}

#[test]
fn a_closed_port_is_refused() {
    let p = pair();
    let got = slot();
    let app = p.app;
    p.exec
        .spawn(async move {
            let r = app.tcp_connect(HOST, 9).await.map(|_| ());
            *got.borrow_mut() = Some(r);
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some(Err(NetError::Stack(StackError::Refused {
            ip: HOST,
            port: 9
        })))
    );
    assert_eq!(
        conn_error(NetError::Stack(StackError::Refused { ip: HOST, port: 9 })),
        ConnError::Refused
    );
}

#[test]
fn a_silent_peer_times_out_on_the_executor() {
    let p = pair();
    let got = slot();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let r = app
                .tcp_connect_timeout([10, 100, 0, 77], 80, Duration::from_millis(200))
                .await
                .map(|_| ());
            *got.borrow_mut() = Some((r, now()));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, t) = got.borrow_mut().take().unwrap();
    assert!(
        matches!(
            r,
            Err(NetError::Timeout | NetError::Stack(StackError::DeadlineExceeded))
        ),
        "{r:?}"
    );
    assert!(t - t0 >= 200_000_000 && t - t0 < 300_000_000, "{}", t - t0);
}

#[test]
fn a_read_deadline_fires_and_the_stream_stays_usable() {
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let got = slot();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            c.set_timeout(Some(Duration::from_millis(50)));
            let mut b = [0u8; 4];
            let first = c.read(&mut b).await.map(|_| ());
            c.set_timeout(None);
            let second = c.read(&mut b).await;
            *got.borrow_mut() = Some((first, second, b));
        })
        .unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            app.exec.after(Duration::from_millis(100)).await;
            c.write_all(b"late").await.unwrap();
            app.exec.after(Duration::from_secs(1)).await;
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((Err(NetError::Timeout), Ok(4), *b"late"))
    );
}

#[test]
fn udp_round_trip() {
    let p = pair();
    let kern = p.kern;
    let app = p.app;
    let got = slot();
    let server = kern.udp_bind(53).unwrap();
    p.exec
        .spawn(async move {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            buf[..n].reverse();
            server.send_to(from, &buf[..n]).await.unwrap();
        })
        .unwrap();
    p.exec
        .spawn(async move {
            let s = app.udp_bind(0).unwrap();
            let to = Endpoint { ip: HOST, port: 53 };
            s.send_to(to, b"ping").await.unwrap();
            let mut buf = [0u8; 64];
            let (n, from) = s.recv_from(&mut buf).await.unwrap();
            *got.borrow_mut() = Some((buf[..n].to_vec(), from));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((b"gnip".to_vec(), Endpoint { ip: HOST, port: 53 }))
    );
}

/// Een core die blijft rekenen slaapt niet, dus niemand belt de deurbel:
/// zijn pomp ziet RX dan op de uitstelbare ronde van `lo`, ook als zijn
/// poll-ronde na stilte al op `hi` stond. Zonder die ronde wachtte het
/// datagram hier op de timer van de kern-pomp, een (gesimuleerde) seconde.
#[test]
fn a_busy_core_still_sees_rx_within_lo() {
    let p = pair_with(RxPoll {
        lo: Duration::from_micros(300),
        hi: Duration::from_secs(1),
        hold: 0,
    });
    let (kern, app) = (p.kern, p.app);
    // Eerst stilte, tot beide pompen op `hi` staan.
    let t0 = now();
    p.run_until(|| elapsed(t0) >= 5_000_000_000);
    let got: &'static Cell<Option<u64>> = leak(Cell::new(None));
    let server = kern.udp_bind(53).unwrap();
    p.exec
        .spawn(async move {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            server.send_to(from, &buf[..n]).await.unwrap();
        })
        .unwrap();
    // De rekenaar: elke ronde 20 µs werk en een yield, tot het antwoord er
    // is; zolang hij leeft, doet geen ronde niets.
    p.exec
        .spawn(async move {
            while got.get().is_none() {
                NOW.with(|c| c.set(c.get() + 20_000));
                yield_now().await;
            }
        })
        .unwrap();
    let sent = now();
    p.exec
        .spawn(async move {
            let s = app.udp_bind(0).unwrap();
            let to = Endpoint { ip: HOST, port: 53 };
            s.send_to(to, b"ping").await.unwrap();
            let mut buf = [0u8; 64];
            s.recv_from(&mut buf).await.unwrap();
            got.set(Some(now()));
        })
        .unwrap();
    p.run_until(|| got.get().is_some());
    let rtt = got.get().unwrap() - sent;
    assert!(rtt < 5_000_000, "round trip on a busy core took {rtt} ns");
}

/// Multicast: een socket op de poort van de groep hoort een datagram naar
/// de groep pas na de join; de kern zendt naar de groep zoals de switch
/// het naar elk slot floodt.
#[test]
fn a_group_datagram_arrives_only_after_the_join() {
    const MDNS: Endpoint = Endpoint {
        ip: [224, 0, 0, 251],
        port: 5353,
    };
    let p = pair();
    let kern = p.kern;
    let app = p.app;
    let got = slot();
    let listen = app.udp_bind(MDNS.port).unwrap();
    p.exec
        .spawn(async move {
            let mut buf = [0u8; 64];
            let mut out = Vec::new();
            // Vóór de join: de stack laat het datagram vallen, de wacht
            // loopt af.
            let mut l = listen;
            l.set_timeout(Some(Duration::from_millis(50)));
            out.push(l.recv_from(&mut buf).await.map(|(n, _)| buf[..n].to_vec()));
            app.join_group(MDNS.ip).unwrap();
            // Een tweede join is geen fout en geen tweede groep.
            app.join_group(MDNS.ip).unwrap();
            l.set_timeout(Some(Duration::from_millis(50)));
            out.push(l.recv_from(&mut buf).await.map(|(n, _)| buf[..n].to_vec()));
            *got.borrow_mut() = Some(out);
        })
        .unwrap();
    p.exec
        .spawn(async move {
            let s = kern.udp_bind(5354).unwrap();
            s.send_to(MDNS, b"early").await.unwrap();
            // Na de wacht van de luisteraar: de join is er dan.
            kern.exec.after(Duration::from_millis(60)).await;
            s.send_to(MDNS, b"probe").await.unwrap();
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let out = got.borrow_mut().take().unwrap();
    assert_eq!(out[0], Err(NetError::Timeout));
    assert_eq!(out[1], Ok(b"probe".to_vec()));
    // Alleen link-local: een groep daarbuiten weigert de stack.
    assert_eq!(
        app.join_group([239, 1, 1, 1]),
        Err(NetError::Stack(StackError::NotLinkLocalMulticast {
            ip: [239, 1, 1, 1]
        }))
    );
}

#[test]
fn budget_env_and_address_parsing() {
    assert_eq!(budget_for(16 << 20, None), 2 << 20);
    assert_eq!(budget_for(1 << 20, None), BUDGET_MIN);
    assert_eq!(budget_for(1 << 30, None), BUDGET_MAX);
    assert_eq!(budget_for(1 << 30, Some("512k")), 512 << 10);
    assert_eq!(budget_for(1 << 30, Some("3M")), 3 << 20);
    assert_eq!(budget_for(16 << 20, Some("123456")), 123_456);
    assert_eq!(budget_for(16 << 20, Some("junk")), 2 << 20);
    assert_eq!(budget_for(16 << 20, Some("0")), 2 << 20);
    assert_eq!(parse_ip4("10.100.0.1"), Some(HOST));
    assert_eq!(parse_ip4("1.1.1.1"), Some([1, 1, 1, 1]));
    assert_eq!(parse_ip4("1.1.1"), None);
    assert_eq!(parse_ip4("1.1.1.1.1"), None);
    assert_eq!(parse_ip4("1.1.1.256"), None);
    assert_eq!(parse_addr("10.100.0.3:9000"), Some(([10, 100, 0, 3], 9000)));
    assert_eq!(parse_addr(" 10.100.0.1:10100\n"), Some((HOST, 10100)));
    assert_eq!(parse_addr("10.100.0.1"), None);
    assert_eq!(parse_addr("10.100.0:9000"), None);
    assert_eq!(parse_addr("10.100.0.3.4:9000"), None);
    assert_eq!(parse_addr("10.100.0.3:0"), None);
    assert_eq!(parse_addr("node:80"), None);
    let c = slot_config(3, 2 << 20);
    assert_eq!(
        (c.ip, c.mac, c.gw, c.prefix),
        (slot_ip(3), mac_of(3).0, HOST, 16)
    );
    assert_eq!(c.mtu, NET_MTU);
    assert_eq!(sys::ADDRESS, (HOST, abi::systemapi::PORT));
}

#[test]
fn transport_errors_map_to_what_the_client_retries() {
    assert_eq!(
        conn_error(NetError::Stack(StackError::Reset)),
        ConnError::Reset
    );
    assert_eq!(
        conn_error(NetError::Stack(StackError::Closed)),
        ConnError::Closed
    );
    assert_eq!(conn_error(NetError::Timeout), ConnError::Refused);
    assert_eq!(conn_error(NetError::NotUp), ConnError::Refused);
}

// ---- Flush en het net-afscheid ----

/// Een kern die één verbinding aanneemt en alles leest tot EOF.
async fn sink(l: TcpListener, got: &'static RefCell<Option<Vec<u8>>>) {
    let mut c = l.accept().await.unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = c.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    *got.borrow_mut() = Some(all);
}

/// Het geval van 29-09: een regel die vlak vóór de exit de zendbuffer in
/// gaat, komt aan, omdat de shutdown pas klaar is als de FIN bevestigd is.
/// Daarna gaat er niets nieuws meer open.
#[test]
fn a_line_written_just_before_shutdown_arrives() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let got = slot();
    p.exec.spawn(sink(l, got)).unwrap();
    let (app, kern) = (p.app, p.kern);
    let done = slot();
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, sys::ADDRESS.1).await.unwrap();
            let _udp = app.udp_bind(0).unwrap();
            let _l = app.tcp_listen(8080).unwrap();
            c.write_all(b"HOPOS_APPSPIKE_NETLOG last words")
                .await
                .unwrap();
            // Geen flush, geen pauze: meteen het afscheid.
            let d = app.shutdown(Duration::from_millis(200)).await;
            // Op het moment van terugkeer (hierna zou de core parkeren) heeft
            // de stack van de kern elke byte al.
            let at_exit = kern.stats().unwrap().tcp_bytes_in;
            let again = app.tcp_connect(HOST, sys::ADDRESS.1).await.map(|_| ());
            let bind = app.udp_bind(0).map(|_| ());
            *done.borrow_mut() = Some((d, again, bind, now(), at_exit));
        })
        .unwrap();
    p.run_until(|| done.borrow().is_some());
    let (d, again, bind, t, at_exit) = done.borrow_mut().take().unwrap();
    assert!(d.drained, "{d:?}");
    assert_eq!(at_exit, b"HOPOS_APPSPIKE_NETLOG last words".len());
    assert_eq!((d.closed, d.untracked), (3, 0));
    assert!(t - t0 < 200_000_000, "afscheid duurde {} ns", t - t0);
    assert_eq!(again, Err(NetError::ShuttingDown));
    assert_eq!(bind, Err(NetError::ShuttingDown));
    // Bevestigd is bij de kern: zijn lezer krijgt de regel en dan EOF.
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take().unwrap(),
        b"HOPOS_APPSPIKE_NETLOG last words"
    );
}

/// Zonder verbindingen is het afscheid meteen klaar.
#[test]
fn shutdown_without_connections_is_immediate() {
    let p = pair();
    let app = p.app;
    let done = slot();
    p.exec
        .spawn(async move {
            let d = app.shutdown(Duration::from_millis(200)).await;
            *done.borrow_mut() = Some(d);
        })
        .unwrap();
    p.run_until(|| done.borrow().is_some());
    let d = done.borrow_mut().take().unwrap();
    assert!(d.drained && d.closed == 0 && d.waited_us == 0, "{d:?}");
}

/// Een flush is pas klaar als de peer alles heeft: direct daarna ligt elke
/// byte in de ontvangstring van de kern, zonder dat de pomp van de app nog
/// iets hoeft te doen.
#[test]
fn flush_returns_once_the_peer_has_every_byte() {
    const N: usize = 12 << 10;
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let peer = slot();
    p.exec
        .spawn(async move {
            // Aannemen en dan niet lezen: wat binnen is, blijft in de ring.
            *peer.borrow_mut() = Some(l.accept().await.unwrap());
        })
        .unwrap();
    let (app, kern) = (p.app, p.kern);
    let flushed = slot();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            let data: Vec<u8> = (0..N).map(|i| i as u8).collect();
            c.write_all(&data).await.unwrap();
            let r = c.flush().await;
            // Op het moment van terugkeer, niet later.
            let at_flush = kern.stats().unwrap().tcp_bytes_in;
            *flushed.borrow_mut() = Some((r, at_flush));
            // Open houden: een close hoort niet bij deze toets.
            core::future::pending::<()>().await;
        })
        .unwrap();
    p.run_until(|| flushed.borrow().is_some() && peer.borrow().is_some());
    assert_eq!(flushed.borrow_mut().take(), Some((Ok(()), N)));
    let kc = peer.borrow_mut().take().unwrap();
    let mut buf = vec![0u8; 2 * N];
    let mut have = 0;
    while let Ok(Ok(n)) = p.kern.with(|st| st.tcp_read(kc.h, &mut buf[have..], now())) {
        have += n;
    }
    assert_eq!(have, N);
    assert!(buf[..N].iter().enumerate().all(|(i, &b)| b == i as u8));
}

/// Een peer die niet leest, laat het venster dichtlopen: de flush geeft
/// dan op de deadline van de stream een timeout, en liegt geen `Ok`.
#[test]
fn flush_times_out_on_a_peer_that_does_not_read() {
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let peer = slot();
    p.exec
        .spawn(async move {
            *peer.borrow_mut() = Some(l.accept().await.unwrap());
        })
        .unwrap();
    let app = p.app;
    let got = slot();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            // Schrijven tot ook de eigen zendring vol is.
            c.set_timeout(Some(Duration::from_millis(50)));
            let chunk = vec![0x5a; 64 << 10];
            let mut wrote = 0;
            while let Ok(n) = c.write(&chunk).await {
                wrote += n;
            }
            c.set_timeout(Some(Duration::from_millis(100)));
            let t0 = now();
            let r = c.flush().await;
            *got.borrow_mut() = Some((r, wrote, now() - t0));
            core::future::pending::<()>().await;
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, wrote, dt) = got.borrow_mut().take().unwrap();
    assert_eq!(r, Err(NetError::Timeout));
    assert!(wrote > 64 << 10, "wrote {wrote}");
    assert!((100_000_000..200_000_000).contains(&dt), "{dt}");
}

/// De tabel is vast: wat niet past, werkt wel maar telt als niet gevolgd.
#[test]
fn a_full_open_table_counts_instead_of_growing() {
    assert_eq!(open_slots_for(0), OPEN_MIN);
    assert_eq!(open_slots_for(2 << 20), 128);
    assert_eq!(open_slots_for(BUDGET_MAX), OPEN_MAX);
    assert_eq!(open_slots_for(usize::MAX), OPEN_MAX);
    let p = pair();
    let slots = open_slots_for(BUDGET);
    let ls: Vec<TcpListener> = (0..=slots)
        .map(|i| p.app.tcp_listen(1000 + i as u16).unwrap())
        .collect();
    assert!(ls[..slots].iter().all(|l| l.slot.is_some()));
    assert_eq!(ls[slots].slot, None);
    assert_eq!(p.app.untracked.get(), 1);
    // Eén dicht, en de plek is weer vrij.
    drop(ls);
    let l = p.app.tcp_listen(999).unwrap();
    assert_eq!(l.slot, Some(0));
}

// ---- DNS over het testnet: de kern speelt de server op poort 53 ----

/// Wat de nep-server met de zoveelste vraag doet.
type Answerer = fn(usize, u16, &str) -> Vec<Vec<u8>>;

/// Een DNS-server op de kern die elke vraag aan `answer` voorlegt (het
/// volgnummer, het id en de gevraagde naam) en stuurt wat die teruggeeft,
/// in volgorde; niets is zwijgen. Telt de vragen in `seen`.
async fn fake_dns(server: UdpSocket, answer: Answerer, seen: &'static Cell<usize>) {
    let mut buf = [0u8; 512];
    loop {
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        let q = &buf[..n];
        let id = u16::from_be_bytes([q[0], q[1]]);
        // De naam uit de vraag: labels vanaf offset 12.
        let mut name = std::string::String::new();
        let mut at = 12;
        while q[at] != 0 {
            let len = usize::from(q[at]);
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(core::str::from_utf8(&q[at + 1..at + 1 + len]).unwrap());
            at += 1 + len;
        }
        let i = seen.get();
        seen.set(i + 1);
        for reply in answer(i, id, &name) {
            server.send_to(from, &reply).await.unwrap();
        }
    }
}

/// Draait één resolve van `host` in slot 1 tegen een nep-server die met
/// `answer` antwoordt; de uitkomst, het aantal vragen en de duur.
fn resolve_against(answer: Answerer, host: &'static str) -> (Result<[u8; 4]>, usize, u64) {
    let p = pair();
    let seen: &'static Cell<usize> = leak(Cell::new(0));
    let server = p.kern.udp_bind(dns::PORT).unwrap();
    p.exec.spawn(fake_dns(server, answer, seen)).unwrap();
    let got = slot();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let r = app.resolve_record(HOST, host, 1).await;
            *got.borrow_mut() = Some((r, now()));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, t) = got.borrow_mut().take().unwrap();
    (r, seen.get(), t - t0)
}

const A: &[u8] = &[140, 82, 121, 4];

fn good(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
    vec![dns::tests::answer(
        id,
        0,
        host,
        &[(dns::tests::AT_QNAME, 1, A)],
    )]
}

#[test]
fn a_name_resolves_over_udp() {
    let (r, seen, took) = resolve_against(good, "github.com");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 1));
    assert!(took < 100_000_000, "resolve duurde {took} ns");
}

#[test]
fn a_wrong_id_is_ignored_and_the_right_answer_still_counts() {
    fn liar(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        // Eerst een vervalsing met een ander id, dan een met een andere
        // naam (beide met een ander adres), dan het echte antwoord: de
        // eerste twee mogen niet winnen en de wacht niet breken.
        let fake = &[(dns::tests::AT_QNAME, 1, &[6u8; 4][..])];
        let mut out = vec![
            dns::tests::answer(id ^ 1, 0, host, fake),
            dns::tests::answer(id, 0, "evil.example", fake),
        ];
        out.extend(good(0, id, host));
        out
    }
    let (r, seen, _) = resolve_against(liar, "github.com");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 1));
}

#[test]
fn silence_gets_one_retry_with_a_new_id() {
    fn second(i: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        if i == 1 {
            good(i, id, host)
        } else {
            Vec::new()
        }
    }
    let (r, seen, took) = resolve_against(second, "pool.ntp.org");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 2));
    let t = u64::try_from(DNS_TIMEOUT.as_nanos()).unwrap();
    assert!(took >= t && took < 2 * t, "{took}");

    fn never(_: usize, _: u16, _: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }
    let (r, seen, took) = resolve_against(never, "pool.ntp.org");
    assert_eq!(
        (r, seen),
        (Err(NetError::Dns(DnsError::Timeout { attempts: 2 })), 2)
    );
    assert!(took >= 2 * t && took < 3 * t, "{took}");
}

#[test]
fn crooked_answers_over_the_wire_are_errors() {
    fn truncated(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        let m = dns::tests::answer(id, 0, host, &[(dns::tests::AT_QNAME, 1, A)]);
        vec![m[..m.len() - 2].to_vec()]
    }
    fn only_aaaa(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        vec![dns::tests::answer(
            id,
            0,
            host,
            &[(dns::tests::AT_QNAME, 28, &[0; 16])],
        )]
    }
    fn nxdomain(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        vec![dns::tests::answer(id, 3, host, &[])]
    }
    for (f, want) in [
        (truncated as Answerer, DnsError::Truncated),
        (only_aaaa, DnsError::NoAnswer),
        (nxdomain, DnsError::NxDomain),
    ] {
        let (r, seen, _) = resolve_against(f, "example.com");
        // Een antwoord dat nee zegt, wordt niet nog eens gevraagd.
        assert_eq!((r, seen), (Err(NetError::Dns(want)), 1));
    }
}

#[test]
fn an_address_needs_no_server_and_a_name_without_server_says_so() {
    let p = pair();
    let got = slot();
    let app = p.app;
    p.exec
        .spawn(async move {
            let ip = app.resolve("10.0.2.2").await;
            let name = app.resolve("github.com").await;
            *got.borrow_mut() = Some((ip, name));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((Ok([10, 0, 2, 2]), Err(NetError::Dns(DnsError::NoServer))))
    );
}

#[test]
fn ipv6_udp_roundtrip_drop_and_timeout_use_the_real_pumps() {
    let p = pair();
    let server = p.kern.udp6_bind(5540).unwrap();
    let address = Endpoint6 {
        ip: p.kern.ipv6_addresses().unwrap().0,
        port: server.local().unwrap().port,
    };
    let mut client = p.app.udp6_bind(0).unwrap();
    client.set_timeout(Some(Duration::from_secs(3)));
    let done = leak(Cell::new(false));
    p.exec
        .spawn(async move {
            let mut bytes = [0; 64];
            let (n, peer) = server.recv_from(&mut bytes).await.unwrap();
            assert_eq!(&bytes[..n], b"Matter over IPv6");
            server.send_to(peer, b"IPv6 reply").await.unwrap();
        })
        .unwrap();
    p.exec
        .spawn(async move {
            client.send_to(address, b"Matter over IPv6").await.unwrap();
            let mut bytes = [0; 64];
            let (n, peer) = client.recv_from(&mut bytes).await.unwrap();
            assert_eq!(peer, address);
            assert_eq!(&bytes[..n], b"IPv6 reply");
            client.set_timeout(Some(Duration::from_millis(10)));
            assert!(matches!(
                client.recv_from(&mut bytes).await,
                Err(NetError::Timeout)
            ));
            let local = client.local().unwrap();
            drop(client);
            let rebound = p.app.udp6_bind(local.port).unwrap();
            drop(rebound);
            done.set(true);
        })
        .unwrap();
    p.run_until(|| done.get());
}

#[test]
fn aaaa_resolves_over_the_real_udp_pumps() {
    let p = pair();
    let server = p.kern.udp_bind(53).unwrap();
    let got = slot();
    let app = p.app;
    p.exec
        .spawn(async move {
            let mut buf = [0; 512];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[n - 4..n - 2], &[0, 28]);
            let mut response = buf[..n].to_vec();
            response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
            response[6..8].copy_from_slice(&1_u16.to_be_bytes());
            response.extend_from_slice(&[0xc0, 12, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
            response.extend_from_slice(&[0xfd, 0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]);
            server.send_to(from, &response).await.unwrap();
        })
        .unwrap();
    p.exec
        .spawn(async move {
            *got.borrow_mut() = Some(app.resolve6_via(HOST, "thread.example").await);
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take().unwrap(),
        Ok([0xfd, 0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7])
    );
}
