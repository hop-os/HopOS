//! Raspberry Pi 4 (BCM2711, 4x Cortex-A72): het BCM2711-eigene. De rest
//! (boot, kaart, plan, mailbox, GIC-bedrading) is `board_raspi`.
//!
//! Boot zonder UEFI: de EEPROM-bootloader laadt start4.elf, die
//! `kernel8.img` rauw op 0x80000 laadt (config.txt: `arm_64bit=1`).
//!
//! PSCI: de stock armstub8 parkeert de andere cores in een spin-table en
//! heeft GÉÉN PSCI: een SMC verdwijnt dan in een lege EL3-vector (hang).
//! Een zelfgebouwde upstream-TF-A `bl31.bin` als armstub (config.txt
//! `armstub=bl31.bin`) is op dit board dus verplicht; die levert ons op EL2
//! af met PSCI via SMC, precies als op de Pi 5 (image/rpi4.sh,
//! docs/boards-pi.md).
//!
//! Netwerk: de geïntegreerde GENET v5 (`driver-genet`) op zijn lijn, GIC
//! SPI 157 (INTRL2_0, ring 16; docs/sequence-hopos.md §14.1 O3). In de
//! Go-kern was hij gepold. ONGETOETST op ijzer: QEMU `raspi4b` heeft geen
//! GENET.
//!
//! Adressen: "low peripheral mode" (de default), MMIO onder 4 GB, in de
//! laatste 64 MB van de vierde gigabyte.

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

use board::Error;
use board_raspi::driver_gicv2::Gic;
use board_raspi::map::{self, L2, Tables};
use board_raspi::{NicCtx, Raspi, Soc};
use cpu::irq::Line;
use dev::Pa;
use driver_genet::{Genet, IrqAck};
use driver_pl011::Pl011;
use netdev::AckSlot;
use sync::Signal;

// De VL805 achter de PCIe-brug (usb.rs), alleen met de bouwsteen `usb`.
#[cfg(feature = "usb")]
mod usb;

/// De PL011 UART0 op GPIO14/15 (header-pin 8/10); de Pi 4 heeft geen
/// aparte debug-connector. De bootloader zet hem op 115200 (config.txt
/// `uart_2ndstage=1`); `dtoverlay=disable-bt` houdt hem bij de header.
pub const UART0: Pa = Pa(0xFE20_1000);
/// GPFSEL1 van de GPIO-bank: de functie van pin 10 tot 19, drie bits per pin.
const GPFSEL1: Pa = Pa(0xFE20_0004);
/// De GIC-400: distributor op +0x1000, CPU-interface op +0x2000.
pub const GIC: Pa = Pa(0xFF84_0000);
/// De VideoCore-mailbox (brcm,bcm2835-mbox, klassieke basis).
pub const VCMAIL: Pa = Pa(0xFE00_B880);
/// De RNG200 (DT rng@7e104000; de soc-ranges leggen 0x7e00_0000 op
/// 0xfe00_0000). Go: `RNG200Base`.
pub const RNG200: Pa = Pa(0xFE10_4000);
/// Het PM-blok met de watchdog (DT watchdog@7e100000). Go: de
/// `WatchdogBase` uit `raspi.SetupPlan`.
pub const PM: Pa = Pa(0xFE10_0000);
/// De GENET v5.
pub const GENET: Pa = Pa(0xFD58_0000);
/// De compatible van de GENET in de DTB.
pub const GENET_COMPAT: &str = "brcm,bcm2711-genet-v5";
/// Waar de peripherals beginnen: de laatste 64 MB onder 4 GB.
pub const PERIPH: u64 = 0xFC00_0000;
/// De lijn van de GENET: GIC SPI 157, level-high (bcm2711.dtsi, `genet`
/// `interrupts` 0: INTRL2_0, waar ring 16 meldt).
pub const GENET_INTID: u32 = 32 + 157;

/// De bel van de NIC-lijn: de dispatch luidt hem, de RX-pomp wacht erop.
static NIC_BELL: Signal = Signal::new();
/// De ack van de GENET, gezet vóór de lijn scherp gaat.
static NIC_ACK: AckSlot<IrqAck> = AckSlot::new();

/// De device-ack van de NIC-lijn: `RXDMA_MBDONE` dicht, de lijn valt.
fn nic_ack() {
    NIC_ACK.ack();
}

// De UART-klok is 48 MHz (de firmware-standaard `init_uart_clock`).
static UART: Pl011 =
    // SAFETY: 0xFE20_1000 is de PL011 van de BCM2711 en ligt in het
    // Device-venster van de vaste tabel (0xFC00_0000 tot 4 GB).
    unsafe { Pl011::new(UART0) }.with_clock(48_000_000);

// SAFETY: de GIC-400 van de BCM2711, in hetzelfde Device-venster.
static GIC400: Gic = unsafe { Gic::new(GIC.add(0x1000), GIC.add(0x2000)) };

/// De BCM2711 als SoC.
pub struct Bcm2711;

/// De Pi 4 als board.
pub type Rpi4 = Raspi<Bcm2711>;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Rpi4;

impl Soc for Bcm2711 {
    type Nic = Genet;
    const NAME: &'static str = "rpi4";
    const SOC: &'static str = "BCM2711";
    const MAC_FALLBACK: u8 = 0x04;
    const VCMAIL: Pa = VCMAIL;
    const RNG200: Pa = RNG200;
    const PM: Pa = PM;

    #[cfg(feature = "usb")]
    fn usb_hosts(ctx: &board_raspi::usb::UsbCtx) -> board::UsbHosts {
        usb::hosts(ctx)
    }

    fn uart() -> &'static Pl011 {
        &UART
    }

    /// GPIO14/15 op ALT0, de PL011, zoals Linux ze uit de DT zet
    /// (`uart0_gpio14`). De firmware laat ze op ALT5, de mini-UART, ook met
    /// `disable-bt` (die overlay zet alleen de DT voor Linux); TF-A kiest daarom
    /// zelf de mini-UART (`rpi3_use_mini_uart`). Zonder dit ging de console
    /// van de kern nergens heen (07-11, op ijzer).
    fn uart_pins() {
        let f = dev::read32(GPFSEL1) & !(0x3f << 12);
        dev::write32(GPFSEL1, f | (0b100_100 << 12));
    }

    fn gic() -> &'static Gic {
        &GIC400
    }

    fn mpidr(core: usize) -> u64 {
        slots::mpidr(core)
    }

    fn core_of(mpidr: u64) -> usize {
        slots::core_of(mpidr)
    }

    fn tables() -> Option<Tables> {
        let (l1, gb0, gb3) = arch::tables()?;
        Some(Tables {
            l1,
            l2: [
                Some(L2 {
                    gb: 0,
                    table: gb0,
                    fixed: abi::Region::new(0, map::FIXED_END),
                }),
                Some(L2 {
                    gb: 3,
                    table: gb3,
                    fixed: abi::Region::new(PERIPH, (4 << 30) - PERIPH),
                }),
            ],
        })
    }

    /// De GENET-keten, boardvast bewezen in de Go-kern (11-07, één boot):
    /// v5-rev, MAC-reset (U-Boot-sequence), PHY-scan (BCM54213PE op adres
    /// 1, géén reset-GPIO hier), autonegotiatie, ring-16-DMA in de
    /// NIC-regio. De firmware laat de GENET met rust.
    fn probe_nic(ctx: &NicCtx) -> Result<Option<Genet>, Error> {
        // Alleen een GENET die de DTB aanzet, bestaat: QEMU `raspi4b` haalt
        // de node weg (hij emuleert hem niet), en de eerste lees gaf daar
        // een synchrone abort (29-09, ESR 0x96000010). De echte DTB van de
        // Pi 4 heeft hem op "okay" (getoetst tegen bcm2711-rpi-4-b.dtb).
        if board_raspi::device_enabled(GENET_COMPAT) != Some(true) {
            cpu::println!("net: no enabled {GENET_COMPAT} in the DTB, no NIC");
            return Ok(None);
        }
        // SAFETY: GENET is het blok van de BCM2711 (Device-venster); de
        // NIC-regio is van deze driver alleen, met zijn buffers Normal-WB
        // (`arch`), en busadres = fysiek adres op de scb-bus.
        let mut nic =
            unsafe { Genet::new(GENET, Pa(ctx.dma.base), ctx.dma.size, ctx.mac, ctx.now) };
        nic.check_rev().map_err(|_| Error::Nic("genet: not a v5"))?;
        nic.reset()
            .map_err(|_| Error::Nic("genet: DMA did not stop"))?;
        let phy = driver_mdio::scan(&mut nic).ok_or(Error::Nic("genet: no PHY on MDIO"))?;
        cpu::println!(
            "net: genet rev {:#x}, PHY at {} ({:#06x}:{:#06x}), autonegotiating",
            nic.rev(),
            phy.addr,
            phy.id1,
            phy.id2
        );
        // De autonegotiatie start hier, maar de probe wacht er niet op: de
        // link-wacht is van de taak (`netdev::Device::link`, hopos
        // `net::start`), en de GENET zet zijn MAC pas dan op de snelheid.
        nic.start_link(phy.addr)
            .map_err(|_| Error::Nic("genet: MDIO stuck starting autoneg"))?;
        nic.init()
            .map_err(|_| Error::Nic("genet: DMA region too small"))?;
        // De lijn: de GIC staat al (`start_interrupts` komt eerst).
        NIC_ACK.set(nic.irq_ack());
        cpu::irq::enable(Line(GENET_INTID), Some(nic_ack), Some(&NIC_BELL))
            .map_err(|_| Error::Irq("genet line refused"))?;
        nic.set_irq(&NIC_BELL);
        cpu::println!(
            "net: genet up, link pending (autoneg), RX and TX done on INTID {GENET_INTID}"
        );
        Ok(Some(nic))
    }
}

/// Het slot-plan: dat van `board_raspi`, met de A72-nummering.
pub mod slots {
    pub use board_raspi::slots::*;

    /// Het MPIDR-target van logische core `core`: de Cortex-A72 nummert in
    /// affiniteit 0 (géén MT-formaat), anders dan de A76 van de Pi 5.
    #[must_use]
    pub const fn mpidr(core: usize) -> u64 {
        core as u64
    }

    /// De logische core bij een MPIDR: aff0.
    #[must_use]
    pub const fn core_of(mpidr: u64) -> usize {
        (mpidr & 0xff) as usize
    }
}

board_raspi::pi_entry!(0xFE20_1000u64, board_raspi::map::FIXED_END);

mod arch {
    //! De vaste tabellen van de Pi 4, als data in het image, en hun adressen.
    //!
    //! - niveau 1: gigabyte 0 en 3 via een eigen niveau-2-tabel, de rest
    //!   leeg tot `discover` er DRAM bij zet;
    //! - gigabyte 0: kern-RAM en laadvenster Normal WB, het kooi-venster
    //!   Device, de DMA-regio Normal-NC (`board_raspi::map`), op het eerste
    //!   blok na (0x1400_0000): de buffers van de GENET, Normal-WB en XN,
    //!   de driver veegt ze;
    //! - gigabyte 3: de peripherals (0xFC00_0000 tot 4 GB) Device; het RAM
    //!   eronder (op een 4 en 8 GB-Pi) zet `discover` erbij.
    use dev::Pa;

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    core::arch::global_asm!(
        r#"
    .section .data.pagetables, "aw"
    .balign 4096
    .global __boot_ttbr0
__boot_ttbr0:
    .quad __pi_l2_gb0 + 3
    .quad 0
    .quad 0
    .quad __pi_l2_gb3 + 3
    .fill 508, 8, 0

    .balign 4096
    .global __pi_l2_gb0
__pi_l2_gb0:
    .set blk, 0
    .rept 128
    .quad {nrm} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept 32
    .quad {dev} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {buf_blocks}
    .quad {wb_xn} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {nc_blocks}
    .quad {nc} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .fill 344, 8, 0

    .balign 4096
    .global __pi_l2_gb3
__pi_l2_gb3:
    .fill 480, 8, 0
    .set blk, 480
    .rept 32
    .quad {dev3} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
"#,
        nrm = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL),
        dev = const cpu::boot::block(0, cpu::boot::ATTR_DEVICE),
        nc = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL_NC),
        wb_xn = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL) | cpu::boot::xn(false),
        buf_blocks = const BUF_BLOCKS,
        nc_blocks = const 8 - BUF_BLOCKS,
        dev3 = const cpu::boot::block(0xC000_0000, cpu::boot::ATTR_DEVICE),
    );

    /// De buffers van de GENET onderin de DMA-regio, Normal-WB.
    pub(super) const BUF_BLOCKS: u64 = driver_genet::DMA_NEED.div_ceil(board_raspi::map::MB2);

    // De `.rept`-tellingen hierboven tegen het plan.
    const _: () = {
        use board_raspi::map::{DEVICE_WINDOW, DMA, FIXED_END, LOADER, MB2, NET_DMA};
        assert!(LOADER.base + LOADER.size == 128 * MB2);
        // De NIC-regio begint de DMA-regio, en de buffers vullen hele
        // blokken binnen de NIC-helft (de mailbox en de USB blijven NC).
        assert!(NET_DMA.base == DMA.base && BUF_BLOCKS * MB2 <= NET_DMA.size);
        assert!(DEVICE_WINDOW.size == 32 * MB2);
        assert!(DMA.size == 8 * MB2);
        assert!(FIXED_END == 168 * MB2);
        assert!(super::PERIPH == 0xC000_0000 + 480 * MB2);
        // De RNG200 en het PM-blok liggen in het Device-venster.
        assert!(super::RNG200.0 >= super::PERIPH && super::PM.0 >= super::PERIPH);
        assert!(super::RNG200.0 < 1 << 32 && super::PM.0 < 1 << 32);
    };

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    pub(super) fn tables() -> Option<(Pa, Pa, Pa)> {
        unsafe extern "C" {
            static __boot_ttbr0: u8;
            static __pi_l2_gb0: u8;
            static __pi_l2_gb3: u8;
        }
        let a = |p: *const u8| Pa(p as usize as u64);
        Some((
            a(&raw const __boot_ttbr0),
            a(&raw const __pi_l2_gb0),
            a(&raw const __pi_l2_gb3),
        ))
    }

    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    pub(super) fn tables() -> Option<(Pa, Pa, Pa)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::Board;

    /// Blok 160 (0x1400_0000) draagt de buffers van de GENET (Normal-WB);
    /// 161 tot 167 blijven NC.
    #[test]
    fn the_genet_buffers_are_block_160() {
        assert_eq!(arch::BUF_BLOCKS, 1);
        assert_eq!(
            board_raspi::map::NET_DMA.base / board_raspi::map::MB2,
            128 + 32
        );
    }

    #[test]
    fn a72_numbers_cores_in_aff0() {
        assert_eq!(slots::mpidr(3), 3);
        assert_eq!(<Bcm2711 as Soc>::mpidr(1), 1);
    }

    #[test]
    fn the_board_has_a_name_and_no_host_tables() {
        assert_eq!(<Rpi4 as Board>::NAME, "rpi4");
        assert!(Bcm2711::tables().is_none());
        const { assert!(PERIPH < GENET.0 && GENET.0 < UART0.0 && UART0.0 < GIC.0) };
    }
}

/// Het blok van dit board in zijn image (image/cfg/rpi4.cfg) zegt wat de
/// code doet zonder die regels: de sharegroup van Hop en de klasse van zijn
/// core. Zo lopen config en standaard niet uiteen.
#[cfg(test)]
mod cfg_block {
    use board::Board;

    #[test]
    fn the_board_block_says_what_the_code_does() {
        let t = include_str!("../../../image/cfg/rpi4.cfg");
        assert_eq!(
            fw::bootcfg::get(t, "hopos.hop.sharegroup"),
            <crate::Machine as Board>::HOP_GROUP
        );
        let class = <crate::Machine as Board>::HOP_CORE_CLASS.map(|c| format!("{c}"));
        assert_eq!(
            fw::bootcfg::get(t, "hopos.hop.coreclass"),
            class.as_deref().unwrap_or("")
        );
    }
}
