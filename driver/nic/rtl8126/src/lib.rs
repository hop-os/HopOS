//! De Realtek RTL8126A (10ec:8126, 5GbE, Orion O6) en de RTL8125-familie
//! (10ec:8125, 2,5GbE, Orion O6N): de LAN-poorten van de Radxa Orions.
//!
//! Geschreven naar de Linux r8169-driver (`r8169_main.c`, mainline
//! 09-09-2026, `RTL_GIGA_MAC_VER_70`), kruislings getoetst aan Realteks
//! eigen r8126 waar mainline zwijgt; de Go-voorganger is
//! `OLD/metal/driver/nic/rtl8126`, en het registerrecept staat regel voor
//! regel in `docs/v1/archief/rtl8126-recept.md` van de Go-boom.
//!
//! Vorm: één RX- en één TX-ring met de klassieke 16-byte-descriptors
//! (opts1, opts2, adres), geen offloads, geen PHY-firmware-blob. Wie
//! `&mut self` heeft, is de enige die de ringen aanraakt. De firmware (UEFI)
//! wees de BAR's toe; het board geeft BAR2 (het MMIO-blok; BAR0 is de
//! I/O-alias) en zet bus-mastering aan. De driver kent geen PCI.
//!
//! Volgorde, zoals mainline (probe, open, phy_start): [`Rtl8126::new`]
//! (chip-id, `hw_init`, `hw_reset`, MAC, ringen, `hw_start`), dan
//! [`Rtl8126::start_link`] (PHY aan, autoneg), en de link in de taak
//! ([`netdev::Device::link`], één blik per keer; de MAC volgt de PHY zelf).
//!
//! De PHY zit achter de PHY-OCP-ruimte: clause 22 op 0xa400 + 2 · reg; de
//! registernamen en bits komen uit de gedeelde laag (`driver-mdio`).
//!
//! De interrupt heeft een eigen les, en die staat bij [`IrqAck::ack`] en
//! `rearm`: masker dicht bij de ack, en pas weer open als de pomp de ring
//! leeg las ([`receive`](netdev::Device::receive)), met een eigen blik op
//! de ring. Op de lijn: RX en de TX-completion (r8169's `irq_mask` met
//! TxOK en TxErr).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod regs;
mod variant;

use core::fmt;
use core::mem::size_of;
use dev::Pa;
use driver_mdio::{self as mdio, Link};
use netdev::{Mac, TxError};
use regs::*;
use sync::Signal;
use variant::{Step, Variant};

/// Realtek.
pub const VENDOR: u16 = 0x10ec;
/// RTL8126A 5GbE (Orion O6).
pub const DEVICE_8126: u16 = 0x8126;
/// RTL8125B/D/CP/BP 2,5GbE (Orion O6N).
pub const DEVICE_8125: u16 = 0x8125;

/// Drijft deze driver `vendor:device`? Welke variant het is, beslist de
/// chip-XID bij de reset.
#[must_use]
pub fn supported(vendor: u16, device: u16) -> bool {
    vendor == VENDOR && (device == DEVICE_8126 || device == DEVICE_8125)
}

/// Hoeveel van BAR2 het board moet mappen.
pub const MMIO_LEN: u64 = size_of::<Regs>() as u64;

/// Eén RX- of TX-buffer: boven 1522 plus marge.
pub const BUF_SIZE: usize = 2048;
/// RX-descriptors: 256 diep, voor een 5GbE-poort die tussen twee rondes
/// van de pomp 512 KB moet kunnen bufferen.
pub const N_RX: u16 = 256;
/// TX-descriptors.
pub const N_TX: u16 = 64;
/// De ringen: 256-byte-gealigneerd (eis van het silicium), elk op een eigen
/// pagina.
const RX_RING_OFF: u64 = 0;
const TX_RING_OFF: u64 = 0x1000;
/// De frame-buffers in een eigen blok van 2 MB, los van de descriptors:
/// het coherent/streaming-onderscheid dat de igb ook maakt, zodat een board
/// de buffers cacheable mag mappen terwijl de descriptors device blijven.
pub const BUF_OFF: u64 = 2 << 20;
/// Wat de driver van de DMA-regio vraagt.
pub const DMA_NEED: u64 = BUF_OFF + (N_RX as u64 + N_TX as u64) * BUF_SIZE as u64;
/// ETH_ZLEN: mainline padt korte frames in software
/// (`rtl_quirk_packet_padto`, VER_61+).
const ETH_MIN_FRAME: usize = 60;

const _: () = {
    assert!(RX_RING_OFF + N_RX as u64 * DESC <= TX_RING_OFF);
    assert!(TX_RING_OFF + N_TX as u64 * DESC <= BUF_OFF);
    assert!(TX_RING_OFF.is_multiple_of(256));
    assert!(BUF_SIZE >= netdev::MAX_FRAME);
    assert!(BUF_SIZE as u32 <= RX_LEN_MASK);
};

/// Waarom de driver weigert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// TxConfig leest alles-enen: het device is niet op de bus.
    OffBus,
    /// Een chip-XID die deze driver niet kent.
    UnknownChip {
        /// De XID (TxConfig[31:20] & 0xfcf).
        xid: u32,
    },
    /// De DMA-regio is te klein of niet op 2 MB.
    Dma {
        /// De basis.
        base: u64,
        /// De maat.
        size: u64,
    },
    /// Een stap wachtte vergeefs (mainline logt en gaat door; wij geven
    /// hem terug: op ijzer wil je weten wélke stap hing).
    Timeout {
        /// Welke stap.
        what: &'static str,
        /// Het register.
        reg: u16,
        /// Wat erin stond.
        val: u32,
    },
    /// Geen geldig unicast-MAC in MAC0_BKP of MAC0.
    NoMac,
    /// PHYSR meldt een snelheidscode die we niet kennen.
    Speed(u16),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::OffBus => f.write_str("rtl8126: TxConfig reads all-ones (device off the bus?)"),
            Self::UnknownChip { xid } => write!(
                f,
                "rtl8126: unsupported chip XID {xid:#x} (known: 8126A 0x649/0x64a, 8125B 0x641, 8125D 0x688-0x68a, 8125CP 0x708, 8125BP 0x681)"
            ),
            Self::Dma { base, size } => write!(
                f,
                "rtl8126: DMA region {size:#x} at {base:#x} invalid (need {DMA_NEED:#x}, 2 MB aligned)"
            ),
            Self::Timeout { what, reg, val } => {
                write!(f, "rtl8126: {what} timed out (reg {reg:#x}={val:#x})")
            }
            Self::NoMac => f.write_str("rtl8126: no valid MAC in MAC0_BKP/MAC0"),
            Self::Speed(v) => write!(f, "rtl8126: unknown PHYSR speed code {v:#x}"),
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Het interrupt-pad: alleen IntrMask en IntrStatus. `Copy`, zodat het
/// board hem naast de driver houdt.
#[derive(Clone, Copy)]
pub struct IrqAck {
    base: Pa,
}

impl IrqAck {
    fn regs(&self) -> &'static Regs {
        // SAFETY: `base` kwam uit `Rtl8126::new`, dat een gemapt blok
        // eiste; masker en status delen niets met de ringen.
        unsafe { dev::regs(self.base) }
    }
}

impl netdev::IrqAck for IrqAck {
    /// Laat de lijn los: eerst het masker dicht, dan IntrStatus in één keer
    /// schoon (W1C van álle bits, `rtl8169_irq_mask_and_ack`).
    ///
    /// Alleen acken is niet genoeg; r8169 doet in zijn harde IRQ hetzelfde
    /// (`rtl_irq_disable`, dan NAPI, dan `rtl_irq_enable`). Zolang de ring
    /// vol staat, latcht RxDescUnavail direct na elke ack opnieuw, en op één
    /// core kwam de pomp dan nooit aan de beurt (O6N 17/18-09: "INTID 477
    /// keeps asserting after 256 acks in one pass").
    ///
    /// Álle bits, niet alleen de gelezen: met "schrijf terug wat je las"
    /// bleef op de O6N een storm van lege interrupts over (36.000/s, één
    /// frame per ~340 claims, rtt p50 1 ms in plaats van 160 µs). De W1C van
    /// 0xffffffff maakte hem weg (18-09, bundel 44: 1.813 claims in 40 s met
    /// een rtt-run, p50 164 µs). Events die zo verdwijnen, zijn geen verlies:
    /// de pomp leest de ring, niet de bits.
    fn ack(&self) {
        let r = self.regs();
        r.intr_mask.write(0);
        r.intr_status.write(u32::MAX);
        let _ = r.chip_cmd.read(); // commit: de PCI-writes posten
    }
}

/// Eén RTL8126A of RTL8125.
pub struct Rtl8126 {
    base: Pa,
    now: fn() -> u64,
    mac: Mac,
    xid: u32,
    v: &'static Variant,
    rx_ring: Pa,
    tx_ring: Pa,
    rx_bufs: Pa,
    tx_bufs: Pa,
    rx_head: u16,
    tx_head: u16,
    tx_pending: u16,
    irq: Option<&'static Signal>,
    /// De meetlat; `doorbells` telt TxPoll.
    stats: netdev::Stats,
}

impl Rtl8126 {
    /// Chip-id, `hw_init`, `hw_reset`, het MAC, de ringen en `hw_start`: na
    /// afloop staat de MAC aan met RX en TX, en IMR 0. De PHY staat nog uit
    /// ([`link_up`](Self::link_up)). `now` geeft monotone nanoseconden.
    ///
    /// # Safety
    ///
    /// `base` is BAR2 van een RTL8125/8126, gemapt als Device voor minstens
    /// [`MMIO_LEN`] bytes en voor altijd; memory-decode en bus-mastering
    /// staan aan. `[dma, dma+dma_size)` is gemapt geheugen dat alleen deze
    /// driver en het device gebruiken, nu en zolang het programma draait, op
    /// een adres dat het device ziet zoals de CPU.
    pub unsafe fn new(base: Pa, dma: Pa, dma_size: u64, now: fn() -> u64) -> Result<Self> {
        if dma_size < DMA_NEED || !dma.is_aligned(BUF_OFF) || dma.0 == 0 {
            return Err(Error::Dma {
                base: dma.0,
                size: dma_size,
            });
        }
        let mut n = Self::at(base, dma, now, &variant::V8126A);
        n.reset()?;
        n.init()?;
        Ok(n)
    }

    /// De staat zonder één registertoegang (ook voor de tests).
    fn at(base: Pa, dma: Pa, now: fn() -> u64, v: &'static Variant) -> Self {
        let bufs = dma.add(BUF_OFF);
        Self {
            base,
            now,
            mac: Mac::default(),
            xid: 0,
            v,
            rx_ring: dma.add(RX_RING_OFF),
            tx_ring: dma.add(TX_RING_OFF),
            rx_bufs: bufs,
            tx_bufs: bufs.add(u64::from(N_RX) * BUF_SIZE as u64),
            rx_head: 0,
            tx_head: 0,
            tx_pending: 0,
            irq: None,
            stats: netdev::Stats::default(),
        }
    }

    fn regs(&self) -> &'static Regs {
        // SAFETY: de voorwaarde van `new`.
        unsafe { dev::regs(self.base) }
    }

    /// Wacht begrensd tot `reg & mask == want` (`rtl_loop_wait_high` en
    /// `_low`): `budget` in nanoseconden, de Go-maat was pogingen maal pauze.
    fn wait8(
        &self,
        what: &'static str,
        reg: &Reg8,
        off: u16,
        mask: u8,
        want: u8,
        budget: u64,
    ) -> Result {
        if dev::poll_until(self.now, budget, || reg.read() & mask == want) {
            return Ok(());
        }
        Err(Error::Timeout {
            what,
            reg: off,
            val: u32::from(reg.read()),
        })
    }

    /// `rtl_pci_commit`: een lees die de geposte writes naar het device duwt.
    fn commit(&self) {
        let _ = self.regs().chip_cmd.read();
    }

    // De MAC-OCP-ruimte via OCPDR, zonder busy-vlag.
    fn mac_ocp_write(&self, reg: u16, v: u16) {
        self.regs()
            .ocpdr
            .write(OCP_FLAG | (u32::from(reg) << 15) | u32::from(v));
    }

    fn mac_ocp_read(&self, reg: u16) -> u16 {
        let r = self.regs();
        r.ocpdr.write(u32::from(reg) << 15);
        (r.ocpdr.read() & 0xffff) as u16
    }

    fn mac_ocp_modify(&self, reg: u16, mask: u16, set: u16) {
        self.mac_ocp_write(reg, (self.mac_ocp_read(reg) & !mask) | set);
    }

    // De PHY-OCP-ruimte via GPHY_OCP; bit 31 is bezig (schrijf: wacht tot
    // laag; lees: wacht tot hoog). Mainline wacht 25 µs × 10, wij ruimer.
    fn phy_ocp_write(&self, reg: u16, v: u16) -> Result {
        let r = self.regs();
        r.gphy_ocp
            .write(OCP_FLAG | (u32::from(reg) << 15) | u32::from(v));
        if dev::poll_until(self.now, 2_500_000, || r.gphy_ocp.read() & OCP_FLAG == 0) {
            return Ok(());
        }
        Err(Error::Timeout {
            what: "phy-ocp write",
            reg,
            val: r.gphy_ocp.read(),
        })
    }

    fn phy_ocp_read(&self, reg: u16) -> Result<u16> {
        let r = self.regs();
        r.gphy_ocp.write(u32::from(reg) << 15);
        if dev::poll_until(self.now, 2_500_000, || r.gphy_ocp.read() & OCP_FLAG != 0) {
            return Ok((r.gphy_ocp.read() & 0xffff) as u16);
        }
        Err(Error::Timeout {
            what: "phy-ocp read",
            reg,
            val: r.gphy_ocp.read(),
        })
    }

    fn phy_ocp_modify(&self, reg: u16, mask: u16, set: u16) -> Result {
        let v = self.phy_ocp_read(reg)?;
        self.phy_ocp_write(reg, (v & !mask) | set)
    }

    fn phy_step(&self, s: Step) -> Result {
        match s {
            Step::Ocp(reg, mask, set) => self.phy_ocp_modify(reg, mask, set),
            Step::ParamG(parm, mask, val) => {
                self.phy_ocp_write(0xa436, parm)?;
                self.phy_ocp_modify(0xa438, mask, val)
            }
            Step::Param8125(parm, mask, val) => {
                self.phy_ocp_write(0xb87c, parm)?;
                self.phy_ocp_modify(0xb87e, mask, val)
            }
        }
    }

    // De EPHY-ruimte via EPHYAR. Registermasker 0x1f zoals mainline
    // (EPHYAR_REG_MASK): de tabellen dragen offsets vanaf 0x20 die daarmee
    // op reg & 0x1f landen; wij reproduceren mainline letterlijk.
    fn ephy_write(&self, reg: u16, v: u16) {
        let r = self.regs();
        r.ephyar
            .write(0x8000_0000 | u32::from(v) | (u32::from(reg & 0x1f) << 16));
        let _ = dev::poll_until(self.now, 1_000_000, || r.ephyar.read() & 0x8000_0000 == 0);
        dev::delay(self.now, 10_000);
    }

    fn ephy_read(&self, reg: u16) -> u16 {
        let r = self.regs();
        r.ephyar.write(u32::from(reg & 0x1f) << 16);
        if dev::poll_until(self.now, 1_000_000, || r.ephyar.read() & 0x8000_0000 != 0) {
            return (r.ephyar.read() & 0xffff) as u16;
        }
        0xffff
    }

    /// Het probe-deel van mainline: chip-id, `rtl_hw_init_8125`,
    /// `rtl_hw_reset` en het MAC. De MAC-kern staat daarna in rust.
    fn reset(&mut self) -> Result {
        let r = self.regs();
        let tx = r.tx_config.read();
        if tx == u32::MAX {
            return Err(Error::OffBus);
        }
        self.xid = (tx >> 20) & 0xfcf;
        self.v = variant::by_xid(self.xid).ok_or(Error::UnknownChip { xid: self.xid })?;

        // rtl_init_rxcfg + rtl8169_irq_mask_and_ack (de probe-volgorde).
        r.rx_config.write(RX_CFG_BASE);
        self.irq_mask_and_ack();
        self.hw_init()?;
        self.hw_reset()?;

        // MAC0_BKP (zes bytes in draadvolgorde), terugval MAC0.
        let mut m = [0u8; 6];
        for (b, reg) in m.iter_mut().zip(&r.mac0_bkp) {
            *b = reg.read();
        }
        if !valid_unicast(m) {
            let [a, b, c, d] = r.mac0.read().to_le_bytes();
            let [e, f, _, _] = r.mac4.read().to_le_bytes();
            m = [a, b, c, d, e, f];
        }
        if !valid_unicast(m) {
            return Err(Error::NoMac);
        }
        self.mac = Mac(m);
        self.rar_set();
        if self.v.dash {
            self.dash_start();
        }
        Ok(())
    }

    fn irq_mask_and_ack(&self) {
        let r = self.regs();
        r.intr_mask.write(0);
        r.intr_status.write(u32::MAX);
        self.commit();
    }

    /// `rtl_enable_rxdvgate` + `rtl_wait_txrx_fifo_empty` (VER_63+): het
    /// RX-datapad dicht, en wachten tot de FIFO's leeg zijn.
    fn enable_rxdv_gate(&self) -> Result {
        let r = self.regs();
        r.misc.update(|v| v | MISC_RXDV_GATED);
        dev::delay(self.now, 2_000_000);
        r.chip_cmd.update(|v| v | CMD_STOP_REQ);
        self.wait8(
            "rx/tx fifo empty",
            &r.mcu,
            0xd3,
            MCU_RXTX_EMPTY,
            MCU_RXTX_EMPTY,
            4_200_000,
        )?;
        if dev::poll_until(self.now, 4_200_000, || {
            r.intr_mitig.read() & 0x0103 == 0x0103
        }) {
            return Ok(());
        }
        Err(Error::Timeout {
            what: "fifo empty (0xe2)",
            reg: 0xe2,
            val: u32::from(r.intr_mitig.read()),
        })
    }

    /// `rtl_hw_init_8125`: één keer bij de probe, vóór de eerste CmdReset:
    /// uit OOB-modus, link-list klaar.
    fn hw_init(&self) -> Result {
        let r = self.regs();
        self.enable_rxdv_gate()?;
        r.chip_cmd.update(|v| v & !(CMD_TX_ENB | CMD_RX_ENB));
        dev::delay(self.now, 1_000_000);
        r.mcu.update(|v| v & !MCU_NOW_IS_OOB);
        self.mac_ocp_modify(0xe8de, 1 << 14, 0);
        let ok = MCU_LINK_LIST_OK;
        self.wait8("link list ready (1)", &r.mcu, 0xd3, ok, ok, 4_200_000)?;
        self.mac_ocp_write(0xc0aa, 0x07d0);
        self.mac_ocp_write(0xc0a6, 0x0150);
        self.mac_ocp_write(0xc01e, 0x5555);
        self.wait8("link list ready (2)", &r.mcu, 0xd3, ok, ok, 4_200_000)
    }

    /// `rtl_hw_reset`: CmdReset, en wachten tot hij zichzelf wist.
    fn hw_reset(&self) -> Result {
        let r = self.regs();
        r.chip_cmd.write(CMD_RESET);
        self.wait8("chip reset", &r.chip_cmd, 0x37, CMD_RESET, 0, 10_000_000)
    }

    /// `rtl_rar_set`: MAC4 vóór MAC0, onder Cfg9346-unlock, elk gepost.
    fn rar_set(&self) {
        let r = self.regs();
        let m = self.mac.0;
        r.cfg9346.write(CFG_UNLOCK);
        r.mac4.write(u32::from_le_bytes([m[4], m[5], 0, 0]));
        self.commit();
        r.mac0.write(u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
        self.commit();
        r.cfg9346.write(CFG_LOCK);
    }

    /// `rtl8125bp_driver_start`: de OOB-handdruk die mainline op de 8125BP
    /// altijd doet, via de ERI-OOB-ruimte.
    fn dash_start(&self) {
        let r = self.regs();
        for (reg, data) in [(0x14u32, 0x05u32), (0x18, 0x00), (0x10, 0x01)] {
            r.eridr.write(data);
            r.eriar.write(0x8000_0000 | 0x0002_0000 | 0x1000 | reg);
            let _ = dev::poll_until(self.now, 10_000_000, || r.eriar.read() & 0x8000_0000 == 0);
        }
    }

    /// De ringen klaar, `rtl8169_cleanup` (mainline doet die vlak vóór
    /// `hw_start` opnieuw), en `hw_start`.
    fn init(&mut self) -> Result {
        for i in 0..N_RX {
            self.arm_rx(i);
        }
        dev::clear(self.tx_ring, (u64::from(N_TX) * DESC) as usize);
        // De laatste TX-descriptor draagt RingEnd, ook als hij leeg is.
        dev::write32(self.tx_desc(N_TX - 1), RING_END);
        dev::mb();

        self.irq_mask_and_ack();
        let r = self.regs();
        r.rx_config.update(|v| v & !RX_ACCEPT_MASK);
        self.enable_rxdv_gate()?;
        dev::delay(self.now, 2_000_000);
        self.hw_reset()?;
        self.hw_start()
    }

    /// `rtl_hw_start` voor VER_70 (`rtl_hw_start_8125` naar
    /// `rtl_hw_start_8126a` naar `rtl_hw_start_8125_common`), zonder
    /// ASPM/LTR/EEE-timer/tally (optioneel volgens de bron). De
    /// OCP-"hw-parameters" van Realtek schrijft mainline onvoorwaardelijk; die
    /// nemen we integraal over tot bewezen is dat het zonder kan.
    fn hw_start(&self) -> Result {
        let r = self.regs();
        let v = self.v;
        r.cfg9346.write(CFG_UNLOCK);

        // rtl_hw_aspm_clkreq_enable(false).
        self.mac_ocp_modify(0xe092, 0x00ff, 0);
        if v.clkreq_cfg2 {
            r.config2.update(|x| x & !(1 << 7));
        } else {
            r.int_cfg0.update(|x| x & !(1 << 3));
        }
        r.config5.update(|x| x & !1);
        r.cplus_cmd.update(|x| x & CPCMD_MASK); // geen offloads

        // rtl_hw_start_8125: legacy ISR/IMR, mitigation uit.
        r.int_cfg0.write(0);
        let words = usize::from((v.mitig_end - 0xa00) / 4);
        for m in r.mitig.iter().take(words) {
            m.write(0);
        }
        if v.int_cfg1 {
            r.int_cfg1.write(0);
        }

        // rtl_hw_config: de EPHY-tabel van de variant, dan 8125_common.
        for &(reg, mask, bits) in v.ephy {
            self.ephy_write(reg, (self.ephy_read(reg) & !mask) | bits);
        }
        self.start_8125_common();
        self.wait_e00e()?;
        r.misc.update(|x| x & !MISC_RXDV_GATED); // RX open (verplicht)

        // Ringen en maten; hoog vóór laag (mainline-commentaar).
        r.rx_max_size.write(BUF_SIZE as u16); // groter weigert de MAC
        r.tx_desc_hi.write((self.tx_ring.0 >> 32) as u32);
        r.tx_desc_lo.write(self.tx_ring.0 as u32);
        r.rx_desc_hi.write((self.rx_ring.0 >> 32) as u32);
        r.rx_desc_lo.write(self.rx_ring.0 as u32);
        r.cfg9346.write(CFG_LOCK);
        self.commit();
        r.chip_cmd.write(CMD_TX_ENB | CMD_RX_ENB);
        r.rx_config.write(RX_CFG_BASE);
        r.tx_config.write(TX_CFG);
        // rtl_set_rx_mode: alle multicast, plus broadcast en het eigen adres.
        r.mar4.write(u32::MAX);
        r.mar0.write(u32::MAX);
        r.rx_config
            .update(|x| (x & !RX_ACCEPT_OK_MASK) | RX_ACCEPT_DEFAULT);
        r.intr_mask.write(0); // gepold tot `set_irq`
        dev::mb();
        Ok(())
    }

    /// `rtl_hw_start_8125_common`, met de variantwaarden op de lijnen die
    /// per mac_version verschillen (recept §15c).
    fn start_8125_common(&self) {
        let r = self.regs();
        let v = self.v;
        r.config3.update(|x| x & !0x02); // rtl_pcie_state_l2l3_disable
        r.r0382.write(0x221b);
        r.rss_ctrl.write(0);
        r.qnum_ctrl.write(0);
        self.mac_ocp_modify(0xd40a, 0x0010, 0); // UPS uit
        r.config1.update(|x| x & !0x10);
        self.mac_ocp_write(0xc140, 0xffff);
        self.mac_ocp_write(0xc142, 0xffff);
        self.mac_ocp_modify(0xd3e2, 0x0fff, 0x03a9);
        self.mac_ocp_modify(0xd3e4, 0x00ff, 0x0000);
        self.mac_ocp_modify(0xe860, 0x0000, 0x0080);
        self.mac_ocp_modify(0xeb58, 0x0001, 0x0000); // klassiek TX-formaat (verplicht)
        if v.rx_desc_fmt {
            r.rx_desc_fmt.update(|x| x & !0x02); // klassiek RX-formaat (VER_70/80)
        }
        self.mac_ocp_modify(0xe614, 0x0700, v.e614);
        self.mac_ocp_modify(0xe63e, 0x0c30, v.e63e);
        self.mac_ocp_modify(0xc0b4, 0x0000, 0x000c);
        self.mac_ocp_modify(0xeb6a, 0x00ff, 0x0033);
        self.mac_ocp_modify(0xeb50, 0x03e0, 0x0040);
        self.mac_ocp_modify(0xe056, 0x00f0, 0x0000);
        self.mac_ocp_modify(0xe040, 0x1000, 0x0000);
        self.mac_ocp_modify(0xea1c, 0x0003, 0x0001);
        self.mac_ocp_modify(0xea1c, v.ea1c_second, 0x0000);
        self.mac_ocp_modify(0xe0c0, 0x4f0f, 0x4403);
        self.mac_ocp_modify(0xe052, 0x0080, 0x0068);
        self.mac_ocp_modify(0xd430, 0x0fff, 0x047f);
        self.mac_ocp_modify(0xea1c, 0x0004, 0x0000);
        self.mac_ocp_modify(0xeb54, 0x0000, 0x0001); // TCAM wissen
        dev::delay(self.now, 1_000);
        self.mac_ocp_modify(0xeb54, 0x0001, 0x0000);
        r.r1880.update(|x| x & !0x0030);
        self.mac_ocp_write(0xe098, 0xc302);
    }

    /// Mac-OCP 0xe00e bit 13 moet zakken (`rtl_hw_start_8125_common`, het
    /// eind).
    fn wait_e00e(&self) -> Result {
        if dev::poll_until(self.now, 10_000_000, || {
            self.mac_ocp_read(0xe00e) & (1 << 13) == 0
        }) {
            return Ok(());
        }
        Err(Error::Timeout {
            what: "mac-ocp 0xe00e bit 13",
            reg: 0xe00e,
            val: u32::from(self.mac_ocp_read(0xe00e)),
        })
    }

    /// PHY aan (BMCR.PDOWN weg), de VER_70-PHY-config zonder firmware-blob,
    /// een soft-reset, alles adverteren en autoneg herstarten. De link zelf
    /// komt later, in de taak ([`netdev::Device::link`]).
    pub fn start_link(&mut self) -> Result {
        let bmcr = phy_c22(mdio::reg::BMCR);
        // genphy_resume: power-down eraf, 20 ms (rtlgen_resume).
        self.phy_ocp_modify(bmcr, BMCR_POWER_DOWN, 0)?;
        dev::delay(self.now, 20_000_000);
        // rtl81xx_hw_phy_config zonder blob: 10M-gphy aan, de variant-
        // stappen, legacy force mode (clause 22), ALDPS uit, EEE-PHY uit.
        self.phy_ocp_modify(0xa442, 0, 1 << 11)?; // rtl8168g_enable_gphy_10m
        for &s in self.v.phy {
            self.phy_step(s)?;
        }
        for (reg, mask) in [
            (0xa5b4, 1 << 15), // rtl8125_legacy_force_mode
            (0xa430, 1 << 2),  // rtl8168g_disable_aldps
            (0xa6d8, 0x0010),  // rtl8125_common_config_eee_phy (drie)
            (0xa428, 0x0080),
            (0xa4a2, 0x0200),
        ] {
            self.phy_ocp_modify(reg, mask, 0)?;
        }
        if self.v.eee_phy {
            self.phy_ocp_modify(0xa432, 0, 0x0010)?; // rtl8168g_config_eee_phy
        }

        // genphy_soft_reset: RESET | ANRESTART, wachten tot bit 15 zakt
        // (tot 600 ms).
        let b = self.phy_ocp_read(bmcr)?;
        self.phy_ocp_write(
            bmcr,
            (b & !BMCR_ISOLATE) | BMCR_RESET | mdio::BMCR_AN_RESTART,
        )?;
        let mut b = Ok(0);
        let done = dev::poll_until(self.now, 600_000_000, || {
            b = self.phy_ocp_read(bmcr);
            b.map_or(true, |b| b & BMCR_RESET == 0)
        });
        let b = b?;
        if !done {
            return Err(Error::Timeout {
                what: "PHY soft reset",
                reg: bmcr,
                val: u32::from(b),
            });
        }
        dev::delay(self.now, 1_000_000);

        // rtl822x_config_aneg + genphy_restart_aneg: alle advertenties
        // expliciet (mainline leunt niet op power-on-defaults).
        self.phy_ocp_modify(PHY_NBASET, 0x1180, self.v.adv)?; // 2.5G (+5G), geen 10G
        self.phy_ocp_modify(phy_c22(mdio::reg::ANAR), 0x0de0, 0x0de0)?; // 10/100, pause
        self.phy_ocp_modify(phy_c22(mdio::reg::GBCR), 0x0300, mdio::GBCR_1000_FD)?;
        let b = self.phy_ocp_read(bmcr)?;
        self.phy_ocp_write(
            bmcr,
            (b & !BMCR_ISOLATE) | mdio::BMCR_AN_ENABLE | mdio::BMCR_AN_RESTART,
        )
    }

    /// Eén kijkje naar de link: BMSR is latched-low, dus twee keer lezen
    /// (`genphy_update_link`); staat hij, dan de snelheid uit PHYSR.
    fn poll_link(&mut self) -> Result<Option<Link>> {
        let bmsr = phy_c22(mdio::reg::BMSR);
        let _ = self.phy_ocp_read(bmsr)?;
        if self.phy_ocp_read(bmsr)? & mdio::BMSR_LINK == 0 {
            return Ok(None);
        }
        decode_physr(self.phy_ocp_read(PHY_PHYSR)?).map(Some)
    }

    /// De variantnaam ("RTL8126A", "RTL8125B", ...).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.v.name
    }

    /// De chip-XID uit TxConfig.
    #[must_use]
    pub fn xid(&self) -> u32 {
        self.xid
    }

    /// Het interrupt-pad, voor het board.
    #[must_use]
    pub fn irq_ack(&self) -> IrqAck {
        IrqAck { base: self.base }
    }

    /// Hangt de bel aan de driver en laat de NIC zijn lijn trekken op
    /// RX-werk, een verzonden frame en link-wissel. Aanroepen nadat de lijn bij de controller
    /// scherp staat; zonder blijft IMR 0 en pollt de pomp.
    pub fn set_irq(&mut self, bell: &'static Signal) {
        self.irq = Some(bell);
        self.rearm();
    }

    /// IntrStatus schoon en dán het masker open, de volgorde van
    /// `EnableIRQ`. Eerst stond hier "open zonder IntrStatus aan te raken":
    /// een bit dat tijdens het dichte masker latchte, zou de lijn bij het
    /// openen vanzelf trekken. Dat doet deze chip niet: de interrupt is een
    /// flank op het zetten van een bit onder een open masker, en een bit dat
    /// al stond maakt daarna geen flank meer; elk volgend frame wachtte op de
    /// vangrail van 10 ms (O6N, bundels 47/48, 20-09: cyclus 21 ms, 45 MB/s,
    /// tegen 1,5 ms en 118 MB/s met de rearm vlak na de ack).
    fn rearm(&self) {
        let r = self.regs();
        r.intr_status.write(u32::MAX);
        r.intr_mask.write(INT_RX | INT_TX);
        self.commit();
    }

    fn rx_desc(&self, i: u16) -> Pa {
        self.rx_ring.add(u64::from(i) * DESC)
    }

    fn tx_desc(&self, i: u16) -> Pa {
        self.tx_ring.add(u64::from(i) * DESC)
    }

    fn rx_buf(&self, i: u16) -> Pa {
        self.rx_bufs.add(u64::from(i) * BUF_SIZE as u64)
    }

    /// Geeft RX-descriptor `i` (terug) aan de hardware: adres, opts2 0, en
    /// als laatste opts1 = Own | RingEnd (de laatste) | buffergrootte
    /// (`rtl8169_mark_to_asic`).
    fn arm_rx(&self, i: u16) {
        let d = self.rx_desc(i);
        dev::write32(d.add(DESC_ADDR_LO), self.rx_buf(i).0 as u32);
        dev::write32(d.add(DESC_ADDR_HI), (self.rx_buf(i).0 >> 32) as u32);
        dev::write32(d.add(DESC_OPTS2), 0);
        dev::mb();
        let end = if i == N_RX - 1 { RING_END } else { 0 };
        dev::write32(d, DESC_OWN | end | BUF_SIZE as u32);
    }

    fn ring_tx(&mut self) {
        self.regs().tx_poll.write(1);
        self.stats.doorbells += 1;
    }
}

type Reg8 = dev::Reg<u8>;

fn valid_unicast(m: [u8; 6]) -> bool {
    m[0] & 1 == 0 && m.iter().any(|&b| b != 0)
}

/// `rtlgen_read_status`: bit 3 is full duplex, de snelheid uit [5:4] en
/// [10:9].
fn decode_physr(v: u16) -> Result<Link> {
    let mbps = match v & 0x0630 {
        0x0000 => 10,
        0x0010 => 100,
        0x0020 => 1000,
        0x0210 => 2500,
        0x0220 => 5000,
        0x0200 => 10000,
        _ => return Err(Error::Speed(v)),
    };
    Ok(Link {
        mbps,
        full_duplex: v & (1 << 3) != 0,
    })
}

impl netdev::Device for Rtl8126 {
    /// Zet één frame op de TX-ring; TxPoll volgt in `flush`. Korter dan 60
    /// bytes wordt in software gepad. Een descriptor die nog van de NIC is,
    /// maakt de ring vol; dan eerst nog één keer bellen, want "TxPoll
    /// requests are lost when the Tx packets are too close" (mainline
    /// `rtl_tx`): goedkoper dan een vals "DMA stuck".
    ///
    /// Vol is wachten op TxOK, en die flank komt alleen als het bit niet
    /// al stond (zie `rearm`): een completion tussen de W1C en het openen
    /// van `rearm` liet hem staan. Daarom bij vol eerst de TX-bits wissen
    /// en dan nog één blik op de descriptor, zoals `receive` doet voor RX.
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), TxError> {
        if frame.is_empty() || frame.len() > BUF_SIZE {
            return Err(TxError::Size(frame.len()));
        }
        let i = self.tx_head;
        let d = self.tx_desc(i);
        if dev::read32(d) & DESC_OWN != 0 {
            self.ring_tx();
            self.regs().intr_status.write(INT_TX);
            self.commit();
            if dev::read32(d) & DESC_OWN != 0 {
                self.stats.tx_full += 1;
                return Err(TxError::Full);
            }
        }
        let buf = self.tx_bufs.add(u64::from(i) * BUF_SIZE as u64);
        dev::copy_in(buf, frame);
        let mut len = frame.len();
        if len < ETH_MIN_FRAME {
            dev::clear(buf.add(len as u64), ETH_MIN_FRAME - len);
            len = ETH_MIN_FRAME;
        }
        dev::push(buf, len);
        dev::write32(d.add(DESC_ADDR_LO), buf.0 as u32);
        dev::write32(d.add(DESC_ADDR_HI), (buf.0 >> 32) as u32);
        dev::write32(d.add(DESC_OPTS2), 0);
        dev::mb();
        let end = if i == N_TX - 1 { RING_END } else { 0 };
        dev::write32(d, DESC_OWN | FIRST_FRAG | LAST_FRAG | end | len as u32);
        self.tx_head = (i + 1) % N_TX;
        self.tx_pending += 1;
        Ok(())
    }

    /// Haalt één frame op (zie `receive_one`).
    ///
    /// Leeg met een bedrade lijn: dan gaat het masker weer open (de ack
    /// sloot het), en daarna kijkt de driver nog één keer. Een frame dat
    /// tussen de lege lees en de W1C van `rearm` viel, maakt geen flank;
    /// zonder die blik bleef het tot de vangrail liggen (de Go-pomp deed
    /// daarvoor een extra ronde). Tot 04-10 stond de rearm in `flush`, en
    /// die roept de pomp alleen na een ronde met werk.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        if let Some(n) = self.receive_one(buf) {
            return Some(n);
        }
        self.irq?;
        self.rearm();
        self.receive_one(buf)
    }

    /// Eén TxPoll per burst.
    fn flush(&mut self) {
        if self.tx_pending > 0 {
            dev::mb();
            self.ring_tx();
            self.tx_pending = 0;
        }
    }

    fn mac(&self) -> Mac {
        self.mac
    }

    fn irq(&self) -> Option<&'static Signal> {
        self.irq
    }

    /// Eén blik op de PHY. De MAC volgt de PHY zelf: niets te zetten.
    fn link(&mut self) -> Option<u32> {
        self.poll_link().ok()?.map(|l| l.mbps)
    }

    fn stats(&self) -> netdev::Stats {
        self.stats
    }
}

impl Rtl8126 {
    /// Haalt één frame op. Een fout (RES), een fragment of een lengte die
    /// niet in de buffer past, wordt herwapend zonder kopie. Geen
    /// RX-doorbell: de MAC pollt de Own-bit zelf.
    fn receive_one(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let i = self.rx_head;
            let opts1 = dev::read32(self.rx_desc(i));
            if opts1 & DESC_OWN != 0 {
                // Ook gepold moeten de overflow-latches weg zodra de ring
                // leeg is; alleen de geziene overflow-bits, de rest blijft.
                let r = self.regs();
                let pending = r.intr_status.read() & (INT_RX_OVERFLOW | INT_RX_FIFO_OVER);
                if pending != 0 && self.irq.is_none() {
                    r.intr_status.write(pending);
                    self.commit();
                }
                return None;
            }
            dev::mb();
            let whole = opts1 & (FIRST_FRAG | LAST_FRAG) == FIRST_FRAG | LAST_FRAG;
            // De lengte draagt de 4 bytes FCS.
            let len = (opts1 & RX_LEN_MASK) as usize;
            let good = opts1 & RX_RES == 0 && whole && len > 4 && len - 4 <= BUF_SIZE;
            let n = if good { (len - 4).min(buf.len()) } else { 0 };
            if n > 0 {
                let src = self.rx_buf(i);
                dev::pull(src, n);
                if let Some(dst) = buf.get_mut(..n) {
                    dev::copy_out(dst, src);
                }
            } else {
                self.stats.rx_bad += 1;
            }
            self.arm_rx(i);
            self.rx_head = (i + 1) % N_RX;
            if n > 0 {
                return Some(n);
            }
        }
    }
}

#[cfg(test)]
mod tests;
