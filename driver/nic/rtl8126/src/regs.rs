//! Het registerblok van de RTL8125/8126 (r8169_main.c, `enum
//! rtl_registers` en `rtl8125_registers`) op BAR2, met een assertie per
//! offset, en de bitwaarden die de driver schrijft.

use core::mem::{offset_of, size_of};
use dev::Reg;

/// Het MMIO-blok. BAR0 is de I/O-alias; BAR2 is dit.
#[repr(C)]
pub(crate) struct Regs {
    /// MAC-adres [3:0].
    pub(crate) mac0: Reg<u32>,
    /// MAC-adres [5:4].
    pub(crate) mac4: Reg<u32>,
    /// Multicast-filter, twee woorden.
    pub(crate) mar0: Reg<u32>,
    pub(crate) mar4: Reg<u32>,
    _r0: [u8; 0x10],
    pub(crate) tx_desc_lo: Reg<u32>,
    pub(crate) tx_desc_hi: Reg<u32>,
    _r1: [u8; 0x0c],
    /// INT_CFG0_8125.
    pub(crate) int_cfg0: Reg<u8>,
    _r2: [u8; 2],
    pub(crate) chip_cmd: Reg<u8>,
    /// IntrMask_8125 (32 bit).
    pub(crate) intr_mask: Reg<u32>,
    /// IntrStatus_8125 (32 bit, write-1-to-clear).
    pub(crate) intr_status: Reg<u32>,
    pub(crate) tx_config: Reg<u32>,
    pub(crate) rx_config: Reg<u32>,
    _r3: [u8; 8],
    pub(crate) cfg9346: Reg<u8>,
    _r4: u8,
    pub(crate) config1: Reg<u8>,
    pub(crate) config2: Reg<u8>,
    pub(crate) config3: Reg<u8>,
    _r5: u8,
    pub(crate) config5: Reg<u8>,
    _r6: [u8; 0x15],
    pub(crate) phy_status: Reg<u32>,
    /// ERI-data (de DASH-handdruk van de 8125BP).
    pub(crate) eridr: Reg<u32>,
    /// ERI-adres en commando.
    pub(crate) eriar: Reg<u32>,
    _r7: [u8; 2],
    /// INT_CFG1_8125.
    pub(crate) int_cfg1: Reg<u16>,
    _r8: [u8; 4],
    /// EPHY-toegang (de tabel van de 8125B).
    pub(crate) ephyar: Reg<u32>,
    _r9: [u8; 0x0c],
    /// TxPoll_8125 (16 bit): bit 0 = queue 0.
    pub(crate) tx_poll: Reg<u16>,
    _r10: [u8; 0x1e],
    /// MAC-OCP.
    pub(crate) ocpdr: Reg<u32>,
    _r11: [u8; 4],
    /// PHY-OCP.
    pub(crate) gphy_ocp: Reg<u32>,
    _r12: [u8; 0x17],
    pub(crate) mcu: Reg<u8>,
    _r13: [u8; 4],
    /// Bit 1 = nieuw RX-descriptorformaat (VER_70/80).
    pub(crate) rx_desc_fmt: Reg<u8>,
    _r14: u8,
    pub(crate) rx_max_size: Reg<u16>,
    _r15: [u8; 4],
    pub(crate) cplus_cmd: Reg<u16>,
    pub(crate) intr_mitig: Reg<u16>,
    pub(crate) rx_desc_lo: Reg<u32>,
    pub(crate) rx_desc_hi: Reg<u32>,
    _r16: [u8; 4],
    pub(crate) misc: Reg<u32>,
    _r17: [u8; 0x28e],
    pub(crate) r0382: Reg<u16>,
    _r18: [u8; 0x67c],
    /// De interrupt-mitigation-tabel, 0xa00 tot 0xa80 of 0xb00.
    pub(crate) mitig: [Reg<u32>; 64],
    _r19: [u8; 0xd80],
    pub(crate) r1880: Reg<u16>,
    _r20: [u8; 0x15e],
    /// MAC0_BKP: het adres uit de EEPROM of eFuse, in draadvolgorde.
    pub(crate) mac0_bkp: [Reg<u8>; 6],
    _r21: [u8; 0x2b1a],
    pub(crate) rss_ctrl: Reg<u32>,
    _r22: [u8; 0x2fc],
    pub(crate) qnum_ctrl: Reg<u16>,
}

const _: () = {
    assert!(offset_of!(Regs, mac0) == 0x00);
    assert!(offset_of!(Regs, mac4) == 0x04);
    assert!(offset_of!(Regs, mar0) == 0x08);
    assert!(offset_of!(Regs, mar4) == 0x0c);
    assert!(offset_of!(Regs, tx_desc_lo) == 0x20);
    assert!(offset_of!(Regs, tx_desc_hi) == 0x24);
    assert!(offset_of!(Regs, int_cfg0) == 0x34);
    assert!(offset_of!(Regs, chip_cmd) == 0x37);
    assert!(offset_of!(Regs, intr_mask) == 0x38);
    assert!(offset_of!(Regs, intr_status) == 0x3c);
    assert!(offset_of!(Regs, tx_config) == 0x40);
    assert!(offset_of!(Regs, rx_config) == 0x44);
    assert!(offset_of!(Regs, cfg9346) == 0x50);
    assert!(offset_of!(Regs, config1) == 0x52);
    assert!(offset_of!(Regs, config2) == 0x53);
    assert!(offset_of!(Regs, config3) == 0x54);
    assert!(offset_of!(Regs, config5) == 0x56);
    assert!(offset_of!(Regs, phy_status) == 0x6c);
    assert!(offset_of!(Regs, eridr) == 0x70);
    assert!(offset_of!(Regs, eriar) == 0x74);
    assert!(offset_of!(Regs, int_cfg1) == 0x7a);
    assert!(offset_of!(Regs, ephyar) == 0x80);
    assert!(offset_of!(Regs, tx_poll) == 0x90);
    assert!(offset_of!(Regs, ocpdr) == 0xb0);
    assert!(offset_of!(Regs, gphy_ocp) == 0xb8);
    assert!(offset_of!(Regs, mcu) == 0xd3);
    assert!(offset_of!(Regs, rx_desc_fmt) == 0xd8);
    assert!(offset_of!(Regs, rx_max_size) == 0xda);
    assert!(offset_of!(Regs, cplus_cmd) == 0xe0);
    assert!(offset_of!(Regs, intr_mitig) == 0xe2);
    assert!(offset_of!(Regs, rx_desc_lo) == 0xe4);
    assert!(offset_of!(Regs, rx_desc_hi) == 0xe8);
    assert!(offset_of!(Regs, misc) == 0xf0);
    assert!(offset_of!(Regs, r0382) == 0x382);
    assert!(offset_of!(Regs, mitig) == 0xa00);
    assert!(offset_of!(Regs, r1880) == 0x1880);
    assert!(offset_of!(Regs, mac0_bkp) == 0x19e0);
    assert!(offset_of!(Regs, rss_ctrl) == 0x4500);
    assert!(offset_of!(Regs, qnum_ctrl) == 0x4800);
    assert!(size_of::<Regs>() == 0x4804);
};

// ChipCmd.
pub(crate) const CMD_RESET: u8 = 0x10;
pub(crate) const CMD_RX_ENB: u8 = 0x08;
pub(crate) const CMD_TX_ENB: u8 = 0x04;
pub(crate) const CMD_STOP_REQ: u8 = 0x80;
// Cfg9346.
pub(crate) const CFG_UNLOCK: u8 = 0xc0;
pub(crate) const CFG_LOCK: u8 = 0x00;
// MCU.
pub(crate) const MCU_NOW_IS_OOB: u8 = 0x80;
pub(crate) const MCU_RXTX_EMPTY: u8 = 0x30;
pub(crate) const MCU_LINK_LIST_OK: u8 = 0x02;
// MISC.
pub(crate) const MISC_RXDV_GATED: u32 = 1 << 19;
/// RxConfig: RX_FETCH_DFLT_8125 (8 << 27) | RX_DMA_BURST (7 << 8) |
/// RX_PAUSE_SLOT_ON (1 << 11); de accept-bits [5:0] komen erbij.
pub(crate) const RX_CFG_BASE: u32 = 0x4000_0f00;
pub(crate) const RX_ACCEPT_MASK: u32 = 0x3f;
pub(crate) const RX_ACCEPT_OK_MASK: u32 = 0x0f;
/// Broadcast, multicast en het eigen adres.
pub(crate) const RX_ACCEPT_DEFAULT: u32 = 0x08 | 0x04 | 0x02;
/// TxConfig: TX_DMA_BURST 7 << 8 | InterFrameGap 3 << 24.
pub(crate) const TX_CFG: u32 = 0x0300_0700;
/// CPlusCmd: de bits die mainline bewaart (CPCMD_MASK).
pub(crate) const CPCMD_MASK: u16 = 0x2063;
/// De vlag van OCPDR en GPHY_OCP: schrijf, of bezig.
pub(crate) const OCP_FLAG: u32 = 0x8000_0000;

// Interrupt-bits (32 bit op de 8125-familie; de lage 16 zijn de klassieke).
pub(crate) const INT_RX_OK: u32 = 0x0001;
pub(crate) const INT_RX_ERR: u32 = 0x0002;
pub(crate) const INT_TX_OK: u32 = 0x0004;
pub(crate) const INT_TX_ERR: u32 = 0x0008;
pub(crate) const INT_RX_OVERFLOW: u32 = 0x0010;
pub(crate) const INT_LINK_CHG: u32 = 0x0020;
pub(crate) const INT_RX_FIFO_OVER: u32 = 0x0040;
/// Wat de pomp wekt aan de ontvangstkant.
pub(crate) const INT_RX: u32 =
    INT_RX_OK | INT_RX_ERR | INT_RX_OVERFLOW | INT_LINK_CHG | INT_RX_FIFO_OVER;
/// En aan de zendkant: een descriptor is klaar (r8169 `TxOK | TxErr`).
pub(crate) const INT_TX: u32 = INT_TX_OK | INT_TX_ERR;

// Descriptor-bits (opts1).
pub(crate) const DESC_OWN: u32 = 1 << 31;
pub(crate) const RING_END: u32 = 1 << 30;
pub(crate) const FIRST_FRAG: u32 = 1 << 29;
pub(crate) const LAST_FRAG: u32 = 1 << 28;
/// Receive error summary.
pub(crate) const RX_RES: u32 = 1 << 21;
pub(crate) const RX_LEN_MASK: u32 = 0x3fff;

// PHY-registers in de OCP-ruimte: clause 22 op 0xa400 + 2 · reg, en de
// Realtek-uitbreidingen.
pub(crate) const PHY_C22_BASE: u16 = 0xa400;
pub(crate) const PHY_PHYSR: u16 = 0xa434;
/// 10GBT_CTRL: de 2.5G/5G-advertentie.
pub(crate) const PHY_NBASET: u16 = 0xa5d4;
/// BMCR: soft-reset (zelfwissend). De gedeelde laag (`driver-mdio`) kent
/// alleen de autoneg-bits; de reset, isolate en power-down gebruikt alleen
/// deze PHY-config, dus ze staan hier.
pub(crate) const BMCR_RESET: u16 = 1 << 15;
/// BMCR: power-down.
pub(crate) const BMCR_POWER_DOWN: u16 = 1 << 11;
/// BMCR: de MII isoleren.
pub(crate) const BMCR_ISOLATE: u16 = 1 << 10;

/// Clause 22 register `r` als OCP-adres.
pub(crate) const fn phy_c22(r: u8) -> u16 {
    PHY_C22_BASE + 2 * r as u16
}

/// Eén descriptor: opts1 (op 0), opts2, adres laag, adres hoog.
pub(crate) const DESC: u64 = 16;
pub(crate) const DESC_OPTS2: u64 = 4;
pub(crate) const DESC_ADDR_LO: u64 = 8;
pub(crate) const DESC_ADDR_HI: u64 = 12;
