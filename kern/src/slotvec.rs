//! [`SlotVec`]: een tabel op slotnummer zonder vast aantal.
//!
//! Er is geen maximum aantal slots (Derek, 01-11): de kern deelt het laagste
//! vrije nummer uit, zoals Linux een fd, en een tabel per slot groeit dus
//! met het hoogste nummer dat er ooit tegelijk was, zoals de fd-tabel van
//! een proces (`expand_files`). Groeien is faalbaar ("past niet"); een slot
//! dat eruit gaat, geeft zijn plek terug, en een lege staart gaat eraf.

use crate::{Error, Result, Slot};
use alloc::vec::Vec;

/// Per slot een `T`, op slotnummer.
#[derive(Debug)]
pub struct SlotVec<T>(Vec<Option<T>>);

impl<T> SlotVec<T> {
    /// Een lege tabel, voor in een `static`.
    #[must_use]
    pub const fn new() -> Self {
        SlotVec(Vec::new())
    }

    /// De waarde van `slot`.
    #[must_use]
    pub fn get(&self, slot: Slot) -> Option<&T> {
        self.0.get(slot.get())?.as_ref()
    }

    /// De waarde van `slot`, veranderlijk.
    pub fn get_mut(&mut self, slot: Slot) -> Option<&mut T> {
        self.0.get_mut(slot.get())?.as_mut()
    }

    /// Zet `v` op `slot` en geeft de oude waarde; faalt alleen als de tabel
    /// niet kan groeien.
    pub fn insert(&mut self, slot: Slot, v: T) -> Result<Option<T>> {
        let i = slot.get();
        if i >= self.0.len() {
            let more = i + 1 - self.0.len();
            self.0.try_reserve(more).map_err(|_| Error::OutOfMemory {
                bytes: more * core::mem::size_of::<Option<T>>(),
            })?;
            self.0.resize_with(i + 1, || None);
        }
        Ok(self.0.get_mut(i).and_then(|e| e.replace(v)))
    }

    /// Haalt `slot` eruit.
    pub fn remove(&mut self, slot: Slot) -> Option<T> {
        let old = self.0.get_mut(slot.get())?.take();
        while self.0.last().is_some_and(Option::is_none) {
            self.0.pop();
        }
        old
    }

    /// De slots met een waarde, oplopend.
    pub fn iter(&self) -> impl Iterator<Item = (Slot, &T)> {
        self.0
            .iter()
            .enumerate()
            .filter_map(|(i, v)| Some((Slot::new(i)?, v.as_ref()?)))
    }

    /// Het laagste slot zonder waarde waarvoor `free` ook ja zegt: de plek
    /// van een nieuwe bewoner, zoals de laagste vrije fd.
    #[must_use]
    pub fn lowest_free(&self, free: impl Fn(Slot) -> bool) -> Option<Slot> {
        (1..=abi::layout::SLOT_MAX)
            .filter_map(Slot::new)
            .find(|&s| self.get(s).is_none() && free(s))
    }
}

impl<T> Default for SlotVec<T> {
    fn default() -> Self {
        SlotVec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(i: usize) -> Slot {
        Slot::new(i).unwrap()
    }

    #[test]
    fn grows_past_any_old_ceiling_and_gives_back_its_tail() {
        let mut t = SlotVec::new();
        for i in 1..=1000 {
            assert_eq!(t.insert(s(i), i).unwrap(), None);
        }
        assert_eq!(t.get(s(1000)), Some(&1000));
        assert_eq!(t.lowest_free(|_| true), Some(s(1001)));
        assert_eq!(t.remove(s(7)), Some(7));
        assert_eq!(
            t.lowest_free(|_| true),
            Some(s(7)),
            "het laagste vrije, als een fd"
        );
        assert_eq!(t.lowest_free(|x| x != s(7)), Some(s(1001)));
        for i in 8..=1000 {
            t.remove(s(i));
        }
        assert_eq!(t.0.len(), 7, "de lege staart is eraf");
        assert_eq!(
            t.iter().map(|(x, _)| x.get()).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
    }
}
