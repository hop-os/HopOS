//! De brug tegen een nep-stroom op een executor met een klok die de test
//! zelf verzet: bytes erdoor, termijnen die aflopen op het timerwiel, de
//! kap, sluiten, en een heel leanhttp-verzoek over de brug.

use super::*;
use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::Waker;
use std::boxed::Box;
use std::rc::Rc;
use std::string::String;
use std::vec::Vec;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(1_000_000_000) };
}

/// De klok van de test: staat stil tot de test hem verzet.
fn now() -> u64 {
    NOW.with(Cell::get)
}

fn advance(d: Duration) {
    NOW.with(|c| c.set(c.get() + u64::try_from(d.as_nanos()).unwrap()));
}

fn exec() -> &'static Exec {
    let e: &'static Exec = Box::leak(Box::new(Exec::new()));
    e.set_clock(now);
    e
}

/// Wat de nep-stroom zag en nog heeft, gedeeld met de test.
#[derive(Default)]
struct Wire {
    input: Vec<u8>,
    at: usize,
    /// Na de input: EOF (`true`) of stilte (`false`, een read blijft hangen).
    eof: bool,
    out: Vec<u8>,
    /// Een write die niet past: blijft hangen.
    stall_writes: bool,
    fail: Option<NetError>,
    closes: u32,
}

struct Fake(Rc<RefCell<Wire>>);

impl Stream for Fake {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, NetError>> {
        poll_fn(move |_| {
            let mut w = self.0.borrow_mut();
            if let Some(e) = w.fail {
                return Poll::Ready(Err(e));
            }
            let rest = &w.input[w.at..];
            if rest.is_empty() && !w.eof {
                return Poll::Pending;
            }
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            w.at += n;
            Poll::Ready(Ok(n))
        })
    }

    fn write(&mut self, data: &[u8]) -> impl Future<Output = Result<usize, NetError>> {
        poll_fn(move |_| {
            let mut w = self.0.borrow_mut();
            if let Some(e) = w.fail {
                return Poll::Ready(Err(e));
            }
            if w.stall_writes {
                return Poll::Pending;
            }
            w.out.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        })
    }

    fn close(self) -> Result<(), NetError> {
        self.0.borrow_mut().closes += 1;
        Ok(())
    }
}

fn conn(wire: Wire) -> (TcpConn<Fake>, Rc<RefCell<Wire>>, &'static Exec) {
    let w = Rc::new(RefCell::new(wire));
    let e = exec();
    (TcpConn::new(Fake(w.clone()), e), w, e)
}

fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

#[test]
fn bytes_pass_through_both_ways() {
    let (mut c, w, _) = conn(Wire {
        input: b"hello".to_vec(),
        eof: true,
        ..Wire::default()
    });
    let mut buf = [0u8; 8];
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Ready(Ok(5)));
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Ready(Ok(0)));
    assert_eq!(c.poll_write(&mut cx(), b"back"), Poll::Ready(Ok(4)));
    assert_eq!(w.borrow().out, b"back");
}

#[test]
fn a_silent_read_times_out_on_its_deadline() {
    let (mut c, _, e) = conn(Wire::default());
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Pending);
    // De wekker staat in het wiel: de taak wordt op de termijn gewekt, niet
    // pas als de peer iets stuurt.
    assert_eq!(e.next_deadline(), Some(now() + 2_000_000_000));
    advance(Duration::from_millis(1999));
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Pending);
    advance(Duration::from_millis(1));
    assert_eq!(
        c.poll_read(&mut cx(), &mut buf),
        Poll::Ready(Err(IoError::TimedOut))
    );
}

#[test]
fn the_cap_shortens_a_long_read_deadline() {
    let (c, _, e) = conn(Wire::default());
    let mut c = c.with_read_cap(Duration::from_secs(5));
    // leanhttp's keep-alive-stilte: 60 s, afgekapt op 5.
    c.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Pending);
    assert_eq!(e.next_deadline(), Some(now() + 5_000_000_000));
    advance(Duration::from_secs(5));
    assert_eq!(
        c.poll_read(&mut cx(), &mut buf),
        Poll::Ready(Err(IoError::TimedOut))
    );
}

#[test]
fn no_deadline_stays_no_deadline_under_a_cap() {
    let (c, _, _) = conn(Wire::default());
    let mut c = c.with_read_cap(Duration::from_secs(5));
    c.set_read_timeout(None).unwrap();
    let mut buf = [0u8; 8];
    advance(Duration::from_secs(3600));
    assert_eq!(c.poll_read(&mut cx(), &mut buf), Poll::Pending);
}

#[test]
fn a_stalled_write_times_out_on_its_deadline() {
    let (mut c, _, _) = conn(Wire {
        stall_writes: true,
        ..Wire::default()
    });
    c.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(c.poll_write(&mut cx(), b"x"), Poll::Pending);
    advance(Duration::from_secs(1));
    assert_eq!(
        c.poll_write(&mut cx(), b"x"),
        Poll::Ready(Err(IoError::TimedOut))
    );
}

#[test]
fn closing_twice_is_closing_once() {
    let (mut c, w, _) = conn(Wire::default());
    assert_eq!(c.poll_close(&mut cx()), Poll::Ready(Ok(())));
    assert_eq!(c.poll_close(&mut cx()), Poll::Ready(Ok(())));
    assert_eq!(w.borrow().closes, 1);
    let mut buf = [0u8; 4];
    assert_eq!(
        c.poll_read(&mut cx(), &mut buf),
        Poll::Ready(Err(IoError::Closed))
    );
    assert_eq!(
        c.poll_write(&mut cx(), b"x"),
        Poll::Ready(Err(IoError::Closed))
    );
}

#[test]
fn net_errors_become_connection_errors() {
    assert_eq!(io_error(NetError::Timeout), IoError::TimedOut);
    assert_eq!(
        io_error(NetError::Stack(StackError::DeadlineExceeded)),
        IoError::TimedOut
    );
    assert_eq!(io_error(NetError::Stack(StackError::Reset)), IoError::Reset);
    for e in [
        StackError::Closed,
        StackError::TcpClosed,
        StackError::StackClosed,
    ] {
        assert_eq!(io_error(NetError::Stack(e)), IoError::Closed);
    }
    assert_eq!(io_error(NetError::Busy), IoError::Other);
    // En een reset van de stroom komt als reset bij leanhttp.
    let (mut c, _, _) = conn(Wire {
        fail: Some(NetError::Stack(StackError::Reset)),
        ..Wire::default()
    });
    let mut buf = [0u8; 4];
    assert_eq!(
        c.poll_read(&mut cx(), &mut buf),
        Poll::Ready(Err(IoError::Reset))
    );
}

/// Pollt `f` tot hij klaar is; de nep-stroom is nooit `Pending` als er
/// input en EOF klaarliggen, dus een paar rondes zijn genoeg.
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    for _ in 0..10_000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx()) {
            return v;
        }
    }
    panic!("future bleef hangen");
}

#[test]
fn leanhttp_serves_a_request_over_the_bridge() {
    let (c, w, _) = conn(Wire {
        input: b"GET /health HTTP/1.1\r\nHost: n\r\n\r\n".to_vec(),
        eof: true,
        ..Wire::default()
    });
    let mut paths = Vec::new();
    let mut handler = async |ex: &mut leanhttp::Exchange<'_, TcpConn<Fake>>| {
        paths.push(ex.req.path.clone());
        ex.write_header(200)?;
        ex.write(b"ok").await.map(|_| ())
    };
    // Eén verzoek per aanroep: de verbinding komt terug (keep-alive), en de
    // volgende aanroep ziet de EOF en sluit.
    let out = block_on(leanhttp::serve(c, &mut handler));
    let Ok(leanhttp::Outcome::Kept(kept)) = out else {
        panic!("keep-alive mag: de verbinding hoort terug te komen")
    };
    let out = block_on(kept.serve(&mut handler));
    assert!(matches!(out, Ok(leanhttp::Outcome::Closed)));
    assert_eq!(paths, ["/health"]);
    let text = String::from_utf8(w.borrow().out.clone()).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.ends_with("\r\n\r\nok"), "{text}");
    // De server sloot de verbinding zelf, één keer.
    assert_eq!(w.borrow().closes, 1);
}
