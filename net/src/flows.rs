//! De conntrack van de masquerade: flows in een vaste slab, met twee
//! indexen (Go: `flowsFwd` en `flowsRev`).
//!
//! Een flow is volledig beschreven door zijn eigen velden, en BEIDE
//! sleutels zijn daaruit af te leiden: de voorwaartse (slot-IP:poort naar
//! peer) en de omgekeerde (node-poort plus peer). De indexen houden dus
//! alleen een slab-nummer; verwijderen is één plek, en een nieuwere flow
//! met dezelfde sleutel kan nooit per ongeluk mee (de Go-code toetste
//! daarvoor pointer-gelijkheid).

use crate::Error;
use alloc::vec::Vec;

/// De finalizer van splitmix64: goedkoop, en genoeg om opeenvolgende IP's en
/// poorten over de indexen te spreiden.
const fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Reserveert `n` plaatsen faalbaar (bij boot) en vult ze met `v`.
fn filled<T: Clone>(n: usize, v: T) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| Error::OutOfMemory(n.saturating_mul(core::mem::size_of::<T>())))?;
    out.resize(n, v);
    Ok(out)
}

/// Het conntrack-plafond: de anti-DoS-grens, en wat een kern-flip draagt.
/// De volle conntrack van de switch (`hopswitch.MaxFlows`): het
/// anti-DoS-plafond van de NAT (een app laat HOP's geheugen op core 0 nooit
/// vollopen).
pub const MAX_FLOWS: usize = 4096;

/// Het eerlijke deel per slot ónder het globale plafond. Zonder dit is de
/// conntrack één gedeelde pot: één app die 4096 verbindingen opent laat elke
/// buur geen nieuwe meer maken. Ruim gekozen: acht slots × 512 vullen al het
/// globale plafond (een eerlijkheidsgrens, geen reservering).
pub const MAX_FLOWS_PER_SLOT: usize = 512;

const EMPTY: u16 = u16::MAX;
const INDEX: usize = MAX_FLOWS * 2;

/// Eén uitgaande masquerade-verbinding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Flow {
    pub(crate) proto: u8,
    pub(crate) slot: u16,
    /// Slot/client → peer zag FIN.
    pub(crate) fin_fwd: bool,
    /// Peer/dienst → client zag FIN.
    pub(crate) fin_rev: bool,
    /// Een hairpin naar 10.100.0.1 in plaats van het node-IP: de reply
    /// draagt dat adres weer als bron.
    pub(crate) via_gw: bool,
    pub(crate) slot_ip: u32,
    pub(crate) dst_ip: u32,
    pub(crate) slot_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) node_port: u16,
    /// Laatste levensteken, nanoseconden op de klok van de switch.
    pub(crate) seen: u64,
}

/// De voorwaartse sleutel: `(proto, slot-IP, peer-IP, slot-poort, peer-poort)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FKey(
    pub(crate) u8,
    pub(crate) u32,
    pub(crate) u32,
    pub(crate) u16,
    pub(crate) u16,
);

/// De omgekeerde sleutel: `(proto, node-poort, peer-IP, peer-poort)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RKey(
    pub(crate) u8,
    pub(crate) u16,
    pub(crate) u32,
    pub(crate) u16,
);

impl Flow {
    pub(crate) fn fkey(&self) -> FKey {
        FKey(
            self.proto,
            self.slot_ip,
            self.dst_ip,
            self.slot_port,
            self.dst_port,
        )
    }
    pub(crate) fn rkey(&self) -> RKey {
        RKey(self.proto, self.node_port, self.dst_ip, self.dst_port)
    }
    fn hash(&self, d: Dir) -> u64 {
        match d {
            Dir::Fwd => self.fkey().hash(),
            Dir::Rev => self.rkey().hash(),
        }
    }
}

/// Een van de twee indexen; ze zijn elkaars spiegelbeeld op de sleutel na.
#[derive(Clone, Copy)]
enum Dir {
    Fwd,
    Rev,
}

impl FKey {
    fn hash(&self) -> u64 {
        mix((u64::from(self.1) << 32 | u64::from(self.2))
            ^ (u64::from(self.3) << 24 | u64::from(self.4) << 8 | u64::from(self.0))
                .rotate_left(17))
    }
}

impl RKey {
    fn hash(&self) -> u64 {
        mix(u64::from(self.2) << 32
            | u64::from(self.1) << 16
            | u64::from(self.3) ^ u64::from(self.0) << 40)
    }
}

/// Een flow-nummer in de slab.
pub(crate) type Id = u16;

/// De slab plus de twee indexen.
pub(crate) struct FlowTable {
    slab: Vec<Option<Flow>>,
    free: Vec<Id>,
    fwd: Vec<Id>,
    rev: Vec<Id>,
    len: usize,
    /// Per slot met flows het aantal, op slot gesorteerd: hoogstens
    /// [`MAX_FLOWS`] ingangen (elke ingang heeft minstens één flow), en die
    /// ruimte is bij boot gereserveerd. Een tabel per slotnummer zou een
    /// grens op het aantal slots zijn; die is er niet.
    by_slot: Vec<(u16, u16)>,
}

impl FlowTable {
    /// Een lege tabel; alloceert één keer.
    pub(crate) fn new() -> Result<Self, Error> {
        let mut free = filled(MAX_FLOWS, 0 as Id)?;
        for (i, f) in free.iter_mut().enumerate() {
            // Omgekeerd, zodat `pop` met 0 begint. MAX_FLOWS past in een u16.
            *f = (MAX_FLOWS - 1 - i) as Id;
        }
        Ok(Self {
            slab: filled(MAX_FLOWS, None)?,
            free,
            fwd: filled(INDEX, EMPTY)?,
            rev: filled(INDEX, EMPTY)?,
            len: 0,
            by_slot: {
                let mut v = Vec::new();
                v.try_reserve_exact(MAX_FLOWS)
                    .map_err(|_| Error::OutOfMemory(MAX_FLOWS * 4))?;
                v
            },
        })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len >= MAX_FLOWS
    }

    /// Het aantal flows van `slot`.
    pub(crate) fn count(&self, slot: u16) -> usize {
        match self.by_slot.binary_search_by_key(&slot, |e| e.0) {
            Ok(i) => self.by_slot.get(i).map_or(0, |e| usize::from(e.1)),
            Err(_) => 0,
        }
    }

    /// Telt een flow van `slot` bij (`up`) of eraf. Groeien past altijd: er
    /// zijn nooit meer ingangen dan flows, en de slab is begrensd.
    fn tally(&mut self, slot: u16, up: bool) {
        match self.by_slot.binary_search_by_key(&slot, |e| e.0) {
            Ok(i) if up => {
                if let Some(e) = self.by_slot.get_mut(i) {
                    e.1 += 1;
                }
            }
            Ok(i) => {
                if self.by_slot.get(i).is_some_and(|e| e.1 <= 1) {
                    self.by_slot.remove(i);
                } else if let Some(e) = self.by_slot.get_mut(i) {
                    e.1 -= 1;
                }
            }
            Err(i) if up && self.by_slot.len() < self.by_slot.capacity() => {
                self.by_slot.insert(i, (slot, 1));
            }
            Err(_) => {}
        }
    }

    pub(crate) fn get(&self, id: Id) -> Option<&Flow> {
        self.slab.get(usize::from(id))?.as_ref()
    }

    pub(crate) fn get_mut(&mut self, id: Id) -> Option<&mut Flow> {
        self.slab.get_mut(usize::from(id))?.as_mut()
    }

    fn index(&self, d: Dir) -> &[Id] {
        match d {
            Dir::Fwd => &self.fwd,
            Dir::Rev => &self.rev,
        }
    }

    /// De plek in index `d` van de flow waarvoor `is` geldt (`Ok`), of de
    /// lege plek waar hij zou komen (`Err`): lineair proberen vanaf `hash`.
    fn pos(&self, d: Dir, hash: u64, is: impl Fn(&Flow) -> bool) -> Result<usize, usize> {
        let mask = INDEX - 1;
        let mut i = (hash as usize) & mask;
        loop {
            let id = self.index(d).get(i).copied().unwrap_or(EMPTY);
            if id == EMPTY {
                return Err(i);
            }
            if self.get(id).is_some_and(&is) {
                return Ok(i);
            }
            i = (i + 1) & mask;
        }
    }

    fn fpos(&self, k: &FKey) -> Result<usize, usize> {
        self.pos(Dir::Fwd, k.hash(), |f| f.fkey() == *k)
    }

    fn rpos(&self, k: &RKey) -> Result<usize, usize> {
        self.pos(Dir::Rev, k.hash(), |f| f.rkey() == *k)
    }

    /// De flow met voorwaartse sleutel `k`.
    pub(crate) fn by_fwd(&self, k: &FKey) -> Option<Id> {
        let i = self.fpos(k).ok()?;
        self.fwd.get(i).copied()
    }

    /// De flow met omgekeerde sleutel `k`.
    pub(crate) fn by_rev(&self, k: &RKey) -> Option<Id> {
        let i = self.rpos(k).ok()?;
        self.rev.get(i).copied()
    }

    /// Voegt `fl` toe. `None` als de slab vol is of een van beide sleutels
    /// al bezet is (de aanroeper toetste dat; dit is de tweede lijn).
    pub(crate) fn insert(&mut self, fl: Flow) -> Option<Id> {
        let fp = self.fpos(&fl.fkey()).err()?;
        let rp = self.rpos(&fl.rkey()).err()?;
        let id = self.free.pop()?;
        *self.slab.get_mut(usize::from(id))? = Some(fl);
        *self.fwd.get_mut(fp)? = id;
        *self.rev.get_mut(rp)? = id;
        self.len += 1;
        self.tally(fl.slot, true);
        Some(id)
    }

    /// Het enige verwijderpad: beide indexen en de slab. `false` als `id`
    /// al leeg was.
    pub(crate) fn remove(&mut self, id: Id) -> bool {
        let Some(fl) = self.get(id).copied() else {
            return false;
        };
        if let Ok(p) = self.fpos(&fl.fkey()) {
            self.unindex(Dir::Fwd, p);
        }
        if let Ok(p) = self.rpos(&fl.rkey())
            && self.rev.get(p) == Some(&id)
        {
            self.unindex(Dir::Rev, p);
        }
        if let Some(s) = self.slab.get_mut(usize::from(id)) {
            *s = None;
        }
        // De vrije lijst heeft precies MAX_FLOWS plaatsen; hij kan niet vol.
        self.free.push(id);
        self.len -= 1;
        self.tally(fl.slot, false);
        true
    }

    /// Haalt plek `i` uit index `d` en schuift de probe-keten erachter op
    /// (backward-shift deletion: geen grafstenen).
    fn unindex(&mut self, d: Dir, mut i: usize) {
        let mask = INDEX - 1;
        let index = match d {
            Dir::Fwd => &mut self.fwd,
            Dir::Rev => &mut self.rev,
        };
        if let Some(e) = index.get_mut(i) {
            *e = EMPTY;
        }
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let id = index.get(j).copied().unwrap_or(EMPTY);
            let Some(Some(fl)) = self.slab.get(usize::from(id)) else {
                break;
            };
            let h = (fl.hash(d) as usize) & mask;
            let stays = if i <= j {
                i < h && h <= j
            } else {
                i < h || h <= j
            };
            if !stays {
                if let Some(e) = index.get_mut(i) {
                    *e = id;
                }
                if let Some(e) = index.get_mut(j) {
                    *e = EMPTY;
                }
                i = j;
            }
        }
    }

    /// Alle levende flow-nummers (voor een veeg of een snapshot).
    pub(crate) fn ids(&self) -> impl Iterator<Item = Id> + '_ {
        self.slab
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_some())
            .map(|(i, _)| i as Id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fl(slot: u16, sport: u16, np: u16) -> Flow {
        Flow {
            proto: 6,
            slot,
            fin_fwd: false,
            fin_rev: false,
            via_gw: false,
            slot_ip: 0x0A64_0000 | (u32::from(slot) + 1),
            dst_ip: 0x5DB8_D822,
            slot_port: sport,
            dst_port: 443,
            node_port: np,
            seen: 0,
        }
    }

    #[test]
    fn fill_remove_and_refill_keeps_both_indexes() {
        let mut t = FlowTable::new().unwrap();
        let mut ids = Vec::new();
        for i in 0..MAX_FLOWS as u16 {
            ids.push(t.insert(fl(i % 8 + 1, i, 20000 + i)).unwrap());
        }
        assert!(t.is_full());
        assert!(t.insert(fl(1, 60000, 60000)).is_none());
        for (n, id) in ids.iter().enumerate().filter(|(n, _)| n % 3 == 0) {
            assert!(t.remove(*id), "flow {n}");
        }
        for i in 0..MAX_FLOWS as u16 {
            let f = fl(i % 8 + 1, i, 20000 + i);
            let present = i % 3 != 0;
            assert_eq!(t.by_fwd(&f.fkey()).is_some(), present, "fwd {i}");
            assert_eq!(t.by_rev(&f.rkey()).is_some(), present, "rev {i}");
        }
        assert_eq!(
            t.count(1)
                + t.count(2)
                + t.count(3)
                + t.count(4)
                + t.count(5)
                + t.count(6)
                + t.count(7)
                + t.count(8),
            t.len()
        );
        assert!(t.insert(fl(1, 0, 20000)).is_some());
    }

    #[test]
    fn the_count_per_slot_knows_no_slot_ceiling() {
        let mut t = FlowTable::new().unwrap();
        let a = t.insert(fl(40_000, 1, 20001)).unwrap();
        t.insert(fl(40_000, 2, 20002)).unwrap();
        t.insert(fl(300, 3, 20003)).unwrap();
        assert_eq!((t.count(40_000), t.count(300), t.count(7)), (2, 1, 0));
        assert!(t.remove(a));
        assert_eq!(t.count(40_000), 1);
    }
}
