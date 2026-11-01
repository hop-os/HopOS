//! Wat de DWMAC4 anders doet dan de kern: de config- en kanaalwoorden, de
//! lees- en schrijfvorm van de descriptors, de MDIO-velden, de FIFO-maten,
//! de indeling met [`BUF_OFF`] en de registerprogrammering.

use super::*;
use crate::mdio_cmd;
use crate::tests::{TEST_MAC, rig};
use core::sync::atomic::{AtomicU32, Ordering::SeqCst};
use netdev::Device as _;

/// Een draaiende nep-driver: FIFO's van 4 KB, dan de start (nog zonder
/// link).
fn running() -> crate::tests::Rig<Hw> {
    let mut f = rig::<Hw>();
    f.n.regs().hw_feature1.write((5 << 6) | 5);
    f.n.bring_up().unwrap();
    f
}

#[test]
fn mac_config_sets_the_right_speed_bits() {
    // PS = MII (10/100), FES = 100 in plaats van 10, geen van beide = GMII.
    for (mbps, fd, ps, fes, dm) in [
        (10, false, true, false, false),
        (10, true, true, false, true),
        (100, true, true, true, true),
        (1000, true, false, false, true),
        (1000, false, false, false, false),
    ] {
        let got = mac_config(0, mbps, fd);
        assert_eq!(got & CFG_PS != 0, ps, "{mbps}: PS in {got:#x}");
        assert_eq!(got & CFG_FES != 0, fes, "{mbps}: FES in {got:#x}");
        assert_eq!(got & CFG_DM != 0, dm, "{mbps} fd={fd}: DM");
    }
}

#[test]
fn mac_config_clears_the_old_speed_keeps_core_init_and_leaves_jumbo_and_acs_off() {
    // Van 100 terug naar 1000: blijft FES staan, dan klokt een node na een
    // link-flap op de verkeerde snelheid. En jumbo moet uit blijven.
    let prev = mac_config(0, 100, true);
    let got = mac_config(prev | CFG_JE, 1000, true);
    assert_eq!(got & (CFG_FES | CFG_PS | CFG_JE), 0, "{got:#x}");
    assert_eq!(mac_config(got, 1000, true), got, "not idempotent");
    assert_eq!(got & CORE_INIT, CORE_INIT);
    // Met ACS aan zou de kern vier bytes te veel aftrekken.
    assert_eq!(got & CFG_ACS, 0);
}

#[test]
fn rx_control_carries_a_real_buffer_size() {
    let got = rx_control(0);
    assert_eq!(((got & RX_RBSZ_MASK) >> RX_RBSZ_SHIFT) as usize, BUF_SIZE);
    assert_ne!(got & (PBL << RX_PBL_SHIFT), 0);
    // Een tweede keer over de eerste heen verdubbelt of vervuilt niets.
    assert_eq!(rx_control(got), got);
}

#[test]
fn the_descriptor_words_sit_where_dwmac4_descs_h_says() {
    // TX: de lengte twee keer, in TDES2 (met IOC) en in TDES3.
    let (d2, d3) = tx_desc23(1000);
    assert_eq!(d2, TX_IOC | 1000);
    assert_eq!(d3, TX_OWN | TX_FIRST | TX_LAST | 1000);
    // RX: de lengte in RDES3 [14:0]; de statusbits erboven lekken niet.
    assert_eq!(Hw::rx_raw_len(0xFFFF_8000 | 100), 100);
    assert!(Hw::rx_whole(RX_FIRST | RX_LAST | RX_OWN));
    assert!(!Hw::rx_whole(RX_FIRST | 68));
    assert!(!Hw::rx_whole(RX_ERR_SUMMARY | RX_FIRST | RX_LAST));
}

#[test]
fn mdio_fields_sit_at_the_dwmac4_positions() {
    // Een verwisselde shift geeft een bus die stil naar het verkeerde
    // register schrijft.
    let want = (3 << 21) | (5 << 16) | (CSR_100_150M << 8) | (3 << 2) | 1;
    assert_eq!(mdio_cmd(&Hw::MII, 3, 5, CSR_100_150M, false), want);
    assert_eq!(
        mdio_cmd(&Hw::MII, 3, 5, CSR_100_150M, true),
        (want & !(3 << 2)) | (1 << 2)
    );
}

#[test]
fn fifo_sizes_come_from_hw_feature1() {
    assert_eq!(fifo_sizes((5 << 6) | 4), (4096, 2048));
    assert_eq!(fifo_sizes(0), (128, 128));
    assert_eq!(fifo_sizes(0x1F), (128, 0));
}

#[test]
fn the_buffers_sit_in_their_own_block_behind_the_descriptors() {
    let r = Rings::<Hw>::at(Pa(0));
    // De buffers in hun eigen blok van 2 MB (het board mapt het Normal-WB),
    // de descriptors ervoor (NC).
    assert_eq!(r.rx_buf.0, BUF_OFF);
    assert!(r.tx(NUM_TX - 1).0 + DESC_SIZE <= BUF_OFF);
    assert_eq!(r.tx_buf(NUM_TX - 1).0 + BUF_SIZE as u64, NEED_BYTES);
    crate::tests::rings_fit::<Hw>();
}

#[test]
fn a_returned_descriptor_gets_its_buffer_address_back() {
    let f = running();
    let n = &f.n;
    for i in [0, 1, NUM_RX - 1] {
        let d = n.ring.rx(i);
        assert_eq!(dev::read32(d), lo(n.ring.rx_buf(i)), "rx {i}");
        assert_eq!(dev::read32(d.add(DES3)), RX_OWN | RX_BUF1_VALID | RX_IOC);
    }
    assert_eq!(dev::read32(n.ring.tx(NUM_TX - 1).add(DES3)), 0);
    // De DMA schreef de schrijfvorm (status in alle vier de woorden): een
    // teruggave en een verzending zetten het adres er opnieuw in.
    let d = n.ring.rx(0);
    dev::write32(d, 0xdead_beef);
    dev::write32(d.add(DES3), RX_FIRST | RX_LAST | 68);
    assert_eq!(Hw::rx_status(d), Some(RX_FIRST | RX_LAST | 68));
    Hw::rx_give(d, n.ring.rx_buf(0), false);
    assert_eq!(dev::read32(d), lo(n.ring.rx_buf(0)));
    assert_eq!(Hw::rx_status(d), None);
    let t = n.ring.tx(0);
    dev::write32(t, 0xdead_beef);
    Hw::tx_give(t, n.ring.tx_buf(0), 60, false);
    assert_eq!(dev::read32(t), lo(n.ring.tx_buf(0)));
    assert_eq!(dev::read32(t.add(8)), TX_IOC | 60);
    assert!(!Hw::tx_free(t));
}

#[test]
fn program_lays_out_the_rings_and_the_registers_and_masks_the_mmc() {
    static SPEED: AtomicU32 = AtomicU32::new(0);
    let mut f = running();
    let n = &f.n;
    let r = n.regs();
    let c = &r.chan;
    // Ringen: basis, lengte (aantal min één), tails.
    assert_eq!(c.rx_base.read(), lo(n.ring.rx_desc));
    assert_eq!(c.tx_base.read(), lo(n.ring.tx_desc));
    assert_eq!(c.rx_ring_len.read(), u32::from(NUM_RX) - 1);
    assert_eq!(c.tx_ring_len.read(), u32::from(NUM_TX) - 1);
    assert_eq!(c.rx_end.read(), lo(n.ring.tx_desc)); // één voorbij RX
    assert_eq!(c.tx_end.read(), lo(n.ring.tx_desc));
    // De AXI-kant zoals de DTS hem voorschrijft.
    assert_eq!(r.dma_sys_bus_mode.read(), SYS_BUS_MODE);
    // MTL: 4 KB-FIFO's, dus TQS = RQS = 15.
    assert_eq!(
        r.mtl_tx_op_mode.read(),
        MTL_TSF | MTL_TXQ_EN | (15 << MTL_TQS_SHIFT)
    );
    assert_eq!(r.mtl_rx_op_mode.read(), MTL_RSF | (15 << MTL_RQS_SHIFT));
    // RBSZ en start.
    let rxc = c.rx_control.read();
    assert_eq!(((rxc & RX_RBSZ_MASK) >> RX_RBSZ_SHIFT) as usize, BUF_SIZE);
    assert_ne!(rxc & CHAN_START, 0);
    assert_ne!(c.tx_control.read() & CHAN_START, 0);
    // Het MAC-adres in de filter; de MAC nog uit tot de link, dan aan op
    // gigabit full duplex, en de klok van de SoC eerst.
    let m = TEST_MAC.0;
    assert_eq!(
        r.addr0_lo.read(),
        u32::from_le_bytes([m[0], m[1], m[2], m[3]])
    );
    assert_eq!(r.addr0_hi.read(), (u32::from(m[5]) << 8) | u32::from(m[4]));
    assert_eq!(r.rxq_ctrl0.read(), RXQ0_DCB_ENABLE);
    assert_eq!(r.config.read() & (CFG_TE | CFG_RE), 0, "no link, no MAC");
    f.n.fix_speed = Some(|m| SPEED.store(m, SeqCst));
    f.n.adjust_link(1000, true);
    assert_eq!(SPEED.load(SeqCst), 1000);
    let cfg = r.config.read();
    assert_eq!(cfg & (CFG_TE | CFG_RE | CFG_DM), CFG_TE | CFG_RE | CFG_DM);
    assert_eq!(cfg & SPEED_MASK, 0);
    // Interrupts dicht tot het board ze bedraadt.
    assert_eq!(c.intr_ena.read(), 0);
    // En de MMC-tellers zetten de lijn nooit: zonder deze maskers stormde
    // de Radxa na 2 GiB binnen (03-10).
    assert_eq!(r.mmc_rx_mask.read(), MMC_INT_ALL);
    assert_eq!(r.mmc_tx_mask.read(), MMC_INT_ALL);
    assert_eq!(r.mmc_ipc_mask.read(), MMC_INT_ALL);
}

#[test]
fn an_unclocked_block_is_refused_on_its_fifo_sizes() {
    let mut f = rig::<Hw>();
    // HW_FEATURE1 leest nul: 128 B-FIFO's, en fifo/256 - 1 zou omlopen.
    assert_eq!(
        f.n.bring_up(),
        Err(Error::Fifo {
            tx: 128,
            rx: 128,
            hw_feature1: 0
        })
    );
}

#[test]
fn the_doorbells_are_tails_and_the_irq_is_the_4_10_layout() {
    static BELL: sync::Signal = sync::Signal::new();
    let mut f = running();
    let n = &mut f.n;
    n.transmit(&[1; 60]).unwrap();
    n.flush();
    assert_eq!(n.regs().chan.tx_end.read(), lo(n.ring.tx(1)));
    // NIE op bit 15 (4.10+), niet de 16 van de 3.x: 20-09 op de Radxa; RIE
    // en TIE.
    n.set_irq(&BELL);
    assert_eq!(n.regs().chan.intr_ena.read(), 0x0000_8041);
}

#[test]
fn diag_is_one_line_with_the_numbers() {
    let f = running();
    let s = f.n.diag().to_string();
    assert!(!s.contains('\n'));
    assert!(s.contains("rxdesc[0] 0xc1000000"), "{s}");
}
