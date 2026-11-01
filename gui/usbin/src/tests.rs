//! De Go-tests van `gui/usbin` voor de weg naar het glas (deliver_test.go),
//! plus de regelvorm die de Rust-vorm erbij vraagt.

use crate::deliver::{
    self, ConnError, Deliverer, InputAddr, InputConn, InputQueue, KEEPALIVE_NS, Line, Lines,
    allowed,
};
use abi::layout::{Slot, slot_ip4};
use core::future::{Future, pending, ready};
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use driver_hid::{Event, Kind};
use std::format;
use std::string::String;
use std::vec::Vec;

fn ev(kind: Kind, code: i32) -> Event {
    Event {
        kind,
        code,
        ..Event::default()
    }
}

/// Pollt een future die zonder wachten klaar hoort te zijn.
fn run_ready<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("future was not ready"),
    }
}

// Go: TestInputListensOnNodeStackAndRejectsNonHolder
//
// De socket is van de binary (hopos/src/net.rs, `input`) en wordt hier niet
// getoetst; wat hier blijft is het adres dat met de grant meereist en de
// weigering van wie het glas niet vasthoudt.
#[test]
fn input_addr_and_only_the_holder_is_allowed() {
    assert_eq!(
        format!("{InputAddr}"),
        "10.100.0.1:7879",
        "app-facing address changed"
    );
    let holder = Slot::new(3);
    let loopback = u32::from_be_bytes([127, 0, 0, 1]);
    assert!(!allowed(loopback, holder), "non-holder was accepted");
    assert!(
        !allowed(slot_ip4(Slot::new(2).unwrap()), holder),
        "neighbour slot accepted"
    );
    assert!(
        allowed(u32::from_be_bytes([10, 100, 0, 4]), holder),
        "holder refused"
    );
    assert!(
        !allowed(u32::from_be_bytes([10, 100, 0, 4]), None),
        "no holder, still accepted"
    );
}

/// Een verbinding die opschrijft en na `fail_after` regels dichtgaat.
struct Pipe {
    got: Vec<u8>,
    left: usize,
}

impl InputConn for Pipe {
    fn write(&mut self, line: &[u8]) -> impl Future<Output = Result<(), ConnError>> {
        let r = if self.left == 0 {
            Err(ConnError::Closed)
        } else {
            self.left -= 1;
            self.got.extend_from_slice(line);
            Ok(())
        };
        ready(r)
    }
    fn remote_ip4(&self) -> u32 {
        0
    }
}

// Go: TestIdleInputHeartbeatAndShutdown
#[test]
fn idle_input_heartbeat_and_shutdown() {
    assert_eq!(KEEPALIVE_NS, 5_000_000_000);
    let q = InputQueue::new();
    let (_tx, rx) = q.split().unwrap();
    let mut d = Deliverer::new(rx, None);
    // Een stille rij en een keepalive die afgaat: één lege regel.
    let lines = run_ready(d.next(ready(())));
    assert_eq!(lines.iter().collect::<Vec<_>>(), [b"\n".as_slice()]);
    // Een verbinding die dichtgaat, beëindigt `serve`.
    let mut p = Pipe {
        got: Vec::new(),
        left: 1,
    };
    let e = run_ready(d.serve(&mut p, || ready(())));
    assert_eq!(e, ConnError::Closed);
    assert_eq!(p.got, b"\n");
}

fn line_of(d: &Deliverer<'_>, e: Event) -> String {
    let mut l = Line::new();
    d.body(&e, &mut l);
    String::from_utf8(l.as_bytes().to_vec()).unwrap()
}

/// Exact de regels van Go's `body`.
#[test]
fn body_matches_go() {
    let q = InputQueue::new();
    let (_tx, rx) = q.split().unwrap();
    let d = Deliverer::new(rx, Some((1920, 1080)));
    assert_eq!(d.cursor(), (960, 540));
    assert_eq!(
        line_of(&d, ev(Kind::KeyDown, 65)),
        "{\"k\":\"key\",\"c\":65,\"v\":1}\n"
    );
    assert_eq!(
        line_of(&d, ev(Kind::KeyUp, 16)),
        "{\"k\":\"key\",\"c\":16,\"v\":0}\n"
    );
    assert_eq!(
        line_of(&d, ev(Kind::MouseMove, 0)),
        "{\"k\":\"move\",\"x\":960,\"y\":540}\n"
    );
    assert_eq!(
        line_of(&d, ev(Kind::MouseDown, 2)),
        "{\"k\":\"btn\",\"c\":2,\"v\":1,\"x\":960,\"y\":540}\n"
    );
    assert_eq!(
        line_of(&d, ev(Kind::MouseUp, 0)),
        "{\"k\":\"btn\",\"c\":0,\"v\":0,\"x\":960,\"y\":540}\n"
    );
    let wheel = Event {
        kind: Kind::MouseWheel,
        dy: -3,
        ..Event::default()
    };
    assert_eq!(
        line_of(&d, wheel),
        "{\"k\":\"wheel\",\"c\":0,\"v\":-3,\"x\":960,\"y\":540}\n"
    );
    // De langste regel past.
    let big = Event {
        kind: Kind::MouseDown,
        code: i32::MIN,
        ..Event::default()
    };
    assert!(line_of(&d, big).ends_with("}\n"));
}

/// Bewegingen worden samengevoegd tot de eerste niet-beweging, die daarna
/// meekomt; de cursor klemt op het scherm.
#[test]
fn moves_merge_and_clamp() {
    let q = InputQueue::new();
    let (mut tx, rx) = q.split().unwrap();
    let mut d = Deliverer::new(rx, Some((100, 50)));
    let mv = |dx, dy| Event {
        kind: Kind::MouseMove,
        dx,
        dy,
        ..Event::default()
    };
    for e in [mv(10, 10), mv(1000, 1000), ev(Kind::MouseDown, 0), mv(1, 1)] {
        assert!(tx.try_send(e).is_ok());
    }
    let lines = run_ready(d.next(pending::<()>()));
    let got: Vec<_> = lines
        .iter()
        .map(|l| String::from_utf8(l.to_vec()).unwrap())
        .collect();
    assert_eq!(
        got,
        [
            "{\"k\":\"move\",\"x\":99,\"y\":49}\n",
            "{\"k\":\"btn\",\"c\":0,\"v\":1,\"x\":99,\"y\":49}\n"
        ]
    );
    // De beweging na de klik is een eigen ronde: eerst naar de hoek
    // geklemd, dan de nog wachtende (1, 1) erbij gedraind, één regel.
    let mut out = Lines::default();
    d.handle(mv(-500, -500), &mut out);
    assert_eq!(d.cursor(), (1, 1));
    assert_eq!(out.len(), 1);
}

/// Vol is weggooien: de pollus blokkeert nooit.
#[test]
fn full_queue_drops() {
    let q = InputQueue::new();
    let (mut tx, _rx) = q.split().unwrap();
    for _ in 0..deliver::QUEUE_DEPTH {
        assert!(tx.try_send(ev(Kind::KeyDown, 65)).is_ok());
    }
    assert!(tx.try_send(ev(Kind::KeyDown, 65)).is_err());
}
