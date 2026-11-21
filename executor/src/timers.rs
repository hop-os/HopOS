//! De timers van een executor: twee min-heaps op één tabel die groeit.
//!
//! Tot 21-11 was dit een vaste tabel van `TIMERS` plaatsen (32 op een
//! app-core), en een timer die geen plaats vond spinde op ronde-korrel
//! (`HOPOS_TIMER_OVERFLOW`). 16-11 vulde een herstel met 16 parallelle
//! S3-GET's die tabel (elke GET een termijn plus de lees- en schrijftermijn
//! van zijn `TcpConn`), en EasyFlor bouwde er een eigen wiel omheen. Hier
//! groeit de tabel mee, zodat een app daar niet omheen hoeft te bouwen
//! (docs/lagen-plan.md van EasyFlor, P4).
//!
//! De vorm:
//!
//! - [`Timers::places`] houdt de registraties; een vrije plaats staat in een
//!   lijst door de tabel heen ([`Place::Free`]). Een [`Handle`] is een plaats
//!   met de generatie van zijn bewoner, zoals voorheen: een `After` die zijn
//!   plaats kwijt is (verlopen, hergebruikt), raakt nooit die van een ander.
//! - Twee heaps van plaatsnummers, op deadline: de gewone timers
//!   ([`Timers::wake`], de slaap kijkt alleen daarnaar) en de
//!   [uitstelbare](crate::Executor::after_deferrable) ([`Timers::lazy`]).
//!   Elke registratie weet haar positie in haar heap, dus annuleren is
//!   O(log n) en laat niets achter (een luie wis liet bij een termijn per
//!   lees duizenden dode plaatsen staan tot hun deadline).
//! - Groeien is het koude pad: de eerste registratie reserveert `first`
//!   plaatsen (`TIMERS` van de executor), daarna verdubbelt de tabel, met
//!   `try_reserve` (handboek §6: allocatie kan falen, en dat is een
//!   `Result`). De tabel krimpt nooit: in de vaste toestand alloceert een
//!   timer niets, en de capaciteit is de hoogste stand afgerond naar boven.
//! - Het plafond `max` ([`crate::TIMER_CEILING`]) houdt een lek uit de heap:
//!   daarboven, of als de heap vol is, is het [`NoRoom`] en spint de timer
//!   als vroeger, geteld.

use alloc::vec::Vec;
use core::task::Waker;

/// Het einde van de vrije lijst.
const NONE: usize = usize::MAX;

/// Eén registratie.
struct Entry {
    /// De deadline, in nanoseconden op de klok.
    at: u64,
    /// De generatie waarmee deze registratie haar plaats nam.
    generation: u32,
    /// Uitstelbaar: staat in [`Timers::lazy`] in plaats van [`Timers::wake`].
    deferrable: bool,
    waker: Waker,
    /// De positie in haar heap.
    pos: usize,
}

/// Een plaats in de tabel.
enum Place {
    /// Vrij; de volgende vrije plaats (of [`NONE`]).
    Free(usize),
    Busy(Entry),
}

/// Een registratie zoals de `After` haar kent: de plaats en de generatie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle {
    place: usize,
    generation: u32,
}

/// Geen plaats: het plafond is bereikt of de heap kon de tabel niet laten
/// groeien.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NoRoom;

/// De timers van één executor; de executor houdt ze in een `RefCell` en
/// leent ze nooit over een poll of een wek heen.
pub(crate) struct Timers {
    places: Vec<Place>,
    /// De eerste vrije plaats in `places`, of [`NONE`].
    free: usize,
    /// De heap van de gewone timers: plaatsnummers, de vroegste vooraan.
    wake: Vec<usize>,
    /// De heap van de uitstelbare timers.
    lazy: Vec<usize>,
    /// De generatie van de vorige registratie.
    generation: u32,
}

impl Timers {
    /// Een lege tabel; alloceert niets.
    pub(crate) const fn new() -> Self {
        Self {
            places: Vec::new(),
            free: NONE,
            wake: Vec::new(),
            lazy: Vec::new(),
            generation: 0,
        }
    }

    /// Lopende registraties.
    pub(crate) fn live(&self) -> usize {
        self.wake.len().saturating_add(self.lazy.len())
    }

    /// Plaatsen waarvoor geheugen gereserveerd is.
    pub(crate) fn capacity(&self) -> usize {
        self.places.capacity()
    }

    /// De vroegste deadline van een gewone timer.
    pub(crate) fn next_wake(&self) -> Option<u64> {
        self.wake.first().map(|&p| at(&self.places, p))
    }

    /// Registreert een deadline met zijn waker. `Ok(true)` als de tabel
    /// daarvoor groeide (een allocatie, de meetlat telt hem).
    pub(crate) fn add(
        &mut self,
        deadline: u64,
        deferrable: bool,
        waker: &Waker,
        first: usize,
        max: usize,
    ) -> Result<(Handle, bool), NoRoom> {
        let grew = self.room(deferrable, first, max)?;
        let place = if self.free == NONE {
            let p = self.places.len();
            // Past zonder allocatie: `room` zag plaats achter `len`.
            self.places.push(Place::Free(NONE));
            p
        } else {
            let p = self.free;
            self.free = match self.places.get(p) {
                Some(Place::Free(next)) => *next,
                // Een bezette plaats in de vrije lijst kan niet; dan liever
                // de lijst kwijt (de tabel groeit erna gewoon door).
                _ => NONE,
            };
            p
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let heap = if deferrable {
            &mut self.lazy
        } else {
            &mut self.wake
        };
        let pos = heap.len();
        // Past zonder allocatie: `room` zag plaats in deze heap.
        heap.push(place);
        if let Some(slot) = self.places.get_mut(place) {
            *slot = Place::Busy(Entry {
                at: deadline,
                generation,
                deferrable,
                waker: waker.clone(),
                pos,
            });
        }
        sift_up(heap, &mut self.places, pos);
        Ok((Handle { place, generation }, grew))
    }

    /// Zet een nieuwe waker op een registratie die nog van `h` is. `false`
    /// als zij er niet meer is (verlopen, of de plaats is van een ander).
    pub(crate) fn refresh(&mut self, h: Handle, waker: &Waker) -> bool {
        match self.places.get_mut(h.place) {
            Some(Place::Busy(e)) if e.generation == h.generation => {
                e.waker.clone_from(waker);
                true
            }
            _ => false,
        }
    }

    /// Haalt de registratie van `h` weg, als zij nog van `h` is, en geeft
    /// haar waker terug (de aanroeper dropt hem buiten de lening).
    pub(crate) fn remove(&mut self, h: Handle) -> Option<Waker> {
        let (deferrable, pos) = match self.places.get(h.place) {
            Some(Place::Busy(e)) if e.generation == h.generation => (e.deferrable, e.pos),
            _ => return None,
        };
        let heap = if deferrable {
            &mut self.lazy
        } else {
            &mut self.wake
        };
        remove_at(heap, &mut self.places, pos);
        self.release(h.place)
    }

    /// De waker van één verstreken timer (`at <= now`), uit de tabel; `None`
    /// als er geen meer verstreken is. De vroegste eerst, binnen elke heap.
    pub(crate) fn pop_due(&mut self, now: u64) -> Option<Waker> {
        for deferrable in [false, true] {
            let heap = if deferrable {
                &mut self.lazy
            } else {
                &mut self.wake
            };
            if let Some(&top) = heap.first()
                && at(&self.places, top) <= now
            {
                remove_at(heap, &mut self.places, 0);
                return self.release(top);
            }
        }
        None
    }

    /// Zorgt voor plaats voor één registratie meer: een vrije plaats in de
    /// tabel en in de heap van zijn soort. Groeit zo nodig (verdubbelen,
    /// minstens `first`, hooguit `max`). `Ok(true)` als er gegroeid is.
    fn room(&mut self, deferrable: bool, first: usize, max: usize) -> Result<bool, NoRoom> {
        let want = self.live().saturating_add(1);
        if want > max {
            return Err(NoRoom);
        }
        let heap = if deferrable { &self.lazy } else { &self.wake };
        let place_ok = self.free != NONE || self.places.len() < self.places.capacity();
        if place_ok && heap.len() < heap.capacity() {
            return Ok(false);
        }
        let target = first
            .max(self.places.capacity().saturating_mul(2))
            .max(want)
            .min(max);
        // Alle drie op dezelfde maat: een heap is nooit langer dan de tabel,
        // dus daarna groeit alleen de tabel weer, en de heaps met haar mee.
        // Een half gelukte groei laat wat gereserveerd is staan; dat is
        // geen fout, alleen geheugen dat de volgende groei al heeft.
        let places_more = target.saturating_sub(self.places.len());
        let wake_more = target.saturating_sub(self.wake.len());
        let lazy_more = target.saturating_sub(self.lazy.len());
        self.places
            .try_reserve_exact(places_more)
            .map_err(|_| NoRoom)?;
        self.wake.try_reserve_exact(wake_more).map_err(|_| NoRoom)?;
        self.lazy.try_reserve_exact(lazy_more).map_err(|_| NoRoom)?;
        Ok(true)
    }

    /// Zet plaats `p` terug in de vrije lijst en geeft de waker van zijn
    /// bewoner. De plaats is al uit haar heap.
    fn release(&mut self, p: usize) -> Option<Waker> {
        let slot = self.places.get_mut(p)?;
        match core::mem::replace(slot, Place::Free(self.free)) {
            Place::Busy(e) => {
                self.free = p;
                Some(e.waker)
            }
            Place::Free(next) => {
                // Was al vrij: niets veranderen.
                *slot = Place::Free(next);
                None
            }
        }
    }
}

/// De deadline op plaats `p` (`u64::MAX` voor een vrije, wat niet voorkomt
/// in een heap).
fn at(places: &[Place], p: usize) -> u64 {
    match places.get(p) {
        Some(Place::Busy(e)) => e.at,
        _ => u64::MAX,
    }
}

fn set_pos(places: &mut [Place], p: usize, pos: usize) {
    if let Some(Place::Busy(e)) = places.get_mut(p) {
        e.pos = pos;
    }
}

/// Wisselt de posities `a` en `b` in de heap, met de posities in de tabel.
fn swap(heap: &mut [usize], places: &mut [Place], a: usize, b: usize) {
    let (Some(&pa), Some(&pb)) = (heap.get(a), heap.get(b)) else {
        return;
    };
    heap.swap(a, b);
    set_pos(places, pa, b);
    set_pos(places, pb, a);
}

/// De deadline op heappositie `i`, of `None` voorbij het einde.
fn key(heap: &[usize], places: &[Place], i: usize) -> Option<u64> {
    heap.get(i).map(|&p| at(places, p))
}

fn sift_up(heap: &mut [usize], places: &mut [Place], mut i: usize) {
    while i > 0 {
        let parent = (i - 1) / 2;
        match (key(heap, places, i), key(heap, places, parent)) {
            (Some(k), Some(pk)) if k < pk => {
                swap(heap, places, i, parent);
                i = parent;
            }
            _ => return,
        }
    }
}

fn sift_down(heap: &mut [usize], places: &mut [Place], mut i: usize) {
    loop {
        let Some(mut least) = key(heap, places, i) else {
            return;
        };
        let mut to = i;
        for child in [
            i.saturating_mul(2).saturating_add(1),
            i.saturating_mul(2).saturating_add(2),
        ] {
            if let Some(k) = key(heap, places, child)
                && k < least
            {
                least = k;
                to = child;
            }
        }
        if to == i {
            return;
        }
        swap(heap, places, i, to);
        i = to;
    }
}

/// Haalt heappositie `i` weg: de laatste komt op haar plaats en zakt of
/// stijgt naar waar hij hoort.
fn remove_at(heap: &mut Vec<usize>, places: &mut [Place], i: usize) {
    let Some(last) = heap.len().checked_sub(1) else {
        return;
    };
    if i > last {
        return;
    }
    swap(heap, places, i, last);
    heap.pop();
    if i < heap.len() {
        sift_down(heap, places, i);
        sift_up(heap, places, i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Een eenvoudige LCG: herhaalbaar, zonder crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    /// De heap-invariant en de posities, voor beide heaps.
    fn check(t: &Timers) {
        for (heap, deferrable) in [(&t.wake, false), (&t.lazy, true)] {
            for (i, &p) in heap.iter().enumerate() {
                let Some(Place::Busy(e)) = t.places.get(p) else {
                    panic!("a free place in a heap");
                };
                assert_eq!(e.pos, i);
                assert_eq!(e.deferrable, deferrable);
                if i > 0 {
                    assert!(at(&t.places, heap[(i - 1) / 2]) <= e.at, "heap order");
                }
            }
        }
        let busy = t
            .places
            .iter()
            .filter(|p| matches!(p, Place::Busy(_)))
            .count();
        assert_eq!(busy, t.live());
    }

    #[test]
    fn grows_from_first_doubles_and_stops_at_the_ceiling() {
        let mut t = Timers::new();
        assert_eq!(t.capacity(), 0, "new allocates nothing");
        let w = Waker::noop();
        let (_, grew) = t.add(10, false, w, 4, 16).unwrap();
        assert!(grew);
        assert!(t.capacity() >= 4);
        let mut grows = 1;
        for i in 1..16 {
            let (_, grew) = t.add(10 + i, i % 3 == 0, w, 4, 16).unwrap();
            grows += usize::from(grew);
        }
        assert_eq!(grows, 3, "4, 8, 16");
        assert_eq!(t.add(99, false, w, 4, 16), Err(NoRoom));
        check(&t);
    }

    #[test]
    fn a_model_of_add_remove_and_expire_agrees() {
        // Willekeurige registraties, annuleringen en klokstappen naast een
        // naïeve lijst: dezelfde timers vuren, op dezelfde ronde.
        let mut rng = Lcg(7);
        let mut t = Timers::new();
        let mut model: Vec<(Handle, u64, bool)> = Vec::new();
        let mut now = 0u64;
        for _ in 0..20_000 {
            match rng.next() % 10 {
                0..=4 => {
                    let at = now + 1 + rng.next() % 500;
                    let deferrable = rng.next().is_multiple_of(4);
                    let (h, _) = t.add(at, deferrable, Waker::noop(), 8, 1 << 16).unwrap();
                    model.push((h, at, deferrable));
                }
                5 | 6 if !model.is_empty() => {
                    let i = usize::try_from(rng.next()).unwrap() % model.len();
                    let (h, _, _) = model.swap_remove(i);
                    assert!(t.remove(h).is_some());
                    assert!(t.remove(h).is_none(), "a second remove is a no-op");
                }
                _ => {
                    now += rng.next() % 40;
                    let mut fired = 0;
                    while t.pop_due(now).is_some() {
                        fired += 1;
                    }
                    let due = model.iter().filter(|(_, at, _)| *at <= now).count();
                    model.retain(|(_, at, _)| *at > now);
                    assert_eq!(fired, due);
                }
            }
            assert_eq!(t.live(), model.len());
            let next = model.iter().filter(|m| !m.2).map(|m| m.1).min();
            assert_eq!(t.next_wake(), next);
        }
        check(&t);
    }

    /// Een waker die zijn nummer opschrijft.
    struct Mark(usize, std::sync::Arc<std::sync::Mutex<Vec<usize>>>);
    impl std::task::Wake for Mark {
        fn wake(self: std::sync::Arc<Self>) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    #[test]
    fn pop_due_goes_earliest_first() {
        // Duizend timers, één ronde ver voorbij alles: de wekken komen in
        // volgorde van deadline.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut t = Timers::new();
        let mut rng = Lcg(3);
        let mut ats = Vec::new();
        for i in 0..1_000 {
            let at = rng.next() % 10_000;
            ats.push(at);
            let w = Waker::from(std::sync::Arc::new(Mark(i, log.clone())));
            t.add(at, i % 5 == 0, &w, 32, 1 << 16).unwrap();
        }
        check(&t);
        // Per heap in volgorde: eerst alle gewone, dan alle uitstelbare.
        while let Some(w) = t.pop_due(10_000) {
            w.wake();
        }
        let order = log.lock().unwrap().clone();
        assert_eq!(order.len(), 1_000);
        let (plain, lazy): (Vec<usize>, Vec<usize>) = order.iter().partition(|&&i| i % 5 != 0);
        for part in [plain, lazy] {
            assert!(
                part.windows(2).all(|w| ats[w[0]] <= ats[w[1]]),
                "out of order"
            );
        }
        assert_eq!(t.live(), 0);
    }

    #[test]
    fn a_stale_handle_touches_nothing() {
        let mut t = Timers::new();
        let (a, _) = t.add(5, false, Waker::noop(), 4, 64).unwrap();
        assert!(t.pop_due(5).is_some());
        let (b, _) = t.add(9, false, Waker::noop(), 4, 64).unwrap();
        assert_eq!(a.place, b.place, "the place is reused");
        assert!(!t.refresh(a, Waker::noop()));
        assert!(t.remove(a).is_none());
        assert_eq!(t.next_wake(), Some(9));
        assert!(t.refresh(b, Waker::noop()));
        assert!(t.remove(b).is_some());
        assert_eq!(t.next_wake(), None);
        check(&t);
    }
}
