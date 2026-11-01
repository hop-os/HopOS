//! `mdio_test.go` en `rtl8211f_test.go`, geport: PHY's op de tafel.

use super::rtl8211f::{self, *};
use super::*;
use std::collections::HashMap;
use std::vec::Vec;

/// Een clause-22-PHY op de tafel: leest uit een registermap en schrijft mee
/// wat de driver aanraakte.
#[derive(Default)]
struct FakePhy {
    reg: HashMap<u8, u16>,
    written: Vec<u8>,
    read: Vec<u8>,
}

impl Mdio for FakePhy {
    fn read(&mut self, _phy: u8, reg: u8) -> Result<u16> {
        self.read.push(reg);
        Ok(self.reg.get(&reg).copied().unwrap_or(0))
    }
    fn write(&mut self, _phy: u8, reg: u8, val: u16) -> Result {
        self.written.push(reg);
        // BMSR is read-only: de schrijf van de driver verandert hem niet.
        if reg != reg::BMSR {
            self.reg.insert(reg, val);
        }
        Ok(())
    }
}

/// Een PHY die meteen link en AN-complete meldt.
fn linked(lpa: u16, gsta: u16) -> FakePhy {
    let mut p = FakePhy::default();
    p.reg.insert(reg::BMSR, BMSR_AN_COMPLETE | BMSR_LINK);
    p.reg.insert(reg::ANLPAR, lpa);
    p.reg.insert(reg::GBSR, gsta);
    p
}

#[test]
fn autoneg_gigabit() {
    let mut p = linked(LPA_100_FD, GBSR_LP_1000_FD);
    start_autoneg(&mut p, 0, true).unwrap();
    assert!(p.written.contains(&reg::GBCR), "GBCR not advertised");
    assert_eq!(p.reg[&reg::BMCR], BMCR_AN_ENABLE | BMCR_AN_RESTART);
    let l = link(&mut p, 0, true).unwrap().unwrap();
    assert_eq!(
        l,
        Link {
            mbps: 1000,
            full_duplex: true
        }
    );
    assert_eq!(l.to_string(), "1000 Mbps full duplex");
}

/// De ePHY van de SG2002 kan alleen 10/100 en heeft register 9/10 niet:
/// niets schrijven wat er niet is, en een (onmogelijke) 1000-melding in GBSR
/// mag de uitkomst niet bepalen.
#[test]
fn fast_phy_leaves_gigabit_registers_alone() {
    let mut p = linked(LPA_100_FD, GBSR_LP_1000_FD);
    start_autoneg(&mut p, 0, false).unwrap();
    let l = link(&mut p, 0, false).unwrap().unwrap();
    assert_eq!((l.mbps, l.full_duplex), (100, true));
    assert!(!p.written.contains(&reg::GBCR));
    assert!(!p.read.contains(&reg::GBSR));
}

#[test]
fn speed_from_anlpar() {
    for (lpa, mbps, fd) in [
        (LPA_100_FD, 100, true),
        (LPA_100_HD, 100, false),
        (LPA_10_FD, 10, true),
        (0, 10, false),
    ] {
        let mut p = linked(lpa, 0);
        let l = link(&mut p, 0, false).unwrap().unwrap();
        assert_eq!((l.mbps, l.full_duplex), (mbps, fd), "lpa {lpa:#x}");
    }
}

/// Geen link is geen fout maar `None`, na één lees van BMSR: de wacht is
/// van de taak. Link zonder autonegotiatie klaar telt ook niet.
#[test]
fn no_link_is_none_after_one_look() {
    let mut p = FakePhy::default();
    assert_eq!(link(&mut p, 0, true), Ok(None));
    assert_eq!(p.read, [reg::BMSR]);
    p.reg.insert(reg::BMSR, BMSR_LINK);
    assert_eq!(link(&mut p, 0, true), Ok(None));
}

/// Een bus met meerdere adressen.
struct Bus {
    ids: HashMap<u8, (u16, u16)>,
}

impl Mdio for Bus {
    fn read(&mut self, phy: u8, reg: u8) -> Result<u16> {
        let Some(&(a, b)) = self.ids.get(&phy) else {
            return Ok(0xffff);
        };
        Ok(if reg == reg::ID1 { a } else { b })
    }
    fn write(&mut self, _: u8, _: u8, _: u16) -> Result {
        Ok(())
    }
}

#[test]
fn scan_skips_empty_addresses() {
    let mut b = Bus {
        ids: HashMap::from([(0, (0xffff, 0xffff)), (1, (0, 0)), (2, (0x0043, 0x5649))]),
    };
    assert_eq!(
        scan(&mut b),
        Some(Phy {
            addr: 2,
            id1: 0x0043,
            id2: 0x5649
        })
    );
    let mut empty = Bus {
        ids: HashMap::new(),
    };
    assert_eq!(scan(&mut empty), None);
}

/// Een PHY met pagina's: een vergeten pagina-wissel wordt zichtbaar.
#[derive(Default)]
struct PagedPhy {
    page: u16,
    regs: HashMap<(u16, u8), u16>,
}

impl Mdio for PagedPhy {
    fn read(&mut self, _: u8, reg: u8) -> Result<u16> {
        if reg == PAGE_SELECT {
            return Ok(self.page);
        }
        Ok(self.regs.get(&(self.page, reg)).copied().unwrap_or(0))
    }
    fn write(&mut self, _: u8, reg: u8, val: u16) -> Result {
        if reg == PAGE_SELECT {
            self.page = val;
        } else {
            self.regs.insert((self.page, reg), val);
        }
        Ok(())
    }
}

/// De beginstand die op 06-08 een link gaf waarover niets aankwam: rx aan,
/// tx uit.
#[test]
fn rtl8211f_sets_both_delays_for_rgmii_id() {
    let mut f = PagedPhy::default();
    f.regs.insert((PAGE_DELAY, REG_RX_DELAY), BIT_RX_DELAY);
    rtl8211f::configure(&mut f, 1, true, true).unwrap();
    assert_eq!(f.page, PAGE_STD, "PHY left on the delay page");
    assert_ne!(f.regs[&(PAGE_DELAY, REG_TX_DELAY)] & BIT_TX_DELAY, 0);
    assert_ne!(f.regs[&(PAGE_DELAY, REG_RX_DELAY)] & BIT_RX_DELAY, 0);
    assert_eq!(rtl8211f::delays(&mut f, 1).unwrap(), (true, true));
    assert_eq!(f.page, PAGE_STD);
}

#[test]
fn rtl8211f_clears_delays_and_keeps_other_bits() {
    let mut f = PagedPhy::default();
    f.regs
        .insert((PAGE_DELAY, REG_TX_DELAY), BIT_TX_DELAY | 0x00ff);
    f.regs
        .insert((PAGE_DELAY, REG_RX_DELAY), BIT_RX_DELAY | 0xff00);
    rtl8211f::configure(&mut f, 1, false, false).unwrap();
    assert_eq!(f.regs[&(PAGE_DELAY, REG_TX_DELAY)], 0x00ff);
    assert_eq!(f.regs[&(PAGE_DELAY, REG_RX_DELAY)], 0xff00);
}

/// Een bus die op het TX-register faalt: de pagina moet toch terug.
struct Flaky(PagedPhy);

impl Mdio for Flaky {
    fn read(&mut self, phy: u8, reg: u8) -> Result<u16> {
        if reg == REG_TX_DELAY {
            return Err(Error::Bus { phy, reg });
        }
        self.0.read(phy, reg)
    }
    fn write(&mut self, phy: u8, reg: u8, val: u16) -> Result {
        self.0.write(phy, reg, val)
    }
}

#[test]
fn rtl8211f_returns_to_page_zero_on_error() {
    let mut f = Flaky(PagedPhy::default());
    assert_eq!(
        rtl8211f::configure(&mut f, 1, true, true),
        Err(Error::Bus {
            phy: 1,
            reg: REG_TX_DELAY
        })
    );
    assert_eq!(f.0.page, PAGE_STD);
    assert!(rtl8211f::delays(&mut f, 1).is_err());
    assert_eq!(f.0.page, PAGE_STD);
}

#[test]
fn rtl8211f_is_recognised_by_its_measured_id() {
    assert!(is_rtl8211f(0x001c, 0xc916));
    assert!(!is_rtl8211f(0x0007, 0xc0f0)); // de Broadcom van de Pi 5
}

#[test]
fn modify_keeps_unmasked_bits() {
    let mut p = FakePhy::default();
    p.reg.insert(reg::BMCR, 0x0f0f);
    p.modify(0, reg::BMCR, 0x000f, 0x0010).unwrap();
    assert_eq!(p.reg[&reg::BMCR], 0x0f10);
}
