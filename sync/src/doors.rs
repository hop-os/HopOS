//! De deuren van een vaste pool werkers: [`Doors`].

use crate::{Local, Oneshot, Signal};
use core::cell::Cell;

/// Eén deur: de antwoordplek waarin de acceptor het werk legt, en of de
/// werker bezet is (van de overdracht tot [`Doors::free`]).
struct Door<T> {
    job: Oneshot<T>,
    busy: Cell<bool>,
}

/// Een vaste pool van `N` werkers met één acceptor (handboek §2: geen taak
/// per verbinding).
///
/// De acceptor legt het werk achter de eerste vrije deur (een [`Oneshot`],
/// die de werker wekt); de werker neemt het ([`take`](Self::take)), dient
/// het, en meldt zich vrij ([`free`](Self::free)), en dat luidt de bel van
/// de acceptor. Met alle werkers bezet wacht [`place`](Self::place) dus op
/// een gebeurtenis, niet op de klok (docs/apps.md); wie liever weigert,
/// neemt [`hand`](Self::hand). Elke deur en de bel hebben één wachter:
/// werker `i` de zijne, de acceptor die van de vrije deuren.
///
/// Kwamen er minder werkers op dan deuren (een OOM bij het spawnen), dan
/// zegt [`staffed`](Self::staffed) dat vóór de eerste overdracht: de deuren
/// zonder werker blijven dan bezet, en geen job komt er terecht.
///
/// Alles op de executor van één core: de deuren staan in een [`Local`].
pub struct Doors<T, const N: usize> {
    doors: Local<[Door<T>; N]>,
    freed: Signal,
}

impl<T, const N: usize> Doors<T, N> {
    /// Een pool met alle deuren vrij, voor in een `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            doors: Local::new(
                [const {
                    Door {
                        job: Oneshot::new(),
                        busy: Cell::new(false),
                    }
                }; N],
            ),
            freed: Signal::new(),
        }
    }

    /// Alleen de eerste `n` deuren hebben een werker: de rest blijft
    /// voorgoed bezet, dus [`hand`](Self::hand) en [`place`](Self::place)
    /// leggen er nooit een job achter (die zou daar hangen met alles wat
    /// hij vasthoudt). Te roepen vóór de eerste overdracht, door wie de
    /// werkers spawnde; een werker roept [`free`](Self::free) alleen met
    /// zijn eigen deur, dus een dichte deur gaat nooit meer open.
    pub fn staffed(&self, n: usize) {
        for d in self.doors.iter().skip(n) {
            d.busy.set(true);
        }
    }

    /// Geeft `job` aan de eerste vrije werker en wekt hem; alle werkers
    /// bezet geeft de job terug.
    ///
    /// # Errors
    ///
    /// `job` zelf, als geen deur vrij is.
    pub fn hand(&self, job: T) -> Result<(), T> {
        match self.doors.iter().find(|d| !d.busy.replace(true)) {
            Some(d) => {
                d.job.put(job);
                Ok(())
            }
            None => Err(job),
        }
    }

    /// Geeft `job` aan een vrije werker; zijn ze alle bezet, dan wacht hij
    /// tot er een [`free`](Self::free) meldt.
    pub async fn place(&self, mut job: T) {
        while let Err(back) = self.hand(job) {
            job = back;
            self.freed.wait().await;
        }
    }

    /// Werker `i` wacht op werk en neemt het; zijn deur is bezet tot
    /// [`free`](Self::free). Een werker zonder deur (`i >= N`) krijgt nooit
    /// iets.
    pub async fn take(&self, i: usize) -> T {
        match self.doors.get().get(i) {
            Some(d) => d.job.recv().await,
            None => core::future::pending().await,
        }
    }

    /// Werker `i` is klaar: zijn deur gaat open en de acceptor hoort het.
    pub fn free(&self, i: usize) {
        if let Some(d) = self.doors.get().get(i) {
            d.busy.set(false);
        }
        self.freed.set();
    }
}

impl<T, const N: usize> Default for Doors<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use core::task::Poll;

    #[test]
    fn the_first_free_door_gets_the_job_and_a_full_pool_gives_it_back() {
        let d: Doors<u32, 2> = Doors::new();
        let (_, w) = waker();
        assert_eq!(d.hand(1), Ok(()));
        assert_eq!(d.hand(2), Ok(()));
        assert_eq!(d.hand(3), Err(3));
        let mut t1 = core::pin::pin!(d.take(1));
        assert_eq!(poll_once(&mut t1, &w), Poll::Ready(2));
        // Werker 1 dient nog: zijn deur blijft dicht tot hij vrij meldt.
        assert_eq!(d.hand(3), Err(3));
        d.free(1);
        assert_eq!(d.hand(3), Ok(()));
        // Een werker zonder deur wacht voor altijd.
        let mut none = core::pin::pin!(d.take(2));
        assert_eq!(poll_once(&mut none, &w), Poll::Pending);
    }

    #[test]
    fn a_door_without_a_worker_never_gets_a_job() {
        // Drie deuren, maar de spawn stopte na één werker (F10): de job
        // moet bij deur 0 komen of terug, nooit achter deur 1 of 2.
        let d: Doors<u32, 3> = Doors::new();
        let (_, w) = waker();
        d.staffed(1);
        assert_eq!(d.hand(1), Ok(()));
        assert_eq!(d.hand(2), Err(2));
        let mut t0 = core::pin::pin!(d.take(0));
        assert_eq!(poll_once(&mut t0, &w), Poll::Ready(1));
        assert_eq!(d.hand(2), Err(2));
        d.free(0);
        assert_eq!(d.hand(2), Ok(()));
        let mut t0 = core::pin::pin!(d.take(0));
        assert_eq!(poll_once(&mut t0, &w), Poll::Ready(2));
        for i in 1..3 {
            let mut t = core::pin::pin!(d.take(i));
            assert_eq!(poll_once(&mut t, &w), Poll::Pending);
        }
        // Met alle deuren bemand verandert er niets.
        let all: Doors<u32, 2> = Doors::new();
        all.staffed(2);
        assert_eq!(all.hand(1), Ok(()));
        assert_eq!(all.hand(2), Ok(()));
        assert_eq!(all.hand(3), Err(3));
    }

    #[test]
    fn a_full_pool_waits_for_a_free_door_not_for_the_clock() {
        let d: Doors<u32, 1> = Doors::new();
        let (woken, w) = waker();
        let mut t0 = core::pin::pin!(d.take(0));
        assert_eq!(poll_once(&mut t0, &w), Poll::Pending);
        assert_eq!(d.hand(7), Ok(()));
        assert_eq!(poll_once(&mut t0, &w), Poll::Ready(7));
        let mut p = core::pin::pin!(d.place(8));
        assert_eq!(poll_once(&mut p, &w), Poll::Pending);
        let before = woken.0.load(std::sync::atomic::Ordering::SeqCst);
        d.free(0);
        assert!(woken.0.load(std::sync::atomic::Ordering::SeqCst) > before);
        assert_eq!(poll_once(&mut p, &w), Poll::Ready(()));
        let mut again = core::pin::pin!(d.take(0));
        assert_eq!(poll_once(&mut again, &w), Poll::Ready(8));
    }
}
