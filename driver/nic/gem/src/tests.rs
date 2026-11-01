//! Host-tests over een nep-registerblok en een nep-DMA-regio in RAM.

use super::*;
use netdev::Device;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

const BUS_OFF: u64 = 0x10_0000_0000;

struct Fake {
    regs: Vec<u64>,
    dma: Vec<u64>,
}

impl Fake {
    fn new() -> Self {
        Self {
            regs: vec![0; (MMIO_SIZE as usize).div_ceil(8)],
            dma: vec![0; DMA_NEED as usize / 8],
        }
    }
    fn base(&mut self) -> Pa {
        Pa(self.regs.as_mut_ptr() as usize as u64)
    }
    fn dma(&mut self) -> Pa {
        Pa(self.dma.as_mut_ptr() as usize as u64)
    }
    fn nic(&mut self) -> Gem {
        let (b, d) = (self.base(), self.dma());
        // SAFETY: de vectoren leven langer dan de driver in elke test.
        unsafe { Gem::new(b, BUS_OFF, d, DMA_NEED, [0x2c, 0xcf, 0x67, 1, 2, 3]) }
    }
}

const LINK_1G: Link = Link {
    mbps: 1000,
    full_duplex: true,
};

/// `init` en dan de link (zoals de taak hem na de probe ziet).
fn up(n: &mut Gem) {
    n.init().unwrap();
    n.adjust_link(LINK_1G);
}

#[test]
fn init_lays_out_rings_with_the_bus_offset() {
    let mut f = Fake::new();
    let b = f.base();
    let d = f.dma();
    // DCFG1.DBWDEF = 4: een 128-bit AXI.
    dev::write32(b.add(0x280), 4 << 25);
    let mut n = f.nic();
    n.init().unwrap();
    // Zonder link alleen de management-poort, en geen snelheid.
    assert_eq!(dev::read32(b.add(0x000)), CTRL_MGMT_EN);
    assert_eq!(dev::read32(b.add(0x004)) & CFG_GIGABIT, 0);
    n.adjust_link(LINK_1G);
    let bus0 = d.0 + OFF_RX_BUFS + BUS_OFF;
    assert_eq!(dev::read32(d), bus0 as u32);
    assert_eq!(dev::read32(d.add(8)), (bus0 >> 32) as u32);
    let last = d.add(((N_RX - 1) * 16) as u64);
    assert_eq!(dev::read32(last) & RX_WRAP, RX_WRAP);
    let tx_last = d.add(((N_RX + N_TX - 1) * 16) as u64);
    assert_eq!(dev::read32(tx_last.add(4)), TX_USED | TX_WRAP);
    let cfg = dev::read32(b.add(0x004));
    assert_eq!(cfg >> CFG_DBW_SHIFT & 3, 2, "DBW follows DCFG1");
    assert_ne!(cfg & CFG_GIGABIT, 0);
    assert_ne!(cfg & CFG_PAE, 0);
    assert_eq!(dev::read32(b.add(0x054)) & 0x1_ffff, AMP_RP1);
    assert_eq!(dev::read32(b.add(0x018)), (d.0 + BUS_OFF) as u32);
    assert_eq!(dev::read32(b.add(0x4d4)), ((d.0 + BUS_OFF) >> 32) as u32);
    assert_eq!(dev::read32(b.add(0x088)), 0x0167_cf2c);
    assert_eq!(dev::read32(b.add(0x08c)), 0x0302);
    assert_eq!(
        dev::read32(b.add(0x000)),
        CTRL_MGMT_EN | CTRL_TX_EN | CTRL_RX_EN
    );
}

#[test]
fn a_small_region_is_refused() {
    let mut f = Fake::new();
    let (b, d) = (f.base(), f.dma());
    // SAFETY: de vectoren leven de hele test.
    let mut n = unsafe { Gem::new(b, 0, d, DMA_NEED - 1, [0; 6]) };
    assert!(matches!(n.init(), Err(Error::Dma { .. })));
}

#[test]
fn transmit_hands_the_descriptor_over_and_a_busy_one_is_full() {
    let mut f = Fake::new();
    let b = f.base();
    let d = f.dma();
    let mut n = f.nic();
    up(&mut n);
    // De nep-GEM zendt nooit: USED blijft 0, dus dezelfde descriptor is bij
    // de wrap nog van de hardware.
    n.transmit(&[0x77; 100]).unwrap();
    let t0 = d.add((N_RX * 16) as u64);
    assert_eq!(dev::read32(t0.add(4)), 100 | TX_LAST);
    let bus = d.0 + OFF_TX_BUFS + BUS_OFF;
    assert_eq!(dev::read32(t0), bus as u32);
    assert_eq!(dev::read32(t0.add(8)), (bus >> 32) as u32);
    assert_ne!(dev::read32(b) & CTRL_TX_START, 0);
    assert_eq!(dev::read8(d.add(OFF_TX_BUFS)), 0x77);
    n.tx_head = 0;
    assert_eq!(n.transmit(&[1; 60]), Err(TxError::Full));
    assert_eq!(n.transmit(&[]), Err(TxError::Size(0)));
}

/// Een TSTART die over de PCIe verloren ging (de zender staat stil, de
/// laatste descriptor is nog van de hardware) komt opnieuw bij een volle
/// ring en bij een lege RX-ronde; een zender die nog loopt of niets heeft,
/// krijgt er geen.
#[test]
fn a_dropped_tstart_is_given_again() {
    let mut f = Fake::new();
    let b = f.base();
    let mut n = f.nic();
    up(&mut n);
    let mut buf = [0u8; 2048];
    let tstart = |b: Pa| dev::read32(b) & CTRL_TX_START != 0;
    // Niets gegeven: geen TSTART.
    assert_eq!(n.receive(&mut buf), None);
    assert!(!tstart(b));
    n.transmit(&[1; 60]).unwrap();
    assert!(tstart(b));
    // De zender loopt (TGO): geen tweede.
    dev::write32(b, 0);
    dev::write32(b.add(0x014), TX_GO);
    assert_eq!(n.receive(&mut buf), None);
    assert!(!tstart(b));
    // Hij staat stil met de descriptor nog bij de hardware: wel.
    dev::write32(b.add(0x014), 0);
    assert_eq!(n.receive(&mut buf), None);
    assert!(tstart(b));
    // Een volle ring met een stille zender ook.
    for _ in 1..N_TX {
        n.transmit(&[2; 60]).unwrap();
    }
    dev::write32(b, 0);
    assert_eq!(n.transmit(&[3; 60]), Err(TxError::Full));
    assert!(tstart(b));
    // Alles terug van de hardware: geen TSTART meer.
    let t0 = f.dma().add((N_RX * 16) as u64);
    for i in 0..N_TX as u64 {
        let w1 = t0.add(i * 16 + 4);
        dev::write32(w1, dev::read32(w1) | TX_USED);
    }
    dev::write32(b, 0);
    assert_eq!(n.receive(&mut buf), None);
    assert!(!tstart(b));
}

#[test]
fn receive_takes_complete_frames_returns_the_rest_and_rearms() {
    static REARMS: AtomicU32 = AtomicU32::new(0);
    static BELL: Signal = Signal::new();
    let mut f = Fake::new();
    let b = f.base();
    let d = f.dma();
    let mut n = f.nic();
    up(&mut n);
    n.set_irq(&BELL, || {
        REARMS.fetch_add(1, SeqCst);
    });
    assert_eq!(dev::read32(b.add(0x028)), INT_LINE, "IER open");
    let mut buf = [0u8; 2048];
    // Frame 0 zonder EOF (kapot), frame 1 compleet.
    dev::write32(d, dev::read32(d) | RX_OWNED);
    dev::write32(d.add(4), RX_SOF | 60);
    dev::write32(d.add(16), dev::read32(d.add(16)) | RX_OWNED);
    dev::write32(d.add(20), RX_SOF | RX_EOF | 64);
    dev::copy_in(d.add(OFF_RX_BUFS + BUF as u64), &[0x42; 64]);
    assert_eq!(n.receive(&mut buf), Some(64));
    assert_eq!(buf[..64], [0x42; 64]);
    assert_eq!(dev::read32(d) & RX_OWNED, 0, "back to the DMA");
    assert_eq!(dev::read32(d.add(20)), 0);
    assert_eq!(REARMS.load(SeqCst), 0);
    assert_eq!(n.receive(&mut buf), None);
    assert_eq!(REARMS.load(SeqCst), 1, "an empty ring rearms");
    assert!(n.irq().is_some());
}

/// De buffers in hun eigen blokken (het board mapt ze Normal-WB), de
/// descriptors eronder (NC); de kopie in en uit is `memcpy` met een veeg:
/// byte voor byte het frame, met een staart die niet op 8 of 16 valt, en
/// niets ernaast.
#[test]
fn frames_cross_the_wb_buffers_byte_for_byte() {
    assert!(((N_RX + N_TX) * 16) as u64 <= BUF_OFF);
    assert_eq!((BUF_OFF, BUF_OFF + BUF_BLOCK), (OFF_RX_BUFS, DMA_NEED));
    let pattern = |n: usize| (0..n).map(|i| (i * 7 + 3) as u8).collect::<Vec<u8>>();
    for size in [17, 61, 1514] {
        let mut f = Fake::new();
        let d = f.dma();
        let mut n = f.nic();
        up(&mut n);
        let frame = pattern(size);
        n.transmit(&frame).unwrap();
        let mut sent = vec![0u8; BUF];
        dev::copy_out(&mut sent, d.add(OFF_TX_BUFS));
        assert_eq!(&sent[..size], &frame[..], "tx {size}");
        assert!(sent[size..].iter().all(|&b| b == 0), "tx {size}: beyond");

        dev::write32(d, dev::read32(d) | RX_OWNED);
        dev::write32(d.add(4), RX_SOF | RX_EOF | size as u32);
        dev::copy_in(d.add(OFF_RX_BUFS), &pattern(BUF));
        let mut out = vec![0xccu8; 2048];
        assert_eq!(n.receive(&mut out), Some(size), "rx {size}");
        assert_eq!(&out[..size], &frame[..], "rx {size}");
        assert!(out[size..].iter().all(|&b| b == 0xcc), "rx {size}: beyond");
    }
}

#[test]
fn ack_masks_and_clears_the_explicit_bit() {
    let mut f = Fake::new();
    let b = f.base();
    // SAFETY: het nep-blok leeft de hele test.
    unsafe { ack_irq(b) };
    assert_eq!(dev::read32(b.add(0x02c)), INT_LINE);
    assert_eq!(dev::read32(b.add(0x024)), INT_LINE);
}

#[test]
fn stop_is_macb_reset_hw_for_a_gem_someone_left_running() {
    let mut f = Fake::new();
    let b = f.base();
    // Een GEM zoals een vorige kern hem achterliet: RX en TX aan, RCOMP
    // open en gelatcht.
    dev::write32(b, CTRL_MGMT_EN | CTRL_TX_EN | CTRL_RX_EN);
    dev::write32(b.add(0x030), !INT_RCOMP);
    dev::write32(b.add(0x044), 5);
    // SAFETY: het nep-blok leeft de hele test.
    unsafe { stop(b) };
    assert_eq!(dev::read32(b) & (CTRL_TX_EN | CTRL_RX_EN), 0);
    assert_eq!(dev::read32(b.add(0x02c)), u32::MAX, "every interrupt off");
    assert_eq!(dev::read32(b.add(0x024)), u32::MAX, "the latch cleared");
    assert_eq!(dev::read32(b.add(0x014)), u32::MAX);
    assert_eq!(dev::read32(b.add(0x020)), u32::MAX);
    assert_eq!(dev::read32(b.add(0x044)), 0);
}

#[test]
fn the_rx_queue_pointer_names_its_descriptor() {
    let ring = 0x10_1400_0000;
    assert_eq!(rx_index(0x1400_0000, ring), Some(0));
    assert_eq!(rx_index(0x1400_0000 + 16 * 37, ring), Some(37));
    assert_eq!(rx_index(0x1400_0000 + 16 * N_RX as u32, ring), None);
    assert_eq!(rx_index(0x1400_0008, ring), None);
    assert_eq!(rx_index(0x13ff_fff0, ring), None, "below the ring");
}

#[test]
fn mdio_frames_and_a_stuck_bus() {
    let mut f = Fake::new();
    let b = f.base();
    let mut n = f.nic();
    assert_eq!(
        Mdio::read(&mut n, 1, 2),
        Err(driver_mdio::Error::Bus { phy: 1, reg: 2 })
    );
    dev::write32(b.add(0x008), STATUS_MDIO_IDLE);
    Mdio::write(&mut n, 1, 4, 0x01e1).unwrap();
    assert_eq!(
        dev::read32(b.add(0x034)),
        MAN_CLAUSE22 | MAN_WRITE | (1 << 23) | (4 << 18) | MAN_MUST_BE_10 | 0x01e1
    );
}

#[test]
fn config_and_lengths() {
    let half100 = Link {
        mbps: 100,
        full_duplex: false,
    };
    let c = cfg_for(2 << 25, Some(half100));
    assert_eq!(c & (CFG_FD | CFG_SPEED100 | CFG_GIGABIT), CFG_SPEED100);
    assert_eq!(c >> CFG_DBW_SHIFT & 3, 1);
    assert_eq!(cfg_for(0, Some(LINK_1G)) >> CFG_DBW_SHIFT & 3, 0);
    assert_eq!(
        cfg_for(0, None) & (CFG_FD | CFG_SPEED100 | CFG_GIGABIT),
        0,
        "no link, no speed"
    );
    assert_eq!(rx_len(RX_SOF | RX_EOF | 1514), Some(1514));
    assert_eq!(rx_len(RX_SOF | RX_EOF | 1600), None);
    assert_eq!(rx_len(RX_EOF | 64), None);
}
