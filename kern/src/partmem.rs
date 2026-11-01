//! De partitie-pool: elk slot krijgt precies de maat die HOP voor die job
//! vroeg, uit één pool, nooit meer dan de pool heeft.
//!
//! De pool is het vrije DRAM van het board, fysiek RAM dat de kooi per slot
//! op het canonieke IPA-adres van de app legt (de map ontkoppelt IPA van PA,
//! dus variabele fysieke partities passen er zo in). Best-fit met coalescing
//! bij vrijgave houdt fragmentatie klein; 2 MiB-uitlijning omdat de
//! stage-2-partitieblokken 2 MiB zijn.
//!
//! # De levensloop is een type (handboek §1.2)
//!
//! ```text
//!   alloc ──> Partition<Free> ──dispatched──> Partition<Owned> ──release(Stopped)──> pool
//!                 │
//!              abandon
//!                 v
//!                pool
//! ```
//!
//! - [`Partition<Free>`]: geclaimd en VRIJ VAN UITVOERING. Er heeft nooit een
//!   core in gedraaid, dus teruggeven ([`Partition::abandon`]) is de gewone
//!   rollback (lifecycle.md: "only confirmed absence permits ordinary
//!   rollback").
//! - [`Partition<Owned>`]: er is gedispatcht, ook als de uitkomst onzeker
//!   was. [`Partition::release`] bestaat alleen hier en eist een
//!   [`Stopped`], dat alleen de kern maakt nadat alle contexten van de
//!   eigenaar bevestigd stil staan (E2, E9).
//!
//! Een beëindiging die de kern niet kan bevestigen, heeft geen toestand: dat
//! is `fatal` in de lifecycle, en de koude boot geeft alles terug
//! (docs/sequence-hopos.md §13.5; tot 01-11 was dat een quarantaine).
//!
//! Een token vergeten (droppen) laat de claim in het grootboek staan: dat is
//! de veilige kant, fail-closed. Het grootboek van de pool is de waarheid, de
//! tokens zijn het bewijs dat een overgang mag.
//!
//! De pool zelf is eigendom van de lifecycle-actor ([`crate::slots`]); er is
//! geen `partMu` meer.

use crate::{Error, GRAIN, Region, Result, SLOT_CAP, Slot, align_grain};
use bounded::BoundedVec;
use core::marker::PhantomData;

/// Het maximale aantal vrije stukken. Elke levende partitie splitst hooguit
/// één stuk in twee, plus de regio's van het board.
pub const MAX_FREE_REGIONS: usize = 2 * SLOT_CAP + 64;

/// De vertaalregels van de architectuur die de pool nodig heeft.
///
/// Eén fysieke claim bezit de zichtbare partitie én eventuele
/// vertaalopslag; de architectuur meldt hoeveel. Allocatie, vrijgave en
/// adoptie blijven zo generiek.
#[derive(Copy, Clone)]
pub struct Geometry {
    /// De grootste zichtbare partitie die het app-venster beschrijft.
    pub link_window: fn(u64) -> u64,
    /// De extra vertaalopslag achter een partitie van deze maat.
    pub reserve: fn(u64) -> u64,
}

fn flat_window(size: u64) -> u64 {
    size
}

fn no_reserve(_: u64) -> u64 {
    0
}

impl Geometry {
    /// Een venster zonder grens en zonder extra tabelopslag: de host-tests.
    pub const FLAT: Geometry = Geometry {
        link_window: flat_window,
        reserve: no_reserve,
    };
}

/// Typestate: geclaimd, nog nooit uitgevoerd.
#[derive(Debug)]
pub struct Free;
/// Typestate: er is (mogelijk) uitgevoerd.
#[derive(Debug)]
pub struct Owned;

/// Het bewijs dat alle contexten van `slot` bevestigd stil staan.
///
/// Alleen de kern maakt er een ([`crate::slots`], na de toets bij de kooi);
/// een time-out of een ontbrekende heartbeat levert er nooit een op (E9).
#[derive(Debug)]
pub struct Stopped {
    slot: Slot,
}

impl Stopped {
    /// Alleen voor de kern: de kooi bevestigde de beëindiging van `slot`.
    pub(crate) fn confirmed(slot: Slot) -> Stopped {
        Stopped { slot }
    }
}

/// Een partitie van één slot, in toestand `S`.
///
/// Geen `Clone`: er is per slot precies één token, en wie hem heeft mag de
/// volgende overgang doen.
///
/// ```compile_fail,E0599
/// use kern::partmem::{Free, Partition, PartitionPool, Stopped};
/// fn early(f: Partition<Free>, pool: &mut PartitionPool, s: Stopped) {
///     f.release(pool, s); // Nooit gedispatcht: dat heet `abandon`.
/// }
/// ```
#[must_use = "een gedropte partitie blijft geclaimd; geef hem door of terug"]
#[derive(Debug)]
pub struct Partition<S> {
    slot: Slot,
    region: Region,
    _state: PhantomData<S>,
}

impl<S> Partition<S> {
    fn to<T>(self) -> Partition<T> {
        Partition {
            slot: self.slot,
            region: self.region,
            _state: PhantomData,
        }
    }

    /// Het slot dat de partitie bezit.
    #[must_use]
    pub fn slot(&self) -> Slot {
        self.slot
    }

    /// Het zichtbare bereik (inclusief de ABI-staart, exclusief de
    /// vertaalopslag).
    #[must_use]
    pub fn region(&self) -> Region {
        self.region
    }
}

impl Partition<Free> {
    /// Het startschot is gegeven (ook als de uitkomst onzeker was): vanaf nu
    /// kan er uitgevoerd zijn, en teruggeven vraagt een [`Stopped`].
    pub fn dispatched(self) -> Partition<Owned> {
        self.to()
    }

    /// Geef een partitie terug waarin nooit een core draaide.
    pub fn abandon(self, pool: &mut PartitionPool) {
        pool.release_ledger(self.slot);
    }
}

impl Partition<Owned> {
    /// Geef de partitie terug aan de pool: alleen met het bewijs dat alle
    /// contexten van dít slot stil staan.
    pub fn release(self, pool: &mut PartitionPool, proof: Stopped) -> Result {
        if proof.slot != self.slot {
            return Err(Error::NotOwned {
                slot: proof.slot.get(),
            });
        }
        pool.release_ledger(self.slot);
        Ok(())
    }
}

/// De pool: vrije stukken en het grootboek per slot.
///
/// # Invariants
///
/// De vrije stukken zijn op basis gesorteerd, overlappen niet, en overlappen
/// geen claim uit het grootboek. Elke claim loopt door
/// `carve`/`take_range`: dat is de dubbeluitgifte-invariant.
pub struct PartitionPool {
    free: BoundedVec<Region, MAX_FREE_REGIONS>,
    capacity: u64,
    owners: [Option<Region>; SLOT_CAP + 1],
    max_slots: usize,
    geo: Geometry,
}

impl PartitionPool {
    /// Bouwt de eigendomskaart zoals elke boot hem bouwt: al het herbruikbare
    /// geheugen (`pool`) minus de actieve kern `own`. De vorige kern heeft
    /// na een overdracht geen claim.
    pub fn new(
        pool: &[Region],
        own: Region,
        geo: Geometry,
        max_slots: usize,
    ) -> Result<PartitionPool> {
        let mut p = PartitionPool {
            free: BoundedVec::new(),
            capacity: 0,
            owners: [None; SLOT_CAP + 1],
            max_slots: max_slots.min(SLOT_CAP),
            geo,
        };
        let mut sources: BoundedVec<Region, MAX_FREE_REGIONS> = BoundedVec::new();
        for r in pool {
            if r.size == 0 {
                continue;
            }
            if r.end().is_none() {
                return Err(Error::Range {
                    base: r.base,
                    size: r.size,
                });
            }
            sources.push(*r).map_err(|_| Error::Full {
                cap: MAX_FREE_REGIONS,
            })?;
        }
        for src in sources.iter() {
            p.carve_source(*src, own)?;
        }
        p.free.as_mut_slice().sort_unstable_by_key(|r| r.base);
        Ok(p)
    }

    /// Eén bron: de eigen kern eruit knippen, naar binnen uitlijnen op de
    /// korrel, en wat overblijft als vrij stuk opnemen (`layout.CarvePool`).
    fn carve_source(&mut self, src: Region, own: Region) -> Result {
        let mut pieces = [Some(src), None];
        if own.size != 0 && src.overlaps(own) {
            let (s_end, o_end) = (src.end().unwrap_or(u64::MAX), own.end().unwrap_or(u64::MAX));
            pieces = [None, None];
            if own.base > src.base {
                pieces[0] = Some(Region::new(src.base, own.base - src.base));
            }
            if o_end < s_end {
                pieces[1] = Some(Region::new(o_end, s_end - o_end));
            }
        }
        for r in pieces.into_iter().flatten() {
            let pad = r.base.wrapping_neg() & (GRAIN - 1);
            if pad >= r.size {
                continue;
            }
            let base = r.base + pad;
            let end = r.end().unwrap_or(u64::MAX) & !(GRAIN - 1);
            if end > base {
                self.free
                    .push(Region::new(base, end - base))
                    .map_err(|_| Error::Full {
                        cap: MAX_FREE_REGIONS,
                    })?;
                self.capacity += end - base;
            }
        }
        Ok(())
    }

    fn check_slot(&self, slot: Slot) -> Result {
        if slot.get() > self.max_slots {
            return Err(Error::SlotRange {
                slot: slot.get(),
                max: self.max_slots,
            });
        }
        Ok(())
    }

    /// De volledige fysieke claim van een zichtbare partitie van `size`.
    fn claim_size(&self, size: u64) -> Result<u64> {
        if size == 0 || size > (self.geo.link_window)(size) {
            return Err(Error::PartitionSize { size });
        }
        let extra = (self.geo.reserve)(size);
        if !extra.is_multiple_of(GRAIN) {
            return Err(Error::PartitionSize { size });
        }
        size.checked_add(extra).ok_or(Error::PartitionSize { size })
    }

    /// Reserveert `size` bytes voor `slot`, opgerond naar de korrel.
    ///
    /// De maat in het token ÍS de partitie. Dat de aanroeper vroeger zijn
    /// eigen getal hield, was de bug van 31-07: met een `memory_limit` van
    /// 24 MB kreeg de kooi 0x1800000 te zien terwijl de partitie 32 MB was.
    /// Eén bron van waarheid.
    ///
    /// Een nieuwe eigenaar begint pas na expliciete vrijgave van de vorige;
    /// een mislukte allocatie raakt de bestaande reservering niet aan (Derek,
    /// 19-08: anders ligt het geheugen van een draaiende bewoner vrij).
    pub fn alloc(&mut self, slot: Slot, size: u64) -> Result<Partition<Free>> {
        self.check_slot(slot)?;
        let size = align_grain(size)
            .filter(|s| *s != 0)
            .ok_or(Error::PartitionSize { size })?;
        let claim = self.claim_size(size)?;
        if self.owners.get(slot.get()).copied().flatten().is_some() {
            return Err(Error::StillOwned { slot: slot.get() });
        }
        let base = self.carve(claim).ok_or(Error::NoPartition { size })?;
        let region = Region::new(base, size);
        if let Some(o) = self.owners.get_mut(slot.get()) {
            *o = Some(region);
        }
        Ok(Partition {
            slot,
            region,
            _state: PhantomData,
        })
    }

    /// Kiest de basis voor een claim en knipt hem uit de vrije lijst.
    ///
    /// Best-fit: de KLEINSTE regio die dit nog kan dragen. GEMETEN 31-07: een
    /// 64 MB-retry pakte 0x8be00000 uit de 127 MB-regio terwijl de losse
    /// 64 MB-regio vrij lag, en daarna paste 124 MB nergens meer. Binnen de
    /// gekozen regio de HOOGSTE bruikbare basis: op servers is het lage DRAM
    /// schaars (venster-kandidaten, het onder-4 GB-bereik voor DMA).
    fn carve(&mut self, claim: u64) -> Option<u64> {
        let best = self.best_fit(claim)?;
        let r = *self.free.get(best)?;
        let base = (r.base + r.size - claim) & !(GRAIN - 1);
        self.take_range(base, base + claim).ok()?;
        Some(base)
    }

    fn best_fit(&self, claim: u64) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (idx, r) in self.free.iter().enumerate().rev() {
            if r.size < claim || ((r.base + r.size - claim) & !(GRAIN - 1)) < r.base {
                continue;
            }
            match best {
                Some(b) if self.free.get(b).is_some_and(|x| x.size <= r.size) => {}
                _ => best = Some(idx),
            }
        }
        best
    }

    fn release_ledger(&mut self, slot: Slot) {
        let Some(l) = self.owners.get_mut(slot.get()).and_then(Option::take) else {
            return;
        };
        let extra = (self.geo.reserve)(l.size);
        // Een terugvoeg die niet past, lekt de claim: fail-closed.
        let _ = self.insert_free(Region::new(l.base, l.size + extra));
    }

    /// Adopteert een bestaande partitie na een kern-flip: hij wordt uit de
    /// vrije lijst geknipt in plaats van eruit gesneden, en HELEMAAL of niet
    /// (E8). Een blob dat een bezette partitie beschrijft, wordt geweigerd.
    /// De voortgezette eigenaar houdt zijn geheugen ongewist (E3).
    pub fn adopt(&mut self, slot: Slot, base: u64, size: u64) -> Result<Partition<Owned>> {
        self.check_slot(slot)?;
        if size == 0
            || !base.is_multiple_of(GRAIN)
            || !size.is_multiple_of(GRAIN)
            || base.checked_add(size).is_none()
        {
            return Err(Error::Range { base, size });
        }
        let claim = self.claim_size(size)?;
        let end = base.checked_add(claim).ok_or(Error::Range { base, size })?;
        if self.owners.get(slot.get()).copied().flatten().is_some() {
            return Err(Error::StillOwned { slot: slot.get() });
        }
        if !self.free_span(base, end) {
            return Err(Error::NotFree { base, size });
        }
        self.take_range(base, end)?;
        let region = Region::new(base, size);
        if let Some(o) = self.owners.get_mut(slot.get()) {
            *o = Some(region);
        }
        Ok(Partition {
            slot,
            region,
            _state: PhantomData,
        })
    }

    /// Neemt een permanent blok voor een apparaat met DMA in gewoon DRAM
    /// (de videocodec: page tables, firmware, referentieframes). Het telt af
    /// van de capaciteit: een node die zijn codec aanzet heeft echt minder
    /// ruimte.
    pub fn reserve_device(&mut self, size: u64) -> Result<u64> {
        let size = align_grain(size)
            .filter(|s| *s != 0)
            .ok_or(Error::PartitionSize { size })?;
        let base = self.carve(size).ok_or(Error::NoPartition { size })?;
        self.capacity = self.capacity.saturating_sub(size);
        Ok(base)
    }

    /// Geeft een apparaatreservering terug (het ijzer kwam niet op).
    pub fn release_device(&mut self, base: u64, size: u64) -> Result {
        let Some(size) = align_grain(size).filter(|s| *s != 0) else {
            return Ok(());
        };
        self.insert_free(Region::new(base, size))?;
        self.capacity += size;
        Ok(())
    }

    /// Het zichtbare bereik van `slot`, als het er een heeft.
    #[must_use]
    pub fn partition_of(&self, slot: Slot) -> Option<Region> {
        self.owners.get(slot.get()).copied().flatten()
    }

    /// De totale grootte van de pool: het plafond dat HOP krijgt.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Het grootste gat: de grootste partitie die NU te plaatsen is.
    ///
    /// De som geeft het verkeerde antwoord. GEMETEN 19-08 op de LicheeRV: de
    /// gerapporteerde capaciteit flapperde tussen 162 en 198 MB van 222, en
    /// in dat venster weigerde de node een andere job van 28 MB die er wél
    /// in paste. Met dit getal zegt de toelating in één keer nee.
    #[must_use]
    pub fn largest(&self) -> u64 {
        let mut best = 0;
        for r in self.free.iter() {
            let mut size = r.size & !(GRAIN - 1);
            while size > 0 && ((r.base + r.size - size) & !(GRAIN - 1)) < r.base {
                size -= GRAIN;
            }
            size = (self.geo.link_window)(size);
            let extra = (self.geo.reserve)(size);
            size = size.saturating_sub(extra);
            best = best.max(size);
        }
        (self.geo.link_window)(best)
    }

    /// Ligt `[base, end)` helemaal binnen één vrij stuk?
    #[must_use]
    pub fn free_span(&self, base: u64, end: u64) -> bool {
        self.free
            .iter()
            .any(|r| base >= r.base && end <= r.base + r.size)
    }

    /// Haalt alles wat vrij is in `[base, end)` uit de vrije lijst en geeft
    /// terug hoeveel bytes dat waren.
    ///
    /// Een VERSE lijst, geen filter ter plekke: een claim midden in een regio
    /// splitst hem, dus de uitvoer kan groeien. GEMETEN 01-09: het eigen
    /// kernvenster viel midden in de grote QEMU-regio, waarmee de tweede
    /// pool-regio verdween en de adoptie zijn partitie "niet vrij" vond.
    fn take_range(&mut self, base: u64, end: u64) -> Result<u64> {
        if end <= base {
            return Ok(0);
        }
        let mut next: BoundedVec<Region, MAX_FREE_REGIONS> = BoundedVec::new();
        let full = |_| Error::Full {
            cap: MAX_FREE_REGIONS,
        };
        let mut took = 0;
        for r in self.free.iter() {
            let r_end = r.base + r.size;
            if end <= r.base || base >= r_end {
                next.push(*r).map_err(full)?;
                continue;
            }
            if base > r.base {
                next.push(Region::new(r.base, base - r.base))
                    .map_err(full)?;
            }
            if end < r_end {
                next.push(Region::new(end, r_end - end)).map_err(full)?;
            }
            took += end.min(r_end) - base.max(r.base);
        }
        self.free = next;
        Ok(took)
    }

    /// Voegt een stuk gesorteerd in en smelt het met beide buren.
    fn insert_free(&mut self, r: Region) -> Result {
        let pos = self
            .free
            .iter()
            .position(|f| f.base >= r.base)
            .unwrap_or(self.free.len());
        self.free.push(r).map_err(|_| Error::Full {
            cap: MAX_FREE_REGIONS,
        })?;
        if let Some(tail) = self.free.as_mut_slice().get_mut(pos..) {
            tail.rotate_right(1);
        }
        let s = self.free.as_mut_slice();
        if let (Some(a), Some(b)) = (s.get(pos).copied(), s.get(pos + 1).copied())
            && a.base + a.size == b.base
        {
            if let Some(x) = s.get_mut(pos) {
                x.size += b.size;
            }
            self.free.remove(pos + 1);
        }
        let s = self.free.as_mut_slice();
        if pos > 0
            && let (Some(a), Some(b)) = (s.get(pos - 1).copied(), s.get(pos).copied())
            && a.base + a.size == b.base
        {
            if let Some(x) = s.get_mut(pos - 1) {
                x.size += b.size;
            }
            self.free.remove(pos);
        }
        Ok(())
    }

    /// De slots met een partitie (voor de flip-snapshot en de status).
    pub fn owners(&self) -> impl Iterator<Item = (Slot, Region)> + '_ {
        self.owners
            .iter()
            .enumerate()
            .filter_map(|(i, o)| Some((Slot::new(i)?, (*o)?)))
    }

    /// De vertaalopslag achter een partitie van `size` (voor de adoptie).
    #[must_use]
    pub fn reserve_of(&self, size: u64) -> u64 {
        (self.geo.reserve)(size)
    }

    /// Het hoogste slotnummer dat dit board kent.
    #[must_use]
    pub fn max_slots(&self) -> usize {
        self.max_slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn s(i: usize) -> Slot {
        Slot::new(i).unwrap()
    }

    fn pool(regs: &[Region]) -> PartitionPool {
        PartitionPool::new(regs, Region::default(), Geometry::FLAT, SLOT_CAP).unwrap()
    }

    fn release(pool: &mut PartitionPool, p: Partition<Free>) {
        let slot = p.slot();
        p.dispatched()
            .release(pool, Stopped::confirmed(slot))
            .unwrap();
    }

    /// Hou het token vast zoals de lifecycle-actor dat doet.
    fn keep(p: Partition<Free>) -> Region {
        let r = p.region();
        let _ = p;
        r
    }

    fn licheerv() -> [Region; 2] {
        [
            Region::new(0x8800_0000, 0x7F0_0000), // 127 MB (pool A)
            Region::new(0x8000_0000, 0x400_0000), // 64 MB (pool B)
        ]
    }

    // Best-fit: een 64 MB-partitie hoort in de 64 MB-regio te landen, zodat
    // de 124 MB erna nog past (31-07).
    #[test]
    fn part_alloc_best_fit_houdt_grote_regio_heel() {
        let mut p = pool(&licheerv());
        let small = keep(p.alloc(s(1), 64 * MIB).unwrap());
        assert!((0x8000_0000..0x8400_0000).contains(&small.base));
        let big = keep(p.alloc(s(2), 124 * MIB).unwrap());
        assert!(big.base >= 0x8800_0000 && big.base + 124 * MIB <= 0x8FF0_0000);
    }

    #[test]
    fn part_alloc_grote_eerst_dan_kleine() {
        let mut p = pool(&licheerv());
        keep(p.alloc(s(1), 124 * MIB).unwrap());
        keep(p.alloc(s(2), 64 * MIB).unwrap());
    }

    #[test]
    fn allocation_rejects_overflow_before_changing_pool() {
        let mut p = pool(&[Region::new(0x8000_0000, 64 * MIB)]);
        let before = p.largest();
        for size in [0, u64::MAX, u64::MAX - 1] {
            assert!(p.alloc(s(1), size).is_err());
        }
        assert_eq!(p.largest(), before);
        assert_eq!(p.capacity(), 64 * MIB);
    }

    #[test]
    fn part_alloc_geeft_de_echte_maat() {
        let mut p = pool(&[Region::new(0x8800_0000, 0x400_0000)]);
        let r = keep(p.alloc(s(1), 23 * MIB).unwrap());
        assert!(r.size >= 23 * MIB && r.size.is_multiple_of(GRAIN));
        assert_eq!(p.partition_of(s(1)), Some(r));
    }

    #[test]
    fn part_alloc_fail_keeps_the_reservation() {
        let mut p = pool(&[Region::new(0x8000_0000, 64 * MIB)]);
        let first = keep(p.alloc(s(1), 40 * MIB).unwrap());
        assert!(p.alloc(s(1), 200 * MIB).is_err());
        assert_eq!(p.partition_of(s(1)), Some(first));
        let other = keep(p.alloc(s(2), 24 * MIB).unwrap());
        assert!(!other.overlaps(first));
    }

    #[test]
    fn part_alloc_reuse_requires_release() {
        let mut p = pool(&[Region::new(0x8000_0000, 32 * MIB)]);
        let a = p.alloc(s(1), 30 * MIB).unwrap();
        assert_eq!(
            p.alloc(s(1), 30 * MIB).unwrap_err(),
            Error::StillOwned { slot: 1 }
        );
        release(&mut p, a);
        keep(p.alloc(s(1), 30 * MIB).unwrap());
    }

    #[test]
    fn pool_largest_is_the_hole_not_the_sum() {
        let mut p = pool(&[
            Region::new(0x8800_0000, 126 * MIB),
            Region::new(0x8000_0000, 64 * MIB),
            Region::new(0x8600_0000, 32 * MIB),
        ]);
        assert_eq!(p.largest(), 126 * MIB);
        keep(p.alloc(s(1), 126 * MIB).unwrap());
        keep(p.alloc(s(2), 36 * MIB).unwrap());
        let free = (126 + 64 + 32) * MIB - 126 * MIB - 36 * MIB;
        assert_eq!(free, 60 * MIB);
        assert_eq!(p.largest(), 32 * MIB);
        assert!(p.alloc(s(3), 36 * MIB).is_err());
    }

    fn oud() -> [Region; 3] {
        [
            Region::new(0x8800_0000, 126 * MIB),
            Region::new(0x8000_0000, 64 * MIB),
            Region::new(0x8600_0000, 32 * MIB),
        ]
    }

    fn nieuw() -> [Region; 1] {
        [Region::new(0x8240_0000, 218 * MIB)]
    }

    // Dereks twee klachten (19-08), in de oude en de nieuwe vorm naast elkaar.
    #[test]
    fn lichee_rv_one_region_places_what_three_could_not() {
        // 200 MB op een lege node.
        assert!(pool(&oud()).alloc(s(1), 200 * MIB).is_err());
        keep(pool(&nieuw()).alloc(s(1), 200 * MIB).unwrap());

        // Vrij maar niet plaatsbaar.
        let mut p = pool(&oud());
        for (i, mb) in [60u64, 30, 32].into_iter().enumerate() {
            keep(p.alloc(s(i + 1), mb * MIB).unwrap());
        }
        let vrij = (222 - 60 - 30 - 32) * MIB;
        assert!(p.largest() < vrij);
        assert!(p.alloc(s(4), 96 * MIB).is_err());
        let mut p = pool(&nieuw());
        for (i, mb) in [60u64, 30, 32].into_iter().enumerate() {
            keep(p.alloc(s(i + 1), mb * MIB).unwrap());
        }
        keep(p.alloc(s(4), 96 * MIB).unwrap());

        // Vrij IS plaatsbaar zolang er alleen bijkomt.
        let mut p = pool(&nieuw());
        let mut used = 0;
        for (i, mb) in [32u64, 128, 20, 14].into_iter().enumerate() {
            keep(p.alloc(s(i + 1), mb * MIB).unwrap());
            used += mb * MIB;
            assert_eq!(p.largest(), 218 * MIB - used);
        }

        // Een stop maakt ook in één regio een gat.
        let mut p = pool(&nieuw());
        keep(p.alloc(s(1), 60 * MIB).unwrap());
        let mid = p.alloc(s(2), 60 * MIB).unwrap();
        keep(p.alloc(s(3), 60 * MIB).unwrap());
        release(&mut p, mid);
        assert!(p.largest() < (218 - 120) * MIB);
    }

    // De eigen kern valt uit de pool, ook midden in een regio.
    #[test]
    fn own_kernel_is_carved_out() {
        let own = Region::new(0x8c00_0000, 32 * MIB);
        let p = PartitionPool::new(&oud(), own, Geometry::FLAT, SLOT_CAP).unwrap();
        assert_eq!(p.capacity(), (222 - 32) * MIB);
        assert_eq!(p.largest(), 64 * MIB);
        assert!(!p.free_span(own.base, own.base + own.size));
    }

    #[test]
    fn device_reservation_deelt_de_pool_met_de_slots() {
        let mut p = pool(&[Region::new(0x8000_0000, 512 * MIB)]);
        let before = p.capacity();
        let arena = p.reserve_device(256 * MIB).unwrap();
        assert_eq!(arena % GRAIN, 0);
        assert_eq!(p.capacity(), before - 256 * MIB);
        let part = keep(p.alloc(s(1), 128 * MIB).unwrap());
        assert!(!part.overlaps(Region::new(arena, 256 * MIB)));
        assert!(p.reserve_device(1 << 30).is_err());
        keep(p.alloc(s(2), 64 * MIB).unwrap());
    }

    // adopt_test.go.
    #[test]
    fn part_adopt_claimt_en_geeft_niet_opnieuw_uit() {
        let mut p = pool(&[Region::new(0x8000_0000, 64 * MIB)]);
        let a = p.adopt(s(3), 0x8200_0000, 16 * MIB).unwrap();
        assert_eq!(a.region(), Region::new(0x8200_0000, 16 * MIB));
        let _ = a;
        for i in 4..8 {
            if let Ok(q) = p.alloc(s(i), 8 * MIB) {
                assert!(!q.region().overlaps(Region::new(0x8200_0000, 16 * MIB)));
                let _ = q;
            }
        }
    }

    #[test]
    fn part_adopt_weigert_bezet_bereik() {
        let mut p = pool(&[Region::new(0x8000_0000, 64 * MIB)]);
        let r = keep(p.alloc(s(1), 16 * MIB).unwrap());
        assert!(matches!(
            p.adopt(s(2), r.base, 16 * MIB),
            Err(Error::NotFree { .. })
        ));
        assert!(p.partition_of(s(2)).is_none());
    }

    #[test]
    fn take_range_midden_in_regio_raakt_buurregio_niet() {
        let mut p = pool(&[
            Region::new(0x4000_0000, 256 * MIB),
            Region::new(0x8000_0000, 64 * MIB),
        ]);
        p.take_range(0x4800_0000, 0x4A00_0000).unwrap();
        assert!(
            p.free_span(0x8000_0000, 0x8400_0000),
            "neighbor region vanished"
        );
        assert!(p.free_span(0x4000_0000, 0x4800_0000));
        assert!(p.free_span(0x4A00_0000, 0x5000_0000));
    }

    #[test]
    fn part_adopt_rejects_wrapped_range() {
        let mut p = pool(&[Region::new(0x8000_0000, 64 * MIB)]);
        assert!(p.adopt(s(1), !(GRAIN - 1), 4 * MIB).is_err());
    }

    // E2/E9 (ownership_test.go).
    #[test]
    fn stop_proof_of_another_slot_is_refused() {
        let mut p = pool(&[Region::new(0x8000_0000, 32 * MIB)]);
        let a = p.alloc(s(1), 16 * MIB).unwrap().dispatched();
        assert!(a.release(&mut p, Stopped::confirmed(s(2))).is_err());
        assert!(
            p.partition_of(s(1)).is_some(),
            "wrong proof released memory"
        );
    }

    #[test]
    fn dropped_token_stays_claimed() {
        let mut p = pool(&[Region::new(0x8000_0000, 4 * MIB)]);
        drop(p.alloc(s(1), 4 * MIB).unwrap());
        assert!(p.alloc(s(2), 2 * MIB).is_err(), "dropped claim was reused");
    }

    #[test]
    fn translation_reserve_is_part_of_the_claim() {
        fn window(s: u64) -> u64 {
            s
        }
        fn reserve(s: u64) -> u64 {
            if s > 8 << 20 { GRAIN } else { 0 }
        }
        let geo = Geometry {
            link_window: window,
            reserve,
        };
        let mut p = PartitionPool::new(
            &[Region::new(0x8000_0000, 16 * MIB)],
            Region::default(),
            geo,
            SLOT_CAP,
        )
        .unwrap();
        let a = p.alloc(s(1), 14 * MIB).unwrap();
        assert_eq!(a.region().size, 14 * MIB);
        assert_eq!(p.largest(), 0, "table storage handed out twice");
        release(&mut p, a);
        assert_eq!(p.largest(), 14 * MIB);
    }
}
