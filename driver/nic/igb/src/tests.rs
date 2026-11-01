//! De driver op nep-geheugen: registers en DMA-regio in RAM, en de klok
//! van de test speelt de hardware (reset wist zichzelf, de MDIC wordt
//! klaar, de link komt op), zoals de tests van virtio-blk.

use super::*;
use netdev::{Device as _, IrqAck as _};
use std::cell::RefCell;
use std::vec;
use std::vec::Vec;
use sync::Signal;

/// Wat de nep-hardware doet als de driver op de klok kijkt.
#[derive(Default, Clone, Copy)]
struct Hw {
    regs: Pa,
    /// CTRL.RST wist zichzelf.
    reset_clears: bool,
    /// De link komt op na de eerste MDIC-schrijf.
    link: bool,
    /// RXDCTL.ENABLE blijft niet staan.
    rx_queue_stuck: bool,
    now: u64,
}

thread_local! {
    static HW: RefCell<Hw> = RefCell::new(Hw::default());
}

fn reg(off: usize) -> Pa {
    HW.with(|h| h.borrow().regs).add(off as u64)
}

fn now() -> u64 {
    HW.with(|h| {
        let mut h = h.borrow_mut();
        h.now += 1_000_000;
        if h.regs.0 == 0 {
            return h.now;
        }
        let ctrl = h.regs.add(offset_of!(Regs, ctrl) as u64);
        if h.reset_clears {
            dev::write32(ctrl, dev::read32(ctrl) & !CTRL_RST);
        }
        if h.rx_queue_stuck {
            dev::write32(h.regs.add(offset_of!(Regs, rxdctl) as u64), 0);
        }
        let mdic = h.regs.add(offset_of!(Regs, mdic) as u64);
        let m = dev::read32(mdic);
        if m & (MDIC_OP_READ | MDIC_OP_WRITE) != 0 && m & MDIC_READY == 0 {
            let data = if m & MDIC_OP_READ != 0 {
                0x796d
            } else {
                m & 0xffff
            };
            dev::write32(mdic, (m & !0xffff) | data | MDIC_READY);
            if h.link && m & MDIC_OP_WRITE != 0 {
                let st = h.regs.add(offset_of!(Regs, status) as u64);
                dev::write32(st, STATUS_LU | STATUS_FD | (0b10 << STATUS_SPEED_SHIFT));
            }
        }
        h.now
    })
}

/// Registers en een DMA-regio die op 2 MB begint.
struct Mem {
    _regs: Vec<u64>,
    _dma: Vec<u64>,
    base: Pa,
    dma: Pa,
}

fn mem(hw: Hw) -> Mem {
    let mut regs = vec![0u64; MMIO_LEN as usize / 8 + 1];
    let mut dma = vec![0u64; (DMA_NEED + BUF_OFF) as usize / 8];
    let base = Pa(regs.as_mut_ptr() as usize as u64);
    let raw = dma.as_mut_ptr() as usize as u64;
    let dma_pa = Pa(raw.next_multiple_of(BUF_OFF));
    HW.with(|h| *h.borrow_mut() = Hw { regs: base, ..hw });
    Mem {
        _regs: regs,
        _dma: dma,
        base,
        dma: dma_pa,
    }
}

fn with_mac(m: &Mem) {
    dev::write32(m.base.add(0x5400), 0x5634_1252);
    dev::write32(m.base.add(0x5404), 0x8000_0078);
}

fn hw() -> Hw {
    Hw {
        reset_clears: true,
        ..Hw::default()
    }
}

#[test]
fn new_resets_reads_the_mac_and_programs_the_rings() {
    let m = mem(hw());
    with_mac(&m);
    // SAFETY: registers en DMA liggen in `m`, dat de test overleeft.
    let n = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.unwrap();
    assert_eq!(n.mac(), Mac([0x52, 0x12, 0x34, 0x56, 0x78, 0x00]));
    assert_eq!(dev::read32(reg(0x2800)), m.dma.0 as u32);
    assert_eq!(dev::read32(reg(0x2804)), (m.dma.0 >> 32) as u32);
    assert_eq!(dev::read32(reg(0x2808)), 256 * 16);
    assert_eq!(dev::read32(reg(0x280c)), SRRCTL_BSIZE_2K | SRRCTL_DESC_ADV1);
    assert_eq!(dev::read32(reg(0x2818)), 255, "all but one RX descriptor");
    assert_eq!(dev::read32(reg(0x0100)), RCTL_EN | RCTL_BAM | RCTL_SECRC);
    assert_eq!(dev::read32(reg(0x3800)), (m.dma.0 + 4096) as u32);
    assert_eq!(dev::read32(reg(0x3808)), 64 * 16);
    assert_eq!(
        dev::read32(reg(0x0400)),
        TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD
    );
    assert_eq!(dev::read32(reg(0x150c)), u32::MAX, "interrupts masked");
    assert_eq!(dev::read32(reg(0x1528)), u32::MAX, "MSI-X vectors masked");
    assert_eq!(dev::read32(reg(0x1514)), 0, "GPIE untouched until set_irq");
    assert!(n.irq().is_none(), "polled until set_irq");
    // RX-descriptor 3 wijst naar zijn eigen buffer in het 2 MB-blok.
    let d3 = m.dma.add(3 * 16);
    let want = m.dma.0 + BUF_OFF + 3 * BUF_SIZE as u64;
    assert_eq!(dev::read32(d3), want as u32);
    assert_eq!(dev::read32(d3.add(8)), 0);
}

#[test]
fn a_bad_dma_region_is_refused_before_any_mmio() {
    // Basis 0 is ongemapt: een registertoegang zou de test laten crashen.
    // SAFETY: de toetsen falen vóór er een register wordt aangeraakt.
    let e = unsafe { Igb::new(Pa(0), Pa(0), DMA_NEED - 1, now) }.err();
    assert_eq!(
        e,
        Some(Error::DmaTooSmall {
            need: DMA_NEED,
            have: DMA_NEED - 1
        })
    );
    // SAFETY: zie hierboven.
    let e = unsafe { Igb::new(Pa(0), Pa(0x1000), DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::DmaAlign(0x1000)));
}

#[test]
fn reset_failures_are_named() {
    let m = mem(Hw::default()); // RST wist zichzelf niet
    with_mac(&m);
    // SAFETY: registers en DMA liggen in `m`.
    let e = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert!(matches!(e, Some(Error::ResetStuck { .. })), "{e:?}");

    let m = mem(hw()); // geen MAC
    // SAFETY: zie hierboven.
    let e = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::NoMac));

    let m = mem(hw());
    dev::write32(m.base.add(8), u32::MAX);
    // SAFETY: zie hierboven.
    let e = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::OffBus));
}

/// Go's volgorde (igb.go `Init`): RCTL en RDT pas als RXDCTL.ENABLE
/// terugleest. Blijft de queue uit, dan staat de ring nog dicht en zegt de
/// driver welke queue.
#[test]
fn rdt_waits_for_the_rx_queue_enable() {
    let m = mem(Hw {
        rx_queue_stuck: true,
        ..hw()
    });
    with_mac(&m);
    // SAFETY: registers en DMA liggen in `m`.
    let e = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::QueueStuck { tx: false }));
    assert_eq!(dev::read32(reg(0x2818)), 0, "RDT untouched");
    assert_eq!(dev::read32(reg(0x0100)), 0, "RCTL untouched");
}

#[test]
fn start_link_restarts_autoneg_through_the_mdic() {
    let m = mem(Hw { link: true, ..hw() });
    with_mac(&m);
    // SAFETY: registers en DMA liggen in `m`.
    let mut n = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.unwrap();
    n.start_link().unwrap();
    assert_eq!(n.link(), Some(1000));
    let mdic = dev::read32(reg(0x20));
    assert_eq!(mdic & 0xffff, 0x1200, "BMCR AN enable + restart");
    assert_eq!((mdic >> 21) & 0x1f, u32::from(PHY_ADDR));
    assert_ne!(dev::read32(reg(0)) & CTRL_SLU, 0);
    // Een lees over dezelfde bus.
    assert_eq!(n.read(PHY_ADDR, 2).unwrap(), 0x796d);
}

#[test]
fn no_link_is_none() {
    let m = mem(hw());
    with_mac(&m);
    // SAFETY: registers en DMA liggen in `m`.
    let mut n = unsafe { Igb::new(m.base, m.dma, DMA_NEED, now) }.unwrap();
    n.start_link().unwrap();
    assert_eq!(n.link(), None);
}

#[test]
fn status_decodes_speed_and_duplex() {
    assert_eq!(decode_status(0), None);
    for (bits, mbps) in [(0b00, 10), (0b01, 100), (0b10, 1000), (0b11, 1000)] {
        let l = decode_status(STATUS_LU | (bits << STATUS_SPEED_SHIFT)).unwrap();
        assert_eq!((l.mbps, l.full_duplex), (mbps, false));
    }
    assert!(supported(0x8086, 0x10c9) && supported(0x8086, 0x1533));
    assert!(!supported(0x10ec, 0x10c9) && !supported(0x8086, 0x1234));
}

/// Een driver zonder `new` op nep-geheugen, zoals de Go-test.
fn fake(m: &Mem) -> Igb {
    Igb::at(m.base, m.dma, now)
}

fn writeback(m: &Mem, i: u16, status: u32, len: u32) {
    let d = m.dma.add(u64::from(i) * 16);
    dev::write32(d.add(8), status);
    dev::write32(d.add(12), len);
}

/// `receive_bounds_test.go`: een device-lengte mag nooit bytes van de
/// buurbuffer blootgeven, en de descriptor gaat terug naar de hardware.
#[test]
fn receive_bounds_and_recycle() {
    for (size, want) in [
        (64, Some(64)),
        (BUF_SIZE, Some(BUF_SIZE)),
        (BUF_SIZE + 1, None),
    ] {
        let m = mem(hw());
        let mut n = fake(&m);
        n.init().unwrap();
        writeback(&m, 0, RX_DD | RX_EOP, size as u32);
        dev::copy_in(m.dma.add(BUF_OFF), &vec![0x5a; BUF_SIZE]);
        let mut out = vec![0xccu8; 8192];
        let got = n.receive(&mut out);
        assert_eq!(got, want, "size {size}");
        let k = got.unwrap_or(0);
        assert!(out[..k].iter().all(|&b| b == 0x5a));
        assert!(out[k..].iter().all(|&b| b == 0xcc), "beyond the packet");
        assert_eq!(n.rx_head, 1, "descriptor consumed");
        assert_eq!(dev::read32(m.dma.add(8)), 0, "descriptor re-armed");
        assert_eq!(n.stats.rx_bad, u64::from(want.is_none()));
        // Batching: RDT pas bij de flush, en dan op de herwapende.
        assert_eq!(dev::read32(reg(0x2818)), 255);
        n.flush();
        assert_eq!(dev::read32(reg(0x2818)), 0);
    }
}

#[test]
fn receive_skips_a_fragment_and_delivers_the_next() {
    let m = mem(hw());
    let mut n = fake(&m);
    n.init().unwrap();
    writeback(&m, 0, RX_DD, 100); // geen EOP
    writeback(&m, 1, RX_DD | RX_EOP, 60);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), Some(60));
    assert_eq!((n.rx_head, n.stats.rx_bad), (2, 1));
    assert_eq!(n.receive(&mut out), None);
}

#[test]
fn receive_rings_the_doorbell_itself_every_32_frames() {
    let m = mem(hw());
    let mut n = fake(&m);
    n.init().unwrap();
    let before = n.stats.doorbells;
    for i in 0..RX_SELF_FLUSH {
        writeback(&m, i, RX_DD | RX_EOP, 60);
        assert_eq!(n.receive(&mut [0u8; 64]), Some(60));
    }
    assert_eq!(n.stats.doorbells, before + 1);
    assert_eq!(dev::read32(reg(0x2818)), u32::from(RX_SELF_FLUSH - 1));
}

#[test]
fn transmit_batches_until_flush_and_respects_ownership() {
    let m = mem(hw());
    let mut n = fake(&m);
    n.init().unwrap();
    assert_eq!(n.transmit(&[]), Err(TxError::Size(0)));
    assert_eq!(
        n.transmit(&[0; BUF_SIZE + 1]),
        Err(TxError::Size(BUF_SIZE + 1))
    );
    n.transmit(&[1; 60]).unwrap();
    n.transmit(&[2; 60]).unwrap();
    assert_eq!(dev::read32(reg(0x3818)), 0, "no doorbell before flush");
    n.flush();
    assert_eq!(dev::read32(reg(0x3818)), 2);
    let d0 = m.tx_ring_desc(0);
    assert_eq!(
        dev::read32(d0.add(8)),
        60 | TX_DTYP_DATA | TX_EOP | TX_IFCS | TX_RS | TX_DEXT
    );
    assert_eq!(dev::read32(d0.add(12)), 60 << TX_PAY_SHIFT);
    // Een ronde verder: descriptor 0 is gebruikt en heeft geen DD, dus vol.
    for i in 2..N_TX {
        n.transmit(&[i as u8; 60]).unwrap();
    }
    assert_eq!(n.tx_head, 0);
    assert_eq!(n.transmit(&[9; 60]), Err(TxError::Full));
    // Nooit meer dan N_TX - 2 zonder doorbell: onderweg flushte hij zelf.
    assert!(n.tx_pending <= N_TX - 2);
    // De hardware verzond hem: DD in w3, en er is weer plaats.
    dev::write32(d0.add(12), TX_DD);
    n.transmit(&[9; 60]).unwrap();
}

static BELL: Signal = Signal::new();

/// `igb_configure_msix` plus `igb_irq_enable` voor één vector: de waarden
/// die Linux op een I210 met één queue schrijft.
#[test]
fn set_irq_writes_the_single_vector_msix_mode() {
    let m = mem(hw());
    let mut n = fake(&m);
    n.init().unwrap();
    // Een vector die een vorige kern voor TX-queue 0 achterliet, telt niet:
    // RX en TX van queue 0 gaan samen op vector 0. Queue 1 blijft staan.
    dev::write32(reg(0x1700), 0x8200_8100);
    n.set_irq(&BELL);
    assert_eq!(
        dev::read32(reg(0x1514)),
        0x0000_0010 | 0x8000_0000 | 0x4000_0000 | 0x0000_0001,
        "GPIE: MSIX_MODE | PBA | EIAME | NSICR"
    );
    assert_eq!(
        dev::read32(reg(0x1700)),
        0x8200_8080,
        "IVAR0: RX 0 and TX 0 on vector 0, valid"
    );
    assert_eq!(dev::read32(reg(0x152c)), 1, "EIAC");
    assert_eq!(dev::read32(reg(0x1530)), 1, "EIAM");
    assert_eq!(dev::read32(reg(0x1524)), 1, "EIMS");
    assert!(core::ptr::eq(n.irq().unwrap(), &BELL));
    assert_eq!(ivar_q0(0xffff_ffff, 3), 0xffff_8383);
    assert_eq!(ivar_q0(0, 0), 0x8080, "RX0 and TX0 on vector 0, both valid");

    // De ack sluit de vector (EIMC), de zelftest vuurt hem (EICS).
    n.irq_ack().ack();
    assert_eq!(dev::read32(reg(0x1528)), 1, "EIMC");
    n.fire_irq();
    assert_eq!(dev::read32(reg(0x1520)), 1, "EICS");
    dev::write32(reg(0x1580), 1);
    let d = n.irq_regs();
    assert_eq!((d.gpie, d.ivar0, d.eicr), (0xc000_0011, 0x8200_8080, 1));
    assert_eq!(
        d.to_string(),
        "GPIE 0xc0000011 IVAR0 0x82008080 EIMS 0x1 EICR 0x1"
    );

    // Terug naar pollen: alles dicht, geen bel.
    n.clear_irq();
    assert_eq!(dev::read32(reg(0x1528)), u32::MAX);
    assert!(n.irq().is_none());
}

/// Het ritme van de dwmac: een lege ring heropent het masker en kijkt nog
/// één keer; een volle niet, en gepold blijft EIMS onaangeroerd.
#[test]
fn an_empty_ring_reopens_the_vector_and_looks_once_more() {
    let m = mem(hw());
    let mut n = fake(&m);
    n.init().unwrap();
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(dev::read32(reg(0x1524)), 0, "polled: EIMS untouched");

    n.set_irq(&BELL);
    dev::write32(reg(0x1524), 0); // de ack/EIAM sloot hem
    writeback(&m, 0, RX_DD | RX_EOP, 60);
    assert_eq!(n.receive(&mut out), Some(60));
    assert_eq!(dev::read32(reg(0x1524)), 0, "not while frames come");
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(dev::read32(reg(0x1524)), 1, "reopened at the empty ring");
}

impl Mem {
    fn tx_ring_desc(&self, i: u16) -> Pa {
        self.dma.add(TX_RING_OFF + u64::from(i) * 16)
    }
}
