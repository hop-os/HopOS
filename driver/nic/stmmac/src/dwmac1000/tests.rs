//! Wat de DWMAC1000 anders doet dan de kern: de descriptorbits, de DSL, de
//! RMII-config, de MDIO-velden, de mis-teller en de registerprogrammering.
//!
//! De descriptorwoorden horen op de host bewezen te worden, niet op een
//! bordje waar één ronde een kaartwissel kost: op de eerste DMA-boot
//! (30-07) werd de buffergrootte met een 11-bits masker naar nul geveegd;
//! link stond, TX liep, RX gaf 128 descriptors terug zonder één frame.

use super::*;
use crate::tests::{TEST_MAC, rig};
use crate::{Stmmac, mdio_cmd};
use netdev::Device as _;

fn running() -> crate::tests::Rig<Hw> {
    let mut f = rig::<Hw>();
    f.n.bring_up(100, true).unwrap();
    f
}

#[test]
fn rx_cntl_reports_a_real_buffer_size_and_ring_end_on_the_last() {
    // Een maat die door het masker valt, betekent nul-byte buffers; 2048,
    // de maat die de vendor als buffer heeft, zou er niet in passen.
    assert_eq!((rx_cntl(false) & CNTL_SIZE1_MASK) as usize, MAX_FRAME);
    assert_eq!(2048 & CNTL_SIZE1_MASK, 0);
    assert_eq!(rx_cntl(false) & RING_END, 0);
    assert_eq!(rx_cntl(true), rx_cntl(false) | RING_END);
}

#[test]
fn tx_cntl_sets_first_last_length_and_ring_end() {
    // Zonder FS+LS wacht de MAC op de rest van een frame dat niet komt.
    // Zonder IC geen TI, en dan wekt een vrijgekomen descriptor niemand.
    assert_eq!(
        tx_cntl(64, false),
        TX_CNTL_IC | TX_CNTL_FIRST | TX_CNTL_LAST | 64
    );
    assert_eq!(
        tx_cntl(1518, true) & (RING_END | CNTL_SIZE1_MASK),
        RING_END | 1518
    );
}

#[test]
fn descriptor_fields_sit_at_the_databook_positions() {
    // Normal format: OWN bovenin RDES0/TDES0, RER/TER bit 25, FS/LS in TDES1
    // op 29/30 en in RDES0 op 9/8, de lengte in RDES0 [29:16].
    assert_eq!(DESC_OWN, 0x8000_0000);
    assert_eq!(RING_END, 0x0200_0000);
    assert_eq!(TX_CNTL_IC | TX_CNTL_FIRST | TX_CNTL_LAST, 0xE000_0000);
    assert_eq!(RX_STS_FIRST | RX_STS_LAST, 0x300);
    assert_eq!(Hw::rx_raw_len(0xFFFF_FFFF), 0x3FFF);
    assert_eq!(Hw::rx_raw_len(1518 << 16 | DESC_OWN | 0xFFFF), 1518);
    assert!(Hw::rx_whole(RX_STS_FIRST | RX_STS_LAST));
    assert!(!Hw::rx_whole(RX_STS_FIRST));
    assert!(!Hw::rx_whole(RX_STS_ERROR | RX_STS_FIRST | RX_STS_LAST));
    assert_eq!(
        (DES_STATUS, DES_CNTL, DES_BUF, DES_NEXT + 4),
        (0, 4, 8, DESC_SIZE)
    );
}

#[test]
fn bus_mode_carries_the_skip_length_for_one_descriptor_per_line() {
    // DSL [6:2] = 12 woorden = 48 bytes skip, plus 16 bytes descriptor = 64.
    assert_eq!(u64::from((BUS_MODE >> 2) & 0x1F) * 4 + DESC_SIZE, LINE);
    // ALTDESCRIPTOR (bit 7) blijft uit: het normal format; geen softreset.
    assert_eq!(BUS_MODE, (1 << 16) | (8 << 8) | (12 << 2));
}

/// Elke descriptor en elke buffer op eigen cachelines: de les van 30-07,
/// waar een clean van de één de DMA-schrijf in de ander overschreef.
#[test]
fn every_descriptor_and_buffer_owns_its_cachelines() {
    crate::tests::rings_fit::<Hw>();
}

#[test]
fn mac_conf_sets_the_rmii_bits_and_only_rmii_speeds_pass() {
    let base = CONF_PORT_MII | CONF_DIS_RX_OWN | CONF_TX_EN | CONF_RX_EN;
    assert_eq!(mac_conf(10, false), base);
    assert_eq!(mac_conf(10, true), base | CONF_DUPLEX);
    assert_eq!(mac_conf(100, true), base | CONF_FES100 | CONF_DUPLEX);
    assert!(Hw::speed_ok(10) && Hw::speed_ok(100) && !Hw::speed_ok(1000));
}

#[test]
fn mdio_fields_sit_at_the_dwmac1000_positions() {
    // Go: phy << 11 | reg << 6 | 0x14 | busy (| write).
    assert_eq!(
        mdio_cmd(&Hw::MII, 0, 2, CSR_250_300M, false),
        (2 << 6) | 0x14 | 1
    );
    assert_eq!(
        mdio_cmd(&Hw::MII, 3, 4, CSR_250_300M, true),
        (3 << 11) | (4 << 6) | 0x14 | 2 | 1
    );
}

#[test]
fn missed_counter_splits_ring_fifo_and_overflow() {
    assert_eq!(missed(0), (0, 0, 0));
    assert_eq!(missed(0x1234), (0x1234, 0, 0));
    assert_eq!(missed(1 << 16), (0, 0, 1));
    assert_eq!(missed(0x7FF << 17), (0, 0x7FF, 0));
    assert_eq!(missed((1 << 28) | (1 << 16) | (3 << 17) | 7), (7, 3, 2));
}

#[test]
fn the_descriptors_keep_size_and_address_and_only_own_moves() {
    let f = running();
    let n: &Stmmac<Hw> = &f.n;
    // RX: van de DMA, met de gemelde maat, ring-end alleen op de laatste.
    for i in [0, 1, NUM_DESC - 1] {
        let d = n.ring.rx(i);
        assert_eq!(dev::read32(d.add(DES_STATUS)), DESC_OWN, "rx {i}");
        assert_eq!(dev::read32(d.add(DES_CNTL)), rx_cntl(i == NUM_DESC - 1));
        assert_eq!(dev::read32(d.add(DES_BUF)), lo(n.ring.rx_buf(i)));
    }
    // De ruimte tussen twee descriptors is leeg: de DMA slaat hem over.
    assert_eq!(dev::read32(n.ring.rx(0).add(DESC_SIZE)), 0);
    // Teruggeven zet alleen OWN: RDES1 en RDES2 staan er nog.
    let d = n.ring.rx(0);
    dev::write32(d, 0);
    Hw::rx_give(d, Pa(0), true);
    assert_eq!(dev::read32(d), DESC_OWN);
    assert_eq!(dev::read32(d.add(DES_CNTL)), rx_cntl(false));
    assert_eq!(dev::read32(d.add(DES_BUF)), lo(n.ring.rx_buf(0)));
    // TX: TDES1 en OWN, het adres blijft.
    let t = n.ring.tx(NUM_DESC - 1);
    assert!(Hw::tx_free(t));
    Hw::tx_give(t, Pa(0), 60, true);
    assert!(!Hw::tx_free(t));
    assert_eq!(dev::read32(t.add(DES_CNTL)), tx_cntl(60, true));
    assert_eq!(dev::read32(t.add(DES_BUF)), lo(n.ring.tx_buf(NUM_DESC - 1)));
}

#[test]
fn program_lays_out_the_registers_and_masks_what_we_do_not_handle() {
    let f = running();
    let n = &f.n;
    let r = n.regs();
    assert_eq!(r.rx_list.read(), lo(n.ring.rx_desc));
    assert_eq!(r.tx_list.read(), lo(n.ring.tx_desc));
    assert_eq!(r.bus_mode.read(), BUS_MODE);
    let m = TEST_MAC.0;
    assert_eq!(
        r.addr0_lo.read(),
        u32::from_le_bytes([m[0], m[1], m[2], m[3]])
    );
    assert_eq!(r.addr0_hi.read(), (u32::from(m[5]) << 8) | u32::from(m[4]));
    assert_eq!(r.filter.read(), FILTER_PM | FILTER_PR);
    assert_eq!(r.conf.read(), mac_conf(100, true));
    let want = OP_STORE_FORWARD | OP_FLUSH_TX_FIFO | OP_TX_START | OP_RX_START;
    assert_eq!(r.op_mode.read(), want);
    assert_eq!(r.int_mask.read(), GMAC_INT_ALL);
    assert_eq!(r.mmc_rx_mask.read(), MMC_INT_ALL);
    assert_eq!(r.mmc_tx_mask.read(), MMC_INT_ALL);
    assert_eq!(r.mmc_ipc_mask.read(), MMC_INT_ALL);
}

#[test]
fn the_doorbells_are_poll_demands_and_the_irq_is_the_3x_layout() {
    static BELL: sync::Signal = sync::Signal::new();
    let mut f = running();
    let n = &mut f.n;
    n.transmit(&[1; 60]).unwrap();
    n.flush();
    assert_eq!(n.regs().tx_poll.read(), 1);
    // De 3.x-indeling: NIE op bit 16, niet de 15 van de DWMAC4; RIE en TIE.
    n.set_irq(&BELL);
    assert_eq!(n.regs().intr_ena.read(), 0x0001_0041);
}

#[test]
fn diag_is_one_line_with_the_numbers_and_samples_missed() {
    let mut f = running();
    f.n.regs().missed.write(5 | (2 << 17));
    let s = f.n.diag().to_string();
    assert!(!s.contains('\n'));
    assert!(s.contains("rxdesc[0] 0x80000000"), "{s}");
    assert!(s.contains("missed-ring=5 missed-fifo=2"), "{s}");
    assert_eq!(f.n.stats.rx_missed_ring, 5);
}
