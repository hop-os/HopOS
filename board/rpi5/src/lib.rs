//! Raspberry Pi 5 (BCM2712, 4x Cortex-A76): het BCM2712-eigene. De rest
//! (boot, kaart, plan, mailbox, GIC-bedrading) is `board_raspi`.
//!
//! Boot zonder UEFI: de EEPROM-bootloader laadt `hop-agent5.img` rauw op
//! 0x80000 (hij negeert `kernel_address`, en eist `os_check=0`) en levert
//! ons op EL2 af; PSCI komt van de armstub (TF-A BL31) op EL3 via SMC.
//!
//! Netwerk: de hele keten is van ons, de firmware doet er niets van
//! (gemeten 10-07): RESCAL, de pcie2-RC met de 54 MHz-PLL, link-training
//! (gen 2), de RP1 (1de4:0001), zijn BAR's (BAR1 op PCIe 0: de
//! DMA-loopback-eis), de PHY-reset via RP1-GPIO32, autonegotiatie, de GEM,
//! en de interrupt: RP1-MSI-X naar de MIP naar GIC-SPI 128+n.
//!
//! LET OP de C1-stepping: de BCM2712 C1 heeft een interconnect-deadlock
//! onder PCIe-inbound-DMA (OLD/docs/v1/archief/bcm2712-c1-erratum.md): een
//! stille totale freeze bij aanhoudend RX-verkeer plus fabric-breed werk
//! (image-kopie, cache-onderhoud, TLBI, core-start). D0 heeft de fix. De
//! registerkeuzes die de kans verkleinen staan in `driver-brcmpcie` en
//! `driver-gem` (MAX_BURST 128 B, VDM-QoS, AMP 8/8, PAE).

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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use cpu::irq::{Line, Trigger};
use dev::Pa;
use driver_brcmpcie::{EpBar, InWin, OutWin, Rc};
use driver_gem::Gem;
use driver_pcie::{self as pcie, Bdf, Config as _};
use driver_pl011::Pl011;

// De twee xHCI's in de RP1 (usb.rs).
mod usb;

/// De debug-UART (PL011, de 3-pins JST-SH-connector; Linux ttyAMA10). De
/// firmware zet hem op 115200 zodra hij zelf logt (`uart_2ndstage=1`).
pub const UART10: Pa = Pa(0x10_7D00_1000);
/// De GIC-400: distributor op +0x1000, CPU-interface op +0x2000.
pub const GIC: Pa = Pa(0x10_7FFF_8000);
/// De VideoCore-mailbox (DT mailbox@7c013880).
pub const VCMAIL: Pa = Pa(0x10_7C01_3880);
/// De RNG200 (DT rng@7d208000; de soc-ranges leggen 0 op 0x10_0000_0000).
/// Go: `RNG200Base`.
pub const RNG200: Pa = Pa(0x10_7D20_8000);
/// Het PM-blok met de watchdog (DT watchdog@7d200000, `brcm,bcm2712-pm`).
/// Go: de `WatchdogBase` uit `raspi.SetupPlan`.
pub const PM: Pa = Pa(0x10_7D20_0000);
/// De PCIe-root-complex van de RP1-link (pcie2).
pub const PCIE2: Pa = Pa(0x10_0012_0000);
/// Het gedeelde RESCAL-blok van de PCIe-controllers.
pub const PCIE_RESCAL: u64 = 0x10_0011_9500;
/// De brcm,brcmstb-reset SW_INIT-bank.
pub const PCIE_SW_INIT: Pa = Pa(0x10_0150_4318);
/// Het bridge-reset-id van pcie2 (42/43/44 = pcie0/1/2).
pub const PCIE2_SW_INIT_ID: u32 = 44;
/// De MIP: een MSI-write naar [`MIP_MSI_ADDR`] wordt GIC-SPI 128 + data.
pub const MIP: Pa = Pa(0x10_0013_0000);
/// Het PCIe-adres dat een endpoint in zijn MSI-X-entry zet.
pub const MIP_MSI_ADDR: u64 = 0xff_ffff_f000;
/// De eerste SPI van de MIP.
pub const MIP_FIRST_SPI: u32 = 128;
/// Het ARM-zicht op de RP1: het peripheral-venster (RP1-adres - 0x4000_0000).
pub const RP1: u64 = 0x1f_0000_0000;
/// De GEM in de RP1.
pub const RP1_ETH: Pa = Pa(RP1 + 0x10_0000);
/// RP1-IRQ van de ethernet (dt-bindings/mfd/rp1.h).
pub const RP1_INT_ETH: u32 = 6;
/// De DMA-vertaling van de RP1: busadres = fysiek + dit (het
/// inbound-window van de RC legt PCIe 0x10_0000_0000 op DRAM 0).
pub const RP1_BUS_OFF: u64 = 0x10_0000_0000;
/// De compatible van de RP1-ethernet in de DTB.
pub const GEM_COMPAT: &str = "cdns,macb";

// SAFETY: de debug-PL011 van de BCM2712, in de Device-gigabyte 65 van de
// vaste tabel.
static UART: Pl011 = unsafe { Pl011::new(UART10) };

// SAFETY: de GIC-400 van de BCM2712, in dezelfde Device-gigabyte.
static GIC400: Gic = unsafe { Gic::new(GIC.add(0x1000), GIC.add(0x2000)) };

/// De BCM2712 als SoC.
pub struct Bcm2712;

/// De Pi 5 als board.
pub type Rpi5 = Raspi<Bcm2712>;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Rpi5;

impl Soc for Bcm2712 {
    type Nic = Gem;
    const NAME: &'static str = "rpi5";
    const SOC: &'static str = "BCM2712";
    const MAC_FALLBACK: u8 = 0x05;
    const VCMAIL: Pa = VCMAIL;
    const RNG200: Pa = RNG200;
    const PM: Pa = PM;

    fn uart() -> &'static Pl011 {
        &UART
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

    fn usb_hosts(ctx: &board_raspi::usb::UsbCtx) -> board::UsbHosts {
        usb::hosts(ctx)
    }

    /// De RP1-keten op 5 en 30 s (de flip-jacht van 30-09).
    fn nic_diag() {
        nic_diag();
    }

    fn tables() -> Option<Tables> {
        let (l1, gb0) = arch::tables()?;
        Some(Tables {
            l1,
            l2: [
                Some(L2 {
                    gb: 0,
                    table: gb0,
                    fixed: abi::Region::new(0, map::FIXED_END),
                }),
                None,
            ],
        })
    }

    fn probe_nic(ctx: &NicCtx) -> Result<Option<Gem>, Error> {
        if board_raspi::device_enabled(GEM_COMPAT) == Some(false) {
            cpu::println!("net: {GEM_COMPAT} disabled in the DTB, no NIC");
            return Ok(None);
        }
        rp1_quiesce(ctx.now);
        let rc = rp1_link(ctx.now)?;
        LINK_OURS.store(true, Relaxed);
        // De ethernet-PHY (BCM54213PE) hangt in reset aan RP1-GPIO32
        // (actief laag, DT phy-reset-gpios; gemeten: zonder dit géén PHY op
        // MDIO).
        rp1_gpio_out(32, false);
        dev::delay(ctx.now, 10_000_000);
        rp1_gpio_out(32, true);
        dev::delay(ctx.now, 50_000_000);

        // SAFETY: RP1_ETH ligt achter de net getrainde link in het
        // outbound-window (Device-gigabyte 124); de NIC-regio is van deze
        // driver alleen, Normal-NC met zijn bufferstuk Normal-WB (`arch`),
        // en de RP1 bereikt hem via het inbound-window op fysiek +
        // RP1_BUS_OFF.
        let mut nic = unsafe {
            Gem::new(
                RP1_ETH,
                RP1_BUS_OFF,
                Pa(ctx.dma.base),
                ctx.dma.size,
                ctx.mac,
            )
        };
        nic.mdio_enable();
        let phy = driver_mdio::scan(&mut nic).ok_or(Error::Nic("rp1: no PHY on MDIO"))?;
        cpu::println!(
            "net: rp1 up, PHY at {} ({:#06x}:{:#06x}), autonegotiating",
            phy.addr,
            phy.id1,
            phy.id2
        );
        let link = driver_mdio::autoneg(&mut nic, phy.addr, true, ctx.now, 8_000_000_000)
            .map_err(|_| Error::Nic("rp1: no link within 8 s"))?;
        nic.init(link)
            .map_err(|_| Error::Nic("gem: DMA region too small"))?;
        match wire_irq(&rc, &mut nic) {
            Ok(id) => {
                cpu::println!("net: gem up, {link}, RX on INTID {id} (RP1 MSI-X via the MIP)")
            }
            Err(why) => cpu::println!("net: gem up, {link}, RX polled: {why} HOPOS_NIC_POLLED"),
        }
        Ok(Some(nic))
    }
}

/// Heeft deze kern de RP1-link zelf getraind? Tot dan is een link die up
/// staat een erfenis van de kern vóór een flip, met zijn DMA-masters nog
/// aan het werk ([`rp1_quiesce`]).
static LINK_OURS: AtomicBool = AtomicBool::new(false);

/// Een RC-handvat zonder windows, om te lezen wat er staat of een
/// endpoint stil te leggen: er wordt niets opgezet.
pub(crate) fn rc_bare(now: fn() -> u64) -> Rc {
    let none = OutWin {
        cpu: 0,
        pcie: 0,
        size: 0,
    };
    // SAFETY: PCIE2 en de SW_INIT-bank zijn BCM2712-blokken in de
    // Device-gigabyte 64 (de vaste tabel); zonder windows zet het handvat
    // niets op.
    unsafe {
        Rc::new(
            driver_brcmpcie::Soc::Bcm2712,
            PCIE2,
            PCIE_SW_INIT,
            PCIE2_SW_INIT_ID,
            0,
            none,
            [None; driver_brcmpcie::MAX_IN],
            now,
        )
    }
}

/// Legt een RP1 stil die de vorige kern liet draaien, vóór [`rp1_link`]
/// de link eronder reset. Een koude boot vindt de link down (de RP1 in
/// PERST#) en doet niets; een warme landing erft hem up, met een GEM die
/// nog ontvangt en (gui) xHCI's die nog lopen. Wie daaronder de bridge
/// reset, kan hun AXI-transacties naar de host halverwege laten staan, en
/// de GEM heeft geen eigen reset die dat opruimt. Dat is de verklaring
/// voor 03-10 (P1g, warm: de ring van de nieuwe kern bleef onaangeroerd,
/// `irq(nic=1)`, geen lease); op ijzer nog niet bewezen. De volgorde is
/// die van Linux' kexec-weg: eerst de drivers (`macb_shutdown`,
/// de xHCI-halt), dan de bus-master-bit, dan PERST# (`brcm_pcie_turn_off`).
/// De O6N kent dit niet: daar blijft de link van de firmware staan en
/// stopt de CmdReset van de RTL zijn eigen DMA.
fn rp1_quiesce(now: fn() -> u64) {
    if LINK_OURS.load(Relaxed) {
        return;
    }
    let rc = rc_bare(now);
    let (phy, dl) = rc.link_status();
    if !(phy && dl) {
        return;
    }
    let id = rc.cfg_read32(1, 0, 0, 0);
    if id != RP1_ID {
        cpu::println!(
            "net: the RP1 link was up but the endpoint reads {id:#x}, nothing quiesced HOPOS_RP1_QUIESCE"
        );
        return;
    }
    // SAFETY: RP1_ETH is het GEM-blok achter de link die DL actief meldt;
    // de vorige kern opende de BAR's, en niemand anders raakt de GEM nu.
    unsafe { driver_gem::stop(RP1_ETH) };
    let halted = usb::halt_inherited(now);
    rc.turn_off();
    cpu::println!(
        "net: the RP1 link was up from the previous kernel: gem stopped, {halted} of 2 xHCIs halted, bus mastering off and PERST# held before the bring-up HOPOS_RP1_QUIESCE"
    );
}

/// De RP1 in zijn configruimte: device 0x0001, vendor 0x1de4.
const RP1_ID: u32 = 0x0001_1de4;

/// De link naar de RP1, boardvast bewezen met probe6 (10-07, runs 2/4/5):
/// RESCAL, de pcie2-RC (54 MHz-PLL!), link-training (gen 2), de RP1
/// (1de4:0001) en zijn BAR's. BAR1 MOET op PCIe 0: RP1's eigen DMA bereikt
/// zijn peripherals via de loopback door het eerste inbound-window.
fn rp1_link(now: fn() -> u64) -> Result<Rc, Error> {
    let inb = [
        // RP1-loopback (BAR1).
        Some(InWin {
            pcie: 0,
            cpu: RP1,
            size: 0x40_0000,
        }),
        // Al het DRAM.
        Some(InWin {
            pcie: RP1_BUS_OFF,
            cpu: 0,
            size: 0x10_0000_0000,
        }),
        // De MIP: het MSI-X-doel van de RP1 (bcm2712.dtsi dma-ranges). Zonder
        // dit window slaat een MSI-write op een ongemapt PCIe-adres,
        // kansgedreven fabric-gif midden in de RX-stroom (13-07).
        Some(InWin {
            pcie: MIP_MSI_ADDR,
            cpu: MIP.0,
            size: 0x1000,
        }),
        None,
    ];
    let out = OutWin {
        cpu: RP1,
        pcie: 0,
        size: 0x1000_0000,
    };
    // SAFETY: PCIE2 en de SW_INIT-bank zijn BCM2712-blokken in de
    // Device-gigabyte 64; de windows zijn het adresplan hierboven.
    let rc = unsafe {
        Rc::new(
            driver_brcmpcie::Soc::Bcm2712,
            PCIE2,
            PCIE_SW_INIT,
            PCIE2_SW_INIT_ID,
            2,
            out,
            inb,
            now,
        )
    };
    // BAR-groottes gemeten met probe6: 16 KB / 4 MB / 64 KB.
    let bars = [
        EpBar {
            off: 0x10,
            val: 0x100_0000,
        },
        EpBar { off: 0x14, val: 0 },
        EpBar {
            off: 0x18,
            val: 0x101_0000,
        },
    ];
    // SAFETY: PCIE_RESCAL is het gedeelde RESCAL-blok (Device-gigabyte 64).
    unsafe { rc.bring_up(PCIE_RESCAL, RP1_ID, &bars) }.map_err(|e| {
        cpu::println!("net: {e}");
        Error::Nic("rp1: PCIe bring-up failed")
    })?;
    cpu::println!(
        "net: pcie2 link up (status {:#x}), RP1 1de4:0001",
        rc.status()
    );
    Ok(rc)
}

/// Zet RP1-GPIO `pin` als software-output (funcsel 5 = sys_rio) op het
/// gegeven niveau: eerst het niveau, dán output-enable, geen glitch. Alleen
/// bij een getrainde link met BAR1 op PCIe 0.
fn rp1_gpio_out(pin: u32, high: bool) {
    let (bank, off) = match pin {
        34.. => (2, pin - 34),
        28.. => (1, pin - 28),
        _ => (0, pin),
    };
    let io = Pa(RP1 + 0xd_0000 + bank * 0x4000);
    let rio = Pa(RP1 + 0xe_0000 + bank * 0x4000);
    let pads = Pa(RP1 + 0xf_0000 + bank * 0x4000);
    let o = u64::from(off);
    // Pad: output-disable (bit 7) eraf, de rest laten staan.
    let pad = pads.add(4 + o * 4);
    dev::write32(pad, dev::read32(pad) & !(1 << 7));
    dev::write32(io.add(o * 8 + 4), 5);
    let out = if high {
        rio.add(0x2000)
    } else {
        rio.add(0x3000)
    };
    dev::write32(out, 1 << off);
    dev::write32(rio.add(0x2000 + 4), 1 << off);
    dev::mb();
}

/// De RP1-kant van de MSI (drivers/mfd/rp1.c): per peripheral-IRQ een
/// MSIX_CFG-woord op 0x10_8000 + 8 + 4n, met de SET-alias op +0x800.
const RP1_MSIX_CFG: u64 = RP1 + 0x10_8000 + 0x8 + 0x800;
const RP1_MSIX_ENABLE: u32 = 1 << 0;
const RP1_MSIX_IACK: u32 = 1 << 2;
const RP1_MSIX_IACK_EN: u32 = 1 << 3;

/// De rearm van de board-kant: de RP1 bekijkt de lijn opnieuw (IACK-modus:
/// na één MSI wacht hij hierop voor hij een nog staande lijn opnieuw
/// afvuurt).
fn rp1_iack() {
    dev::write32(Pa(RP1_MSIX_CFG + 4 * u64::from(RP1_INT_ETH)), RP1_MSIX_IACK);
    dev::mb();
}

/// De device-ack van de dispatcher.
fn gem_ack() {
    // SAFETY: RP1_ETH is het GEM-blok; `wire_irq` zet de lijn pas aan
    // nadat de GEM leeft, en die leeft zolang de node draait.
    unsafe { driver_gem::ack_irq(RP1_ETH) };
}

/// De bel van de GEM-lijn: de dispatch luidt hem, de RX-pomp wacht erop.
static GEM_BELL: sync::Signal = sync::Signal::new();

/// De GIC-lijn van MIP-vector `v`, en zijn soort: SPI 128 + v, en een flank.
///
/// Een MSI via de MIP is een puls (Linux, irq-bcm2712-mip.c: de MIP-lijnen
/// gaan als IRQ_TYPE_EDGE_RISING naar de GIC, en de MIP zelf staat met
/// CFG_HOST op flank), en een SPI staat in de GIC-400 na reset op level. De
/// dispatcher zet de soort vóór de enable (`cpu::irq::enable_as`), zodat
/// het board hem niet los kan vergeten.
const fn mip_line(v: u32) -> (Line, Trigger) {
    (Line(32 + MIP_FIRST_SPI + v), Trigger::Edge)
}

/// Bedraadt de GEM-interrupt: RP1-MSI-X-entry naar de MIP naar de GIC.
/// Elke stap die faalt laat de NIC pollen, met de reden.
fn wire_irq(rc: &Rc, nic: &mut Gem) -> Result<u32, &'static str> {
    // 1. De MSI-X-capability van de RP1 (bus 1, dev 0): tabel-BAR en offset.
    let f = pcie::probe(rc, RP1_BDF).ok_or("rp1: nothing on bus 1")?;
    let m = f.msix(rc).ok_or("rp1: no MSI-X capability")?;
    if u32::from(m.size) <= RP1_INT_ETH {
        return Err("rp1: MSI-X table too small");
    }
    let table = f
        .msix_table_addr(rc, &m)
        .ok_or("rp1: MSI-X BAR not assigned")?;
    let entry = Pa(RP1 + table + 16 * u64::from(RP1_INT_ETH));
    MSIX_ENTRY.store(entry.0, Relaxed);
    dev::write32(entry, MIP_MSI_ADDR as u32);
    dev::write32(entry.add(4), (MIP_MSI_ADDR >> 32) as u32);
    dev::write32(entry.add(8), RP1_INT_ETH);
    dev::write32(entry.add(12), 0);
    dev::mb();
    // Function mask eraf, MSI-X aan: één dword over de kop, zoals op ijzer
    // bewezen (`Function::msix_enable` schrijft een halfwoord en zet INTx
    // uit, Linux' weg, maar die is hier nog niet gezien).
    let hdr = rc.read32(RP1_BDF, m.cap);
    rc.write32(RP1_BDF, m.cap, (hdr & !(1 << 30)) | (1 << 31));

    // 2. De MIP open naar de host (mip_probe: MASK_HOST 0, MASK_VPU ~0,
    //    CFG_HOST ~0), de lijn als flank (de MIP levert een MSI als edge).
    for (o, v) in [
        (0x40, 0),
        (0x50, 0),
        (0x60, u32::MAX),
        (0x70, u32::MAX),
        (0x20, u32::MAX),
        (0x30, u32::MAX),
    ] {
        dev::write32(MIP.add(o), v);
    }
    dev::mb();
    let (line, trigger) = mip_line(RP1_INT_ETH);
    let id = line.0;
    cpu::irq::enable_as(line, trigger, Some(gem_ack), Some(&GEM_BELL))
        .map_err(|_| "gic: line refused")?;

    // 3. De RP1: MSI aan in IACK-modus, en de GEM zelf open.
    dev::write32(
        Pa(RP1_MSIX_CFG + 4 * u64::from(RP1_INT_ETH)),
        RP1_MSIX_ENABLE | RP1_MSIX_IACK_EN,
    );
    dev::mb();
    // Eén IACK om te beginnen: de vorige kern kan midden in een afhandeling
    // gesprongen zijn (de MSI gestuurd, de IACK nooit gegeven), en dan
    // wacht de RP1 eeuwig op een kern die er niet meer is. GEMETEN 30-09
    // (drie flips vanuit de koud gebootte kaart-kern): de GEM ontving
    // (rxstatus 0x2, de ringpointer liep) en hield zijn latch (isr 0x2),
    // maar er kwam geen MSI meer, irq(nic=0). Op een koude boot doet de
    // IACK niets.
    rp1_iack();
    nic.set_irq(&GEM_BELL, rp1_iack);
    Ok(id)
}

/// De RP1 achter de RC.
const RP1_BDF: Bdf = Bdf {
    bus: 1,
    dev: 0,
    func: 0,
};

/// De MSI-X-entry van de GEM in de RP1-tabel, zoals `wire_irq` hem zette
/// (0 = nog niet), voor [`nic_diag`].
static MSIX_ENTRY: AtomicU64 = AtomicU64::new(0);

/// Registerdump van de RP1-NIC-keten voor de flip-jacht (30-09 en 03-10:
/// warme flips vanuit de koud gebootte kaart-kern eindigden met een NIC die
/// niets meer ontving): de RX-ring, dan de PCIe-kant (de status van de RC,
/// het command-register van de RP1 met de bus-master-bit, de MSI-X-
/// capability met enable en function mask, de entry met zijn mask-bit, het
/// RP1-MSIX_CFG-woord en de MIP), dan de GEM zelf, met waar zijn
/// RX-queue-pointer in onze ring staat. De tik roept hem op 5 en 30 s, dus
/// een koude boot geeft de referentie en een landing het verschil.
pub fn nic_diag() {
    let rc = rc_bare(cpu::idle::now);
    let (phy, dl) = rc.link_status();
    if !(phy && dl) {
        cpu::println!(
            "net: rp1 diag: pcie {:#x} (phy {phy}, dl {dl}): link down, nothing behind it to read HOPOS_RP1_DIAG",
            rc.status()
        );
        return;
    }
    // De RX-ring ligt vooraan in NET_DMA (driver_gem::Gem::new: rx_ring =
    // dma). Eerst zoals de CPU hem leest, dan na een clean-en-invalidate
    // van die regels: verschillen ze, dan leest de CPU uit zijn cache en is
    // de ring niet non-cacheable gemapt.
    let ring = Pa(map::NET_DMA.base);
    let before = driver_gem::ring_words(ring);
    dev::pull(ring, 64);
    let after = driver_gem::ring_words(ring);
    cpu::println!(
        "net: rp1 diag: rx ring {:#x}: desc0..3 (w0,w1) {before:x?}, after invalidate {after:x?}",
        ring.0
    );
    let cmd = rc.cfg_read32(1, 0, 0, 0x04);
    let ctl = pcie::probe(&rc, RP1_BDF)
        .and_then(|f| f.msix(&rc).map(|m| f.msix_control(&rc, &m)))
        .unwrap_or(0);
    let e = MSIX_ENTRY.load(Relaxed);
    let entry = if e == 0 {
        [0; 4]
    } else {
        [0u64, 4, 8, 12].map(|o| dev::read32(Pa(e).add(o)))
    };
    let cfg = dev::read32(Pa(RP1 + 0x10_8000 + 0x8 + 4 * u64::from(RP1_INT_ETH)));
    let mip = [0x00u64, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70].map(|o| dev::read32(MIP.add(o)));
    cpu::println!(
        "net: rp1 diag: pcie {:#x}, rp1 cmd {cmd:#x} (bus master {}), msix ctl {ctl:#x} (enable {}, function mask {}), entry {entry:x?} (masked {}), msix_cfg[eth] {cfg:#x}, mip {mip:x?} HOPOS_RP1_DIAG",
        rc.status(),
        cmd & (1 << 2) != 0,
        ctl & (1 << 15) != 0,
        ctl & (1 << 14) != 0,
        entry[3] & 1 != 0,
    );
    // SAFETY: RP1_ETH is het GEM-blok achter de getrainde link (net gelezen).
    let g = unsafe { driver_gem::diag(RP1_ETH) };
    cpu::println!(
        "net: rp1 diag: gem nwctrl {:#x} nwcfg {:#x} nwstatus {:#x} dmacfg {:#x} txstatus {:#x} rxqbase {:#x} (rx desc {:?} of our ring) rxstatus {:#x} isr {:#x} imr {:#x} HOPOS_RP1_DIAG",
        g[0],
        g[1],
        g[2],
        g[3],
        g[4],
        g[5],
        driver_gem::rx_index(g[5], ring.0 + RP1_BUS_OFF),
        g[6],
        g[7],
        g[8]
    );
}

/// Het slot-plan: dat van `board_raspi`, met de A76-nummering.
pub mod slots {
    pub use board_raspi::slots::*;

    /// Het MPIDR-target van logische core `core`. LET OP: de Cortex-A76
    /// nummert in affiniteit 1 (MT-formaat: aff0 = thread, altijd 0),
    /// anders dan de A72 van de Pi 4 en QEMU's A53.
    #[must_use]
    pub const fn mpidr(core: usize) -> u64 {
        (core as u64) << 8
    }

    /// De logische core bij een MPIDR: aff1.
    #[must_use]
    pub const fn core_of(mpidr: u64) -> usize {
        ((mpidr >> 8) & 0xff) as usize
    }
}

board_raspi::pi_entry!(0x10_7D00_1000u64, board_raspi::map::FIXED_END);

mod arch {
    //! De vaste tabellen van de Pi 5, als data in het image.
    //!
    //! - niveau 1: gigabyte 0 via een eigen niveau-2-tabel; 64 en 65 (de
    //!   SoC-peripherals op 0x10_0000_0000: PCIe, MIP, GIC, UART, mailbox)
    //!   en 124 (het RP1-venster op 0x1f_0000_0000) als Device-blokken; de
    //!   rest leeg tot `discover` er DRAM bij zet;
    //! - gigabyte 0: als de Pi 4 (`board_raspi::map`), op het bufferstuk
    //!   van de GEM na: zijn framebuffers (`driver_gem::BUF_OFF`, 0x1420_0000
    //!   tot 0x1460_0000) zijn Normal-WB en XN, de driver veegt ze; de
    //!   descriptors eronder en de rest van de DMA-regio blijven NC.
    use board_raspi::map::{DMA, MB2, NET_DMA};
    use dev::Pa;

    /// De NC-blokken van de DMA-regio vóór het bufferstuk van de GEM.
    pub(super) const NC_LO: u64 = (NET_DMA.base + driver_gem::BUF_OFF - DMA.base) / MB2;
    /// Het bufferstuk van de GEM, Normal-WB.
    pub(super) const BUF_BLOCKS: u64 = driver_gem::BUF_BLOCK / MB2;
    /// De NC-blokken erna: de rest van de NIC-helft, mailbox en USB.
    pub(super) const NC_HI: u64 = DMA.size / MB2 - NC_LO - BUF_BLOCKS;

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    core::arch::global_asm!(
        r#"
    .section .data.pagetables, "aw"
    .balign 4096
    .global __boot_ttbr0
__boot_ttbr0:
    .quad __pi_l2_gb0 + 3
    .fill 63, 8, 0
    .quad {d64}
    .quad {d65}
    .fill 58, 8, 0
    .quad {d124}
    .fill 387, 8, 0

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
    .rept {nc_lo}
    .quad {nc} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {buf_blocks}
    .quad {wb_xn} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .rept {nc_hi}
    .quad {nc} + (blk * 0x200000)
    .set blk, blk + 1
    .endr
    .fill 344, 8, 0
"#,
        nrm = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL),
        dev = const cpu::boot::block(0, cpu::boot::ATTR_DEVICE),
        nc = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL_NC),
        wb_xn = const cpu::boot::block(0, cpu::boot::ATTR_NORMAL) | cpu::boot::xn(false),
        nc_lo = const NC_LO,
        buf_blocks = const BUF_BLOCKS,
        nc_hi = const NC_HI,
        d64 = const cpu::boot::block(64 << 30, cpu::boot::ATTR_DEVICE),
        d65 = const cpu::boot::block(65 << 30, cpu::boot::ATTR_DEVICE),
        d124 = const cpu::boot::block(124 << 30, cpu::boot::ATTR_DEVICE),
    );

    // De tellingen hierboven tegen het plan en de adressen.
    const _: () = {
        use board_raspi::map::{DEVICE_WINDOW, FIXED_END, LOADER};
        assert!(LOADER.base + LOADER.size == 128 * MB2);
        assert!(DEVICE_WINDOW.size == 32 * MB2);
        assert!(DMA.size == 8 * MB2);
        // Het bufferstuk: hele blokken, binnen de NIC-helft, boven de
        // descriptors.
        assert!(driver_gem::BUF_OFF.is_multiple_of(MB2));
        assert!(driver_gem::BUF_BLOCK.is_multiple_of(MB2));
        assert!(driver_gem::BUF_OFF + driver_gem::BUF_BLOCK <= NET_DMA.size);
        assert!(NET_DMA.base >= DMA.base && NC_LO + BUF_BLOCKS + NC_HI == 8);
        assert!(FIXED_END == 168 * MB2);
        assert!(super::PCIE2.0 >> 30 == 64 && super::MIP.0 >> 30 == 64);
        assert!(super::UART10.0 >> 30 == 65 && super::GIC.0 >> 30 == 65);
        assert!(super::VCMAIL.0 >> 30 == 65);
        assert!(super::RNG200.0 >> 30 == 65 && super::PM.0 >> 30 == 65);
        assert!(super::RP1 >> 30 == 124);
    };

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    pub(super) fn tables() -> Option<(Pa, Pa)> {
        unsafe extern "C" {
            static __boot_ttbr0: u8;
            static __pi_l2_gb0: u8;
        }
        let a = |p: *const u8| Pa(p as usize as u64);
        Some((a(&raw const __boot_ttbr0), a(&raw const __pi_l2_gb0)))
    }

    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    pub(super) fn tables() -> Option<(Pa, Pa)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::Board;

    /// Blok 161 en 162 (0x1420_0000 tot 0x1460_0000) zijn de framebuffers
    /// van de GEM (Normal-WB), 160 de descriptors en 163 tot 167 de rest
    /// van de DMA-regio (NC).
    #[test]
    fn the_gem_buffers_are_blocks_161_and_162() {
        assert_eq!((arch::NC_LO, arch::BUF_BLOCKS, arch::NC_HI), (1, 2, 5));
        assert_eq!(
            (board_raspi::map::NET_DMA.base + driver_gem::BUF_OFF) / board_raspi::map::MB2,
            128 + 32 + arch::NC_LO
        );
    }

    #[test]
    fn a76_numbers_cores_in_aff1() {
        assert_eq!(slots::mpidr(2), 0x200);
        assert_eq!(slots::core_of(0x8100_0300), 3);
        assert_eq!(<Bcm2712 as Soc>::core_of(<Bcm2712 as Soc>::mpidr(1)), 1);
    }

    #[test]
    fn the_board_has_a_name_and_the_nic_line_is_mip_vector_6() {
        assert_eq!(<Rpi5 as Board>::NAME, "rpi5");
        assert!(Bcm2712::tables().is_none());
        assert_eq!(32 + MIP_FIRST_SPI + RP1_INT_ETH, 166);
        // De NIC-lijn is een flank: een MSI via de MIP (30-09).
        assert_eq!(mip_line(RP1_INT_ETH), (Line(166), Trigger::Edge));
        assert_eq!(RP1_MSIX_CFG, 0x1f_0010_8808);
    }
}
