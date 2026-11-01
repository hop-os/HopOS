//! De store-ops: de rij, de toetsen van de kern, en de hele weg van app via
//! Hop naar hopfs en terug, door de echte system-API.

use super::*;
use crate::cage::tests::SparseMem;
use crate::rpc::{FsActor, tests::disk};
use crate::slots::Mount;
use crate::slots::tests::{FakeConsole, Obey, actor, s, start_job, stop};
use crate::system::{Conn, End, FlipBundle, Hooks, HopImage, LogTee, SlotLogs, System};
use crate::testutil::FakeTimer;
use abi::hopabi::{OP_READ as READ, OP_WRITE as WRITE, Req};
use abi::layout::port_ip4;
use abi::systemapi::store::{DoneHead, LOG_NEWS, NEXT_STORE_LOGS, StoreTask, read_len};
use abi::systemapi::{HEADER_LEN as FH, Kind, decode_header, encode_header};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;
use sync::mpsc::Mailbox;

/// Een verbinding in RAM: alle calls staan er vooraf in, een lege rij is EOF.
struct Pipe {
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    ip: u32,
}

impl Pipe {
    fn new(ip: u32, calls: &[Vec<u8>]) -> Pipe {
        let mut rx = VecDeque::new();
        for c in calls {
            rx.extend(encode_header(Kind::Call, c.len()).unwrap());
            rx.extend(c.iter().copied());
        }
        Pipe {
            rx,
            tx: Vec::new(),
            ip,
        }
    }
}

impl Conn for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = crate::Result<usize>> {
        let n = buf.len().min(self.rx.len());
        for b in buf.iter_mut().take(n) {
            *b = self.rx.pop_front().unwrap();
        }
        core::future::ready(Ok(n))
    }
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = crate::Result<usize>> {
        self.tx.extend_from_slice(buf);
        core::future::ready(Ok(buf.len()))
    }
    fn remote_ip4(&self) -> u32 {
        self.ip
    }
}

/// Een klok die per slaap hoogstens een milliseconde opschuift: een lange
/// wacht van Hop (NEXT_STORE) blijft zo echt wachten terwijl de app in de
/// andere taak verder gaat, in plaats van in één sprong af te lopen.
#[derive(Default)]
struct SlowTimer(core::cell::Cell<u64>);

impl crate::cage::Timer for SlowTimer {
    fn now(&self) -> u64 {
        self.0.get()
    }
    fn sleep(&self, d: core::time::Duration) -> impl Future<Output = ()> {
        let step = d.min(core::time::Duration::from_millis(1));
        self.0.set(self.0.get() + step.as_nanos() as u64);
        sync::yield_now()
    }
}

struct NoHooks;
impl Hooks for NoHooks {
    fn set_clock(&self, _: u64) {}
    fn flip(&self, _: &FlipBundle, _: &[u8; 32]) -> crate::Result {
        Ok(())
    }
    fn hop_restart(
        &self,
        _: Option<&HopImage>,
        _: core::time::Duration,
    ) -> core::result::Result<(), crate::system::Fail> {
        Ok(())
    }
    fn hop_refused(&self, _: &'static str, _: &crate::system::Fail) {}
    fn kern(&self) -> crate::cage::Status {
        crate::cage::Status::default()
    }
}

fn call(op: u8, seq: u32, off: u64, n: u64, path: &[u8], data: &[u8]) -> Vec<u8> {
    let r = Req {
        op,
        seq,
        off,
        n,
        path,
        data,
    };
    let mut v = vec![0u8; HDR_LEN + path.len() + data.len()];
    let n = abi::hopabi::encode_req(&mut v, &r).unwrap();
    v.truncate(n);
    v
}

fn done(seq: u32, ticket: u64, size: u64, status: u16, rest: &[u8]) -> Vec<u8> {
    let mut d = DoneHead {
        status,
        reserved: [0; 6],
    }
    .encode()
    .to_vec();
    d.extend_from_slice(rest);
    call(PrivOp::StoreDone.op(), seq, ticket, size, b"", &d)
}

fn next(seq: u32) -> Vec<u8> {
    call(PrivOp::NextStore.op(), seq, 0, 5_000, b"", b"")
}

/// Een antwoord: (op, status, size, data).
type Res = (u8, u16, u64, Vec<u8>);

fn results(mut b: &[u8]) -> Vec<Res> {
    let mut out = Vec::new();
    while b.len() >= FH {
        let h = decode_header(b[..FH].try_into().unwrap()).unwrap();
        assert_eq!(h.kind, Kind::Result);
        let r = abi::hopabi::decode_resp(&b[FH..FH + h.len]).unwrap();
        out.push((r.op, r.status, r.size, r.data.to_vec()));
        b = &b[FH + h.len..];
    }
    out
}

fn text(r: &Res) -> std::string::String {
    std::string::String::from_utf8_lossy(&r.3).into_owned()
}

/// Hop in slot 1, een app met job `demo` en volume `/data` in slot 2.
fn node<'s>(svc: &'s Servicers, con: &'s FakeConsole) -> crate::slots::tests::Actor<'s> {
    let mut a = actor(svc, con, Obey::Exit, 64, 4);
    start_job(&mut a, 1, b"hop", vec![]).unwrap();
    let vol = Mount {
        local: b"/data".to_vec(),
        shared: b"/volumes/demo".to_vec(),
    };
    start_job(&mut a, 2, b"demo", vec![vol]).unwrap();
    a
}

/// De hele weg: de app schrijft in zijn volume en pusht, Hop leest de bytes
/// door de mount-tabel van het slot van de app; de app pullt naar een ander
/// pad en Hop schrijft ze daar; list en drop; en elke uitkomst komt bij de
/// wachtende app terug.
#[test]
fn a_store_call_travels_from_the_app_through_hop_and_back() {
    let (svc, con, logs) = (Servicers::new(), FakeConsole::default(), SlotLogs::new());
    let tee = LogTee::new(&con, &logs);
    let _a = node(&svc, &con);
    let (ra, rh) = (crate::slots::Reply::new(), crate::slots::Reply::new());
    let inbox: Mailbox<crate::slots::Envelope<'_>, 8> = Mailbox::new();
    let fsin: FsInbox<'_> = Mailbox::new();
    let queue = StoreQueue::new();
    let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))), 8)
        .with_fs(&fsin)
        .with_store(&queue);
    let file = b"/data/state.json";
    let mut app = Pipe::new(
        port_ip4(2),
        &[
            call(WRITE, 1, 0, 0, file, b"hallo"),
            call(OP_STORE_PUSH, 2, 0, 0, file, b""),
            call(OP_STORE_PULL, 3, 0, 0, file, b"/elders.json"),
            call(READ, 4, 0, 64, b"/elders.json", b""),
            call(OP_STORE_LIST, 5, 0, 0, b"", b""),
            call(OP_STORE_DROP, 6, 0, 0, file, b""),
        ],
    );
    let mut hop = Pipe::new(
        port_ip4(1),
        &[
            next(1),
            call(PrivOp::StoreRead.op(), 2, 1, 0, file, &read_len(1 << 20)),
            done(3, 1, 5, STATUS_OK, b""),
            next(4),
            call(PrivOp::StoreWrite.op(), 5, 2, 0, b"/elders.json", b"hallo"),
            done(6, 2, 5, STATUS_OK, b""),
            next(7),
            done(8, 3, 1, STATUS_OK, b"data/state.json"),
            next(9),
            done(10, 4, 0, STATUS_OK, b""),
        ],
    );
    let (fs, _) = disk(16);
    let mut fsa = FsActor::new(fs, &svc, &con);
    let timer = SlowTimer::default();
    let (wa, wh) = (sys.admit(app.ip).unwrap(), sys.admit(hop.ip).unwrap());
    let (mut ma, mut mh) = (SparseMem::default(), SparseMem::default());
    let (mut ba, mut oa) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
    let (mut bh, mut oh) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
    let (mut ea, mut eh) = (None, None);
    {
        let mut fa = pin!(sys.serve(
            &mut app, &wa, &ra, &timer, &mut ma, &NoHooks, &tee, &mut ba, &mut oa
        ));
        let mut fh = pin!(sys.serve(
            &mut hop, &wh, &rh, &timer, &mut mh, &NoHooks, &tee, &mut bh, &mut oh
        ));
        let mut run = pin!(fsa.run(&fsin));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..100_000 {
            if ea.is_none()
                && let Poll::Ready(e) = fa.as_mut().poll(&mut cx)
            {
                ea = Some(e);
            }
            let _ = run.as_mut().poll(&mut cx);
            if eh.is_none()
                && let Poll::Ready(e) = fh.as_mut().poll(&mut cx)
            {
                eh = Some(e);
            }
            if ea.is_some() && eh.is_some() {
                break;
            }
        }
    }
    assert_eq!((ea, eh), (Some(End::Peer), Some(End::Peer)));
    let a = results(&app.tx);
    let h = results(&hop.tx);
    assert_eq!(a.len(), 6, "{a:?}");
    assert_eq!(
        h.len(),
        10,
        "{:?}",
        h.iter().map(|r| (r.1, r.2, text(r))).collect::<Vec<_>>()
    );
    // Hop kreeg de push met de job van het slot, niet van de app.
    assert_eq!(h[0].2, 1, "a task was waiting");
    let t = StoreTask::decode(&h[0].3).unwrap();
    assert_eq!(
        (t.ticket, t.slot, t.op, t.job, t.key, t.path),
        (1, 2, OP_STORE_PUSH, &b"demo"[..], &file[..], &file[..])
    );
    // De bytes kwamen door het volume van slot 2.
    assert_eq!((h[1].1, h[1].2, &h[1].3[..]), (STATUS_OK, 5, &b"hallo"[..]));
    assert!(con.saw("hopfs: slot 2 saved /data/state.json as /volumes/demo/state.json"));
    assert_eq!((a[1].1, a[1].2), (STATUS_OK, 5), "push: {}", text(&a[1]));
    // De pull naar een ander pad.
    let t = StoreTask::decode(&h[3].3).unwrap();
    assert_eq!(
        (t.op, t.key, t.path),
        (OP_STORE_PULL, &file[..], &b"/elders.json"[..])
    );
    assert_eq!(h[4].1, STATUS_OK, "{}", text(&h[4]));
    assert_eq!((a[2].1, a[2].2), (STATUS_OK, 5), "pull: {}", text(&a[2]));
    assert_eq!(&a[3].3[..], b"hallo");
    // List: de namen relatief aan de eigen map, zoals Hop ze gaf.
    let t = StoreTask::decode(&h[6].3).unwrap();
    assert_eq!((t.op, t.key), (OP_STORE_LIST, &b"/"[..]));
    assert_eq!(
        (a[4].1, a[4].2, &a[4].3[..]),
        (STATUS_OK, 1, &b"data/state.json"[..])
    );
    assert_eq!(a[5].1, STATUS_OK, "drop: {}", text(&a[5]));
    assert!(queue.is_empty());
    assert!(con.saw("store: slot 2 push done (size 5) HOPOS_STORE_DONE"));
}

/// Een app die stopt terwijl haar call wacht: de verbinding geeft de plaats
/// vrij, en Hop's lees en afronding krijgen NO_ENT.
#[test]
fn a_stop_cancels_the_waiting_call_and_hop_drops_it() {
    let (svc, con, logs) = (Servicers::new(), FakeConsole::default(), SlotLogs::new());
    let tee = LogTee::new(&con, &logs);
    let mut a = node(&svc, &con);
    let r = crate::slots::Reply::new();
    let inbox: Mailbox<crate::slots::Envelope<'_>, 8> = Mailbox::new();
    let queue = StoreQueue::new();
    let sys = System::new(&inbox, &svc, Some(Privilege::for_test(s(1))), 8).with_store(&queue);
    let mut app = Pipe::new(port_ip4(2), &[call(OP_STORE_PUSH, 1, 0, 0, b"x", b"")]);
    let timer = FakeTimer::default();
    let who = sys.admit(app.ip).unwrap();
    let (mut m, mut b, mut o) = (SparseMem::default(), vec![0u8; 4096], vec![0u8; 4096]);
    let mut cx = Context::from_waker(Waker::noop());
    {
        let mut f = pin!(sys.serve(
            &mut app, &who, &r, &timer, &mut m, &NoHooks, &tee, &mut b, &mut o
        ));
        for _ in 0..5 {
            assert!(f.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(queue.len(), 1);
        stop(&mut a, 2).unwrap();
        let mut end = None;
        for _ in 0..50 {
            if let Poll::Ready(e) = f.as_mut().poll(&mut cx) {
                end = Some(e);
                break;
            }
        }
        assert!(end.is_some(), "the waiting call ended");
    }
    assert!(queue.is_empty(), "the place is free again");
    // Hop komt te laat: de opdracht bestaat niet meer.
    assert!(matches!(
        queue.finish(1, STATUS_OK, 0, b""),
        Err(Fail::Gone)
    ));
    assert!(matches!(
        queue.lookup(1, OP_STORE_PUSH, b"x"),
        Err(Fail::Gone)
    ));
}

fn cx_for<'a>(
    queue: &'a StoreQueue,
    svc: &'a Servicers,
    slot: usize,
    reply: &'a crate::slots::Reply,
    timer: &'a FakeTimer,
    con: &'a &'a FakeConsole,
) -> Ctx<'a, 'a, FakeTimer, &'a FakeConsole> {
    Ctx {
        queue,
        fs: None,
        svc,
        slot: s(slot),
        generation: svc.current(s(slot)).unwrap(),
        hop: None,
        hop_slot: Some(s(1)),
        reply,
        timer,
        log: con,
        news: [None, None],
    }
}

/// H8: één lange wacht voor store en log. Met `NEXT_STORE_LOGS` komt
/// `NEXT_STORE` terug met `LOG_NEWS` zodra er een regel bij kwam (van de
/// kern of van een app), één keer per bel; een opdracht gaat voor, en
/// zonder de vlag wacht hij alleen op opdrachten.
#[test]
fn the_long_wait_of_hop_also_returns_for_new_log_lines() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let _a = node(&svc, &con);
    let (queue, reply, timer, logs) = (
        StoreQueue::new(),
        crate::slots::Reply::new(),
        FakeTimer::default(),
        SlotLogs::new(),
    );
    let console = crate::conport::tests::ring(64);
    let conr = &con;
    let mut cx = cx_for(&queue, &svc, 1, &reply, &timer, &conr);
    cx.news = [Some(console.news()), Some(logs.news())];
    let mut out = [0u8; 2048];
    let mut wait = |flags: u64| {
        let h = Head::new(PrivOp::NextStore.op(), 1, flags, 3, 0, HDR_LEN);
        crate::testutil::block_on(super::next(&cx, &h, &mut out))
            .ok()
            .unwrap()
    };
    assert_eq!(
        wait(NEXT_STORE_LOGS),
        (0, 0),
        "nothing new: the wait runs out"
    );
    console.append(b"net: up\n");
    assert_eq!(wait(0), (0, 0), "without the flag only store calls count");
    assert_eq!(wait(NEXT_STORE_LOGS), (LOG_NEWS, 0));
    assert_eq!(wait(NEXT_STORE_LOGS), (0, 0), "one bell, one answer");
    logs.push(s(2), b"an app line");
    assert_eq!(wait(NEXT_STORE_LOGS), (LOG_NEWS, 0));
    // Een opdracht gaat voor de bel.
    let c2 = cx_for(&queue, &svc, 2, &reply, &timer, &conr);
    let v = valid(&c2, OP_STORE_LIST, b"p", b"").ok().unwrap();
    queue
        .submit(0, (s(2), svc.current(s(2)).unwrap()), OP_STORE_LIST, &v)
        .unwrap();
    console.append(b"x\n");
    assert_eq!(wait(NEXT_STORE_LOGS).0, 1);
    assert_eq!(wait(NEXT_STORE_LOGS), (LOG_NEWS, 0));
}

/// De toetsen van de kern (Go: `storeGate`): `..` en een lege naam zijn een
/// weigering, en een slot zonder job heeft geen naamruimte.
#[test]
fn the_kern_refuses_dotdot_empty_names_and_jobless_slots() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = node(&svc, &con);
    start_job(&mut a, 3, b"", vec![]).unwrap();
    start_job(&mut a, 4, b"a/b", vec![]).unwrap();
    let (queue, reply, timer) = (
        StoreQueue::new(),
        crate::slots::Reply::new(),
        FakeTimer::default(),
    );
    let conr = &con;
    let c2 = cx_for(&queue, &svc, 2, &reply, &timer, &conr);
    for (op, key, local) in [
        (OP_STORE_PULL, &b"../slot1/x"[..], &b""[..]),
        (OP_STORE_PUSH, b"a/../../x", b""),
        (OP_STORE_PUSH, b"x", b"../y"),
    ] {
        assert!(
            matches!(valid(&c2, op, key, local), Err(Fail::Kern(Error::Denied))),
            "{:?}",
            core::str::from_utf8(key)
        );
    }
    for key in [&b""[..], b"/", b"./"] {
        assert!(matches!(
            valid(&c2, OP_STORE_DROP, key, b""),
            Err(Fail::Refused(STATUS_DENIED, _))
        ));
    }
    // List mag de hele map (een lege prefix).
    let v = valid(&c2, OP_STORE_LIST, b"", b"").ok().unwrap();
    assert_eq!(
        (v.key.as_bytes(), v.job.as_bytes()),
        (&b"/"[..], &b"demo"[..])
    );
    // De naam wordt genormaliseerd, het lokale pad blijft wat de app gaf.
    let v = valid(&c2, OP_STORE_PUSH, b"db//x.json", b"").ok().unwrap();
    assert_eq!(
        (v.key.as_bytes(), v.path),
        (&b"/db/x.json"[..], &b"db//x.json"[..])
    );
    let c3 = cx_for(&queue, &svc, 3, &reply, &timer, &conr);
    assert!(matches!(
        valid(&c3, OP_STORE_PUSH, b"x", b""),
        Err(Fail::Refused(STATUS_ERROR, why)) if why.contains("no job identity")
    ));
    let c4 = cx_for(&queue, &svc, 4, &reply, &timer, &conr);
    assert!(matches!(
        valid(&c4, OP_STORE_PUSH, b"x", b""),
        Err(Fail::Refused(STATUS_ERROR, why)) if why.contains("namespace")
    ));
}

/// De rij: begrensd en luid vol, de oudste eerst naar Hop, niemand die hem
/// ophaalt is na [`PICKUP`] een fout, een Hop die wegvalt geeft hem terug,
/// en een lijst die niet past wordt een fout in plaats van afgekapt.
#[test]
fn the_queue_is_bounded_ordered_and_gives_back_what_a_dead_hop_held() {
    let (svc, con) = (Servicers::new(), FakeConsole::default());
    let mut a = node(&svc, &con);
    let (queue, reply, timer) = (
        StoreQueue::new(),
        crate::slots::Reply::new(),
        FakeTimer::default(),
    );
    let conr = &con;
    let c2 = cx_for(&queue, &svc, 2, &reply, &timer, &conr);
    let g2 = svc.current(s(2)).unwrap();
    let v = valid(&c2, OP_STORE_LIST, b"p", b"").ok().unwrap();
    let mut tickets = Vec::new();
    for _ in 0..STORE_DEPTH {
        tickets.push(queue.submit(0, (s(2), g2), OP_STORE_LIST, &v).unwrap());
    }
    assert!(
        queue.submit(0, (s(2), g2), OP_STORE_LIST, &v).is_none(),
        "full"
    );
    let mut out = [0u8; 512];
    let hop1 = svc.current(s(1));
    let n = queue.take(&svc, hop1, &mut out).unwrap();
    let t = StoreTask::decode(&out[..n]).unwrap();
    assert_eq!(t.ticket, tickets[0].1, "the oldest first");
    // Nog niet opgehaald na PICKUP: een fout voor de app, en de plaats vrij.
    let (i1, t1) = tickets[1];
    let late = nanos(PICKUP) + 1;
    assert_eq!(
        queue.check(i1, t1, late, true, |_| true, &mut out),
        Some(Outcome::NoService)
    );
    // Hop valt weg met ticket 0 in handen: een fout voor de app (O5 van
    // 01-11), niet terug in de rij, en de plaats vrij.
    let (i0, t0) = tickets[0];
    assert_eq!(
        queue.check(i0, t0, 10, true, |_| false, &mut out),
        Some(Outcome::HopGone)
    );
    let (i2, t2) = tickets[2];
    let n = queue.take(&svc, hop1, &mut out).unwrap();
    assert_eq!(StoreTask::decode(&out[..n]).unwrap().ticket, t2);
    // Een lijst van meer dan 8 KiB: een fout, niet afgekapt.
    let big = vec![b'x'; MAX_STORE_LIST + 1];
    queue.finish(t2, STATUS_OK, 1, &big).ok().unwrap();
    match queue.check(i2, t2, 20, true, |_| true, &mut out) {
        Some(Outcome::Done { status, len, .. }) => {
            assert_eq!(status, STATUS_ERROR);
            assert!(
                core::str::from_utf8(&out[..len])
                    .unwrap()
                    .contains("narrower prefix")
            );
        }
        o => panic!("{o:?}"),
    }
    // Een app die weg is, krijgt Evicted en geeft haar plaats vrij.
    stop(&mut a, 2).unwrap();
    let (i3, t3) = tickets[3];
    assert_eq!(
        queue.check(i3, t3, 30, false, |_| true, &mut out),
        Some(Outcome::Evicted)
    );
    // En een opdracht van een dode app gaat niet meer naar Hop.
    assert!(queue.take(&svc, hop1, &mut out).is_none());
}
