//! Het kooi-geheugen: wat de kooi per slot en per core nodig heeft buiten de
//! partitie (het kooi-blok van een slot met zijn stage-2-tabellen en
//! ctx-blok, de bewonerslijst van een core), in blokken van
//! [`CAGE_STRIDE`] uit chunks van [`GRAIN`] die uit de partitie-pool komen.
//!
//! Er is geen maximum aantal slots (Derek, 01-11): tot 01-11 legde het plan
//! van elk board een kooi-blok per slot vast, voor `max_slots`. Nu komt een
//! blok bij de plaatsing uit de pool en gaat het bij de stop terug; past het
//! niet, dan is het dezelfde weigering als een partitie die niet past. Een
//! chunk die leeg wordt, gaat terug naar de pool.
//!
//! De app-cores lezen dit geheugen op EL2 met de MMU uit, langs elke cache
//! heen. Waar de kern de pool gecached mapt, zet de kooi een verse chunk
//! eerst op Device in de eigen map, en een lege terug, vóór hij weer in de
//! pool gaat ([`Cage::map_cage`](crate::cage::Cage::map_cage)), zoals het
//! device-venster van het plan dat voor blok 0 doet.
//!
//! Puur boekhouding; eigendom van de lifecycle-actor, die ook de pool
//! bezit. De kooi vraagt en geeft via [`CageMem`].

use crate::partmem::PartitionPool;
use crate::slots::try_push;
use crate::{Error, GRAIN, Region, Result};
use abi::layout::CAGE_STRIDE;
use alloc::vec::Vec;

/// Hoeveel blokken een chunk draagt.
pub const BLOCKS: usize = (GRAIN / CAGE_STRIDE) as usize;
const _: () = assert!(BLOCKS == 32 && GRAIN.is_multiple_of(CAGE_STRIDE));

/// Eén chunk: zijn basis en welke blokken bezet zijn (bit `i` = blok `i`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// De basis, op [`GRAIN`].
    pub base: u64,
    /// De bezette blokken.
    pub used: u32,
}

impl Chunk {
    /// Het eerste blok van een vrije run van `n` blokken.
    fn run(&self, n: usize) -> Option<usize> {
        let mask = if n >= BLOCKS {
            u32::MAX
        } else {
            (1u32 << n) - 1
        };
        (0..=BLOCKS.checked_sub(n)?).find(|&i| self.used & (mask << i) == 0)
    }
}

/// Het masker van `n` blokken vanaf blok `first`.
fn mask(first: usize, n: usize) -> u32 {
    let m = if n >= BLOCKS {
        u32::MAX
    } else {
        (1u32 << n) - 1
    };
    m << first
}

/// Het kooi-geheugen: de chunks en hun bezetting.
#[derive(Debug, Default)]
pub struct CageHeap {
    chunks: Vec<Chunk>,
}

/// Wat een vrijgave teruggeeft: een chunk die leeg werd. De kooi zet hem
/// terug op het attribuut van de pool, dan gaat hij de pool in
/// ([`CageHeap::give_back`]).
#[must_use]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Empty(pub Region);

impl CageHeap {
    /// Een leeg kooi-geheugen.
    #[must_use]
    pub const fn new() -> Self {
        CageHeap { chunks: Vec::new() }
    }

    /// `n` aaneengesloten blokken (1..=[`BLOCKS`]) in een chunk die er al
    /// is. `None`: er is geen run van `n` vrij, en er moet een chunk bij.
    pub fn alloc(&mut self, n: usize) -> Option<u64> {
        if n == 0 || n > BLOCKS {
            return None;
        }
        self.chunks.iter_mut().find_map(|c| {
            let i = c.run(n)?;
            c.used |= mask(i, n);
            Some(c.base + i as u64 * CAGE_STRIDE)
        })
    }

    /// Neemt een verse chunk op (uit de pool, en door de kooi al gemapt).
    pub fn add(&mut self, chunk: Region) -> Result {
        if chunk.size != GRAIN || !chunk.base.is_multiple_of(GRAIN) {
            return Err(Error::Range {
                base: chunk.base,
                size: chunk.size,
            });
        }
        try_push(
            &mut self.chunks,
            Chunk {
                base: chunk.base,
                used: 0,
            },
        )
    }

    /// Geeft `n` blokken vanaf `pa` terug; een chunk die daardoor leeg is,
    /// komt eruit en terug als [`Empty`]. Een vrijgave van iets wat niet
    /// van dit geheugen is, is een fout van de kern: de bezetting blijft.
    pub fn free(&mut self, pa: u64, n: usize) -> Result<Option<Empty>> {
        let base = pa & !(GRAIN - 1);
        let i = ((pa - base) / CAGE_STRIDE) as usize;
        let at = self
            .chunks
            .iter()
            .position(|c| c.base == base)
            .ok_or(Error::NotFree { base: pa, size: 0 })?;
        let m = mask(i, n);
        let c = self
            .chunks
            .get_mut(at)
            .ok_or(Error::NotFree { base: pa, size: 0 })?;
        if n == 0 || i + n > BLOCKS || c.used & m != m || !pa.is_multiple_of(CAGE_STRIDE) {
            return Err(Error::NotFree {
                base: pa,
                size: n as u64 * CAGE_STRIDE,
            });
        }
        c.used &= !m;
        if c.used != 0 {
            return Ok(None);
        }
        self.chunks.swap_remove(at);
        Ok(Some(Empty(Region::new(base, GRAIN))))
    }

    /// De chunks met hun bezetting, voor het handoff-blob van de flip.
    #[must_use]
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Neemt de chunks van de vorige kern over (de flip), met hun
    /// bezetting; de kern haalde ze al uit zijn pool
    /// (`Plan::exclude_from_pool`).
    pub fn adopt(&mut self, chunks: &[Chunk]) -> Result {
        for c in chunks {
            self.add(Region::new(c.base, GRAIN))?;
            if let Some(x) = self.chunks.last_mut() {
                x.used = c.used;
            }
        }
        Ok(())
    }

    /// Een lege chunk terug naar de pool, nadat de kooi hem terugzette.
    pub fn give_back(pool: &mut PartitionPool, e: Empty) -> Result {
        pool.release_device(e.0.base, e.0.size)
    }
}

/// Wat de kooi van het kooi-geheugen ziet: blokken vragen en teruggeven.
/// De lifecycle geeft het mee aan de calls van de kooi die geheugen kunnen
/// kosten (de bouw) of teruggeven (de vrijgave).
pub trait CageMem {
    /// `n` aaneengesloten blokken (1..=[`BLOCKS`]), op [`CAGE_STRIDE`]
    /// gealigneerd en Device in de map van de kern. Een fout is "past niet".
    fn alloc(&mut self, n: usize) -> Result<u64>;
    /// Geeft `n` blokken vanaf `pa` terug.
    fn free(&mut self, pa: u64, n: usize);
}

/// Het kooi-geheugen zoals de lifecycle het de kooi leent: het geheugen,
/// de pool waar een chunk uit komt en in terug gaat, en de map-functie van
/// de kooi ([`crate::cage::Cage::MAP_CAGE`]).
pub struct Lend<'a> {
    /// Het geheugen.
    pub heap: &'a mut CageHeap,
    /// De pool.
    pub pool: &'a mut PartitionPool,
    /// Zet een chunk Device (`true`) of terug.
    pub map: fn(Region, bool) -> bool,
}

impl CageMem for Lend<'_> {
    fn alloc(&mut self, n: usize) -> Result<u64> {
        if let Some(pa) = self.heap.alloc(n) {
            return Ok(pa);
        }
        if n > BLOCKS {
            return Err(Error::PartitionSize {
                size: n as u64 * CAGE_STRIDE,
            });
        }
        // Een verse chunk: uit de pool, telt af van de capaciteit, en pas
        // de kooi in als hij gemapt is.
        let base = self.pool.reserve_device(GRAIN)?;
        let chunk = Region::new(base, GRAIN);
        if !(self.map)(chunk, true) {
            let _ = self.pool.release_device(base, GRAIN);
            return Err(Error::Range { base, size: GRAIN });
        }
        if let Err(e) = self.heap.add(chunk) {
            (self.map)(chunk, false);
            let _ = self.pool.release_device(base, GRAIN);
            return Err(e);
        }
        self.heap.alloc(n).ok_or(Error::NoPartition { size: GRAIN })
    }

    fn free(&mut self, pa: u64, n: usize) {
        // Een vrijgave die niet klopt, laat de bezetting staan: fail-closed,
        // het geheugen blijft van de kooi.
        if let Ok(Some(e)) = self.heap.free(pa, n) {
            (self.map)(e.0, false);
            let _ = CageHeap::give_back(self.pool, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn blocks_come_from_chunks_and_an_empty_chunk_goes_back() {
        let mut h = CageHeap::new();
        assert_eq!(h.alloc(1), None, "no chunk yet");
        h.add(Region::new(8 * MIB, GRAIN)).unwrap();
        let a = h.alloc(1).unwrap();
        let b = h.alloc(4).unwrap();
        assert_eq!((a, b), (8 * MIB, 8 * MIB + CAGE_STRIDE));
        assert_eq!(h.alloc(BLOCKS - 4), None, "only 27 left in the chunk");
        assert_eq!(h.free(a, 1).unwrap(), None);
        assert!(h.free(a, 1).is_err(), "a double free is refused");
        assert_eq!(
            h.free(b, 4).unwrap(),
            Some(Empty(Region::new(8 * MIB, GRAIN))),
            "the last block frees the chunk"
        );
        assert!(h.chunks().is_empty());
        assert!(h.add(Region::new(8 * MIB + 1, GRAIN)).is_err());
    }

    #[test]
    fn adoption_keeps_the_occupation_of_the_previous_kernel() {
        let mut h = CageHeap::new();
        h.adopt(&[Chunk {
            base: 4 * MIB,
            used: 0b101,
        }])
        .unwrap();
        assert_eq!(h.alloc(1), Some(4 * MIB + CAGE_STRIDE));
        assert_eq!(h.alloc(1), Some(4 * MIB + 3 * CAGE_STRIDE));
    }
}
