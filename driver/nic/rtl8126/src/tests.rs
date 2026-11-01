//! De driver tegen een nep-chip: het registerblok en de DMA-regio in RAM,
//! en de klok van de test speelt het silicium. Bij elke blik op de klok
//! wist de chip zijn reset-bit, beantwoordt hij PHY-OCP- en EPHY-verzoeken
//! uit een eigen registermap, en komt de link op na een autoneg-herstart.
//! MAC-OCP heeft geen handdruk (de driver leest direct na de schrijf), dus
//! die ruimte leest hier nul: de tests bewijzen de volgorde en de ringen,
//! niet Realteks parameters.

use super::*;
use netdev::{Device as _, IrqAck as _};
use std::cell::RefCell;
use std::collections::HashMap;
use std::vec;
use std::vec::Vec;

#[derive(Default)]
struct Chip {
    regs: Pa,
    phy: HashMap<u16, u16>,
    /// De PHY-OCP-schrijven, in volgorde.
    phy_writes: Vec<(u16, u16)>,
    ephy_writes: Vec<(u16, u16)>,
    /// Het laatste lees-antwoord (zie `now`).
    gphy_answer: u32,
    ephy_answer: u32,
    /// De link komt op na een AN-herstart.
    link: bool,
    now: u64,
}

thread_local! {
    static CHIP: RefCell<Chip> = RefCell::new(Chip::default());
}

fn at(regs: Pa, off: u64) -> Pa {
    regs.add(off)
}

fn now() -> u64 {
    CHIP.with(|c| {
        let mut c = c.borrow_mut();
        c.now += 10_000;
        let regs = c.regs;
        if regs.0 == 0 {
            return c.now;
        }
        // CmdReset wist zichzelf.
        let cmd = at(regs, 0x37);
        dev::write8(cmd, dev::read8(cmd) & !CMD_RESET);
        // PHY-OCP: een schrijf (vlag hoog) wordt uitgevoerd en de vlag
        // zakt; een lees (vlag laag) krijgt de vlag en de data. Het adres
        // staat in [30:16] als reg / 2: `reg << 15` met een even reg, en
        // bit 15 is dan van de data (zoals bij Linux).
        //
        // De chip kijkt bij elke klokslag. Een lees-antwoord dat blijft
        // staan (de driver slaapt), leest hij de volgende keer als een
        // schrijf van dezelfde waarde; die telt niet mee in het logboek.
        let g = at(regs, 0xb8);
        let v = dev::read32(g);
        let reg = (((v >> 16) & 0x7fff) << 1) as u16;
        let out = if v & OCP_FLAG != 0 {
            let mut val = (v & 0xffff) as u16;
            if v != c.gphy_answer {
                c.phy_writes.push((reg, val));
            }
            if reg == phy_c22(0) {
                // BMCR: de reset wist zichzelf, een AN-herstart geeft de
                // link (als die er is).
                if val & mdio::BMCR_AN_RESTART != 0 && val & BMCR_RESET == 0 && c.link {
                    c.phy
                        .insert(phy_c22(1), mdio::BMSR_LINK | mdio::BMSR_AN_COMPLETE);
                    c.phy.insert(PHY_PHYSR, 0x0220 | 8);
                }
                val &= !BMCR_RESET;
            }
            c.phy.insert(reg, val);
            v & !OCP_FLAG
        } else {
            let a = OCP_FLAG | (v & 0x7fff_0000) | u32::from(*c.phy.get(&reg).unwrap_or(&0));
            c.gphy_answer = a;
            a
        };
        dev::write32(g, out);
        // EPHY: dezelfde handdruk op 0x80, adres in [20:16].
        let e = at(regs, 0x80);
        let v = dev::read32(e);
        let ereg = ((v >> 16) & 0x1f) as u16;
        let out = if v & 0x8000_0000 != 0 {
            if v != c.ephy_answer {
                c.ephy_writes.push((ereg, (v & 0xffff) as u16));
            }
            v & !0x8000_0000
        } else {
            let a = 0x8000_0000 | (v & 0x001f_0000) | 0x1234;
            c.ephy_answer = a;
            a
        };
        dev::write32(e, out);
        c.now
    })
}

struct Mem {
    _regs: Vec<u64>,
    _dma: Vec<u64>,
    base: Pa,
    dma: Pa,
}

const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

/// Een chip met XID `xid`, een MAC in MAC0_BKP, en de FIFO- en
/// link-list-bits die hij na de firmware meldt.
fn chip(xid: u32, link: bool) -> Mem {
    let mut regs = vec![0u64; MMIO_LEN as usize / 8 + 1];
    let mut dma = vec![0u64; (DMA_NEED + BUF_OFF) as usize / 8];
    let base = Pa(regs.as_mut_ptr() as usize as u64);
    let dma_pa = Pa((dma.as_mut_ptr() as usize as u64).next_multiple_of(BUF_OFF));
    dev::write32(at(base, 0x40), xid << 20);
    dev::write8(at(base, 0xd3), MCU_RXTX_EMPTY | MCU_LINK_LIST_OK);
    dev::write16(at(base, 0xe2), 0x0103);
    dev::copy_in(at(base, 0x19e0), &MAC);
    CHIP.with(|c| {
        *c.borrow_mut() = Chip {
            regs: base,
            link,
            ..Chip::default()
        };
    });
    Mem {
        _regs: regs,
        _dma: dma,
        base,
        dma: dma_pa,
    }
}

fn up(m: &Mem) -> Rtl8126 {
    // SAFETY: registers en DMA liggen in `m`, dat de test overleeft.
    unsafe { Rtl8126::new(m.base, m.dma, DMA_NEED, now) }.unwrap()
}

#[test]
fn new_brings_the_mac_up_with_the_rings_in_place() {
    let m = chip(0x649, false);
    let n = up(&m);
    assert_eq!((n.name(), n.xid()), ("RTL8126A", 0x649));
    assert_eq!(n.mac(), Mac(MAC));
    let r = |off| dev::read32(at(m.base, off));
    assert_eq!(r(0xe4), m.dma.0 as u32, "RX ring low");
    assert_eq!(r(0xe8), (m.dma.0 >> 32) as u32, "RX ring high");
    assert_eq!(r(0x20), (m.dma.0 + 0x1000) as u32, "TX ring low");
    assert_eq!(dev::read16(at(m.base, 0xda)), BUF_SIZE as u16);
    assert_eq!(
        dev::read8(at(m.base, 0x37)),
        CMD_TX_ENB | CMD_RX_ENB,
        "RX and TX on"
    );
    assert_eq!(r(0x44), RX_CFG_BASE | RX_ACCEPT_DEFAULT);
    assert_eq!(r(0x40), TX_CFG);
    assert_eq!((r(0x08), r(0x0c)), (u32::MAX, u32::MAX), "all multicast");
    assert_eq!(r(0x38), 0, "polled until set_irq");
    assert_eq!(r(0xf0) & MISC_RXDV_GATED, 0, "RX path open");
    assert_eq!(dev::read8(at(m.base, 0x50)), CFG_LOCK);
    // Het MAC ging ook terug in MAC0/MAC4 (rtl_rar_set).
    assert_eq!(r(0x00), u32::from_le_bytes([2, 0x11, 0x22, 0x33]));
    assert_eq!(r(0x04), u32::from_le_bytes([0x44, 0x55, 0, 0]));
    // De RX-ring: elke descriptor is van de NIC, de laatste met RingEnd.
    let d = |i: u64| dev::read32(m.dma.add(i * 16));
    assert_eq!(d(0), DESC_OWN | BUF_SIZE as u32);
    assert_eq!(d(255), DESC_OWN | RING_END | BUF_SIZE as u32);
    assert_eq!(
        dev::read32(m.dma.add(8)),
        (m.dma.0 + BUF_OFF) as u32,
        "own buffer"
    );
    // De TX-ring: leeg, de laatste met RingEnd.
    assert_eq!(dev::read32(m.dma.add(0x1000 + 63 * 16)), RING_END);
}

#[test]
fn refusals_are_named() {
    // SAFETY: de toets faalt vóór er een register wordt aangeraakt.
    let e = unsafe { Rtl8126::new(Pa(0), Pa(0x1000), DMA_NEED, now) }.err();
    assert_eq!(
        e,
        Some(Error::Dma {
            base: 0x1000,
            size: DMA_NEED
        })
    );
    let m = chip(0x123, false);
    // SAFETY: registers en DMA liggen in `m`.
    let e = unsafe { Rtl8126::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::UnknownChip { xid: 0x103 }));
    let m = chip(0x649, false);
    dev::write32(at(m.base, 0x40), u32::MAX);
    // SAFETY: zie hierboven.
    let e = unsafe { Rtl8126::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::OffBus));
    let m = chip(0x649, false);
    dev::copy_in(at(m.base, 0x19e0), &[0; 6]);
    // SAFETY: zie hierboven.
    let e = unsafe { Rtl8126::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::NoMac));
    let m = chip(0x649, false);
    dev::write8(at(m.base, 0xd3), 0); // de FIFO's worden nooit leeg
    // SAFETY: zie hierboven.
    let e = unsafe { Rtl8126::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert!(
        matches!(
            e,
            Some(Error::Timeout {
                what: "rx/tx fifo empty",
                ..
            })
        ),
        "{e:?}"
    );
}

#[test]
fn the_mac0_fallback_is_used_when_the_backup_is_empty() {
    let m = chip(0x688, false);
    dev::copy_in(at(m.base, 0x19e0), &[0xff; 6]); // multicast: ongeldig
    dev::write32(at(m.base, 0x00), u32::from_le_bytes([0x02, 9, 8, 7]));
    dev::write32(at(m.base, 0x04), u32::from_le_bytes([6, 5, 0, 0]));
    let n = up(&m);
    assert_eq!(n.mac(), Mac([0x02, 9, 8, 7, 6, 5]));
    assert_eq!(n.name(), "RTL8125D");
}

#[test]
fn link_up_on_the_8126a_advertises_5g_and_reads_physr() {
    let m = chip(0x64a, true);
    let mut n = up(&m);
    let l = n.link_up(5_000_000_000).unwrap();
    assert_eq!(
        l,
        Link {
            mbps: 5000,
            full_duplex: true
        }
    );
    let writes = CHIP.with(|c| c.borrow().phy_writes.clone());
    assert!(
        writes.contains(&(PHY_NBASET, 0x0180)),
        "2.5G + 5G advertised"
    );
    assert!(writes.contains(&(phy_c22(9), mdio::GBCR_1000_FD)));
    // Geen variant-stappen op de 8126A: niets naar de param-poorten.
    assert!(!writes.iter().any(|&(r, _)| r == 0xa436 || r == 0xb87c));
}

#[test]
fn the_8125b_runs_its_ephy_table_and_phy_steps() {
    let m = chip(0x641, true);
    let mut n = up(&m);
    assert_eq!(n.name(), "RTL8125B");
    let ephy = CHIP.with(|c| c.borrow().ephy_writes.clone());
    // Zes EPHY-regels, met masker 0x1f op het adres (mainline letterlijk).
    assert_eq!(ephy.len(), 6);
    assert_eq!(ephy[0], (0x0b, 0xa908));
    assert_eq!(ephy[2], (0x4b & 0x1f, 0xa908));
    assert_eq!(ephy[4], (0x22 & 0x1f, (0x1234 & !0x0030) | 0x0020));
    let l = n.link_up(5_000_000_000).unwrap();
    assert_eq!(l.mbps, 5000); // de nep-PHYSR; op ijzer zegt de 8125B 2500
    let writes = CHIP.with(|c| c.borrow().phy_writes.clone());
    let params: Vec<u16> = writes
        .iter()
        .filter(|&&(r, _)| r == 0xa436)
        .map(|&(_, v)| v)
        .collect();
    assert_eq!(params.len(), 10);
    assert_eq!((params[0], params[9]), (0x8044, 0x807a));
    assert!(writes.contains(&(PHY_NBASET, 0x0080)), "2.5G only");
    assert!(writes.contains(&(0xa432, 0x0010)), "EEE PHY bit");
}

#[test]
fn no_link_reports_bmsr_and_phystatus() {
    let m = chip(0x649, false);
    let mut n = up(&m);
    let e = n.link_up(100_000_000).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NoLink {
                bmsr: 0,
                ms: 100,
                ..
            }
        ),
        "{e:?}"
    );
}

#[test]
fn physr_decodes_every_speed() {
    for (v, mbps) in [
        (0x0000, 10),
        (0x0010, 100),
        (0x0020, 1000),
        (0x0210, 2500),
        (0x0220, 5000),
        (0x0200, 10000),
    ] {
        let l = decode_physr(v | 8).unwrap();
        assert_eq!((l.mbps, l.full_duplex), (mbps, true));
    }
    assert_eq!(decode_physr(0x0030), Err(Error::Speed(0x0030)));
    assert!(!valid_unicast([1, 0, 0, 0, 0, 0]) && !valid_unicast([0; 6]));
    assert!(supported(0x10ec, 0x8126) && supported(0x10ec, 0x8125));
    assert!(!supported(0x10ec, 0x8168));
    for (xid, name) in [
        (0x649, "RTL8126A"),
        (0x641, "RTL8125B"),
        (0x689, "RTL8125D"),
        (0x708, "RTL8125CP"),
        (0x681, "RTL8125BP"),
    ] {
        assert_eq!(variant::by_xid(xid).map(|v| v.name), Some(name));
    }
    assert!(variant::by_xid(0x609).is_none(), "the old 8125A is out");
}

/// Een driver zonder `new` op nep-geheugen, zoals de Go-test.
fn fake(m: &Mem) -> Rtl8126 {
    let n = Rtl8126::at(m.base, m.dma, now, &variant::V8126A);
    for i in 0..N_RX {
        n.arm_rx(i);
    }
    n
}

/// `receive_bounds_test.go`: een device-lengte mag nooit bytes van de
/// buurbuffer blootgeven, en de descriptor gaat terug naar de NIC.
#[test]
fn receive_bounds_and_recycle() {
    for (size, want) in [
        (64, Some(64)),
        (BUF_SIZE, Some(BUF_SIZE)),
        (BUF_SIZE + 1, None),
    ] {
        let m = chip(0x649, false);
        let mut n = fake(&m);
        // Writeback: Own weg, First + Last, lengte met 4 bytes FCS.
        dev::write32(
            m.dma,
            FIRST_FRAG | LAST_FRAG | ((size as u32 + 4) & RX_LEN_MASK),
        );
        dev::copy_in(m.dma.add(BUF_OFF), &vec![0x5a; BUF_SIZE]);
        let mut out = vec![0xccu8; 8192];
        let got = n.receive(&mut out);
        assert_eq!(got, want, "size {size}");
        let k = got.unwrap_or(0);
        assert!(out[..k].iter().all(|&b| b == 0x5a));
        assert!(out[k..].iter().all(|&b| b == 0xcc), "beyond the packet");
        assert_eq!(n.rx_head, 1);
        assert_ne!(dev::read32(m.dma) & DESC_OWN, 0, "handed back to the NIC");
    }
}

#[test]
fn receive_drops_errors_and_fragments() {
    let m = chip(0x649, false);
    let mut n = fake(&m);
    dev::write32(m.dma, FIRST_FRAG | LAST_FRAG | RX_RES | 100);
    dev::write32(m.dma.add(16), FIRST_FRAG | 100); // geen LastFrag
    dev::write32(m.dma.add(32), FIRST_FRAG | LAST_FRAG | 64);
    assert_eq!(n.receive(&mut [0; 4096]), Some(60));
    assert_eq!((n.rx_head, n.stats.rx_bad), (3, 2));
    // Leeg: de overflow-latches gaan weg, de andere bits blijven.
    dev::write32(at(m.base, 0x3c), INT_RX_OVERFLOW | INT_RX_OK);
    assert_eq!(n.receive(&mut [0; 4096]), None);
    // (Nep-geheugen kent geen W1C: de schrijf zelf is wat we toetsen.)
    assert_eq!(dev::read32(at(m.base, 0x3c)), INT_RX_OVERFLOW);
}

#[test]
fn transmit_pads_batches_and_honours_ownership() {
    let m = chip(0x649, false);
    let mut n = fake(&m);
    let tx = |i: u64| m.dma.add(0x1000 + i * 16);
    n.transmit(&[7; 20]).unwrap();
    assert_eq!(
        dev::read32(tx(0)),
        DESC_OWN | FIRST_FRAG | LAST_FRAG | 60,
        "padded to 60"
    );
    let buf = m.dma.add(BUF_OFF + u64::from(N_RX) * BUF_SIZE as u64);
    let mut out = [0u8; 60];
    dev::copy_out(&mut out, buf);
    assert!(out[..20].iter().all(|&b| b == 7) && out[20..].iter().all(|&b| b == 0));
    assert_eq!(n.stats.doorbells, 0, "TxPoll waits for flush");
    n.flush();
    assert_eq!(n.stats.doorbells, 1);
    assert_eq!(dev::read16(at(m.base, 0x90)), 1);
    n.flush();
    assert_eq!(n.stats.doorbells, 1, "nothing new, no doorbell");
    for _ in 1..N_TX {
        n.transmit(&[1; 100]).unwrap();
    }
    assert_ne!(
        dev::read32(tx(63)) & RING_END,
        0,
        "last descriptor ends the ring"
    );
    // Descriptor 0 is nog van de NIC: vol, en de driver belt nog eens en
    // wist de TX-bits, zodat de volgende TxOK een flank geeft.
    dev::write32(at(m.base, 0x3c), 0);
    assert_eq!(n.transmit(&[1; 100]), Err(TxError::Full));
    assert_eq!(n.stats.doorbells, 2);
    assert_eq!(dev::read32(at(m.base, 0x3c)), INT_TX, "W1C of TxOK, TxErr");
    dev::write32(tx(0), 0); // de NIC gaf hem terug
    n.transmit(&[1; 100]).unwrap();
    assert_eq!(n.transmit(&[]), Err(TxError::Size(0)));
}

#[test]
fn the_irq_masks_on_ack_and_rearms_on_an_empty_ring() {
    static BELL: Signal = Signal::new();
    let m = chip(0x649, false);
    let mut n = fake(&m);
    n.set_irq(&BELL);
    assert_eq!(dev::read32(at(m.base, 0x38)), INT_RX | INT_TX, "mask open");
    let ack = n.irq_ack();
    dev::write32(at(m.base, 0x3c), INT_RX_OK);
    ack.ack();
    assert_eq!(dev::read32(at(m.base, 0x38)), 0, "mask shut after the ack");
    assert_eq!(dev::read32(at(m.base, 0x3c)), u32::MAX, "W1C of every bit");
    // Een flush opent niets: dat doet pas een lege ring.
    n.flush();
    assert_eq!(dev::read32(at(m.base, 0x38)), 0, "flush leaves the mask");
    // Een frame op de ring: de pomp krijgt het, het masker blijft dicht.
    dev::write32(m.dma, FIRST_FRAG | LAST_FRAG | 64);
    assert_eq!(n.receive(&mut [0; 4096]), Some(60));
    assert_eq!(dev::read32(at(m.base, 0x38)), 0, "not drained yet");
    // Leeg: het masker gaat weer open.
    assert_eq!(n.receive(&mut [0; 4096]), None);
    assert_eq!(dev::read32(at(m.base, 0x38)), INT_RX | INT_TX);
    assert!(!BELL.take(), "the driver never rings the bell itself");
}
