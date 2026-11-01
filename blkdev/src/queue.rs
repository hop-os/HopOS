//! De voorkant van het blokcontract: [`Queue`], voor één opdracht of
//! zestien tegelijk.
//!
//! Wie I/O wil, krijgt een ticket ([`AsyncBlockDevice::start_tag`]) en wacht
//! op zijn eigen completion. Naar het device kijken doet precies één
//! wachter tegelijk, de pacer: hij haalt met één
//! [`reap`](AsyncBlockDevice::reap) alle completions op, wekt de wachters
//! van wat terug is, en wacht zelf op de bel van de lijn, met de
//! [`TERM`](crate::TERM) als termijn. Wie geen ticket krijgt (alles bezet),
//! wacht op het eerste dat vrijkomt (Linux: de wachtrij van sbitmap voor de
//! tags); is er geen pacer (alleen wezen op het device), dan wacht hij zelf
//! op de bel.
//!
//! Is zijn eigen opdracht klaar, dan geeft de pacer de rol aan een andere
//! wachter: één blik op het device per bel, ook met zestien wachters
//! (Linux: één interrupt per completion, blk-mq `nvme_irq`).
//!
//! Eigendom: de [`Queue`] bezit de driver. Hij leeft in de taak die hem
//! gebruikt (de hopfs-actor) en wordt gedeeld door de futures van die ene
//! taak: de staat is een [`LocalCell`] die alleen binnen één poll geleend
//! wordt, nooit over een `.await` (handboek §1.1). Een buffer die aan het
//! device is gegeven, blijft van de driver tot de completion (§1.2): een
//! wachter die weggaat vóór zijn completion, laat zijn ticket achter als
//! wees, en de pacer ruimt het op als het device klaar is.

use crate::{AsyncBlockDevice, BatchRead, BlockIo, Error, LBA_SIZE, Op, Pace, Result, TERM};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use sync::LocalCell;

/// Zoveel tickets houdt een [`Queue`] hoogstens bij: één bit per ticket.
pub const MAX_DEPTH: usize = 64;

/// De voorkant van een device met tickets (zie de module).
pub struct Queue<D, P> {
    st: LocalCell<State<D>>,
    pace: P,
}

/// De staat achter de voorkant, alleen binnen één poll geleend.
struct State<D> {
    dev: D,
    depth: usize,
    /// Tickets die op het device staan (bit `t`).
    out: u64,
    /// Tickets waarvan de wachter wegging vóór de completion.
    orphan: u64,
    /// De waker per ticket, van wie erop wacht.
    wake: [Option<Waker>; MAX_DEPTH],
    /// De wachter die het device pollt.
    pacer: Option<usize>,
    /// Meetlat: het hoogste aantal tickets tegelijk.
    peak: usize,
    /// Meetlat: zo vaak keek de pacer naar het device (een `reap`).
    polls: u64,
    /// Wie op een vrij ticket wacht; gewekt zodra er een vrijkomt.
    room: [Option<Waker>; ROOM],
}

/// Zoveel wachters op een vrij ticket houdt de [`Queue`] bij (de plaatsen
/// in de pool van de hopfs-actor); wie er niet bij past, heeft de termijn.
const ROOM: usize = 16;

const fn bit(t: usize) -> u64 {
    1u64 << (t % MAX_DEPTH)
}

impl<D: AsyncBlockDevice> State<D> {
    /// Haalt de completions op en wekt hun wachters; een dood device wekt
    /// iedereen (elke `poll_tag` geeft dan de fout). Daarna de wezen.
    fn collect(&mut self) {
        self.polls += 1;
        let done = self.dev.reap().unwrap_or(u64::MAX);
        let mut m = done & self.out;
        while m != 0 {
            let t = m.trailing_zeros() as usize;
            m &= m - 1;
            if let Some(w) = self.wake.get_mut(t).and_then(Option::take) {
                w.wake();
            }
        }
        let mut o = self.orphan;
        while o != 0 {
            let t = o.trailing_zeros() as usize;
            o &= o - 1;
            if self.dev.poll_tag(t, &mut []).is_ready() {
                self.out &= !bit(t);
                self.orphan &= !bit(t);
                self.wake_room();
            }
        }
    }

    /// Ticket `t` is klaar: vrij, en de pacer-rol gaat door.
    fn release(&mut self, t: usize) {
        self.out &= !bit(t);
        self.leave(t);
        self.wake_room();
    }

    /// Er is een ticket vrij: wie erop wacht, probeert het.
    fn wake_room(&mut self) {
        for w in self.room.iter_mut().filter_map(Option::take) {
            w.wake();
        }
    }

    /// Wacht op een vrij ticket ([`State::wake_room`]); vol is de termijn.
    fn wait_room(&mut self, w: &Waker) {
        if self.room.iter().flatten().any(|o| o.will_wake(w)) {
            return;
        }
        if let Some(s) = self.room.iter_mut().find(|s| s.is_none()) {
            *s = Some(w.clone());
        }
    }

    /// De wachter van `t` gaat weg: geen waker meer, en was hij de pacer,
    /// dan gaat de rol door.
    fn leave(&mut self, t: usize) {
        if let Some(w) = self.wake.get_mut(t) {
            *w = None;
        }
        if self.pacer == Some(t) {
            self.pacer = None;
            // De volgende wachter pollt verder; geen wachter, dan neemt de
            // eerstvolgende die kijkt de rol.
            if let Some(w) = self.wake.iter_mut().find_map(Option::take) {
                w.wake();
            }
        }
    }

    /// Een ticket voor `op`, of [`Error::Busy`] als er nu geen plaats is.
    fn start(&mut self, op: Op<'_>) -> Result<usize> {
        if self.out.count_ones() as usize >= self.depth {
            self.collect();
        }
        if self.out.count_ones() as usize >= self.depth {
            return Err(Error::Busy);
        }
        let t = match self.dev.start_tag(op) {
            Err(Error::Busy) => {
                // De ruimte zit misschien bij een wees die al terug is.
                self.collect();
                self.dev.start_tag(op)?
            }
            r => r?,
        };
        // Een ticket buiten de diepte of dat al uitstaat, is een driverfout;
        // dan is er geen weg terug (de DMA-buffer kan van twee zijn).
        if t >= self.depth || self.out & bit(t) != 0 {
            return Err(Error::Dead);
        }
        self.out |= bit(t);
        self.peak = self.peak.max(self.out.count_ones() as usize);
        Ok(t)
    }

    fn park(&mut self, t: usize, w: &Waker) {
        if let Some(s) = self.wake.get_mut(t)
            && !s.as_ref().is_some_and(|o| o.will_wake(w))
        {
            *s = Some(w.clone());
        }
    }
}

impl<D: AsyncBlockDevice, P: Pace> Queue<D, P> {
    /// De voorkant van `dev`, wachtend op `pace`.
    pub fn new(dev: D, pace: P) -> Self {
        let depth = dev.depth().clamp(1, MAX_DEPTH);
        Self {
            st: LocalCell::cell(State {
                dev,
                depth,
                out: 0,
                orphan: 0,
                wake: [const { None }; MAX_DEPTH],
                pacer: None,
                peak: 0,
                polls: 0,
                room: [const { None }; ROOM],
            }),
            pace,
        }
    }

    /// Hoeveel tickets er tegelijk kunnen zijn.
    pub fn depth(&self) -> usize {
        self.st.borrow().depth
    }

    /// Meetlat: het hoogste aantal tickets tegelijk tot nu.
    pub fn peak(&self) -> usize {
        self.st.borrow().peak
    }

    /// Meetlat: zo vaak keek de pacer tot nu naar het device: hoogstens één
    /// keer per bel, submit of termijn.
    pub fn polls(&self) -> u64 {
        self.st.borrow().polls
    }

    /// Hoeveel tickets er nu uitstaan.
    pub fn in_flight(&self) -> usize {
        self.st.borrow().out.count_ones() as usize
    }

    /// De driver, kort geleend (een diagnose, een meetlat).
    pub fn with_dev<R>(&self, f: impl FnOnce(&mut D) -> R) -> R {
        f(&mut self.st.borrow_mut().dev)
    }

    /// Een ticket voor `op`, of [`Error::Busy`] als er nu geen plaats is.
    fn try_start(&self, op: Op<'_>) -> Result<usize> {
        self.st.borrow_mut().start(op)
    }

    /// Eén opdracht: een ticket (wachtend op plaats als alles bezet is),
    /// dan de completion; bij een lees de bytes in `into`.
    pub async fn io(&self, op: Op<'_>, into: &mut [u8]) -> Result {
        let t = loop {
            match self.try_start(op) {
                Err(Error::Busy) => {
                    Room {
                        q: self,
                        sleep: self.pace.sleep(TERM),
                        armed: false,
                    }
                    .await;
                }
                r => break r?,
            }
        };
        Wait {
            q: self,
            t,
            into,
            sleep: None,
            done: false,
        }
        .await
    }

    /// De brokmaat: de grootste transfer in hele LBA's, minstens één.
    fn step(&self) -> usize {
        let lba = LBA_SIZE as usize;
        let m = self.st.borrow().dev.max_transfer();
        (m - m % lba).max(lba)
    }
}

/// Het wachten op een vrij ticket: tot er een vrijkomt, of de termijn
/// afloopt. Een device met alleen wezen heeft geen pacer die iets
/// vrijgeeft: dan wacht deze wachter zelf op de bel, en zijn `try_start`
/// haalt de completions op.
#[must_use = "een future doet niets tot hij gepolld wordt"]
struct Room<'q, D: AsyncBlockDevice, P: Pace> {
    q: &'q Queue<D, P>,
    sleep: P::Sleep,
    armed: bool,
}

impl<D: AsyncBlockDevice, P: Pace> Future for Room<'_, D, P> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // `Room` is `Unpin`: een verwijzing, een `Unpin`-slaap en een vlag.
        let this = self.get_mut();
        if this.armed {
            return Poll::Ready(());
        }
        this.armed = true;
        let bell = {
            let mut st = this.q.st.borrow_mut();
            st.wait_room(cx.waker());
            st.pacer.is_none().then(|| st.dev.irq()).flatten()
        };
        if bell.is_some_and(|b| Pin::new(&mut b.wait()).poll(cx).is_ready())
            || Pin::new(&mut this.sleep).poll(cx).is_ready()
        {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

/// Het wachten op één ticket (zie de module).
#[must_use = "een future doet niets tot hij gepolld wordt"]
struct Wait<'q, 'b, D: AsyncBlockDevice, P: Pace> {
    q: &'q Queue<D, P>,
    t: usize,
    into: &'b mut [u8],
    sleep: Option<P::Sleep>,
    done: bool,
}

impl<D: AsyncBlockDevice, P: Pace> Future for Wait<'_, '_, D, P> {
    type Output = Result;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result> {
        // `Wait` is `Unpin`: verwijzingen, getallen en een `Unpin`-slaap.
        let this = self.get_mut();
        loop {
            let irq = {
                let mut st = this.q.st.borrow_mut();
                let t = this.t;
                let pacer = *st.pacer.get_or_insert(t) == t;
                if pacer {
                    st.collect();
                }
                if let Poll::Ready(r) = st.dev.poll_tag(t, this.into) {
                    st.release(t);
                    this.done = true;
                    return Poll::Ready(r);
                }
                if !pacer {
                    st.park(t, cx.waker());
                    return Poll::Pending;
                }
                st.dev.irq()
            };
            if irq.is_some_and(|b| Pin::new(&mut b.wait()).poll(cx).is_ready()) {
                this.sleep = None;
                continue;
            }
            let pace = &this.q.pace;
            let s = this.sleep.get_or_insert_with(|| pace.sleep(TERM));
            // De termijn is om: de volgende ronde kijkt de pacer opnieuw (en
            // de driver toetst zijn time-out). Niet hier opnieuw: met een
            // `Pace` die nooit slaapt ([`crate::Spin`]) zou dat één poll
            // zonder einde zijn.
            if Pin::new(s).poll(cx).is_ready() {
                this.sleep = None;
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
    }
}

impl<D: AsyncBlockDevice, P: Pace> Drop for Wait<'_, '_, D, P> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Weg vóór de completion: het ticket blijft van het device tot het
        // terug is (de wees), en de pacer-rol gaat door.
        if let Ok(mut st) = self.q.st.try_borrow_mut() {
            st.orphan |= bit(self.t);
            st.leave(self.t);
        }
    }
}

/// Een batch lezingen ([`BlockIo::read_batch`]): alles wat past gaat in
/// één poll op het device, en één wachter wacht op allemaal. Hij is de
/// pacer als dat nog niemand is (of als het een van zijn eigen tickets
/// is), anders parkeert hij zijn waker bij elk van zijn tickets: wie de
/// completions ophaalt, wekt hem. Lezingen die niet meer pasten, gaan erop
/// zodra er een terug is. Een lees groter dan één opdracht van het device
/// is [`Error::OutOfRange`] (de aanroeper knipt: hopfs doet het per stap).
#[must_use = "een future doet niets tot hij gepolld wordt"]
struct Batch<'q, 's, 'b, D: AsyncBlockDevice, P: Pace> {
    q: &'q Queue<D, P>,
    ops: &'s mut [BatchRead<'b>],
    sleep: Option<P::Sleep>,
}

impl<D: AsyncBlockDevice, P: Pace> Future for Batch<'_, '_, '_, D, P> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // `Batch` is `Unpin`: verwijzingen, een getal en een `Unpin`-slaap.
        let this = self.get_mut();
        let step = this.q.step();
        loop {
            let wait = {
                let mut st = this.q.st.borrow_mut();
                // Wat nog niet op het device staat, zolang er plaats is.
                for op in this
                    .ops
                    .iter_mut()
                    .filter(|o| !o.done && o.ticket.is_none())
                {
                    let len = op.into.len();
                    if len > step {
                        (op.result, op.done) = (Err(Error::OutOfRange { lba: op.lba, len }), true);
                        continue;
                    }
                    match st.start(Op::Read { lba: op.lba, len }) {
                        Ok(t) => op.ticket = Some(t),
                        Err(Error::Busy) => break,
                        Err(e) => (op.result, op.done) = (Err(e), true),
                    }
                }
                let ours = |p: usize| this.ops.iter().any(|o| o.ticket == Some(p));
                let pacer = match st.pacer {
                    Some(p) => ours(p),
                    None => match this.ops.iter().find_map(|o| o.ticket) {
                        Some(t) => {
                            st.pacer = Some(t);
                            true
                        }
                        None => false,
                    },
                };
                if pacer {
                    st.collect();
                }
                let mut freed = false;
                for op in this.ops.iter_mut() {
                    let Some(t) = op.ticket else { continue };
                    if let Poll::Ready(r) = st.dev.poll_tag(t, op.into) {
                        (op.result, op.done, op.ticket) = (r, true, None);
                        st.release(t);
                        freed = true;
                    }
                }
                if this.ops.iter().all(|o| o.done) {
                    return Poll::Ready(());
                }
                if freed && this.ops.iter().any(|o| !o.done && o.ticket.is_none()) {
                    // Er kwam plaats vrij: meteen de volgende erop.
                    continue;
                }
                let outstanding = this.ops.iter().any(|o| o.ticket.is_some());
                if this.ops.iter().any(|o| !o.done && o.ticket.is_none()) {
                    // Wat niet meer paste, gaat erop zodra er plaats is.
                    st.wait_room(cx.waker());
                }
                if pacer && outstanding {
                    st.dev.irq()
                } else if outstanding {
                    // Gewekt door wie de completions ophaalt.
                    for op in this.ops.iter() {
                        if let Some(t) = op.ticket {
                            st.park(t, cx.waker());
                        }
                    }
                    return Poll::Pending;
                } else {
                    // Niets van ons op het device en geen plaats: gewekt
                    // door het eerste vrije ticket, of zonder pacer (alleen
                    // wezen) door de bel, zoals [`Room`].
                    st.pacer.is_none().then(|| st.dev.irq()).flatten()
                }
            };
            if wait.is_some_and(|b| Pin::new(&mut b.wait()).poll(cx).is_ready()) {
                this.sleep = None;
                continue;
            }
            let pace = &this.q.pace;
            let s = this.sleep.get_or_insert_with(|| pace.sleep(TERM));
            // De termijn is om: de volgende ronde kijkt de pacer opnieuw (en
            // de driver toetst zijn time-out). Niet hier opnieuw: met een
            // `Pace` die nooit slaapt ([`crate::Spin`]) zou dat één poll
            // zonder einde zijn.
            if Pin::new(s).poll(cx).is_ready() {
                this.sleep = None;
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
    }
}

impl<D: AsyncBlockDevice, P: Pace> Drop for Batch<'_, '_, '_, D, P> {
    fn drop(&mut self) {
        // Weg vóór de completions: elk ticket blijft van het device tot het
        // terug is (de wezen), en de pacer-rol gaat door.
        if let Ok(mut st) = self.q.st.try_borrow_mut() {
            for op in self.ops.iter_mut() {
                if let Some(t) = op.ticket.take() {
                    st.orphan |= bit(t);
                    st.leave(t);
                }
            }
        }
    }
}

/// De voorkant als [`BlockIo`] voor hopfs: een gedeelde verwijzing, dus
/// meerdere verzoeken tegelijk, elk brok een ticket.
impl<D: AsyncBlockDevice, P: Pace> BlockIo for &Queue<D, P> {
    async fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result {
        let q = *self;
        let mut l = lba;
        for chunk in buf.chunks_mut(q.step()) {
            let len = chunk.len();
            q.io(Op::Read { lba: l, len }, chunk).await?;
            l += len as u64 / LBA_SIZE;
        }
        Ok(())
    }

    async fn write(&mut self, lba: u64, buf: &[u8]) -> Result {
        let q = *self;
        let mut l = lba;
        for chunk in buf.chunks(q.step()) {
            q.io(
                Op::Write {
                    lba: l,
                    data: chunk,
                },
                &mut [],
            )
            .await?;
            l += chunk.len() as u64 / LBA_SIZE;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result {
        let q = *self;
        q.io(Op::Flush, &mut []).await
    }

    fn read_batch(&mut self, ops: &mut [BatchRead<'_>]) -> impl Future<Output = ()> {
        let q = *self;
        Batch {
            q,
            ops,
            sleep: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::task::Wake;
    use std::vec;
    use std::vec::Vec;

    /// De nep-controller: opdrachten op tags, completions die de test
    /// loslaat. Een schrijf staat bij de start op de schijf (de DMA is
    /// gebeurd), maar is pas klaar als de test hem loslaat.
    struct Ctl {
        disk: Vec<u8>,
        /// Per tag: (lees, lba, lengte).
        tags: Vec<Option<(bool, u64, usize)>>,
        /// Losgelaten, nog niet opgehaald.
        ready: u64,
        /// Opgehaald, nog niet afgerond.
        done: u64,
        reaps: usize,
        /// Een lees van deze LBA's weigert het device bij de start.
        bad: Vec<u64>,
        /// De bel van de lijn, als de test er een bedraadt.
        bell: Option<&'static sync::Signal>,
    }

    #[derive(Clone)]
    struct Fake(Rc<RefCell<Ctl>>);

    fn fake(depth: usize) -> Fake {
        Fake(Rc::new(RefCell::new(Ctl {
            disk: (0..64 * 512).map(|i| (i / 512) as u8).collect(),
            tags: vec![None; depth],
            ready: 0,
            done: 0,
            reaps: 0,
            bad: Vec::new(),
            bell: None,
        })))
    }

    impl Fake {
        fn release(&self, t: usize) {
            self.0.borrow_mut().ready |= 1 << t;
        }
        fn reaps(&self) -> usize {
            self.0.borrow().reaps
        }
    }

    impl AsyncBlockDevice for Fake {
        fn max_transfer(&self) -> usize {
            4096
        }
        fn depth(&self) -> usize {
            self.0.borrow().tags.len()
        }
        fn irq(&self) -> Option<&'static sync::Signal> {
            self.0.borrow().bell
        }
        fn start_tag(&mut self, op: Op<'_>) -> Result<usize> {
            let mut c = self.0.borrow_mut();
            let t = c.tags.iter().position(Option::is_none).ok_or(Error::Busy)?;
            let rec = match op {
                Op::Read { lba, .. } if c.bad.contains(&lba) => return Err(Error::Io { lba }),
                Op::Read { lba, len } => (true, lba, len),
                Op::Write { lba, data } => {
                    let o = lba as usize * 512;
                    c.disk[o..o + data.len()].copy_from_slice(data);
                    (false, lba, data.len())
                }
                Op::Flush => (false, 0, 0),
            };
            c.tags[t] = Some(rec);
            Ok(t)
        }
        fn poll_tag(&mut self, t: usize, into: &mut [u8]) -> Poll<Result> {
            let mut c = self.0.borrow_mut();
            if c.done & (1 << t) == 0 {
                return Poll::Pending;
            }
            c.done &= !(1 << t);
            if let Some((true, lba, len)) = c.tags[t].take() {
                let o = lba as usize * 512;
                let n = len.min(into.len());
                into[..n].copy_from_slice(&c.disk[o..o + n]);
            }
            Poll::Ready(Ok(()))
        }
        fn reap(&mut self) -> Result<u64> {
            let mut c = self.0.borrow_mut();
            c.reaps += 1;
            let m = c.ready;
            c.ready = 0;
            c.done |= m;
            Ok(m)
        }
    }

    /// Een slaap die één ronde duurt.
    struct Tick;

    impl Pace for Tick {
        type Sleep = sync::YieldNow;
        fn sleep(&self, _d: core::time::Duration) -> sync::YieldNow {
            sync::yield_now()
        }
    }

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, SeqCst);
        }
    }

    fn waker() -> (Arc<Count>, Waker) {
        let c = Arc::new(Count(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    fn poll<F: Future>(f: Pin<&mut F>, w: &Waker) -> Poll<F::Output> {
        f.poll(&mut Context::from_waker(w))
    }

    fn rd(lba: u64) -> Op<'static> {
        Op::Read { lba, len: 512 }
    }

    #[test]
    fn three_reads_overlap_one_waiter_polls_and_each_is_woken_by_its_own_completion() {
        let dev = fake(4);
        let q = Queue::new(dev.clone(), Tick);
        let (mut a, mut b, mut c) = (vec![0u8; 512], vec![0u8; 512], vec![0u8; 512]);
        {
            let mut fa = core::pin::pin!(q.io(rd(1), &mut a));
            let mut fb = core::pin::pin!(q.io(rd(2), &mut b));
            let mut fc = core::pin::pin!(q.io(rd(3), &mut c));
            let ((_, wa), (nb, wb), (nc, wc)) = (waker(), waker(), waker());
            assert!(poll(fa.as_mut(), &wa).is_pending());
            assert!(poll(fb.as_mut(), &wb).is_pending());
            assert!(poll(fc.as_mut(), &wc).is_pending());
            assert_eq!(q.in_flight(), 3, "drie opdrachten tegelijk op het device");
            assert_eq!(dev.reaps(), 1, "alleen de pacer kijkt naar het device");
            // De completion van b: de pacer haalt hem op en wekt alleen b.
            dev.release(1);
            assert!(poll(fa.as_mut(), &wa).is_pending());
            assert_eq!((nb.0.load(SeqCst), nc.0.load(SeqCst)), (1, 0));
            assert_eq!(poll(fb.as_mut(), &wb), Poll::Ready(Ok(())));
            // De pacer is klaar: de rol gaat naar c.
            dev.release(0);
            assert_eq!(poll(fa.as_mut(), &wa), Poll::Ready(Ok(())));
            assert_eq!(nc.0.load(SeqCst), 1, "c neemt het pollen over");
            assert!(poll(fc.as_mut(), &wc).is_pending());
            dev.release(2);
            assert_eq!(poll(fc.as_mut(), &wc), Poll::Ready(Ok(())));
        }
        assert_eq!((q.in_flight(), q.peak()), (0, 3));
        assert!(b.iter().all(|&x| x == 2) && c.iter().all(|&x| x == 3));
    }

    #[test]
    fn a_full_device_makes_the_next_request_wait_for_a_completion() {
        let dev = fake(2);
        let q = Queue::new(dev.clone(), Tick);
        let (_, w) = waker();
        let mut f1 = core::pin::pin!(q.io(rd(1), &mut []));
        let mut f2 = core::pin::pin!(q.io(rd(2), &mut []));
        let mut f3 = core::pin::pin!(q.io(rd(3), &mut []));
        assert!(poll(f1.as_mut(), &w).is_pending());
        assert!(poll(f2.as_mut(), &w).is_pending());
        for _ in 0..4 {
            assert!(poll(f3.as_mut(), &w).is_pending());
        }
        assert_eq!(q.in_flight(), 2, "de derde wacht op plaats");
        dev.release(0);
        assert_eq!(poll(f1.as_mut(), &w), Poll::Ready(Ok(())));
        assert!(poll(f3.as_mut(), &w).is_pending());
        assert!(poll(f3.as_mut(), &w).is_pending());
        assert_eq!(q.in_flight(), 2, "nu staat de derde erop");
    }

    #[test]
    fn a_waiter_that_leaves_early_keeps_its_ticket_until_the_device_is_done() {
        let dev = fake(1);
        let q = Queue::new(dev.clone(), Tick);
        let (_, w) = waker();
        let mut buf = vec![0u8; 512];
        {
            let mut gone = core::pin::pin!(q.io(rd(5), &mut buf));
            assert!(poll(gone.as_mut(), &w).is_pending());
        }
        assert_eq!(q.in_flight(), 1, "de wees houdt zijn tag");
        let mut next = core::pin::pin!(q.io(rd(6), &mut []));
        for _ in 0..4 {
            assert!(poll(next.as_mut(), &w).is_pending());
        }
        assert_eq!(
            dev.0.borrow().tags[0].map(|t| t.1),
            Some(5),
            "niet hergebruikt"
        );
        dev.release(0);
        for _ in 0..4 {
            let _ = poll(next.as_mut(), &w);
        }
        assert_eq!(dev.0.borrow().tags[0].map(|t| t.1), Some(6));
        dev.release(0);
        assert_eq!(poll(next.as_mut(), &w), Poll::Ready(Ok(())));
        assert_eq!(q.in_flight(), 0);
    }

    fn batch<'b>(bufs: &'b mut [Vec<u8>], lbas: &[u64]) -> Vec<BatchRead<'b>> {
        bufs.iter_mut()
            .zip(lbas)
            .map(|(b, &l)| BatchRead::new(l, b))
            .collect()
    }

    /// Een batch: alle drie in één poll op het device, één reap per ronde,
    /// klaar als de laatste terug is, elk met zijn eigen bytes.
    #[test]
    fn a_batch_puts_every_read_on_the_device_in_one_poll_and_waits_once() {
        let dev = fake(4);
        let q = Queue::new(dev.clone(), Tick);
        let mut bufs = vec![vec![0u8; 512]; 3];
        let mut ops = batch(&mut bufs, &[4, 5, 6]);
        {
            let (_, w) = waker();
            let mut io = &q;
            let mut f = core::pin::pin!(io.read_batch(&mut ops));
            assert!(poll(f.as_mut(), &w).is_pending());
            assert_eq!(q.in_flight(), 3, "alle drie tegelijk");
            assert_eq!(dev.reaps(), 1);
            dev.release(2);
            dev.release(0);
            assert!(poll(f.as_mut(), &w).is_pending());
            assert_eq!(q.in_flight(), 1);
            dev.release(1);
            assert_eq!(poll(f.as_mut(), &w), Poll::Ready(()));
        }
        assert!(ops.iter().all(|o| o.result == Ok(())));
        for (b, l) in bufs.iter().zip([4u8, 5, 6]) {
            assert!(b.iter().all(|&x| x == l));
        }
        assert_eq!((q.in_flight(), q.peak()), (0, 3));
    }

    /// Meer dan de diepte: de rest gaat erop zodra er een terug is. Een
    /// lees die het device weigert, krijgt zijn fout; de andere niet.
    #[test]
    fn a_batch_deeper_than_the_device_waits_for_room_and_a_failure_stays_alone() {
        let dev = fake(2);
        dev.0.borrow_mut().bad.push(9);
        let q = Queue::new(dev.clone(), Tick);
        let mut bufs = vec![vec![0u8; 512]; 4];
        let mut ops = batch(&mut bufs, &[1, 9, 2, 3]);
        {
            let (_, w) = waker();
            let mut io = &q;
            let mut f = core::pin::pin!(io.read_batch(&mut ops));
            assert!(poll(f.as_mut(), &w).is_pending());
            assert_eq!(q.in_flight(), 2, "1 en 2 erop, 9 geweigerd, 3 wacht");
            dev.release(0);
            assert!(poll(f.as_mut(), &w).is_pending());
            assert_eq!(
                dev.0.borrow().tags[0].map(|t| t.1),
                Some(3),
                "3 op de vrije tag"
            );
            dev.release(0);
            dev.release(1);
            assert_eq!(poll(f.as_mut(), &w), Poll::Ready(()));
        }
        let r: Vec<Result> = ops.iter().map(|o| o.result).collect();
        assert_eq!(r, [Ok(()), Err(Error::Io { lba: 9 }), Ok(()), Ok(())]);
        assert!(bufs[3].iter().all(|&x| x == 3) && bufs[1].iter().all(|&x| x == 0));
    }

    /// Naast een gewone wachter: wie pollt, wekt de ander; en wie weggaat
    /// vóór zijn completions, laat zijn tickets als wezen achter.
    #[test]
    fn a_batch_shares_the_pacer_and_leaves_orphans_when_dropped() {
        let dev = fake(4);
        let q = Queue::new(dev.clone(), Tick);
        let (_, wa) = waker();
        let (nb, wb) = waker();
        let mut a = vec![0u8; 512];
        let mut single = core::pin::pin!(q.io(rd(7), &mut a));
        assert!(poll(single.as_mut(), &wa).is_pending(), "de pacer");
        let mut bufs = vec![vec![0u8; 512]; 2];
        {
            let mut ops = batch(&mut bufs, &[1, 2]);
            let mut io = &q;
            let mut f = core::pin::pin!(io.read_batch(&mut ops));
            assert!(poll(f.as_mut(), &wb).is_pending());
            assert_eq!(q.in_flight(), 3);
            dev.release(1);
            assert!(poll(single.as_mut(), &wa).is_pending());
            assert_eq!(nb.0.load(SeqCst), 1, "de pacer wekt de batch");
        }
        assert_eq!(q.in_flight(), 3, "beide lezingen van de batch zijn wezen");
        dev.release(2);
        dev.release(0);
        assert_eq!(poll(single.as_mut(), &wa), Poll::Ready(Ok(())));
        assert_eq!(q.in_flight(), 0, "de wees is opgeruimd");
    }

    /// Een slaap die nooit afloopt (zo ziet de test wie de wachter wekt).
    struct Clock;

    impl Pace for &Clock {
        type Sleep = core::future::Pending<()>;
        fn sleep(&self, _d: core::time::Duration) -> Self::Sleep {
            core::future::pending()
        }
    }

    /// De pacer pollt niet per ronde, ook niet vlak na zijn submit: hij
    /// slaapt op de bel, en de bel wekt hem. Eén blik op het device per bel.
    #[test]
    fn the_pacer_sleeps_on_the_bell_not_per_round() {
        static BELL: sync::Signal = sync::Signal::new();
        let dev = fake(4);
        dev.0.borrow_mut().bell = Some(&BELL);
        let q = Queue::new(dev.clone(), &Clock);
        let mut bufs = vec![vec![0u8; 512]; 2];
        let mut ops = batch(&mut bufs, &[1, 2]);
        let (n, w) = waker();
        let mut io = &q;
        let mut f = core::pin::pin!(io.read_batch(&mut ops));
        assert!(poll(f.as_mut(), &w).is_pending());
        assert_eq!(n.0.load(SeqCst), 0, "net gesubmit, toch geen wek per ronde");
        dev.release(0);
        dev.release(1);
        BELL.set();
        assert_eq!(n.0.load(SeqCst), 1, "de bel wekt de pacer");
        assert_eq!(poll(f.as_mut(), &w), Poll::Ready(()));
        assert_eq!(q.polls(), 2, "één blik bij de submit, één per bel");
    }

    /// Wie geen ticket krijgt, wordt gewekt door het eerste dat vrijkomt,
    /// niet pas door de timer (die loopt hier nooit af): een lees en een
    /// batch die allebei op plaats wachten.
    #[test]
    fn a_full_device_wakes_the_waiters_for_room_when_a_ticket_frees() {
        let dev = fake(1);
        let q = Queue::new(dev.clone(), &Clock);
        let (_, wa) = waker();
        let (nb, wb) = waker();
        let (nc, wc) = waker();
        let mut a = core::pin::pin!(q.io(rd(1), &mut []));
        assert!(poll(a.as_mut(), &wa).is_pending());
        let mut b = core::pin::pin!(q.io(rd(2), &mut []));
        assert!(poll(b.as_mut(), &wb).is_pending(), "geen plaats");
        let mut bufs = vec![vec![0u8; 512]; 1];
        let mut ops = batch(&mut bufs, &[3]);
        let mut io = &q;
        let mut c = core::pin::pin!(io.read_batch(&mut ops));
        assert!(poll(c.as_mut(), &wc).is_pending(), "geen plaats");
        assert_eq!((nb.0.load(SeqCst), nc.0.load(SeqCst)), (0, 0));
        dev.release(0);
        assert_eq!(poll(a.as_mut(), &wa), Poll::Ready(Ok(())));
        assert_eq!(
            (nb.0.load(SeqCst), nc.0.load(SeqCst)),
            (1, 1),
            "beide gewekt"
        );
        assert!(poll(b.as_mut(), &wb).is_pending());
        assert_eq!(dev.0.borrow().tags[0].map(|t| t.1), Some(2), "b kreeg hem");
    }

    /// Alleen een wees op het device, dus geen pacer: wie op plaats wacht,
    /// wacht zelf op de bel, en de bel geeft hem het ticket.
    #[test]
    fn with_only_orphans_the_waiter_for_room_listens_to_the_bell() {
        static BELL: sync::Signal = sync::Signal::new();
        let dev = fake(1);
        dev.0.borrow_mut().bell = Some(&BELL);
        let q = Queue::new(dev.clone(), &Clock);
        let (n, w) = waker();
        {
            let mut gone = core::pin::pin!(q.io(rd(5), &mut []));
            assert!(poll(gone.as_mut(), &w).is_pending());
        }
        let mut next = core::pin::pin!(q.io(rd(6), &mut []));
        assert!(poll(next.as_mut(), &w).is_pending(), "geen plaats");
        dev.release(0);
        BELL.set();
        assert_eq!(n.0.load(SeqCst), 1, "de bel wekt de wachter op plaats");
        assert!(poll(next.as_mut(), &w).is_pending());
        assert_eq!(
            dev.0.borrow().tags[0].map(|t| t.1),
            Some(6),
            "nu staat hij erop"
        );
    }
}
