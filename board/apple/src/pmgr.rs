//! De power-manager: blokken aan- en uitzetten, en de tunables die de
//! firmware in de boom achterliet.
//!
//! Op elk ander HopOS-board zette de firmware al aan wat wij nodig hebben.
//! Op Apple silicon niet: iBoot laat de PCIe-controller uit, en wie hem wil,
//! zet zelf zijn power-domein aan. Zolang m1n1 ertussen zat deed híj dat
//! (`pmgr_adt_power_enable`, `src/pmgr.c`); dit is dezelfde mechaniek uit
//! dezelfde bron, de ADT.
//!
//! De boom draagt drie dingen: `/arm-io/<blok>` heeft een
//! `clock-gates`-lijst met device-ID's; `/arm-io/pmgr` heeft een
//! `devices`-tabel die elk ID vertaalt naar (registerbank, offset, ouders);
//! en `ps-regs` zegt waar die banken liggen. t8132 gebruikt ps-regs met
//! u16-ID's (GEMETEN 29-08: de boot meldde "ps-regs, u16 ids"); m1n1's
//! `ps-groups` en u8-ID's van andere generaties staan hier niet.

use cpu::idle::now;
use dev::Pa;
use fw::adt::{Adt, Chain, Node};

const AUTO_ENABLE: u32 = 1 << 28;
const WAS_CLK_GATED: u32 = 1 << 9;
const WAS_PWR_GATED: u32 = 1 << 8;
const PS_TARGET: u32 = 0xf;
/// De stand "actief".
pub const PS_ACTIVE: u32 = 0xf;
const FLAG_VIRTUAL: u8 = 0x10;
const DEV_DISABLE: u32 = 1 << 10;
const RESET: u32 = 1 << 31;

/// Eén record in de `devices`-tabel: 48 bytes (m1n1 `struct pmgr_device`,
/// packed).
const REC: usize = 48;
const OFF_FLAGS: usize = 0;
const OFF_PARENTS: usize = 4;
const OFF_ADDR: usize = 10;
const OFF_PSIDX: usize = 11;
const OFF_ID: usize = 26;
const OFF_NAME: usize = 32;
/// m1n1 `PMGR_DIE_OFFSET`.
const DIE_STEP: u64 = 0x20_0000_0000;

const _: () = assert!(OFF_NAME + 16 == REC);

/// De wachttijd op een standwissel.
const POLL_NS: u64 = 10_000_000;

fn le16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2)
        .map_or(0, |w| u16::from_le_bytes([w[0], w[1]]))
}

fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// De uitgepakte tabel; alle slices wijzen in de ADT zelf.
pub(crate) struct Pmgr<'a> {
    t: Adt<'a>,
    chain: Chain,
    ps: &'a [u8],
    devs: &'a [u8],
}

impl<'a> Pmgr<'a> {
    /// Leest de tabel uit de boom.
    pub(crate) fn open(t: Adt<'a>) -> Option<Self> {
        let chain = t.trace(crate::fwinfo::pmgr_path())?;
        let n = chain.node();
        let ps = t.prop(n, "ps-regs").filter(|p| !p.is_empty())?;
        let devs = t.prop(n, "devices").filter(|d| d.len() >= 2 * REC)?;
        Some(Self { t, chain, ps, devs })
    }

    fn rec(&self, i: usize) -> Option<&'a [u8]> {
        self.devs.get(i * REC..(i + 1) * REC)
    }

    fn records(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        (0..self.devs.len() / REC).filter_map(|i| self.rec(i))
    }

    /// Het adres van registerbank `idx`.
    fn psreg(&self, idx: usize) -> Option<u64> {
        let e = self.ps.get(idx * 12..idx * 12 + 12)?;
        let (reg, off) = (le32(e, 0), le32(e, 4));
        let (base, _) = self.t.reg_at(&self.chain, reg as usize)?;
        Some(base + u64::from(off))
    }

    /// Het statusregister van een record op een die.
    fn addr(&self, die: u64, r: &[u8]) -> Option<u64> {
        let bank = self.psreg(usize::from(*r.get(OFF_PSIDX)?))?;
        Some(bank + die * DIE_STEP + (u64::from(*r.get(OFF_ADDR)?) << 3))
    }

    fn find(&self, id: u16) -> Option<&'a [u8]> {
        self.records().find(|r| le16(r, OFF_ID) == id)
    }

    fn is_virtual(r: &[u8]) -> bool {
        r.get(OFF_FLAGS).is_some_and(|f| f & FLAG_VIRTUAL != 0)
    }

    /// Eén device in stand `mode`, ouders eerst bij aanzetten en laatst bij
    /// uitzetten. Virtuele devices hebben geen register.
    fn set_mode(&self, die: u64, id: u16, mode: u32, depth: u32) -> bool {
        if id == 0 || depth > 8 {
            return false;
        }
        let Some(r) = self.find(id) else { return false };
        let real = !Self::is_virtual(r);
        if mode == 0 && real && !self.addr(die, r).is_some_and(|a| set_state(a, mode)) {
            return false;
        }
        for p in [le16(r, OFF_PARENTS), le16(r, OFF_PARENTS + 2)] {
            if p != 0 && !self.set_mode(die, p, mode, depth + 1) {
                return false;
            }
        }
        if mode != 0 && real && !self.addr(die, r).is_some_and(|a| set_state(a, mode)) {
            return false;
        }
        true
    }

    /// Zet het power-domein van node `n` aan (`clock-gates`), met zijn
    /// ouders. Geeft het aantal devices dat aanging.
    pub(crate) fn power_on(&self, n: Node) -> usize {
        let Some(gates) = self.t.prop(n, "clock-gates") else {
            return 0;
        };
        (0..gates.len() / 4)
            .map(|i| le32(gates, 4 * i))
            .filter(|&v| self.set_mode(u64::from(v >> 28), v as u16, PS_ACTIVE, 0))
            .count()
    }

    /// Reset de devices met deze namen: uit, reset aan, even wachten, en in
    /// omgekeerde volgorde terug. OP NAAM, niet via `clock-gates`:
    /// `/arm-io/ans` HEEFT die eigenschap niet (GEMETEN 30-08), en m1n1 doet
    /// het daarom ook op naam. Op de M4 heet het tweede domein **ANS-V**
    /// (GEMETEN 30-08: `ANS ps=0x380700538`, `ANS-V ps=0x380700000`); de
    /// m1n1-namen alleen raakten er één.
    pub(crate) fn reset_named(&self, names: &[&str]) -> usize {
        let mut done = 0;
        for r in self.records() {
            let name = r.get(OFF_NAME..OFF_NAME + 16).unwrap_or_default();
            let len = name.iter().position(|&c| c == 0).unwrap_or(16);
            let name = name.get(..len).unwrap_or_default();
            if !names.iter().any(|n| n.as_bytes() == name) || Self::is_virtual(r) {
                continue;
            }
            let Some(a) = self.addr(0, r).map(Pa) else {
                continue;
            };
            // Wat uit staat, resetten we niet (m1n1 weigert het ook).
            if (dev::read32(a) >> 4) & 0xf != PS_ACTIVE {
                continue;
            }
            dev::write32(a, dev::read32(a) | DEV_DISABLE);
            dev::write32(a, dev::read32(a) | RESET);
            dev::delay(now, 10_000);
            dev::write32(a, dev::read32(a) & !RESET);
            dev::write32(a, dev::read32(a) & !DEV_DISABLE);
            done += 1;
        }
        done
    }
}

/// Schrijft de gewenste stand en wacht tot het blok hem MELDT: de
/// terugmelding staat in een ander veld (PS_ACTUAL, 7:4) dan het doel.
fn set_state(a: u64, mode: u32) -> bool {
    let a = Pa(a);
    let v = dev::read32(a) & !(AUTO_ENABLE | WAS_CLK_GATED | WAS_PWR_GATED | PS_TARGET);
    dev::write32(a, v | mode);
    dev::poll_until(now, POLL_NS, || (dev::read32(a) >> 4) & 0xf == mode)
}

/// Past de tunables `prop` van node `n` toe op `base`: 24 bytes per regel
/// (m1n1 `struct tunable_local`: offset u32, breedte u32, masker u64,
/// waarde u64). Ze staan in de boom omdat ze per silicium-revisie
/// verschillen. Geeft het aantal regels, of `None` als de lijst er niet is
/// (geen fout: niet elke revisie heeft elke lijst).
pub(crate) fn tunables(t: &Adt<'_>, n: Node, prop: &str, base: u64) -> Option<usize> {
    const LINE: usize = 24;
    let v = t
        .prop(n, prop)
        .filter(|v| !v.is_empty() && v.len() % LINE == 0)?;
    let mut count = 0;
    for r in v.chunks_exact(LINE) {
        let off = u64::from(le32(r, 0));
        let width = le32(r, 4);
        let mask = u64::from(le32(r, 8)) | u64::from(le32(r, 12)) << 32;
        let val = u64::from(le32(r, 16)) | u64::from(le32(r, 20)) << 32;
        let a = Pa(base + off);
        match width {
            1 => dev::write8(a, (dev::read8(a) & !(mask as u8)) | val as u8),
            2 => dev::write16(a, (dev::read16(a) & !(mask as u16)) | val as u16),
            4 => dev::write32(a, (dev::read32(a) & !(mask as u32)) | val as u32),
            8 => dev::write64(a, (dev::read64(a) & !mask) | val),
            _ => continue,
        }
        count += 1;
    }
    Some(count)
}

/// Reset de opslag-coprocessor ("ANS", "ANS2", "ANS-V"). Geeft het aantal
/// geresette domeinen (0 = geen tabel of niets actief).
pub fn reset_ans() -> usize {
    crate::fwinfo::adt()
        .and_then(Pmgr::open)
        .map_or(0, |p| p.reset_named(&["ANS", "ANS2", "ANS-V"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunables_apply_mask_and_value() {
        // Een registerblok van 64 bytes en een lijst van twee regels, via een
        // echte ADT-node.
        let mut regs = vec![0xffu8; 64];
        let base = regs.as_mut_ptr() as u64;
        let mut line = Vec::new();
        for (off, width, mask, val) in [(0u32, 4u32, 0xf0u64, 0x30u64), (8, 1, 0x0f, 0x05)] {
            line.extend(off.to_le_bytes());
            line.extend(width.to_le_bytes());
            line.extend(mask.to_le_bytes());
            line.extend(val.to_le_bytes());
        }
        let mut blob = Vec::new();
        blob.extend(2u32.to_le_bytes());
        blob.extend(0u32.to_le_bytes());
        for (k, v) in [("name", b"device-tree\0".to_vec()), ("t", line)] {
            let mut nb = [0u8; 32];
            nb[..k.len()].copy_from_slice(k.as_bytes());
            blob.extend(nb);
            blob.extend((v.len() as u32).to_le_bytes());
            blob.extend(&v);
            while blob.len() % 4 != 0 {
                blob.push(0);
            }
        }
        let t = Adt::new(&blob).unwrap();
        assert_eq!(tunables(&t, Node::ROOT, "t", base), Some(2));
        assert_eq!(
            u32::from_le_bytes(regs[0..4].try_into().unwrap()),
            0xffff_ff3f
        );
        assert_eq!(regs[8], 0xf5);
        assert_eq!(tunables(&t, Node::ROOT, "absent", base), None);
        assert_eq!(reset_ans(), 0);
    }
}
