//! De bewonerslijst van een core: één implementatie voor elke schrijver
//! (de kooi op arm64 en riscv64, de OS-core en de app-cores) en elke lezer
//! in Rust ([`super::next`]). De switchers lezen hem in hun assembly met
//! dezelfde regels.
//!
//! De lijst is een array in het kooi-geheugen (`abi::layout::ROSTER_CAP`,
//! `ROSTER_ENTRIES`), het sched-blok van de core wijst ernaar
//! (`SCHED_LIST_PA`) en draagt de lengte (`SCHED_COUNT`). Eén ingang is de
//! PA van een ctx-blok, 0 is een gat. Een gat houdt zijn plek en de lengte
//! groeit alleen; een nieuwe bewoner neemt het eerste gat (share.go
//! `residentAdd`). Er is geen vast aantal bewoners per core (er is geen
//! maximum aantal slots): een volle lijst groeit door een grotere
//! ([`grow`]), met de ingangen erin, en de oude blijft bestaan, zodat een
//! switcher die hem nog vasthoudt nooit geheugen van een ander leest. De
//! lezer klemt de lengte op de capaciteit van de lijst die hij las.
//!
//! De kern is de enige schrijver (regel 1..3 van het sched-blok, de regel
//! van de cachelines van de C906: `abi::layout::SchedBlock`), dus hij leest
//! zijn eigen schrijfwerk zonder veeg. Eén schrijfvolgorde, die van een
//! ring: groeien is eerst de ingang, een barrière, dan de lengte; krimpen
//! eerst de lengte, dan de ingangen. Een lezer die de nieuwe lengte ziet,
//! ziet de ingang die erbij hoort. Na elke wijziging gaan de woorden naar
//! DRAM (`dev::push`), voor een switcher op een hart dat niet coherent is.
//!
//! Gekozen uit de drie die er tot 03-10 waren (`el2::dispatch` voor de
//! app-cores op arm64, `el2::oscore` voor de OS-core, `cage_riscv.rs` voor
//! riscv64): de vorm van `dispatch` (eerst kijken of hij er al staat, dan
//! pas vol; de ingang vóór de lengte met een barrière; de push).

use super::Error;
use super::layout::{
    CAGE_STRIDE, Core, CtxState, Plan, ROSTER_CAP, ROSTER_ENTRIES, SCHED_COUNT, SCHED_LIST_PA,
};
use dev::Pa;

/// Het sched-blok (en dus de bewonerslijst) van logische core `core`; 0 is
/// de OS-core.
pub fn sched(plan: &Plan, core: Core) -> Result<Pa, Error> {
    plan.park_mbox_pa(core).map_err(Error::Plan)
}

/// De lijst van `sched`, als hij er een heeft.
fn list(sched: Pa) -> Option<Pa> {
    match dev::read64(sched.add(SCHED_LIST_PA)) {
        0 => None,
        pa => Some(Pa(pa)),
    }
}

/// De capaciteit van lijst `l` in ingangen.
fn cap(l: Pa) -> usize {
    usize::try_from(dev::read64(l.add(ROSTER_CAP))).unwrap_or(0)
}

/// Hoeveel ingangen een lijst van `blocks` kooi-blokken draagt.
#[must_use]
pub const fn cap_of(blocks: usize) -> usize {
    ((blocks as u64 * CAGE_STRIDE - ROSTER_ENTRIES) / 8) as usize
}

/// De lengte van de lijst van `sched`, geklemd op de capaciteit van de lijst
/// die er staat. Die klem is een isolatiegrens: voorbij de lijst ligt het
/// geheugen van een ander.
#[must_use]
pub fn len(sched: Pa) -> usize {
    let Some(l) = list(sched) else { return 0 };
    usize::try_from(dev::read64(sched.add(SCHED_COUNT)))
        .unwrap_or(usize::MAX)
        .min(cap(l))
}

/// Ingang `i` van de lijst van `sched` (0 = een gat).
#[must_use]
pub fn get(sched: Pa, i: usize) -> u64 {
    match list(sched) {
        Some(l) if i < cap(l) => dev::read64(l.add(ROSTER_ENTRIES + 8 * i as u64)),
        _ => 0,
    }
}

/// Zet ingang `i` op `ctx` en duwt hem naar DRAM.
fn set(l: Pa, i: usize, ctx: u64) {
    let at = l.add(ROSTER_ENTRIES + 8 * i as u64);
    dev::write64(at, ctx);
    dev::push(at, 8);
}

/// De lengte naar `n` en naar DRAM, na de schrijfwerken ervoor.
fn publish_len(sched: Pa, n: usize) {
    dev::mb();
    dev::write64(sched.add(SCHED_COUNT), n as u64);
    dev::push(sched.add(SCHED_COUNT), 8);
}

/// De plek van `ctx` in de lijst van `sched`.
#[must_use]
pub fn find(sched: Pa, ctx: Pa) -> Option<usize> {
    (0..len(sched)).find(|&i| get(sched, i) == ctx.0)
}

/// Kan [`add`] een bewoner kwijt zonder te groeien (een gat, of plaats
/// achteraan)? Zo niet, dan eerst [`grow`].
#[must_use]
pub fn room(sched: Pa) -> bool {
    let Some(l) = list(sched) else { return false };
    let n = len(sched);
    n < cap(l) || (0..n).any(|i| get(sched, i) == 0)
}

/// Geeft `sched` een nieuwe, grotere lijst op `new` (`blocks` kooi-blokken,
/// Device in de map van de kern): eerst de ingangen erin, dan de
/// capaciteit, dan de wijzer. De oude lijst blijft bestaan (hij is niet van
/// deze functie om te geven): een switcher die hem nog vasthoudt, leest
/// geldige ingangen.
pub fn grow(sched: Pa, new: Pa, blocks: usize) {
    let n = len(sched);
    let cap_new = cap_of(blocks);
    dev::clear(new, blocks * CAGE_STRIDE as usize);
    for i in 0..n.min(cap_new) {
        dev::write64(new.add(ROSTER_ENTRIES + 8 * i as u64), get(sched, i));
    }
    dev::write64(new.add(ROSTER_CAP), cap_new as u64);
    dev::push(new, blocks * CAGE_STRIDE as usize);
    dev::mb();
    dev::write64(sched.add(SCHED_LIST_PA), new.0);
    dev::push(sched.add(SCHED_LIST_PA), 8);
}

/// De lijst die `sched` nu draagt, als (PA, kooi-blokken): wat de kooi aan
/// kooi-geheugen voor deze core houdt.
#[must_use]
pub fn held(sched: Pa) -> Option<(Pa, usize)> {
    let l = list(sched)?;
    let bytes = cap(l) as u64 * 8 + ROSTER_ENTRIES;
    Some((l, bytes.div_ceil(CAGE_STRIDE) as usize))
}

/// Zet `ctx` in de lijst van `sched`: staat hij er al, dan niets
/// (`Ok(false)`); anders het eerste gat, anders achteraan. Eerst kijken of
/// hij er al staat, dán pas "vol": andersom weigerde Go een geldige
/// herstart zodra de lijst ooit vol geweest was. Vol is [`Error::RosterFull`]:
/// de kooi groeit de lijst bij de bouw ([`room`], [`grow`]).
pub fn add(sched: Pa, ctx: Pa) -> Result<bool, Error> {
    let n = len(sched);
    let mut gap = None;
    for i in 0..n {
        match get(sched, i) {
            x if x == ctx.0 => return Ok(false),
            0 if gap.is_none() => gap = Some(i),
            _ => {}
        }
    }
    let l = list(sched).ok_or(Error::RosterFull { count: n })?;
    match gap {
        Some(i) => set(l, i, ctx.0),
        None if n >= cap(l) => return Err(Error::RosterFull { count: n }),
        None => {
            set(l, n, ctx.0);
            publish_len(sched, n + 1);
        }
    }
    dev::mb();
    Ok(true)
}

/// Haalt `ctx` uit de lijst van `sched` (een gat, dat [`add`] hergebruikt).
/// Geeft of hij erin stond.
pub fn remove(sched: Pa, ctx: Pa) -> bool {
    let Some(l) = list(sched) else { return false };
    let mut was = false;
    for i in 0..len(sched) {
        if get(sched, i) == ctx.0 {
            set(l, i, 0);
            was = true;
        }
    }
    if was {
        dev::mb();
    }
    was
}

/// Maakt de lijst van `sched` leeg, of `[ctx]` met `Some(ctx)`: alleen op
/// een core waar geen rotatie meeleest (de mailbox-start, een hart dat uit
/// reset komt). Met `Some` moet er een lijst zijn.
pub fn reset(sched: Pa, ctx: Option<Pa>) -> Result<(), Error> {
    publish_len(sched, 0);
    let Some(l) = list(sched) else {
        return match ctx {
            None => Ok(()),
            Some(_) => Err(Error::RosterFull { count: 0 }),
        };
    };
    dev::clear(l.add(ROSTER_ENTRIES), cap(l) * 8);
    dev::push(l.add(ROSTER_ENTRIES), cap(l) * 8);
    if let Some(ctx) = ctx {
        set(l, 0, ctx.0);
        publish_len(sched, 1);
    }
    Ok(())
}

/// De ctx-blokken in de lijst van `sched`, gaten overgeslagen, in
/// lijstvolgorde. Geeft het aantal.
pub fn each(sched: Pa, mut f: impl FnMut(Pa)) -> usize {
    let mut count = 0;
    for i in 0..len(sched) {
        let ctx = get(sched, i);
        if ctx != 0 {
            f(Pa(ctx));
            count += 1;
        }
    }
    count
}

/// Een bewoner erbij op de rotatie van `sched`: eerst de lijst, dan de
/// staat boot-pending. De staat is de publicatie: wie hem ziet, ziet de
/// lijst die erbij hoort, en een volle lijst laat niets half achter. Het
/// ctx-blok staat dan al klaar.
pub fn enlist(sched: Pa, ctx: Pa) -> Result<(), Error> {
    add(sched, ctx)?;
    super::ctx_write(ctx, super::layout::CTX_STATE, CtxState::BootPending.raw());
    Ok(())
}

/// Haalt `ctx` uit de lijst van ELKE core, de OS-core inbegrepen, vóór zijn
/// nieuwe levensduur. Een gestopt lid blijft als dode ingang in de lijst van
/// zijn oude core staan; komt het blok daarna op een andere core terug (het
/// kooi-geheugen geeft blokken opnieuw uit), dan zou die oude rotatie zijn
/// verse boot-pending staat zien en hem daar óók starten. Vóór elke
/// staatswissel van het blok, zodat de hercontrole van de rotatie de
/// verwijdering ziet.
pub fn forget(plan: &Plan, ctx: Pa) -> Result<(), Error> {
    for c in 0..=plan.app_cores() {
        let core = Core::new(c).ok_or(Error::Plan(abi::Error::OutOfPlan {
            index: c,
            max: plan.app_cores(),
        }))?;
        remove(sched(plan, core)?, ctx);
    }
    Ok(())
}

/// De ctx-blokken in de lijst van `core` (zie [`each`]).
pub fn residents(plan: &Plan, core: Core, f: impl FnMut(Pa)) -> Result<usize, Error> {
    Ok(each(sched(plan, core)?, f))
}

const _: () = assert!(SCHED_LIST_PA.is_multiple_of(8) && SCHED_COUNT < SCHED_LIST_PA);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::el2::harness::{Buf, plan};

    fn ids(sched: Pa) -> Vec<u64> {
        (0..len(sched)).map(|i| get(sched, i)).collect()
    }

    /// Een sched-blok met een lijst van `blocks` kooi-blokken uit `buf`.
    fn with_list(p: &Plan, core: usize, buf: &Buf, blocks: usize) -> Pa {
        let s = sched(p, Core::new(core).unwrap()).unwrap();
        grow(s, buf.pa(), blocks);
        s
    }

    #[test]
    fn a_resident_takes_the_first_gap_and_never_twice() {
        let (_b, p) = plan(2);
        let lb = Buf::new(CAGE_STRIDE as usize, CAGE_STRIDE);
        let s = sched(&p, Core::new(1).unwrap()).unwrap();
        assert!(!room(s) && add(s, Pa(0x100)).is_err(), "no list, no room");
        grow(s, lb.pa(), 1);
        assert_eq!(add(s, Pa(0x100)), Ok(true));
        assert_eq!(add(s, Pa(0x200)), Ok(true));
        assert_eq!(add(s, Pa(0x200)), Ok(false));
        assert_eq!(ids(s), [0x100, 0x200]);
        assert!(remove(s, Pa(0x100)));
        assert!(!remove(s, Pa(0x100)));
        assert_eq!(find(s, Pa(0x100)), None);
        assert_eq!(ids(s), [0, 0x200]);
        assert_eq!(add(s, Pa(0x300)), Ok(true));
        assert_eq!(ids(s), [0x300, 0x200]);
        let mut seen = Vec::new();
        assert_eq!(each(s, |c| seen.push(c.0)), 2);
        assert_eq!(seen, [0x300, 0x200]);
        reset(s, Some(Pa(0x900))).unwrap();
        assert_eq!(ids(s), [0x900]);
        reset(s, None).unwrap();
        assert_eq!(ids(s), []);
    }

    // Geen vast aantal bewoners per core: een volle lijst groeit door een
    // grotere, met de ingangen erin, en een lezer klemt op de capaciteit
    // van de lijst die er staat.
    #[test]
    fn a_full_list_grows_and_keeps_its_residents() {
        let (_b, p) = plan(2);
        let (small, big) = (
            Buf::new(CAGE_STRIDE as usize, CAGE_STRIDE),
            Buf::new(2 * CAGE_STRIDE as usize, CAGE_STRIDE),
        );
        let s = with_list(&p, 0, &small, 1);
        let n = cap_of(1);
        for i in 1..=n {
            add(s, Pa(i as u64 * 0x1000)).unwrap();
        }
        assert!(!room(s));
        assert_eq!(add(s, Pa(0xDEAD_0000)), Err(Error::RosterFull { count: n }));
        assert_eq!(add(s, Pa(0x1000)), Ok(false), "who is there, stays");
        grow(s, big.pa(), 2);
        assert!(room(s));
        assert_eq!(add(s, Pa(0xDEAD_0000)), Ok(true));
        assert_eq!(len(s), n + 1);
        assert_eq!((get(s, 0), get(s, n)), (0x1000, 0xDEAD_0000));
        assert_eq!(held(s), Some((big.pa(), 2)));
        // Een lengte voorbij de lijst wordt geklemd.
        dev::write64(s.add(SCHED_COUNT), u64::MAX);
        assert_eq!(len(s), cap_of(2));
    }

    #[test]
    fn forget_clears_every_core_the_os_core_too() {
        let (_b, p) = plan(2);
        let bufs: Vec<Buf> = (0..=2)
            .map(|_| Buf::new(CAGE_STRIDE as usize, CAGE_STRIDE))
            .collect();
        for (c, b) in bufs.iter().enumerate() {
            let s = with_list(&p, c, b, 1);
            add(s, Pa(0x2000)).unwrap();
        }
        forget(&p, Pa(0x2000)).unwrap();
        for c in 0..=2 {
            assert_eq!(
                residents(&p, Core::new(c).unwrap(), |_| {}),
                Ok(0),
                "core {c}"
            );
        }
    }

    #[test]
    fn enlist_publishes_the_state_after_the_list() {
        let (_b, p) = plan(2);
        let lb = Buf::new(CAGE_STRIDE as usize, CAGE_STRIDE);
        let ctxs = Buf::new(4096, 4096);
        let s = sched(&p, Core::new(1).unwrap()).unwrap();
        let ctx = ctxs.pa();
        // Zonder lijst: geen staat.
        assert!(enlist(s, ctx).is_err());
        assert_eq!(super::super::ctx_state(ctx), Some(CtxState::Empty));
        grow(s, lb.pa(), 1);
        enlist(s, ctx).unwrap();
        assert_eq!(super::super::ctx_state(ctx), Some(CtxState::BootPending));
        assert_eq!(find(s, ctx), Some(0));
    }
}
