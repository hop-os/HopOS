//! De driver tegen een nep-chip: BAR0, de config-space en de DMA-regio in
//! RAM, en de klok van de test speelt het silicium. Bij elke blik op de klok
//! voert de chip een aangevraagde core-clock-reset uit (en wist daarbij,
//! zoals het echte ijzer, INDIR_ACCESS en de command-bits), en beantwoordt
//! hij MI_COM-transacties uit een eigen PHY-registermap. Het SRAM-venster is
//! gewoon RAM: wat erin geschreven wordt, leest terug. De tests bewijzen de
//! volgorde, de registers die de drie lessen van 29-08 dragen, en de ringen.

use super::*;
use netdev::{Device as _, IrqAck as _};
use std::cell::RefCell;
use std::collections::HashMap;
use std::format;
use std::vec;
use std::vec::Vec;

/// PCI_COMMAND zoals de firmware hem achterliet: memory en bus-master aan,
/// en in de statushelft CAP_LIST.
const CMD: u32 = 0x0010_0006;
/// CHIPREV 0xf ("kijk in het product-ID-register") met rommel in de lage
/// bits die de driver niet terug mag schrijven.
const MISC_AT_POWER_ON: u32 = 0xf000_1234;
const PHY_ID1: u16 = 0x5c0d;
const PHY_ID2: u16 = 0x8a40;
const MAC: [u8; 6] = [0x1c, 0xf6, 0x4c, 0x54, 0xfa, 0x90];

#[derive(Default)]
struct Chip {
    bar0: Pa,
    /// Waar de config-space ligt (de ECAM, of de BAR0-spiegel).
    cfg: Pa,
    now: u64,
    ticks: u64,
    phy: HashMap<u8, u16>,
    phy_writes: Vec<(u8, u16)>,
    /// De link komt op na een AN-herstart.
    link: bool,
    /// De MDIO-master antwoordt nooit.
    mdio_dead: bool,
    resets: u32,
    /// MISC_HOST_CTRL bij de eerste klokslag, en vlak vóór de reset.
    misc_first_tick: Option<u32>,
    misc_at_reset: Option<u32>,
    /// PCI_COMMAND direct na de reset (door de chip gewist).
    cmd_after_reset: Option<u32>,
}

thread_local! {
    static CHIP: RefCell<Chip> = RefCell::new(Chip::default());
}

fn at(base: Pa, off: u64) -> Pa {
    base.add(off)
}

fn now() -> u64 {
    CHIP.with(|c| {
        let mut c = c.borrow_mut();
        c.now += 10_000;
        c.ticks += 1;
        let (bar0, cfg) = (c.bar0, c.cfg);
        if bar0.0 == 0 {
            return c.now;
        }
        let misc = at(cfg, 0x68);
        if c.misc_first_tick.is_none() {
            c.misc_first_tick = Some(dev::read32(misc));
        }
        // De core-clock-reset: hij wist het indirecte venster en de
        // command-bits, en het reset-bit zelf.
        let grc_misc = at(bar0, 0x6804);
        if dev::read32(grc_misc) & GRC_MISC_CFG_CORECLK_RESET != 0 {
            c.resets += 1;
            c.misc_at_reset = Some(dev::read32(misc));
            dev::write32(misc, dev::read32(misc) & MISC_CHIPREV_MASK);
            let cmd = at(cfg, 0x04);
            dev::write32(cmd, dev::read32(cmd) & !0x6);
            c.cmd_after_reset = Some(dev::read32(cmd));
            dev::write32(grc_misc, 0);
        }
        mdio_tick(&mut c, at(bar0, 0x44c));
        c.now
    })
}

/// Eén MI_COM-transactie: de PHY op adres 1 antwoordt uit zijn map, de
/// rest van de bus leest alles-enen. Een AN-herstart geeft de link (als die
/// er is), 1000 Mb/s full duplex in de auxiliary status.
fn mdio_tick(c: &mut Chip, com: Pa) {
    let v = dev::read32(com);
    if v & MI_COM_BUSY == 0 || c.mdio_dead {
        return;
    }
    let phy = ((v >> MI_COM_PHY_SHIFT) & 0x1f) as u8;
    let reg = ((v >> MI_COM_REG_SHIFT) & 0x1f) as u8;
    let mut data = 0xffff;
    if phy == PHY_ADDR {
        if v & MI_COM_CMD_WRITE != 0 {
            let val = (v & 0xffff) as u16;
            c.phy_writes.push((reg, val));
            c.phy.insert(reg, val);
            if reg == mdio::reg::BMCR && val & mdio::BMCR_AN_RESTART != 0 && c.link {
                c.phy
                    .insert(mdio::reg::BMSR, mdio::BMSR_LINK | mdio::BMSR_AN_COMPLETE);
                c.phy.insert(MII_AUX_STAT, 7 << 8);
            }
            data = val;
        } else {
            data = *c.phy.get(&reg).unwrap_or(&0);
        }
    }
    dev::write32(com, u32::from(data));
}

struct Mem {
    _bar: Vec<u64>,
    _cfg: Vec<u64>,
    _dma: Vec<u64>,
    bar0: Pa,
    cfg: Pa,
    dma: Pa,
}

impl Mem {
    fn r(&self, off: u64) -> u32 {
        dev::read32(at(self.bar0, off))
    }

    fn c(&self, off: u64) -> u32 {
        dev::read32(at(self.cfg, off))
    }
}

/// Een chip zoals de firmware hem achterlaat: vendor/device op offset 0,
/// CHIPREV 0xf en 0x57766 in het product-ID, bus-master aan, de
/// firmware-mailbox klaar, en een PHY op adres 1.
fn chip(link: bool) -> Mem {
    let mut bar = vec![0u64; BAR0_LEN as usize / 8];
    let mut cfg = vec![0u64; 0x1000 / 8];
    let mut dma = vec![0u64; (DMA_NEED + DMA_ALIGN) as usize / 8];
    let bar0 = Pa(bar.as_mut_ptr() as usize as u64);
    let cfg_pa = Pa(cfg.as_mut_ptr() as usize as u64);
    let dma_pa = Pa((dma.as_mut_ptr() as usize as u64).next_multiple_of(DMA_ALIGN));
    for base in [bar0, cfg_pa] {
        dev::write32(at(base, 0x00), 0x1682_14e4);
        dev::write32(at(base, 0x04), CMD);
        dev::write32(at(base, 0x68), MISC_AT_POWER_ON);
        dev::write32(at(base, 0xfc), 0x5776_6000);
    }
    // Het SRAM-venster leest de firmware-mailbox: ~MAGIC1, de bootcode is
    // klaar.
    dev::write32(at(bar0, 0x84), !FW_MBOX_MAGIC1);
    let phy = HashMap::from([(mdio::reg::ID1, PHY_ID1), (mdio::reg::ID2, PHY_ID2)]);
    CHIP.with(|c| {
        *c.borrow_mut() = Chip {
            bar0,
            cfg: cfg_pa,
            phy,
            link,
            ..Chip::default()
        };
    });
    Mem {
        _bar: bar,
        _cfg: cfg,
        _dma: dma,
        bar0,
        cfg: cfg_pa,
        dma: dma_pa,
    }
}

fn up(m: &Mem) -> Tg3 {
    // SAFETY: registers, config-space en DMA liggen in `m`, dat de test
    // overleeft.
    unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma, DMA_NEED, now) }.unwrap()
}

/// De driver zonder bring-up, op een lege regio (zoals Go's receive-test).
fn bare(m: &Mem) -> Tg3 {
    Tg3::at(m.bar0, m.cfg, Mac(MAC), m.dma, now)
}

fn chip_state<T>(f: impl FnOnce(&Chip) -> T) -> T {
    CHIP.with(|c| f(&c.borrow()))
}

// ── Reset en de drie lessen van 29-08 ──────────────────────────────────────

#[test]
fn misc_host_ctrl_with_indir_access_is_written_first_and_again_after_reset() {
    let m = chip(false);
    let _n = up(&m);
    let want =
        0xf000_0000 | MISC_MASK_PCI_INT | MISC_WORD_SWAP | MISC_INDIR_ACCESS | MISC_PCISTATE_RW;
    // Bij de eerste klokslag (de slaap na MAC_MODE, vóór de reset) stond het
    // venster al open: de schrijf ging vóór alles wat op tijd wacht.
    let (first, at_reset, resets) = chip_state(|c| (c.misc_first_tick, c.misc_at_reset, c.resets));
    assert_eq!(first, Some(want), "INDIR_ACCESS before anything else");
    assert_eq!(at_reset, Some(want));
    assert_eq!(resets, 1, "one core-clock reset");
    // De reset wiste het bit; de driver zette het terug. De rommel in de lage
    // bits is niet teruggeschreven, CHIPREV wel.
    assert_eq!(m.c(0x68), want, "INDIR_ACCESS restored after reset");
}

#[test]
fn pci_command_is_restored_after_the_core_clock_reset() {
    let m = chip(false);
    let _n = up(&m);
    let wiped = chip_state(|c| c.cmd_after_reset);
    assert_eq!(wiped, Some(CMD & !0x6), "the fake reset cleared bus-master");
    assert_eq!(m.c(0x04), CMD, "memory and bus-master back on");
    assert_eq!(m.c(0x70), PCISTATE_ROM_ENABLE | PCISTATE_ROM_RETRY);
    assert_eq!(m.r(0x4000) & MODE_ENABLE, MODE_ENABLE, "memory arbiter on");
}

#[test]
fn the_std_ring_is_announced_with_its_nic_addr() {
    let m = chip(false);
    let _n = up(&m);
    let ring = m.dma.0 + OFF_RX_STD;
    assert_eq!(m.r(0x2450), (ring >> 32) as u32);
    assert_eq!(m.r(0x2454), ring as u32);
    assert_eq!(m.r(0x2458), 512 << 16 | 1536 << 2, "ring size and DMA len");
    // De derde les: RCVDBDI_STD_BD + TG3_BDINFO_NIC_ADDR.
    assert_eq!(
        m.r(0x245c),
        0x6000,
        "NIC_ADDR written (57766 is no 5717_PLUS)"
    );
    assert_eq!(m.r(0x2448), BDINFO_FLAGS_DISABLED, "no jumbo ring");
    assert_eq!(m.r(0x026c), 511, "all buffers offered");
}

#[test]
fn new_leaves_the_chip_configured_and_running() {
    let m = chip(false);
    let n = up(&m);
    assert_eq!(n.asic_rev(), 0x57766);
    assert_eq!(n.mac(), Mac(MAC));
    // De swap-bits (29-08: ze stonden uit) en de host-bits van de init.
    let grc = m.r(0x6800);
    assert_eq!(grc & 0x34, 0x34, "BSWAP/WSWAP_DATA and WSWAP_NONFRM");
    assert_ne!(grc & GRC_MODE_HOST_SENDBDS, 0);
    // De data-engines in MAC_MODE.
    assert_eq!(m.r(0x0400) & MAC_MODE_RUN, MAC_MODE_RUN);
    // Het MAC, de MTU, het multicast-filter.
    assert_eq!(m.r(0x0410), 0x1cf6);
    assert_eq!(m.r(0x0414), 0x4c54_fa90);
    assert_eq!(m.r(0x043c), 1522);
    for i in 0..4 {
        assert_eq!(m.r(0x0470 + i * 4), u32::MAX);
    }
    // Status-blok, interrupts dicht, MDIO zonder auto-poll.
    assert_eq!(m.r(0x3c3c), (m.dma.0 + OFF_STATUS) as u32);
    assert_eq!(m.r(0x3c38), ((m.dma.0 + OFF_STATUS) >> 32) as u32);
    assert_eq!(m.r(0x3c00), MODE_ENABLE | HOSTCC_MODE_32BYTE);
    assert_eq!(m.r(0x0204), 1, "interrupt mailbox masked");
    assert_eq!(m.r(0x0454), MI_MODE_BASE);
    // De 57765-afstemming.
    assert_eq!(m.r(0x7c0c) & 0xff, PCIE_DL_LO_FTSMAX_VAL);
    assert_eq!(m.r(0x6c) & 0xff00_0001, 0x7600_0001, "DMA_RW_CTRL");
    // De engines en de MAC.
    assert_eq!(
        m.r(0x4c00),
        MODE_ENABLE | DMAC_ERR_ENAB | WDMAC_STATUS_TAG_FIX
    );
    assert_eq!(m.r(0x045c), TX_MODE_ENABLE | TX_MODE_MBUF_LOCKUP_FIX);
    assert_eq!(m.r(0x0468), RX_MODE_ENABLE | RX_MODE_IPV6_CSUM);
    assert_eq!(m.r(0x2400), MODE_ENABLE | RCVDBDI_INV_RING_SZ);
}

#[test]
fn producer_descriptors_carry_rxd_flag_end_and_their_own_buffer() {
    let m = chip(false);
    let _n = up(&m);
    for i in 0..u64::from(RX_STD_RING) {
        let d = m.dma.add(OFF_RX_STD + i * RX_BD);
        let buf = m.dma.0 + BUF_OFF + i * BUF_SIZE;
        assert_eq!(dev::read32(d.add(12)), RXD_FLAG_END, "type_flags of {i}");
        assert_eq!(dev::read32(d.add(8)), RX_DMA_SIZE);
        assert_eq!(dev::read32(d.add(4)), buf as u32);
        assert_eq!(dev::read32(d), (buf >> 32) as u32);
        assert_eq!(dev::read32(d.add(28)), i as u32 | 0x1_0000, "opaque");
    }
}

#[test]
fn the_bar0_mirror_serves_as_config_space_without_ecam() {
    let m = chip(false);
    CHIP.with(|c| c.borrow_mut().cfg = m.bar0);
    // SAFETY: registers en DMA liggen in `m`.
    let n = unsafe { Tg3::new(m.bar0, Pa(0), Mac(MAC), m.dma, DMA_NEED, now) }.unwrap();
    assert_ne!(m.r(0x68) & MISC_INDIR_ACCESS, 0);
    assert_eq!(m.r(0x04), CMD);
    assert_eq!(m.c(0x68), MISC_AT_POWER_ON, "the ECAM copy was not touched");
    assert_eq!(n.asic_rev(), 0x57766);
}

#[test]
fn refusals_are_named() {
    let m = chip(false);
    // SAFETY: de toets faalt vóór er een register wordt aangeraakt.
    let e = unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma, DMA_NEED - 1, now) }.err();
    assert_eq!(
        e,
        Some(Error::Dma {
            base: m.dma.0,
            size: DMA_NEED - 1
        })
    );
    // SAFETY: idem.
    let e = unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma.add(8), DMA_NEED, now) }.err();
    assert!(matches!(e, Some(Error::Dma { .. })));

    // Na de reset geen Broadcom-id: de chip antwoordt niet.
    let m = chip(false);
    dev::write32(at(m.bar0, 0), 0xffff_ffff);
    // SAFETY: alles ligt in `m`.
    let e = unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::NoAnswer { id: u32::MAX }));

    // Niemand thuis op adres 1.
    let m = chip(false);
    CHIP.with(|c| c.borrow_mut().phy.clear());
    // SAFETY: alles ligt in `m`.
    let e = unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma, DMA_NEED, now) }.err();
    assert_eq!(e, Some(Error::NoPhy { id: 0 }));

    // Een MDIO-master die nooit afrondt.
    let m = chip(false);
    CHIP.with(|c| c.borrow_mut().mdio_dead = true);
    // SAFETY: alles ligt in `m`.
    let e = unsafe { Tg3::new(m.bar0, m.cfg, Mac(MAC), m.dma, DMA_NEED, now) }.err();
    assert_eq!(
        e,
        Some(Error::Mdio {
            phy: 1,
            reg: 2,
            write: false
        })
    );
    let s = format!("{}", e.unwrap());
    assert!(s.starts_with("tg3: MDIO read phy 1 reg 2"), "{s}");
}

// ── De link ─────────────────────────────────────────────────────────────────

#[test]
fn the_link_restarts_autoneg_and_sets_the_port_mode() {
    let m = chip(true);
    let mut n = up(&m);
    n.start_link().unwrap();
    assert_eq!(
        n.link_now(),
        Ok(Some(Link {
            mbps: 1000,
            full_duplex: true
        }))
    );
    let writes = chip_state(|c| c.phy_writes.clone());
    assert!(writes.contains(&(
        mdio::reg::BMCR,
        mdio::BMCR_AN_ENABLE | mdio::BMCR_AN_RESTART
    )));
    let mode = m.r(0x0400);
    assert_eq!(mode & MAC_MODE_PORT_MASK, MAC_MODE_PORT_GMII);
    assert_eq!(mode & MAC_MODE_HALF_DUPLEX, 0);
    assert_eq!(mode & MAC_MODE_RUN, MAC_MODE_RUN, "engines stay on");
}

#[test]
fn no_cable_is_no_link_and_no_wait() {
    let m = chip(false);
    let mut n = up(&m);
    n.start_link().unwrap();
    assert_eq!(n.link_now(), Ok(None));
    assert_eq!(netdev::Device::link(&mut n), None);
}

#[test]
fn the_aux_status_table_is_tg3s() {
    let want = [
        (1, 10, false),
        (2, 10, true),
        (3, 100, false),
        (5, 100, true),
        (6, 1000, false),
        (7, 1000, true),
    ];
    for (code, mbps, full_duplex) in want {
        assert_eq!(
            decode_aux(code << 8),
            Ok(Link { mbps, full_duplex }),
            "code {code}"
        );
    }
    assert_eq!(decode_aux(4 << 8), Err(Error::LinkState { aux: 0x400 }));
    assert_eq!(decode_aux(0), Err(Error::LinkState { aux: 0 }));
}

// ── RX ──────────────────────────────────────────────────────────────────────

/// Zet één return-descriptor klaar op plek `slot`.
fn ret_desc(m: &Mem, slot: u64, size: u32, index: u32) {
    let d = m.dma.add(OFF_RX_RET + slot * RX_BD);
    dev::write32(d.add(8), size);
    dev::write32(d.add(28), index | 0x1_0000);
}

fn set_rx_producer(m: &Mem, prod: u32) {
    let w = m.dma.add(OFF_STATUS + STATUS_IDX0);
    dev::write32(w, (dev::read32(w) & 0xffff_0000) | prod);
}

/// Go's `TestReceiveBoundsPayloadAndRecycle`: een geldige lengte komt terug
/// zonder FCS, een te grote lengte of een index buiten de ring leest niets,
/// en de descriptor gaat in elk geval terug naar de chip.
#[test]
fn receive_bounds_payload_and_recycle() {
    let cases = [
        ("valid", 0, 68, Some(64)),
        ("last-buffer", RX_STD_RING - 1, 68, Some(64)),
        ("largest", 0, RX_DMA_SIZE, Some(RX_DMA_SIZE as usize - 4)),
        ("oversize", 0, RX_DMA_SIZE + 1, None),
        ("invalid-index", RX_STD_RING, 68, None),
        ("short", 0, 3, None),
        ("runt", 0, 17, None),
        ("huge-descriptor", 0, 0xffff, None),
    ];
    for (name, index, size, want) in cases {
        let m = chip(false);
        let mut n = bare(&m);
        set_rx_producer(&m, 1);
        ret_desc(&m, 0, size, index);
        if index < RX_STD_RING {
            let buf = m.dma.add(BUF_OFF + u64::from(index) * BUF_SIZE);
            dev::copy_in(buf, &[0x5a; BUF_SIZE as usize]);
        }
        let mut out = vec![0xccu8; 8192];
        let got = n.receive(&mut out);
        assert_eq!(got, want, "{name}");
        let got = got.unwrap_or(0);
        assert!(
            out[..got].iter().all(|&b| b == 0x5a),
            "{name}: packet differs"
        );
        assert!(
            out[got..].iter().all(|&b| b == 0xcc),
            "{name}: destination changed beyond packet"
        );
        assert_eq!((n.rx_ret_idx, n.rx_std_idx), (1, 1), "{name}: not recycled");
        assert_eq!(n.stats.rx_bad, u64::from(want.is_none()), "{name}");
        // De mailboxen wachten op de flush (één doorbell per burst).
        assert_eq!(m.r(0x026c), 0, "{name}: mailbox before flush");
        n.flush();
        assert_eq!((m.r(0x026c), m.r(0x0284)), (1, 1), "{name}: mailboxes");
    }
}

#[test]
fn a_frame_larger_than_the_callers_buffer_is_dropped_not_overrun() {
    let m = chip(false);
    let mut n = bare(&m);
    set_rx_producer(&m, 1);
    ret_desc(&m, 0, 100, 0);
    let mut out = [0xccu8; 32];
    assert_eq!(n.receive(&mut out), None);
    assert!(out.iter().all(|&b| b == 0xcc));
    assert_eq!((n.rx_ret_idx, n.stats.rx_bad), (1, 1));
}

#[test]
fn error_flags_drop_the_frame_but_odd_nibble_does_not() {
    let m = chip(false);
    let mut n = bare(&m);
    set_rx_producer(&m, 3);
    for slot in 0..3 {
        ret_desc(&m, slot, 68, slot as u32);
    }
    let d = |slot: u64| m.dma.add(OFF_RX_RET + slot * RX_BD);
    dev::write32(d(0).add(12), RXD_FLAG_ERROR);
    dev::write32(d(1).add(20), 0x0001_0000); // in RXD_ERR_MASK
    dev::write32(d(2).add(20), 0x0010_0000); // ODD_NIBBLE_RCVD_MII: geen fout
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), Some(64), "the third frame");
    assert_eq!((n.rx_ret_idx, n.stats.rx_bad), (3, 2));
    assert_eq!(n.receive(&mut out), None);
}

#[test]
fn a_producer_outside_the_ring_reads_as_empty() {
    let m = chip(false);
    let mut n = bare(&m);
    set_rx_producer(&m, 0xffff);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(n.rx_ret_idx, 0);
}

#[test]
fn a_received_frame_comes_back_without_its_fcs() {
    let m = chip(false);
    let mut n = up(&m);
    let frame: Vec<u8> = (0..64u8).collect();
    let buf5 = m.dma.add(BUF_OFF + 5 * BUF_SIZE);
    dev::copy_in(buf5, &frame);
    dev::copy_in(buf5.add(64), &[0xde, 0xad, 0xbe, 0xef]); // de FCS
    ret_desc(&m, 0, 68, 5);
    set_rx_producer(&m, 1);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), Some(64));
    assert_eq!(&out[..64], &frame[..]);
    assert_eq!(out[64], 0, "no FCS bytes behind the frame");
    n.flush();
    assert_eq!(m.r(0x0284), 1, "return consumer");
    assert_eq!(m.r(0x026c), 0, "std producer one further (511 + 1)");
}

#[test]
fn rx_flushes_itself_every_32_frames() {
    let m = chip(false);
    let mut n = bare(&m);
    for slot in 0..40 {
        ret_desc(&m, slot, 68, slot as u32);
    }
    set_rx_producer(&m, 40);
    let mut out = [0u8; 2048];
    for _ in 0..31 {
        assert_eq!(n.receive(&mut out), Some(64));
    }
    assert_eq!(m.r(0x0284), 0, "still deferred");
    assert_eq!(n.receive(&mut out), Some(64));
    assert_eq!((m.r(0x0284), m.r(0x026c)), (32, 32), "self-flush");
}

// ── De interrupt ────────────────────────────────────────────────────────────

fn set_tag(m: &Mem, tag: u32) {
    dev::write32(m.dma.add(OFF_STATUS + STATUS_TAG), tag);
}

/// `tg3_enable_ints` met TAGGED_STATUS: MASK_PCI_INT eraf, de tag in de
/// mailbox, en één gedwongen interrupt (HOSTCC_MODE_NOW).
#[test]
fn set_irq_opens_on_the_tag_and_forces_one_interrupt() {
    static BELL: Signal = Signal::new();
    let m = chip(false);
    let mut n = up(&m);
    assert!(n.irq().is_none(), "polled until set_irq");
    assert_eq!(m.c(0x68) & MISC_MASK_PCI_INT, MISC_MASK_PCI_INT);
    set_tag(&m, 0x2a);
    n.set_irq(&BELL);
    assert!(n.irq().is_some());
    let misc = m.c(0x68);
    assert_eq!(misc & MISC_MASK_PCI_INT, 0, "INTA# may be driven");
    assert_eq!(misc & MISC_TAGGED_STATUS, MISC_TAGGED_STATUS);
    assert_eq!(
        misc & MISC_INDIR_ACCESS,
        MISC_INDIR_ACCESS,
        "the window stays"
    );
    assert_eq!(m.r(0x0204), 0x2a << 24, "open on the current tag");
    assert_eq!(m.r(0x3c00), COAL_IRQ | HOSTCC_MODE_NOW);
    n.clear_irq();
    assert!(n.irq().is_none());
    assert_eq!(m.c(0x68) & MISC_MASK_PCI_INT, MISC_MASK_PCI_INT);
    assert_eq!(m.r(0x0204), 1, "masked again");
    assert_eq!(
        m.r(0x3c00),
        MODE_ENABLE | HOSTCC_MODE_32BYTE,
        "the polled mode"
    );
}

/// De ack maskeert; de pomp leest, en pas de lege ring heropent, op de tag
/// die vóór de laatste blik gelezen is (`tg3_poll`). Zolang er werk is,
/// blijft de mailbox dicht.
#[test]
fn the_ack_masks_and_only_an_empty_ring_reopens_on_the_tag() {
    static BELL: Signal = Signal::new();
    let m = chip(false);
    let mut n = up(&m);
    n.set_irq(&BELL);
    n.irq_ack().ack();
    assert_eq!(m.r(0x0204), 1, "masked by the ack: INTA# drops");

    // Twee frames, de chip zette tag 7.
    for slot in 0..2 {
        ret_desc(&m, slot, 68, slot as u32);
    }
    set_rx_producer(&m, 2);
    set_tag(&m, 7);
    dev::write32(at(m.bar0, 0x3c00), COAL_IRQ);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), Some(64));
    assert_eq!(n.receive(&mut out), Some(64));
    assert_eq!(m.r(0x0204), 1, "work left: still masked");
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(m.r(0x0204), 7 << 24, "empty: open on tag 7");
    assert_eq!(m.r(0x3c00), COAL_IRQ, "nothing pending, no forced update");

    // De tag loopt rond in de bovenste byte.
    set_tag(&m, 0x1ff);
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(m.r(0x0204), 0xff << 24);
}

/// Gepold blijft de mailbox dicht, ook op een lege ring.
#[test]
fn a_polled_empty_ring_leaves_the_mailbox_masked() {
    let m = chip(false);
    let mut n = up(&m);
    set_tag(&m, 3);
    let mut out = [0u8; 2048];
    assert_eq!(n.receive(&mut out), None);
    assert_eq!(m.r(0x0204), 1);
    assert_eq!(m.r(0x3c00), MODE_ENABLE | HOSTCC_MODE_32BYTE);
}

// ── TX ──────────────────────────────────────────────────────────────────────

#[test]
fn transmit_puts_the_length_in_the_descriptor_and_rings_on_flush() {
    let m = chip(false);
    let mut n = up(&m);
    let frame: Vec<u8> = (0..100u8).collect();
    n.transmit(&frame).unwrap();
    let d = m.dma.add(OFF_TX_BD);
    let buf = m.dma.0 + BUF_OFF + u64::from(RX_STD_RING) * BUF_SIZE;
    assert_eq!(dev::read32(d), (buf >> 32) as u32);
    assert_eq!(dev::read32(d.add(4)), buf as u32);
    assert_eq!(dev::read32(d.add(8)), 100 << 16 | TXD_FLAG_END);
    assert_eq!(dev::read32(d.add(12)), 0);
    let mut copy = [0u8; 100];
    dev::copy_out(&mut copy, Pa(buf));
    assert_eq!(&copy[..], &frame[..]);
    assert_eq!(m.r(0x0304), 0, "doorbell deferred");
    n.flush();
    assert_eq!(m.r(0x0304), 1, "send producer");
    assert_eq!(n.stats.doorbells, 1);
    n.flush();
    assert_eq!(n.stats.doorbells, 1, "nothing new, no doorbell");
}

#[test]
fn transmit_refuses_bad_sizes_and_a_full_ring() {
    let m = chip(false);
    let mut n = bare(&m);
    assert_eq!(n.transmit(&[]), Err(TxError::Size(0)));
    assert_eq!(n.transmit(&[0; 2049]), Err(TxError::Size(2049)));
    // De NIC staat op 1: de volgende plek (1) is nog niet vrij.
    dev::write32(m.dma.add(OFF_STATUS + STATUS_IDX0), 1 << 16);
    assert_eq!(n.transmit(&[0; 60]), Err(TxError::Full));
    assert_eq!(n.stats.tx_full, 1);
    dev::write32(m.dma.add(OFF_STATUS + STATUS_IDX0), 0);
    assert_eq!(n.transmit(&[0; 60]), Ok(()));
    // Een volle ring belt eerst wat klaarstaat.
    dev::write32(m.dma.add(OFF_STATUS + STATUS_IDX0), 2 << 16);
    assert_eq!(n.transmit(&[0; 60]), Err(TxError::Full));
    assert_eq!(m.r(0x0304), 1, "pending frame rung before refusing");
}

// ── Diagnose ────────────────────────────────────────────────────────────────

#[test]
fn describe_is_one_line() {
    let m = chip(false);
    let n = up(&m);
    let d = format!("{}", n.describe());
    assert!(d.contains("ASIC 0x57766"), "{d}");
    assert!(d.contains("mac 1c:f6:4c:54:fa:90"), "{d}");
    assert!(d.contains("fw_mbox 0xb49a89ab"), "{d}");
}

#[test]
fn asic_rev_reads_chiprev_unless_it_says_look_elsewhere() {
    let m = chip(false);
    let n = bare(&m);
    assert_eq!(n.asic_rev(), 0x57766);
    dev::write32(at(m.cfg, 0x68), 0x5000_0000);
    assert_eq!(n.asic_rev(), 0x5);
}

#[test]
fn only_the_proven_device_is_driven() {
    assert!(drives(0x14e4, 0x1682));
    assert!(!drives(0x14e4, 0x1686));
    assert!(!drives(0x10ec, 0x1682));
}
