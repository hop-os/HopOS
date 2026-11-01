//! De gedeelde heap van kern en apps: een allocator met vrije lijsten tussen het einde van
//! het image en de stack, met een plafond en tellers.
//!
//! Tot 29-09 was dit een bump-allocator die alleen het laatst uitgegeven
//! blok terugnam. Dat hield zolang alleen de executor alloceerde; `leannet`
//! laat zijn TCP-ringen groeien en geeft ze bij een gesloten verbinding
//! terug, en in een bump-heap lekte dat allemaal. Een app met veel
//! verbindingen (Hop als bewoner, een HTTP-server) liep zo vroeg of laat
//! tegen het plafond.
//!
//! De vorm is de klassieke met grenslabels, bewust klein gehouden zodat hij
//! in één keer te lezen en te toetsen is:
//!
//! - Elk blok begint met een kop van [`HDR`] bytes: de maat met de
//!   bezet-vlag in bit 0, en de maat van de fysieke voorganger. Met die twee
//!   vindt `free` beide buren in O(1) en voegt ze samen.
//! - Een vrij blok draagt in zijn lijf twee schakels (volgende, vorige) van
//!   een dubbel gelinkte lijst. Er is één lijst per klasse: tot en met
//!   [`SMALL_LIMIT`] een klasse per maat (een veelvoud van de korrel),
//!   daarboven klasse `i` voor de maten in `[2^i, 2^(i+1))`. `alloc` zoekt
//!   first-fit in de klasse van de vraag en neemt daarboven, via een bitmap
//!   van niet-lege klassen, het eerste blok dat past.
//! - Uitlijning tot [`MAX_ALIGN`] (een pagina): past de gevraagde uitlijning
//!   niet op het begin van een vrij blok, dan wordt de voorkant als eigen
//!   vrij blok afgesplitst. Een rest achter het blok die groot genoeg is,
//!   gaat ook terug de lijst in.
//! - Het plafond geldt voor de bytes in gebruik (kop inbegrepen) en is de
//!   maat van het gebied: `init` zet het, en er is geen lagere grens. Tot
//!   01-10 kon `set_ceiling` het onder het gebied leggen (e97b4fc, met
//!   `cpu::memlimit` weg); de grens van een app is nu zijn partitie, die de
//!   kern bij de plaatsing kiest.
//!
//! Heap is voor koude paden (handboek §6); `alloc` en `free` zijn daarom
//! eenvoudig en niet snel, maar wel begrensd: een lijstwandeling stopt na
//! zoveel stappen als er blokken in de heap passen, ook als een kapotte
//! kop een kring maakt.
//!
//! De tellers zijn de meetlat van het handboek: `used` gaat als
//! geheugen-draw naar de control-page (`CtrlMemSys`), en `peak` en `failed`
//! zeggen of het plafond ooit in de buurt kwam.
//!
//! Deze module bezit de heap-staat en de koppen en schakels in het gebied;
//! de lijven van uitgegeven blokken zijn van wie ze kreeg.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{
    AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};

/// De kop van een blok: `size | USED` en de maat van de voorganger. Zestien
/// bytes, zodat elk lijf op de korrel uitgelijnd blijft.
pub const HDR: usize = 16;

/// De korrel: elke blokmaat en elk blokadres is hier een veelvoud van.
const GRAIN: usize = 16;

/// Het kleinste blok: een kop plus de twee schakels van een vrij blok.
const MIN_BLOCK: usize = HDR + 16;

/// De grootste uitlijning die de heap geeft; meer wordt geweigerd en
/// geteld. Een pagina is wat een DMA-buffer of een pagetable vraagt.
pub const MAX_ALIGN: usize = 4096;

/// De bezet-vlag in het maatwoord.
const USED: usize = 1;

/// Tot en met deze blokmaat heeft elke maat (een veelvoud van [`GRAIN`]) een
/// eigen klasse, zodat het eerste blok in een lijst altijd past. Daarboven
/// één klasse per bitpositie.
///
/// Waarom: met alleen klassen per macht van twee (32 tot 63 bytes in één
/// lijst) liep een vraag van 48 bytes langs elk vrij blok van 32 dat er lag.
/// JSON-werk laat duizenden van zulke blokken achter; Stulp mat op de host 505
/// ns per vrijgave plus allocatie met 4547 vrije blokken, en op de LicheeRV
/// werd dat een bootstrap van 250 ms en callbacks van een halve seconde
/// (03-10). Met exacte klassen is dat een lijstkop pakken.
const SMALL_LIMIT: usize = 1024;

/// Het aantal exacte klassen: maat / [`GRAIN`] voor maten tot [`SMALL_LIMIT`].
const SMALL_BINS: usize = SMALL_LIMIT / GRAIN + 1;

/// Het aantal klassen: de exacte, en één per bitpositie boven [`SMALL_LIMIT`].
const BINS: usize = SMALL_BINS + usize::BITS as usize;

/// De lege schakel. Adres 0 is nooit een blok: [`Heap::init`] weigert een
/// gebied dat daar begint.
const NIL: usize = 0;

/// De meetlat van de heap.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes in gebruik, koppen inbegrepen.
    pub used: u64,
    /// De hoogste `used` sinds `init`.
    pub peak: u64,
    /// De maat van het gebied.
    pub capacity: u64,
    /// Het plafond voor `used`.
    pub ceiling: u64,
    /// Geslaagde allocaties.
    pub allocs: u64,
    /// Vrijgaven.
    pub frees: u64,
    /// Geweigerde allocaties: plafond, geen passend blok, of een
    /// uitlijning boven [`MAX_ALIGN`].
    pub failed: u64,
    /// Vrijgaven van iets dat geen bezet blok was (dubbel vrijgegeven, of
    /// een vreemde wijzer); genegeerd en geteld.
    pub bad_frees: u64,
}

/// Wat een wandeling door de heap vond ([`Heap::check`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// Blokken in het gebied.
    pub blocks: usize,
    /// Daarvan vrij.
    pub free_blocks: usize,
    /// Bytes in vrije blokken, koppen inbegrepen.
    pub free_bytes: usize,
    /// Het grootste vrije blok.
    pub largest_free: usize,
}

/// Een gebroken invariant, met het adres waar de wandeling hem vond.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Corrupt {
    /// Het blok (of de klasse) waar het misging.
    pub at: usize,
    /// Welke invariant.
    pub why: &'static str,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "heap corrupt at {:#x}: {}", self.at, self.why)
    }
}

/// De staat van de heap.
///
/// # Invariants
///
/// Na [`Heap::init`] met een niet-leeg gebied:
///
/// - De blokken betegelen `[start, end)` precies: vanaf `start` in stappen
///   van hun maat kom je exact op `end`. Elk blokadres en elke maat is een
///   veelvoud van [`GRAIN`], elke maat minstens [`MIN_BLOCK`].
/// - Het tweede woord van elk blok is de maat van zijn fysieke voorganger,
///   0 voor het eerste blok.
/// - Twee vrije blokken liggen nooit naast elkaar (samenvoegen is
///   volledig).
/// - Elk vrij blok staat in precies één lijst, die van zijn klasse, en de
///   lijsten zijn consistent dubbel gelinkt.
/// - `used` is de som van de maten van de bezette blokken en nooit meer
///   dan `ceiling`.
/// - De heap schrijft alleen in koppen en in lijven van vrije blokken; het
///   lijf van een bezet blok is van wie het kreeg.
///
/// Met een leeg gebied is `start == end == 0` en faalt elke allocatie.
struct State {
    start: usize,
    end: usize,
    ceiling: usize,
    bins: [usize; BINS],
    /// Bit `i` staat als klasse `i` een vrij blok heeft: het zoeken springt
    /// meteen naar de eerste niet-lege klasse die groot genoeg is.
    nonempty: [u64; BINS.div_ceil(64)],
    used: usize,
    peak: usize,
    allocs: u64,
    frees: u64,
    failed: u64,
    bad_frees: u64,
}

impl State {
    const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            ceiling: 0,
            bins: [NIL; BINS],
            nonempty: [0; BINS.div_ceil(64)],
            used: 0,
            peak: 0,
            allocs: 0,
            frees: 0,
            failed: 0,
            bad_frees: 0,
        }
    }

    // ---- Woorden in het gebied ----

    /// Ligt het woord op `a` in het gebied, uitgelijnd?
    fn word_ok(&self, a: usize) -> bool {
        a >= self.start && a.is_multiple_of(8) && a.checked_add(8).is_some_and(|e| e <= self.end)
    }

    /// Leest het woord op `a`; buiten het gebied 0. Dat laatste gebeurt
    /// alleen bij een kapotte kop, en dan faalt de op liever dan dat hij
    /// buiten de heap leest.
    fn rd(&self, a: usize) -> usize {
        if !self.word_ok(a) {
            return 0;
        }
        // SAFETY: `a` ligt uitgelijnd binnen `[start, end)`, en dat gebied
        // is volgens het contract van `Heap::init` geldig, blijvend en van
        // de heap alleen. Alle aanroepers lezen koppen of schakels van vrije
        // blokken, nooit het lijf van een bezet blok (de invariant van
        // `State`), dus er is geen lener buiten de heap die hier schrijft.
        unsafe { core::ptr::with_exposed_provenance::<usize>(a).read() }
    }

    /// Schrijft `v` op `a`; buiten het gebied gebeurt er niets.
    fn wr(&mut self, a: usize, v: usize) {
        if !self.word_ok(a) {
            return;
        }
        // SAFETY: als bij `rd`: `a` ligt uitgelijnd in het eigen gebied, en
        // het is een kop of een schakel van een vrij blok, geen lijf dat
        // iemand anders bezit.
        unsafe { core::ptr::with_exposed_provenance_mut::<usize>(a).write(v) }
    }

    // ---- Koppen en schakels ----

    fn size(&self, b: usize) -> usize {
        self.rd(b) & !(GRAIN - 1)
    }

    fn is_used(&self, b: usize) -> bool {
        self.rd(b) & USED != 0
    }

    fn set_head(&mut self, b: usize, size: usize, used: bool) {
        self.wr(b, size | usize::from(used));
    }

    fn prev_size(&self, b: usize) -> usize {
        self.rd(b.wrapping_add(8))
    }

    fn set_prev_size(&mut self, b: usize, v: usize) {
        self.wr(b.wrapping_add(8), v);
    }

    fn next_free(&self, b: usize) -> usize {
        self.rd(b.wrapping_add(HDR))
    }

    fn prev_free(&self, b: usize) -> usize {
        self.rd(b.wrapping_add(HDR + 8))
    }

    fn set_links(&mut self, b: usize, next: usize, prev: usize) {
        self.wr(b.wrapping_add(HDR), next);
        self.wr(b.wrapping_add(HDR + 8), prev);
    }

    /// Hoeveel stappen een wandeling hooguit zet: meer blokken passen er
    /// niet in het gebied, dus meer stappen is een kring.
    fn max_steps(&self) -> usize {
        (self.end - self.start) / MIN_BLOCK + 1
    }

    /// Zegt het blok na `b` dat zijn voorganger nu `size(b)` groot is.
    fn fix_follower(&mut self, b: usize) {
        let s = self.size(b);
        let n = b.saturating_add(s);
        if n < self.end {
            self.set_prev_size(n, s);
        }
    }

    // ---- De klassen ----

    /// De klasse van maat `size` (> 0): exact tot en met [`SMALL_LIMIT`],
    /// daarboven de positie van de hoogste bit.
    fn bin_of(size: usize) -> usize {
        if size <= SMALL_LIMIT {
            size / GRAIN
        } else {
            SMALL_BINS + (usize::BITS - 1 - size.leading_zeros()) as usize
        }
    }

    fn mark(&mut self, i: usize, nonempty: bool) {
        if let Some(word) = self.nonempty.get_mut(i / 64) {
            if nonempty {
                *word |= 1 << (i % 64);
            } else {
                *word &= !(1 << (i % 64));
            }
        }
    }

    /// De eerste niet-lege klasse vanaf `from`.
    fn next_bin(&self, from: usize) -> Option<usize> {
        let mut w = from / 64;
        let mut bits = self.nonempty.get(w)? & (!0u64 << (from % 64));
        loop {
            if bits != 0 {
                return Some(w * 64 + bits.trailing_zeros() as usize);
            }
            w += 1;
            bits = *self.nonempty.get(w)?;
        }
    }

    /// Zet vrij blok `b` vooraan in de lijst van zijn klasse.
    fn push(&mut self, b: usize) {
        let i = Self::bin_of(self.size(b));
        let Some(&head) = self.bins.get(i) else {
            return;
        };
        self.set_links(b, head, NIL);
        if head != NIL {
            let next = self.next_free(head);
            self.set_links(head, next, b);
        }
        if let Some(slot) = self.bins.get_mut(i) {
            *slot = b;
        }
        self.mark(i, true);
    }

    /// Haalt vrij blok `b` uit zijn lijst. Vóór een maatwijziging, want de
    /// maat wijst de lijst aan.
    fn unlink(&mut self, b: usize) {
        let (n, p) = (self.next_free(b), self.prev_free(b));
        if p == NIL {
            let i = Self::bin_of(self.size(b));
            if let Some(slot) = self.bins.get_mut(i) {
                *slot = n;
            }
            if n == NIL {
                self.mark(i, false);
            }
        } else {
            let pp = self.prev_free(p);
            self.set_links(p, n, pp);
        }
        if n != NIL {
            let nn = self.next_free(n);
            self.set_links(n, nn, p);
        }
    }

    // ---- Uitgeven ----

    /// Geeft `[start, end)` aan de heap als één vrij blok.
    fn init(&mut self, start: usize, end: usize) {
        *self = Self::empty();
        let s = start.checked_add(GRAIN - 1).map(|v| v & !(GRAIN - 1));
        let e = end & !(GRAIN - 1);
        let Some(s) = s.filter(|&s| s != 0 && e >= s && e - s >= MIN_BLOCK) else {
            return;
        };
        self.start = s;
        self.end = e;
        self.ceiling = e - s;
        self.set_head(s, e - s, false);
        self.set_prev_size(s, 0);
        self.push(s);
    }

    /// De blokmaat voor `size` bytes: kop plus lijf, op de korrel, minstens
    /// [`MIN_BLOCK`]. `None` bij overloop.
    fn need(size: usize) -> Option<usize> {
        let body = size.checked_add(GRAIN - 1)? & !(GRAIN - 1);
        Some(body.checked_add(HDR)?.max(MIN_BLOCK))
    }

    /// Past een blok van `need` bytes met een lijf op `align` in vrij blok
    /// `b`? Dan de voorkant die eraf moet: 0, of een eigen vrij blok van
    /// minstens [`MIN_BLOCK`].
    fn fit(&self, b: usize, need: usize, align: usize) -> Option<usize> {
        let body = b.checked_add(HDR)?;
        let mut at = body.checked_add(align - 1)? & !(align - 1);
        if at != body && at - body < MIN_BLOCK {
            at = body.checked_add(MIN_BLOCK + align - 1)? & !(align - 1);
        }
        let gap = at - body;
        (gap.checked_add(need)? <= self.size(b)).then_some(gap)
    }

    /// Zoekt first-fit, eerst in de klasse van `need`, dan in de eerstvolgende
    /// niet-lege klassen daarboven. In een exacte klasse past het eerste blok
    /// (behalve bij een uitlijning boven de korrel).
    fn find(&self, need: usize, align: usize) -> Option<(usize, usize)> {
        let mut steps = self.max_steps();
        let mut from = Self::bin_of(need);
        while let Some(i) = self.next_bin(from) {
            let mut b = *self.bins.get(i)?;
            while b != NIL {
                steps = steps.checked_sub(1)?;
                if let Some(gap) = self.fit(b, need, align) {
                    return Some((b, gap));
                }
                b = self.next_free(b);
            }
            from = i + 1;
        }
        None
    }

    /// Reserveert `size` bytes op `align`; het adres van het lijf.
    fn alloc(&mut self, size: usize, align: usize) -> Option<usize> {
        let r = self.try_alloc(size, align);
        if r.is_none() {
            self.failed = self.failed.wrapping_add(1);
        }
        r
    }

    fn try_alloc(&mut self, size: usize, align: usize) -> Option<usize> {
        if self.start == 0 || !align.is_power_of_two() || align > MAX_ALIGN {
            return None;
        }
        let need = Self::need(size)?;
        if self.used.checked_add(need)? > self.ceiling {
            return None;
        }
        let (b, gap) = self.find(need, align.max(GRAIN))?;
        self.take(b, gap, need)
    }

    /// Neemt vrij blok `b`: de voorkant `gap` blijft vrij, dan het bezette
    /// blok van `need` bytes, en een rest van minstens [`MIN_BLOCK`] gaat
    /// terug de lijst in (een kleinere rest gaat mee als speling).
    fn take(&mut self, b: usize, gap: usize, need: usize) -> Option<usize> {
        let s = self.size(b);
        let rest = s.checked_sub(gap)?.checked_sub(need)?;
        let taken = if rest >= MIN_BLOCK { need } else { need + rest };
        if self.used.checked_add(taken)? > self.ceiling {
            return None;
        }
        self.unlink(b);
        let mut blk = b;
        if gap > 0 {
            // De voorganger van `b` was bezet (geen twee vrije buren), dus
            // de voorkant hoeft niet samengevoegd.
            self.set_head(b, gap, false);
            self.push(b);
            blk = b + gap;
            self.set_prev_size(blk, gap);
        }
        self.set_head(blk, taken, true);
        if rest >= MIN_BLOCK {
            let t = blk + taken;
            self.set_head(t, rest, false);
            self.set_prev_size(t, taken);
            self.push(t);
            self.fix_follower(t);
        } else {
            self.fix_follower(blk);
        }
        self.used += taken;
        self.peak = self.peak.max(self.used);
        self.allocs = self.allocs.wrapping_add(1);
        Some(blk + HDR)
    }

    // ---- Terugnemen ----

    /// Is `b` een bezet blok binnen het gebied? Vangt een dubbele vrijgave
    /// en de meeste vreemde wijzers; een wijzer midden in een lijf die er
    /// als een kop uitziet, vangt hij niet (dat is het contract van
    /// `release`).
    fn plausible(&self, b: usize) -> bool {
        let s = self.size(b);
        b >= self.start
            && b < self.end
            && b.is_multiple_of(GRAIN)
            && self.is_used(b)
            && s >= MIN_BLOCK
            && b.checked_add(s).is_some_and(|e| e <= self.end)
    }

    /// Geeft het blok met lijf `p` terug en voegt het samen met vrije buren.
    fn free(&mut self, p: usize) {
        let b = p.wrapping_sub(HDR);
        if p < HDR || !self.plausible(b) {
            self.bad_frees = self.bad_frees.wrapping_add(1);
            return;
        }
        let mut s = self.size(b);
        self.used = self.used.saturating_sub(s);
        self.frees = self.frees.wrapping_add(1);
        let mut blk = b;
        let n = b + s;
        if n < self.end && !self.is_used(n) {
            self.unlink(n);
            s += self.size(n);
        }
        let ps = self.prev_size(b);
        if ps != 0 {
            let pb = b.wrapping_sub(ps);
            if pb >= self.start && !self.is_used(pb) {
                self.unlink(pb);
                s += self.size(pb);
                blk = pb;
            }
        }
        self.set_head(blk, s, false);
        self.push(blk);
        self.fix_follower(blk);
    }

    // ---- De toets ----

    /// Loopt het gebied en de lijsten af en toetst elke invariant.
    fn check(&self) -> Result<Walk, Corrupt> {
        let bad = |at, why| Err(Corrupt { at, why });
        let mut w = Walk::default();
        if self.start == 0 {
            return Ok(w);
        }
        let (mut b, mut prev, mut prev_free, mut used) = (self.start, 0, false, 0);
        while b < self.end {
            let s = self.size(b);
            if s < MIN_BLOCK
                || !b.is_multiple_of(GRAIN)
                || b.checked_add(s).is_none_or(|e| e > self.end)
            {
                return bad(b, "block size out of range");
            }
            if self.prev_size(b) != prev {
                return bad(b, "prev_size does not match the neighbour");
            }
            let free = !self.is_used(b);
            if free && prev_free {
                return bad(b, "two free neighbours");
            }
            w.blocks += 1;
            if free {
                w.free_blocks += 1;
                w.free_bytes += s;
                w.largest_free = w.largest_free.max(s);
            } else {
                used += s;
            }
            (prev, prev_free) = (s, free);
            b += s;
        }
        if b != self.end {
            return bad(b, "blocks overrun the end");
        }
        if used != self.used {
            return bad(used, "used counter does not match the blocks");
        }
        if self.used > self.ceiling {
            return bad(self.used, "used above the ceiling");
        }
        self.check_bins(w.free_blocks).map(|()| w)
    }

    /// Elke lijst: dubbel gelinkt, alleen vrije blokken van de eigen
    /// klasse, en samen precies `free_blocks` lang.
    fn check_bins(&self, free_blocks: usize) -> Result<(), Corrupt> {
        let bad = |at, why| Err(Corrupt { at, why });
        let mut listed = 0;
        for (i, &head) in self.bins.iter().enumerate() {
            let (mut b, mut back) = (head, NIL);
            while b != NIL {
                listed += 1;
                if listed > free_blocks {
                    return bad(b, "more listed blocks than free blocks");
                }
                if !self.plausible_free(b) {
                    return bad(b, "listed block is not a free block");
                }
                if Self::bin_of(self.size(b)) != i {
                    return bad(b, "free block in the wrong class");
                }
                if self
                    .nonempty
                    .get(i / 64)
                    .is_none_or(|w| w & (1 << (i % 64)) == 0)
                {
                    return bad(b, "non-empty class not marked");
                }
                if self.prev_free(b) != back {
                    return bad(b, "broken back link");
                }
                (back, b) = (b, self.next_free(b));
            }
        }
        if listed != free_blocks {
            return bad(listed, "free block missing from the lists");
        }
        Ok(())
    }

    fn plausible_free(&self, b: usize) -> bool {
        b >= self.start && b < self.end && b.is_multiple_of(GRAIN) && !self.is_used(b)
    }

    fn stats(&self) -> HeapStats {
        HeapStats {
            used: self.used as u64,
            peak: self.peak as u64,
            capacity: (self.end - self.start) as u64,
            ceiling: self.ceiling as u64,
            allocs: self.allocs,
            frees: self.frees,
            failed: self.failed,
            bad_frees: self.bad_frees,
        }
    }
}

/// Het allocatorslot (handboek §1.3): de aanroeper levert de core-identiteit.
/// Alle allocaties en vrijgaven blijven synchroon, zonder ISR-gebruik of
/// callbacks behalve de zuivere core-identiteit. Er ontsnapt geen lening.
///
/// # Invariants
///
/// `st` wordt alleen aangeraakt binnen [`HeapLock::with`], door de core die
/// `owner` van [`FREE`] naar zijn eigen id wisselde, tot hij hem terugzet.
struct HeapLock<C: CoreId> {
    core: PhantomData<fn() -> C>,
    /// De MPIDR-affiniteit van de core die de staat heeft, of [`FREE`].
    owner: AtomicU64,
    st: UnsafeCell<State>,
}

/// Het slot is vrij. Een affiniteit is hooguit 24 bits.
const FREE: u64 = u64::MAX;

// SAFETY: door de invariant raakt precies één core tegelijk `st` aan; de
// Acquire van de wissel en de Release van de vrijgave ordenen de
// schrijvingen van de vorige houder vóór de lezingen van de volgende.
unsafe impl<C: CoreId> Sync for HeapLock<C> {}

/// De meetlat van het slot (§3.8): hoe vaak gepakt, hoe vaak er gewacht
/// werd, en de langste wacht in lees-rondes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LockStats {
    /// Keren gepakt.
    pub taken: u64,
    /// Keren dat een andere core hem had.
    pub contended: u64,
    /// De langste wacht, in rondes van de lees-lus.
    pub longest_spin: u64,
    /// Geweigerd omdat deze core hem al had.
    pub reentered: u64,
}

static LOCK_TAKEN: AtomicU64 = AtomicU64::new(0);
static LOCK_CONTENDED: AtomicU64 = AtomicU64::new(0);
static LOCK_LONGEST: AtomicU64 = AtomicU64::new(0);
static LOCK_REENTERED: AtomicU64 = AtomicU64::new(0);

impl<C: CoreId> HeapLock<C> {
    const fn new(st: State) -> Self {
        Self {
            core: PhantomData,
            owner: AtomicU64::new(FREE),
            st: UnsafeCell::new(st),
        }
    }

    /// Draait `f` met de staat; `None` als deze core hem al heeft.
    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> Option<R> {
        let me = C::id();
        if me == FREE {
            LOCK_REENTERED.fetch_add(1, Relaxed);
            return None;
        }
        let mut spins: u64 = 0;
        while let Err(cur) = self.owner.compare_exchange_weak(FREE, me, Acquire, Relaxed) {
            if cur == me {
                LOCK_REENTERED.fetch_add(1, Relaxed);
                return None;
            }
            while self.owner.load(Relaxed) != FREE {
                spins = spins.wrapping_add(1);
                core::hint::spin_loop();
            }
        }
        LOCK_TAKEN.fetch_add(1, Relaxed);
        if spins > 0 {
            LOCK_CONTENDED.fetch_add(1, Relaxed);
            LOCK_LONGEST.fetch_max(spins, Relaxed);
        }
        // SAFETY: de wissel hierboven maakte deze core de enige houder
        // (de invariant van `HeapLock`); de lening leeft tot de vrijgave
        // hieronder en ontsnapt niet uit `f`.
        let r = f(unsafe { &mut *self.st.get() });
        self.owner.store(FREE, Release);
        Some(r)
    }
}

/// Levert de identiteit van de uitvoerende core, zonder allocatie of yield.
/// Verschillende gelijktijdige cores moeten verschillende waarden teruggeven;
/// `u64::MAX` is gereserveerd voor een vrij slot. ISR's mogen niet alloceren.
pub trait CoreId {
    /// Identiteit die tijdens de allocatie gelijk blijft.
    fn id() -> u64;
}

/// Vrije lijsten per klasse over één gebied; kern en apps delen deze implementatie.
pub struct Heap<C: CoreId> {
    st: HeapLock<C>,
}

impl<C: CoreId> Heap<C> {
    /// Een lege heap: elke allocatie faalt tot [`Heap::init`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            st: HeapLock::new(State::empty()),
        }
    }

    /// Geeft de heap het gebied `[start, end)`, op de korrel bijgesneden.
    /// Een gebied dat op adres 0 begint of kleiner is dan één blok, laat
    /// de heap leeg. Wat de heap eerder uitgaf, is daarna ongeldig.
    ///
    /// # Safety
    ///
    /// `[start, end)` is geldig, beschrijfbaar geheugen dat de rest van het
    /// programma leeft en dat niemand anders dan deze heap aanraakt, behalve
    /// via wat de heap uitgeeft. Blokken uit een vorige `init` worden niet
    /// meer gebruikt.
    pub unsafe fn init(&self, start: usize, end: usize) {
        self.st.with(|st| st.init(start, end));
    }

    /// Reserveert `size` bytes met uitlijning `align` (een macht van twee,
    /// hooguit [`MAX_ALIGN`]); het adres, of `None` als het niet past.
    pub fn reserve(&self, size: usize, align: usize) -> Option<usize> {
        self.st.with(|st| st.alloc(size, align)).flatten()
    }

    /// Geeft het blok op `p` terug. Iets dat geen bezet blok is, wordt
    /// genegeerd en geteld (`bad_frees`).
    ///
    /// # Safety
    ///
    /// `p` kwam uit [`Heap::reserve`] van deze heap en is sindsdien niet
    /// teruggegeven; niemand gebruikt het blok hierna nog. (Een tweede
    /// vrijgave vangt de heap meestal, maar niet als het blok intussen
    /// opnieuw uitgegeven is.)
    pub unsafe fn release(&self, p: usize) {
        self.st.with(|st| st.free(p));
    }

    /// Bytes in gebruik, koppen inbegrepen: de geheugen-draw voor de
    /// control-page.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.stats().used
    }

    /// De maat van het gebied.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.stats().capacity
    }

    /// De meetlat.
    #[must_use]
    pub fn stats(&self) -> HeapStats {
        self.st.with(|st| st.stats()).unwrap_or_default()
    }

    /// De meetlat van het slot, over alle heaps van dit image.
    #[must_use]
    pub fn lock_stats() -> LockStats {
        LockStats {
            taken: LOCK_TAKEN.load(Relaxed),
            contended: LOCK_CONTENDED.load(Relaxed),
            longest_spin: LOCK_LONGEST.load(Relaxed),
            reentered: LOCK_REENTERED.load(Relaxed),
        }
    }

    /// Loopt de hele heap af en toetst de invarianten (zie [`State`]). Voor
    /// tests en diagnose; lineair in het aantal blokken.
    pub fn check(&self) -> Result<Walk, Corrupt> {
        self.st.with(|st| st.check()).unwrap_or(Err(Corrupt {
            at: 0,
            why: "heap busy",
        }))
    }
}

impl<C: CoreId> Default for Heap<C> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `alloc` geeft een lijf van minstens `size` bytes op de gevraagde
// uitlijning binnen het gebied van `init`, of null; het overlapt geen ander
// levend blok, want het komt uit een vrij blok dat uit zijn lijst gaat en
// als bezet gemarkeerd wordt (de invarianten van `State`). `dealloc` geeft
// alleen terug wat `alloc` gaf; dat is het contract van `GlobalAlloc`.
unsafe impl<C: CoreId> GlobalAlloc for Heap<C> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.reserve(layout.size(), layout.align()) {
            Some(p) => core::ptr::with_exposed_provenance_mut(p),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: het contract van `GlobalAlloc::dealloc`: `ptr` kwam uit
        // `alloc` van deze allocator en wordt hierna niet meer gebruikt.
        unsafe { self.release(ptr.expose_provenance()) }
    }
}

#[cfg(test)]
mod tests;
