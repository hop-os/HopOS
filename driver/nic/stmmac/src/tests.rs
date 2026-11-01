//! De kern op een nagebootste generatie: een registerblok en een
//! descriptorformaat van niets, zodat deze toetsen alleen de kern bewijzen
//! (start, reset, MDIO, de ringen, het frame-pad, de interrupt). Wat een
//! echte generatie anders doet, toetst die generatie zelf, klein.
//!
//! Geport uit `dwmac_test.go`, `dwmac4_test.go` en `receive_bounds_test.go`.
//! Ook de helpers voor die generatietoetsen staan hier.

use super::*;
use netdev::{Device as _, IrqAck as _};
use std::cell::Cell;
use std::vec::Vec;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(0) };
}

/// Een klok die per lees een milliseconde verspringt.
pub(crate) fn now() -> u64 {
    NOW.with(|n| {
        n.set(n.get() + 1_000_000);
        n.get()
    })
}

/// Een op een cacheline gealigneerd stuk RAM dat de toets lang genoeg leeft.
pub(crate) struct Mem {
    _v: Vec<u64>,
    pub(crate) base: Pa,
}

pub(crate) fn mem(bytes: usize) -> Mem {
    let mut v = vec![0u64; bytes / 8 + 16];
    let base = Pa((v.as_mut_ptr() as u64).next_multiple_of(dev::LINE));
    Mem { _v: v, base }
}

pub(crate) const TEST_MAC: Mac = Mac([0x02, 0x48, 0x4f, 0x50, 0x12, 0x34]);

/// Een driver op een nep-registerblok en een nep-DMA-regio, zonder reset
/// (in RAM klaart de reset-bit nooit) en nog niet aangezet.
pub(crate) struct Rig<O: Ops> {
    _regs: Mem,
    _dma: Mem,
    pub(crate) n: Stmmac<O>,
}

pub(crate) fn rig<O: Ops>() -> Rig<O> {
    let regs = mem(size_of::<O::Regs>());
    let dma = mem(O::NEED_BYTES as usize);
    let n = Stmmac::at(probe::<O>(&regs), TEST_MAC, dma.base);
    Rig {
        _regs: regs,
        _dma: dma,
        n,
    }
}

/// Een `Probe` op een nep-registerblok.
pub(crate) fn probe<O: Ops>(regs: &Mem) -> Probe<O> {
    // SAFETY: het nep-blok is groot genoeg en leeft de hele toets.
    unsafe { Probe::new(regs.base, 0x5, now) }
}

// --- de nagebootste generatie ----------------------------------------------

#[repr(C)]
struct FakeRegs {
    version: Reg<u32>,
    bus_mode: Reg<u32>,
    mii_addr: Reg<u32>,
    mii_data: Reg<u32>,
    intr_ena: Reg<u32>,
    status: Reg<u32>,
    rx_tail: Reg<u32>,
    tx_tail: Reg<u32>,
}

/// Status en OWN in woord 0, controle in woord 1, het adres in woord 2.
const OWN: u32 = 1 << 31;
const FIRST: u32 = 1 << 29;
const LAST: u32 = 1 << 28;
const ERR: u32 = 1 << 15;
const RING_END: u32 = 1 << 30;

/// Een generatie met kleine ringen.
#[derive(Clone, Copy)]
struct Fake;

impl Ops for Fake {
    type Regs = FakeRegs;

    const NUM_RX: u16 = 8;
    const NUM_TX: u16 = 4;
    const DESC_STRIDE: u64 = 16;
    const BUF_SIZE: usize = 128;
    const BUF_OFF: u64 = 256;
    const MAX_FRAME: usize = 100;
    const RX_LIMIT: usize = 120;
    const MII: Mii = Mii {
        addr_shift: 21,
        reg_shift: 16,
        csr_shift: 8,
        read: 3 << 2,
        write: 1 << 2,
    };
    const INTR: u32 = 0x41;
    const STAT: u32 = 0x41;

    fn version(r: &FakeRegs) -> &Reg<u32> {
        &r.version
    }
    fn bus_mode(r: &FakeRegs) -> &Reg<u32> {
        &r.bus_mode
    }
    fn mii(r: &FakeRegs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.mii_addr, &r.mii_data)
    }
    fn irq(r: &FakeRegs) -> (&Reg<u32>, &Reg<u32>) {
        (&r.intr_ena, &r.status)
    }
    const GIGABIT: bool = false;
    fn program(_: &FakeRegs, _: &Rings<Self>, _: Mac) -> Result {
        Ok(())
    }
    fn set_link(r: &FakeRegs, mbps: u32, full_duplex: bool) {
        r.rx_tail.write(mbps | u32::from(full_duplex) << 31);
    }
    fn rx_doorbell(r: &FakeRegs, tail: Pa) {
        r.rx_tail.write(lo(tail));
    }
    fn tx_doorbell(r: &FakeRegs, tail: Pa) {
        r.tx_tail.write(lo(tail));
    }
    fn rx_give(d: Pa, buf: Pa, last: bool) {
        dev::write32(d.add(8), lo(buf));
        dev::write32(d.add(4), if last { RING_END } else { 0 });
        dev::write32(d, OWN);
    }
    fn rx_status(d: Pa) -> Option<u32> {
        let s = dev::read32(d);
        (s & OWN == 0).then_some(s)
    }
    fn rx_whole(sts: u32) -> bool {
        sts & ERR == 0 && sts & (FIRST | LAST) == FIRST | LAST
    }
    fn rx_raw_len(sts: u32) -> usize {
        (sts & 0x7FFF) as usize
    }
    fn tx_init(d: Pa, buf: Pa, _last: bool) {
        dev::write32(d, 0);
        dev::write32(d.add(8), lo(buf));
    }
    fn tx_free(d: Pa) -> bool {
        dev::read32(d) & OWN == 0
    }
    fn tx_give(d: Pa, _buf: Pa, len: usize, last: bool) {
        dev::write32(d.add(4), len as u32 | if last { RING_END } else { 0 });
        dev::write32(d, OWN);
    }
}

type F = Fake;

/// Een draaiende nep-driver.
fn running<O: Ops>() -> Rig<O> {
    let mut f = rig::<O>();
    f.n.bring_up().unwrap();
    f
}

/// Een goed frame van `len` bytes (zonder FCS) in RX-descriptor `i`.
fn frame_in(n: &Stmmac<F>, i: u16, len: usize) {
    dev::write32(n.ring.rx(i), FIRST | LAST | (len + FCS_LEN) as u32);
}

// --- start, reset en MDIO -------------------------------------------------

#[test]
fn start_refuses_a_dead_mac_a_small_or_misplaced_region_and_a_stuck_reset() {
    let regs = mem(size_of::<FakeRegs>());
    let need = F::NEED_BYTES;
    let mut p = probe::<F>(&regs);
    assert_eq!(p.check(), Err(Error::NoMac { version: 0 }));
    // In RAM blijft de reset-bit staan: het beeld van de Radxa op 05-08.
    assert_eq!(p.reset(), Err(Error::ResetStuck { bus_mode: 1 }));
    let refuse = |dma: Pa, size: u64| {
        // SAFETY: `start` weigert vóór hij iets aanraakt.
        unsafe { probe::<F>(&regs).start(dma, size, TEST_MAC, 1, None) }.err()
    };
    assert_eq!(
        refuse(Pa(0x8000_0000), 64),
        Some(Error::DmaTooSmall { need, have: 64 })
    );
    for dma in [Pa(0x8000_0008), Pa(0xFFFF_FF00)] {
        assert_eq!(
            refuse(dma, need),
            Some(Error::DmaPlace { base: dma.0 }),
            "{dma:?}"
        );
    }
    // Alles goed: dan de reset, die hier blijft hangen.
    assert_eq!(
        refuse(Pa(0x8000_0000), need),
        Some(Error::ResetStuck { bus_mode: 1 })
    );
}

#[test]
fn mdio_cmd_puts_the_fields_where_the_table_says_and_masks() {
    let m = F::MII;
    let want = (3 << 21) | (5 << 16) | (0x5 << 8) | (3 << 2) | MII_BUSY;
    assert_eq!(mdio_cmd(&m, 3, 5, 0x5, false), want);
    assert_eq!(mdio_cmd(&m, 3, 5, 0x5, true), (want & !(3 << 2)) | (1 << 2));
    // Te grote adressen worden gemaskeerd, niet doorgeschoven.
    assert_eq!(mdio_cmd(&m, 0xFF, 0, 0, false) >> 21, 0x1F);
    assert_eq!(
        mdio_cmd(&m, 0, 0xFF, 0xFF, false),
        (0x1F << 16) | (0xF << 8) | (3 << 2) | 1
    );
}

#[test]
fn a_busy_mdio_is_a_bus_error_and_the_command_is_right() {
    let regs = mem(size_of::<FakeRegs>());
    let mut p = probe::<F>(&regs);
    // Vrij: het commando gaat erin, maar in RAM klaart BUSY nooit.
    assert_eq!(
        p.read(1, 2),
        Err(driver_mdio::Error::Bus { phy: 1, reg: 2 })
    );
    assert_eq!(p.mdio_state(), mdio_cmd(&F::MII, 1, 2, 0x5, false));
    // Bezet: er wordt niets geschreven.
    assert_eq!(
        p.write(1, 0, 0x1200),
        Err(driver_mdio::Error::Bus { phy: 1, reg: 0 })
    );
    assert_eq!(p.regs().mii_data.read(), 0);
    // De scan ziet een hangende bus als lege bus, niet als PHY.
    assert_eq!(driver_mdio::scan(&mut p), None);
}

// --- de ringen ------------------------------------------------------------

/// De ringen en buffers van generatie `O` binnen `NEED_BYTES`, zonder
/// overlap; elke buffer op eigen cachelines (een veeg van de één raakt de
/// ander niet), en bij een stap van een hele line ook elke descriptor. Dat
/// laatste is de les van 30-07 op de gecachete ring van de LicheeRV: een
/// clean van de één overschreef de DMA-schrijf in de ander.
pub(crate) fn rings_fit<O: Ops>() {
    let base = Pa(0x8000_0000);
    let r = Rings::<O>::at(base);
    let mut spans = Vec::new();
    for i in 0..O::NUM_RX {
        spans.push((r.rx(i).0, O::DESC_STRIDE, true));
        spans.push((r.rx_buf(i).0, O::BUF_SIZE as u64, false));
    }
    for i in 0..O::NUM_TX {
        spans.push((r.tx(i).0, O::DESC_STRIDE, true));
        spans.push((r.tx_buf(i).0, O::BUF_SIZE as u64, false));
    }
    let own_lines = |desc: bool| !desc || O::DESC_STRIDE.is_multiple_of(dev::LINE);
    spans.sort_unstable();
    for w in spans.windows(2) {
        let ((a, alen, adesc), (b, _, bdesc)) = (w[0], w[1]);
        assert!(a + alen <= b, "{a:#x} overlaps {b:#x}");
        if own_lines(adesc) && own_lines(bdesc) {
            assert!(
                (a + alen - 1) / dev::LINE < b / dev::LINE,
                "{a:#x} and {b:#x} share a line"
            );
        }
    }
    for &(a, _, desc) in &spans {
        assert!(
            !own_lines(desc) || Pa(a).is_aligned(dev::LINE),
            "{a:#x} not on a line"
        );
    }
    // De buffers achter de descriptors, en de regio eindigt op de laatste.
    assert!(r.tx(O::NUM_TX - 1).0 + O::DESC_STRIDE <= r.rx_buf.0);
    let (last, len, _) = spans[spans.len() - 1];
    assert_eq!(last + len, base.0 + O::NEED_BYTES);
}

#[test]
fn the_rings_fit_need_bytes() {
    rings_fit::<F>();
}

#[test]
fn bring_up_gives_rx_to_the_dma_and_keeps_tx() {
    let f = running::<F>();
    let n = &f.n;
    for i in 0..F::NUM_RX {
        let d = n.ring.rx(i);
        assert_eq!(dev::read32(d), OWN, "rx {i}");
        assert_eq!(dev::read32(d.add(8)), lo(n.ring.rx_buf(i)));
        let end = dev::read32(d.add(4)) == RING_END;
        assert_eq!(end, i == F::NUM_RX - 1, "ring end on {i}");
    }
    for i in 0..F::NUM_TX {
        assert_eq!(dev::read32(n.ring.tx(i)), 0, "tx {i}");
        assert_eq!(dev::read32(n.ring.tx(i).add(8)), lo(n.ring.tx_buf(i)));
    }
}

// --- RX -------------------------------------------------------------------

/// Het echte receive-pad met een grote bestemming: een lengte uit het device
/// mag nooit bytes uit de naburige DMA-buffer blootgeven.
#[test]
fn receive_bounds_and_recycle() {
    let top = F::RX_LIMIT - FCS_LEN;
    for size in [64, top, top + 1] {
        let mut f = running::<F>();
        let n = &mut f.n;
        frame_in(n, 0, size);
        // De hele buffer én de volgende gevuld: wie te ver leest, ziet 0x5a.
        dev::copy_in(n.ring.rx_buf(0), &vec![0x5a; 2 * F::BUF_SIZE]);
        let mut out = vec![0xccu8; 8192];
        let got = n.receive(&mut out);
        let want = (size <= top).then_some(size);
        assert_eq!(got, want, "{size}");
        let k = got.unwrap_or(0);
        assert!(out[..k].iter().all(|&b| b == 0x5a), "{size}: packet");
        assert!(out[k..].iter().all(|&b| b == 0xcc), "{size}: beyond");
        // Teruggegeven, met het adres er weer in.
        assert_eq!(n.rx_cur, 1, "{size}: not recycled");
        assert_eq!(dev::read32(n.ring.rx(0)), OWN);
        assert_eq!(dev::read32(n.ring.rx(0).add(8)), lo(n.ring.rx_buf(0)));
        assert_eq!(n.stats.rx_bad_len, u64::from(want.is_none()));
    }
}

#[test]
fn a_bad_frame_is_counted_and_skipped_and_an_empty_ring_rings_the_bell() {
    let mut f = running::<F>();
    let n = &mut f.n;
    // 0: foutframe; 1: gesplitst (FIRST zonder LAST); 2: alleen een FCS;
    // 3: goed.
    dev::write32(n.ring.rx(0), ERR | FIRST | LAST | 68);
    dev::write32(n.ring.rx(1), FIRST | 68);
    dev::write32(n.ring.rx(2), FIRST | LAST | FCS_LEN as u32);
    frame_in(n, 3, 64);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), Some(64));
    let s = n.stats;
    assert_eq!((s.rx_errors, s.rx_bad_len, s.rx_frames), (2, 1, 1));
    assert_eq!(s.rx_last_err, FIRST | LAST | FCS_LEN as u32);
    assert_eq!(n.rx_cur, 4);
    // Batching: de bel pas in de flush, of als de ring leeg is, want de
    // pomp flusht alleen na een frame.
    assert_eq!(n.regs().rx_tail.read(), 0);
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(n.regs().rx_tail.read(), lo(n.ring.rx(4)));
    assert_eq!(n.stats.doorbells, 1);
    n.flush();
    assert_eq!(n.stats.doorbells, 1, "no second bell without work");
}

#[test]
fn a_frame_larger_than_the_caller_buffer_is_cut() {
    let mut f = running::<F>();
    let n = &mut f.n;
    frame_in(n, 0, 90);
    dev::copy_in(n.ring.rx_buf(0), &[7; 90]);
    let mut out = [0u8; 50];
    assert_eq!(n.receive(&mut out), Some(50));
    assert!(out.iter().all(|&b| b == 7));
}

#[test]
fn receive_wraps_the_ring() {
    let mut f = running::<F>();
    let n = &mut f.n;
    let mut out = [0u8; 2048];
    for round in 0..2 {
        for i in 0..F::NUM_RX {
            frame_in(n, i, 60 + usize::from(i));
        }
        for i in 0..F::NUM_RX {
            let want = 60 + usize::from(i);
            assert_eq!(n.receive(&mut out), Some(want), "round {round} desc {i}");
        }
        // Rond: terug bij 0, en alles weer van de DMA.
        assert_eq!(n.rx_cur, 0);
        assert_eq!(n.receive(&mut out), None);
    }
    assert_eq!(n.stats.rx_frames, 2 * u64::from(F::NUM_RX));
}

// --- TX -------------------------------------------------------------------

#[test]
fn transmit_batches_until_flush_wraps_and_stops_when_full() {
    let mut f = running::<F>();
    let n = &mut f.n;
    assert_eq!(n.transmit(&[]), Err(TxError::Size(0)));
    let big = [0; F::MAX_FRAME + 1];
    assert_eq!(n.transmit(&big), Err(TxError::Size(F::MAX_FRAME + 1)));
    n.transmit(&[1; 60]).unwrap();
    let d = n.ring.tx(0);
    assert_eq!(dev::read32(d), OWN);
    assert_eq!(dev::read32(d.add(4)), 60);
    // Geen doorbell vóór de flush, één erna, en geen tweede zonder werk.
    assert_eq!(n.regs().tx_tail.read(), 0);
    n.flush();
    assert_eq!(n.regs().tx_tail.read(), lo(n.ring.tx(1)));
    n.flush();
    assert_eq!(n.stats.doorbells, 1);
    // De ring rond: de laatste draagt de ring-end-bit, en de DMA gaf niets
    // terug, dus plek 0 is nog van hem.
    for _ in 1..F::NUM_TX {
        n.transmit(&[2; 60]).unwrap();
    }
    assert_eq!(n.tx_cur, 0);
    assert_eq!(dev::read32(n.ring.tx(F::NUM_TX - 1).add(4)), 60 | RING_END);
    assert_eq!(n.transmit(&[3; 60]), Err(TxError::Full));
    assert_eq!(n.stats.tx_full, 1);
    // De DMA verzond descriptor 0: er is weer plaats.
    dev::write32(d, 0);
    n.transmit(&[4; 60]).unwrap();
    assert_eq!(n.tx_cur, 1);
    assert_eq!(n.stats.tx_frames, u64::from(F::NUM_TX) + 1);
}

/// De kopie in en uit de buffers: byte voor byte het frame, met een staart
/// die niet op 8 of 16 valt, en niets ernaast.
#[test]
fn frames_cross_the_buffers_byte_for_byte() {
    let pattern = |n: usize| (0..n).map(|i| (i * 7 + 3) as u8).collect::<Vec<u8>>();
    for size in [17, 61, F::MAX_FRAME] {
        let mut f = running::<F>();
        let n = &mut f.n;
        let frame = pattern(size);
        n.transmit(&frame).unwrap();
        let mut sent = vec![0u8; F::BUF_SIZE];
        dev::copy_out(&mut sent, n.ring.tx_buf(0));
        assert_eq!(&sent[..size], &frame[..], "tx {size}");
        assert!(sent[size..].iter().all(|&b| b == 0), "tx {size}: beyond");

        dev::write32(n.ring.rx(0), FIRST | LAST | (size + FCS_LEN) as u32);
        dev::copy_in(n.ring.rx_buf(0), &pattern(F::BUF_SIZE));
        let mut out = vec![0xccu8; 2048];
        assert_eq!(n.receive(&mut out), Some(size), "rx {size}");
        assert_eq!(&out[..size], &frame[..], "rx {size}");
        assert!(out[size..].iter().all(|&b| b == 0xcc), "rx {size}: beyond");
    }
}

// --- de interrupt ---------------------------------------------------------

#[test]
fn the_irq_masks_on_ack_and_rearms_when_the_ring_is_empty() {
    static BELL: Signal = Signal::new();
    let mut f = running::<F>();
    let n = &mut f.n;
    // Zonder `set_irq` blijft de RX-interrupt dicht en pollt de pomp.
    assert!(n.irq().is_none());
    assert_eq!(n.regs().intr_ena.read(), 0);
    n.set_irq(&BELL);
    let r = n.regs();
    assert_eq!(r.intr_ena.read(), F::INTR);
    r.status.write(u32::MAX);
    n.irq_ack().ack();
    assert_eq!(r.status.read(), F::STAT, "W1C of the line's bits only");
    assert_eq!(r.intr_ena.read(), 0, "ack leaves the line masked");
    // De pomp leest de ring leeg: het masker gaat weer open, één keer.
    let mut out = [0u8; 64];
    let rearms = n.stats.rearms;
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(r.intr_ena.read(), F::INTR);
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(n.stats.rearms, rearms + 1);
    assert!(n.irq().is_some());
}
