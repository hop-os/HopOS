//! Gereedschap voor de host-tests: een mini-executor zonder heap-taken.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::cell::Cell;
use std::time::Duration;

/// Draait `f` tot hij klaar is (polt in een lus; de nep-traits wekken niet).
pub(crate) fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("future did not finish");
}

/// Draait twee futures naast elkaar tot beide klaar zijn.
pub(crate) fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = pin!(a);
    let mut b = pin!(b);
    let (mut ra, mut rb) = (None, None);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if ra.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(&mut cx)
        {
            ra = Some(v);
        }
        if rb.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(&mut cx)
        {
            rb = Some(v);
        }
        if let (Some(_), Some(_)) = (&ra, &rb) {
            return (ra.unwrap(), rb.unwrap());
        }
    }
    panic!("futures did not finish");
}

/// Een nep-klok: `sleep` schuift de tijd op en yieldt één keer.
#[derive(Default)]
pub(crate) struct FakeTimer {
    pub(crate) now: Cell<u64>,
}

impl crate::cage::Timer for FakeTimer {
    fn now(&self) -> u64 {
        self.now.get()
    }
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        self.now.set(self.now.get() + d.as_nanos() as u64);
        sync::yield_now()
    }
}

/// Een klok die per slaap hoogstens een milliseconde opschuift: een lange
/// wacht (de stilte van een verbinding, `NEXT_STORE`, de ophaaltijd van een
/// store-call) blijft zo echt wachten terwijl een andere taak verder gaat, in
/// plaats van in één sprong af te lopen zoals op [`FakeTimer`].
#[derive(Default)]
pub(crate) struct SlowTimer(pub(crate) Cell<u64>);

impl crate::cage::Timer for SlowTimer {
    fn now(&self) -> u64 {
        self.0.get()
    }
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        let step = d.min(Duration::from_millis(1));
        self.0.set(self.0.get() + step.as_nanos() as u64);
        sync::yield_now()
    }
}
