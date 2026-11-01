//! Radxa Zero 3E (Rockchip RK3566: 4× Cortex-A55, GIC-600, DW-APB-UART,
//! DWMAC-4.20a GMAC met een RTL8211F): Dereks échte doelbord ("main
//! device", 05-08).
//!
//! Boot-route (het verschil met QEMU en de Pi's): BootROM, TPL/SPL (DDR),
//! TF-A (bl31, EL3), U-Boot (EL2), `booti` met ons arm64-Image, entry op
//! EL2 met x0 = DTB. De keten is van Radxa (donor-bytes uit hun image, zie
//! `image/radxa-zero3.sh`); U-Boot's distro-boot vindt `extlinux.conf`,
//! laadt het Image en `hopos.cfg` als initrd, en geeft de APPEND-regel als
//! bootargs. Alle drie de kanalen GEMETEN werkend op 05-08 (Go). Sinds
//! 30-09 draagt de initrd naast `hopos.cfg` ook het image van Hop (de
//! container van [`initrd`]); de kanalen zelf bleven zoals ze gemeten zijn.
//!
//! Dit crate bezit de adressen van het board (RK3566-TRM en
//! rk356x-base.dtsi), de identity map, het plan, de bedrading van de
//! drivers en de SoC-glue eronder ([`soc`]), de watchdog ([`watchdog`]), de
//! temperatuursensor ([`tsadc`]), de klokknop ([`clock`], met de I2C-bus
//! van vdd_cpu in [`i2c`]) en het TRNG als bron van de DRBG van de kern
//! (`rng`). De drivers kennen geen adres.
//!
//! De DTB en de initrd worden bij [`Board::discover`] naar de heap
//! gekopieerd: U-Boot legt ze in DRAM dat de identity map als Device mapt
//! (de pool), en een FDT-parser die ongealigneerd leest faultt daar. `dev`
//! kopieert woordgewijs; daarna leest de parser gecachet geheugen.

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

extern crate alloc;

pub mod clock;
// De beeldketen (display.rs) alleen in de gui-smaak: een kale node linkt
// geen regel display-code en er tekent geen logconsole in een buffer die
// niemand uitleest (Go: "geen dood gewicht", Derek 06-08). De fb-regio in
// het plan blijft in béíde smaken: één plan is goedkoper dan twee.
#[cfg(feature = "gui")]
mod display;
pub mod i2c;
pub mod initrd;
mod mmu;
mod rng;
pub mod slots;
pub mod soc;
pub mod tsadc;
// De twee DWC3-cores als USB-host (usb.rs), alleen met de bouwsteen `usb`
// (de gui- of de media-smaak).
#[cfg(feature = "usb")]
pub mod usb;
pub mod watchdog;

#[cfg(test)]
mod tests;

use abi::layout::Pool;
use abi::ring::Coherence;
use board::dtb::Kept;
use board::{Board, CoreClass, Dispatched, Error, NoDisk, Plan, Region};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use cpu::irq::Line;
use dev::Pa;
use driver_gicv3::{Gic, SysRegIcc};
use driver_mdio::{Phy, rtl8211f};
use driver_ns16550::Ns16550;
use driver_stmmac::dwmac4::{self, CSR_100_150M, Dwmac4, IrqAck, Probe};
use netdev::AckSlot;
use netdev::Mac;
use sync::Signal;

/// De debug-UART (UART2 op de 40-pins header: pin 8 TX, 10 RX, 6 GND):
/// DesignWare APB, 16550-compatibel, `reg-shift = 2`. U-Boot liet hem op
/// 1500000 8N1 staan; wij pollen en schrijven. GEMETEN 05-08: werkt.
pub const UART2: Pa = Pa(0xFE66_0000);
/// GMAC1 (snps,dwmac-4.20a): VERSION 0x3051, gemeten 05-08.
pub const GMAC1: Pa = Pa(0xFE01_0000);
/// De GIC-600: distributor.
pub const GICD: Pa = Pa(0xFD40_0000);
/// De redistributor-reeks: 128 KB per core, vier cores.
pub const GICR: Pa = Pa(0xFD46_0000);
/// De lengte van de redistributor-reeks.
pub const GICR_LEN: u64 = 0x8_0000;
/// De macirq van GMAC1: GIC_SPI 32, dus INTID 64 (level high).
pub const GMAC1_INTID: u32 = 32 + 32;
/// De PPI van de niet-beveiligde fysieke timer (CNTP): INTID 30.
pub const TIMER_PPI: u32 = 30;
/// De PPI van de EL2-fysieke timer (CNTHP): INTID 26. De deadline van de
/// executor terwijl een bewoner de OS-core heeft (`cpu::el2::OsCore`).
pub const HYP_TIMER_PPI: u32 = 26;
/// De kick van de OS-core. NIET 8 zoals op QEMU virt: TF-A op Rockchip
/// houdt SGI 8..15 als Secure Group 1 (`RK_IRQ_SEC_SGI_0..7` in
/// plat/rockchip), en een niet-beveiligde schrijf naar hun groep is daar
/// RAZ/WI; de kick zou stil verdwijnen. 0..7 zijn van de niet-beveiligde
/// wereld (Linux' IPI's), en wij zijn daar de enige. NOG NIET GEMETEN.
pub const KICK_SGI: u32 = 7;

/// Waar het DRAM begint. GEMETEN 05-08: U-Boot's /memory-node begint op
/// 0x20_0000; de eerste 2 MB is TF-A.
pub const DRAM_BASE: u64 = 0x0020_0000;
/// De kern-RAM: de Image-header, het image, stack en heap (Go: 64 MB vanaf
/// `RamBase`; HOP zat op ~20 MB). Het Image landt op 0x0220_0000:
/// 2 MB-gealigneerd, en `booti` legt een Image met relocatable = 0 op
/// `bi_dram[0].start + text_offset` (GEMETEN 05-08: met de verkeerde som
/// landde hij 2 MB te hoog en zweeg hij na "Starting kernel").
pub const KERN_RAM: Region = Region {
    base: Pa(0x0220_0000),
    size: 0x0400_0000,
};
/// Het structurenvenster: control-pages, kooien, boot-scratch,
/// levenstekens, vluchtrecorder en zwarte doos. Device, zie [`slots`].
pub const STRUCT_WINDOW: Region = Region {
    base: Pa(0x0620_0000),
    size: 0x0020_0000,
};
/// De NIC-DMA (8 MB), Normal-NC, op [`NET_BUF`] na.
pub const NET_DMA: Region = Region {
    base: Pa(0x0640_0000),
    size: 0x0080_0000,
};
/// Het bufferblok van de dwmac4 binnen [`NET_DMA`] (zijn `BUF_OFF`):
/// Normal-WB en niet uitvoerbaar (`mmu`), de driver veegt het zelf, zoals de
/// net-wb van de O6N en de Altra. Het waarom en de meting: de crate-doc van
/// driver/nic/stmmac (dwmac4).
pub const NET_BUF: Region = Region {
    base: Pa(NET_DMA.base.0 + dwmac4::BUF_OFF),
    size: dwmac4::BUF_BLOCK,
};
/// De xHCI-DMA (2 MB), Normal-NC: de twee DWC3-cores (`usb`), elk de helft.
pub const USB_DMA: Region = Region {
    base: Pa(0x06C0_0000),
    size: 0x0020_0000,
};
/// De framebuffer in DRAM (8 MB, 1920x1080x4), Normal-NC. U-Boot laat op
/// dit bord géén scherm achter (GEMETEN 05-08: geen simple-framebuffer, en
/// `Out: serial@fe660000` zonder vidconsole); met de feature `gui` scant
/// de VOP2 hem uit naar HDMI (`display`, `gui-rkscan`).
pub const FB_RAM: Region = Region {
    base: Pa(0x0700_0000),
    size: 0x0080_0000,
};
/// Alle ongecachete DMA-regio's samen.
pub const DMA: Region = Region {
    base: Pa(0x0640_0000),
    size: 0x0140_0000,
};
/// Het staging-venster (16 MB, Device): de boot-scratch-kant van de
/// kern-flip legt hier een nieuwe kern plat (`slots::STAGE_PA`), buiten
/// elke pool, zoals op QEMU virt. Op dit board nog niet op ijzer gebruikt.
pub const STAGE_WINDOW: Region = Region {
    base: Pa(0x0780_0000),
    size: 0x0100_0000,
};
/// Vanaf hier de app-partities.
pub const POOL_BASE: u64 = 0x0880_0000;
/// Tot hier gaat de pool: de identity map dekt de eerste 4 GB, en vanaf
/// 0xF000_0000 zit de MMIO van de SoC.
pub const RAM_MAPPED_END: u64 = 0xF000_0000;

/// Het MDIO-adres van de PHY volgens de DTS (`rgmii_phy1: reg = <0x1>`).
pub const PHY_ADDR_DTS: u8 = 1;
/// Hoe lang de autonegotiatie mag duren (Go: 8 s).
const AUTONEG_NS: u64 = 8_000_000_000;
/// Zonder DTB: vier A55's.
const CORES_DEFAULT: usize = 4;
/// De grootste initrd (config plus image) die de kern naar de heap haalt:
/// Hop is 1,5 MB gestript (30-09), het laadvenster van de Pi's 14 MB; 16 MB
/// uit de heap van ~60 MB laat de kern ruim genoeg over. Het script toetst
/// dezelfde grens (image/radxa-zero3.sh, `INITRD_MAX`).
pub const INITRD_MAX: u64 = 16 << 20;

const _: () = {
    assert!(dwmac4::NEED_BYTES <= NET_DMA.size);
    assert!(NET_BUF.end().0 <= NET_DMA.end().0);
    assert!(NET_DMA.base.0 + dwmac4::NEED_BYTES <= NET_BUF.end().0);
    assert!(NET_DMA.size == abi::layout::NET_DMA_SIZE);
    assert!(USB_DMA.size == abi::layout::USB_DMA_SIZE);
    assert!(NET_DMA.base.0 == DMA.base.0 && USB_DMA.base.0 == NET_DMA.base.0 + NET_DMA.size);
    assert!(FB_RAM.base.0 == USB_DMA.base.0 + 2 * USB_DMA.size);
    assert!(FB_RAM.base.0 + FB_RAM.size == STAGE_WINDOW.base.0);
    assert!(STAGE_WINDOW.base.0 + STAGE_WINDOW.size == POOL_BASE);
    assert!(1920 * 1080 * 4 <= FB_RAM.size);
    // Een arm64 Image landt 2 MB-gealigneerd, onder de DMA-regio.
    assert!(KERN_RAM.end().0 <= DMA.base.0 && KERN_RAM.base.0.is_multiple_of(0x20_0000));
};

/// De UART. Eén per board.
// SAFETY: 0xFE66_0000 is UART2 van de RK3566, gemapt als Device (`mmu`),
// met zijn klok open (U-Boot print erover).
static UART: Ns16550 = unsafe { Ns16550::new(UART2, 2) };

/// De GIC, de controller van `cpu::irq`. Het redistributor-frame is dat van
/// de OS-core; `start_interrupts` zoekt het op met `find_redistributor` en
/// zet het.
// SAFETY: GICD en GICR zijn de GIC-600-blokken van de RK3566 (Device).
static GIC: Gic<SysRegIcc> = unsafe { Gic::new(GICD, GICR, SysRegIcc) };

/// De bel van de NIC: de dispatch luidt hem, de RX-pomp wacht erop.
static NIC_BELL: Signal = Signal::new();

/// De ack van de NIC-lijn, gezet door `probe_nic`, gelezen door de
/// dispatch-taak. Beide draaien op de executor van core 0.
static NIC_ACK: AckSlot<IrqAck> = AckSlot::new();

/// De device-ack van de NIC-lijn bij de dispatcher: masker dicht en status
/// gewist (de level-lijn valt). De driver zet het masker weer open als de
/// pomp de ring leeg las.
fn nic_ack() {
    NIC_ACK.ack();
}

/// De DTB van deze boot: een kopie in de heap.
static DTB: board::dtb::Dtb = board::dtb::Dtb::new();
/// De config (`hopos.cfg`): een stuk van de initrd in de heap.
static CFG: Kept = Kept::new();
/// Het image van de bewoner: een stuk van de initrd in de heap.
pub(crate) static STAGE: Kept = Kept::new();
/// Waar U-Boot de DTB en de initrd liet: gaten in de pool.
static FW_HOLES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Het bij boot gevonden DRAM (bytes, 0 = onbekend).
static MEM_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Het aantal cores uit de FDT, 0 = onbekend.
static CORES: AtomicUsize = AtomicUsize::new(0);
/// Leeft er een NIC uit `probe_nic`? Pas gezet na een gelukte probe: een
/// mislukte (geen link) liet niets achter en mag opnieuw (hopos `nic_retry`).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);

fn console_write(b: &[u8]) {
    UART.write_bytes(b);
}

/// Kopieert `[pa, pa+len)` woordgewijs (via `dev`) naar een nieuwe
/// heap-allocatie die blijft. `None` als de heap nee zegt.
fn copy_to_heap(pa: u64, len: usize) -> Option<&'static [u8]> {
    let layout = core::alloc::Layout::from_size_align(len.max(1), 8).ok()?;
    // SAFETY: een geldige layout met maat > 0; null is afgehandeld.
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is een verse, niet-gedeelde allocatie van `len` bytes die
    // nooit vrijkomt.
    let dst = unsafe { core::slice::from_raw_parts_mut(p, len) };
    dev::copy_out(dst, Pa(pa));
    Some(dst)
}

/// De initrd uit het DRAM naar de heap, gesplitst in config en image
/// ([`initrd`]). Een initrd die niet te
/// splitsen is, telt helemaal niet: liever luid zonder config en zonder
/// bewoner dan een half gelezen config met een image van onbekende maat.
fn take_initrd(start: u64, len: u64) {
    if len > INITRD_MAX || !in_dram(start, len) {
        cpu::println!(
            "stage: initrd {start:#x}+{len:#x} outside the DRAM or over {INITRD_MAX} bytes, ignored HOPOS_STAGE_REFUSED"
        );
    } else if let Some(blob) = copy_to_heap(start, len as usize) {
        match initrd::split(blob) {
            Ok(parts) => {
                CFG.keep(parts.cfg);
                if let Some(img) = parts.image {
                    STAGE.keep(img);
                }
            }
            Err(e) => cpu::println!(
                "stage: initrd of {len} bytes at {start:#x} refused: {e} HOPOS_STAGE_REFUSED"
            ),
        }
    } else {
        cpu::println!("stage: no heap for an initrd of {len} bytes HOPOS_STAGE_REFUSED");
    }
}

/// De rol van de staging uit `hopos.stage` (config of APPEND), als woord
/// zoals op de Pi's. Meldt wat er gestaged is.
fn take_role() {
    let role = board::stage::role_code(Rk3566.boot_param("hopos.stage"));
    slots::ROLE.store(role, Relaxed);
    let name = match role {
        0 => "app",
        1 => "hop",
        _ => "unknown (nothing will be placed)",
    };
    match STAGE.get() {
        Some(img) => cpu::println!(
            "stage: {} KB image from the initrd at {:#x}, role {name}",
            img.len() >> 10,
            img.as_ptr() as usize
        ),
        None => cpu::println!("stage: no image in the initrd, role {name}"),
    }
}

/// Ligt `[pa, pa+len)` in het DRAM dat de identity map dekt?
fn in_dram(pa: u64, len: u64) -> bool {
    pa >= DRAM_BASE && pa.checked_add(len).is_some_and(|e| e <= RAM_MAPPED_END)
}

/// De ABI-staart van een slot Normal write-back in de kernmap (Go:
/// `mapTailNormal` in `kern/slots`, slot-ABI 7), zoals op Apple: de kern
/// mapt de pool hier Device (de tamago-keuze, zie `mmu`), en zonder dit
/// loopt elke ringkopie per 8 bytes vluchtig met onderhoud (01-10, stempel
/// AB: app naar app 29,83 MB/s tegen 461 op de Pi 4). Een 1 GB-blok van de
/// boot-map wordt daarvoor gesplitst. Weigert de map, dan blijft de staart
/// Device: traag maar correct, en de kooi houdt dan haar ringonderhoud.
pub fn map_tail_normal(pa: u64, size: u64) -> Result<(), cpu::memattr::Error> {
    cpu::memattr::normal_wb(pa, size)
}

/// De pool zoals het plan hem nu ziet: DTB-banken min de gaten.
pub(crate) fn pool_now() -> Pool {
    let mut banks = [abi::Region::new(0, 0); fw::fdt::MAX_MEM_REGIONS];
    let mut nb = 0;
    if let Some(regs) = DTB.fdt().and_then(|f| f.mem_regions().ok()) {
        for (slot, r) in banks.iter_mut().zip(regs.iter()) {
            *slot = *r;
            nb += 1;
        }
    }
    let mut holes = [abi::Region::new(0, 0); 2 + fw::fdt::MAX_RESERVE];
    let h = |i: usize| FW_HOLES.get(i).map_or(0, |a| a.load(Relaxed));
    holes[0] = abi::Region::new(h(0), h(1));
    holes[1] = abi::Region::new(h(2), h(3));
    let mut nh = 2;
    if let Some(f) = DTB.fdt() {
        for (slot, r) in holes.iter_mut().skip(2).zip(f.mem_reserve().iter()) {
            *slot = *r;
            nh += 1;
        }
    }
    let (pool, from_dtb) = slots::pool(
        banks.get(..nb).unwrap_or(&[]),
        holes.get(..nh).unwrap_or(&[]),
    );
    if !from_dtb {
        cpu::println!(
            "WARNING HOPOS_POOL_FALLBACK: no usable DTB /memory - partition pool falls back to a fixed 512 MB at {POOL_BASE:#x}"
        );
    }
    pool
}

/// De DTB uit x0 en de initrd die hij aanwijst naar de heap, en de regels
/// die zeggen wat erin stond (`discover`).
fn take_fdt(dtb: u64) {
    let mut head = [0u8; 8];
    let total = if in_dram(dtb, 8) && dtb.is_multiple_of(8) {
        dev::copy_out(&mut head, Pa(dtb));
        fw::fdt::total_size(&head)
    } else {
        None
    };
    let Some(f) = total
        .filter(|&n| in_dram(dtb, n as u64))
        .and_then(|n| copy_to_heap(dtb, n))
        .and_then(|b| DTB.keep(b))
    else {
        cpu::println!(
            "WARNING HOPOS_RAM_CHECK_SKIPPED: no valid DTB (x0={dtb:#x}) - trusting the static layout"
        );
        return;
    };
    FW_HOLES[0].store(dtb, Relaxed);
    FW_HOLES[1].store(f.size() as u64, Relaxed);
    MEM_TOTAL.store(f.mem_total().unwrap_or(0), Relaxed);
    CORES.store(f.cpu_count().unwrap_or(0), Relaxed);
    if let Some((s, e)) = f.initrd() {
        FW_HOLES[2].store(s, Relaxed);
        FW_HOLES[3].store(e - s, Relaxed);
        take_initrd(s, e - s);
    }
    let gic_ok = f
        .gic_v3()
        .is_some_and(|g| g.dist.base == GICD.0 && g.redist.base == GICR.0);
    cpu::println!(
        "fdt: {} bytes at {dtb:#x}, bootargs {:?}, hopos.cfg {} bytes{}",
        f.size(),
        f.bootargs().unwrap_or(""),
        Rk3566.config().len(),
        if gic_ok {
            ""
        } else {
            ", GIC differs from the board plan"
        },
    );
    match f.framebuffer() {
        Some(fb) => cpu::println!(
            "fb: U-Boot left a {}x{} framebuffer at {:#x}; no framebuffer console in v3 yet",
            fb.width,
            fb.height,
            fb.base
        ),
        None => cpu::println!("fb: none from U-Boot (as measured 05-08), UART only"),
    }
    take_role();
}

/// De Radxa Zero 3E als board.
pub struct Rk3566;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Rk3566;

impl Rk3566 {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Het MAC-adres uit `hopos.mac` of `hopos.node` (Go: `nodemac`). Dit
    /// board heeft geen MAC in een fuse waarvan we de registerkaart gemeten
    /// hebben, dus zou anders elke Radxa hetzelfde adres dragen.
    fn node_mac() -> Mac {
        let (m, src) = net::nodemac::identity(
            Rk3566.boot_param("hopos.mac"),
            Rk3566.boot_param("hopos.node"),
        );
        if src == net::nodemac::Source::Fallback {
            cpu::println!(
                "net: WARNING no hopos.mac and no hopos.node: the built-in MAC; a second Radxa Zero 3E on this LAN will collide HOPOS_MAC_FIXED"
            );
        }
        Mac(m)
    }

    /// De PHY: scan, het DTS-adres als dat hetzelfde antwoordt, de
    /// RGMII-delays, en de autonegotiatie. Geeft adres en link.
    fn bring_up_phy(p: &mut Probe) -> Result<(u8, driver_mdio::Link), Error> {
        let Some(found) = driver_mdio::scan(p) else {
            let (sel, gate, rst) = soc::gmac_clocks();
            cpu::println!(
                "net: no PHY on the MDIO bus (expected address {PHY_ADDR_DTS}; mdc/mdio mux {}/{}, mdio-addr {:#010x}, clksel33 {sel:#010x} clkgate17 {gate:#010x} softrst14 {rst:#010x}) HOPOS_NIC_FAIL",
                soc::mux_of(4, 14),
                soc::mux_of(4, 15),
                p.mdio_state()
            );
            return Err(Error::Nic("no PHY on the MDIO bus"));
        };
        // De scan geeft de EERSTE hit, en dat is op deze PHY adres 0: het
        // broadcast-adres. Voor lezen maakt dat niet uit, voor schrijven wel.
        let mut phy: Phy = found;
        if found.addr != PHY_ADDR_DTS && driver_mdio::answers_as(p, PHY_ADDR_DTS, &found) {
            cpu::println!(
                "net: PHY answers at both {} and {PHY_ADDR_DTS} (id {:04x}:{:04x}), using {PHY_ADDR_DTS}, the device-tree address",
                found.addr,
                found.id1,
                found.id2
            );
            phy.addr = PHY_ADDR_DTS;
        }
        // De RGMII-klokvertragingen ZITTEN IN DE PHY (rgmii-id). GEMETEN
        // 06-08: zonder deze stap werkten 1124 ontvangen frames foutloos en
        // kwam geen enkel verzonden frame aan.
        if rtl8211f::is_rtl8211f(phy.id1, phy.id2) {
            let was = rtl8211f::delays(p, phy.addr);
            let set = rtl8211f::configure(p, phy.addr, true, true);
            let now = rtl8211f::delays(p, phy.addr);
            cpu::println!(
                "net: RTL8211F at {} rgmii delays (tx, rx) {was:?} -> {now:?} ({set:?}; rgmii-id needs both on)",
                phy.addr
            );
        } else {
            cpu::println!(
                "net: PHY {:04x}:{:04x} is not an RTL8211F, no rgmii delays applied; if TX frames vanish while RX works, look here first",
                phy.id1,
                phy.id2
            );
        }
        match driver_mdio::autoneg(p, phy.addr, true, cpu::idle::now, AUTONEG_NS) {
            Ok(link) => Ok((phy.addr, link)),
            Err(e) => {
                cpu::println!(
                    "net: PHY {} (id {:04x}:{:04x}): {e}, cable plugged in? HOPOS_NIC_FAIL",
                    phy.addr,
                    phy.id1,
                    phy.id2
                );
                Err(Error::Nic("no link"))
            }
        }
    }
}

impl Default for Rk3566 {
    fn default() -> Self {
        Self::new()
    }
}

/// De DesignWare-WDT van de SoC (`watchdog`): TOP 15, ongeveer 89,5 s,
/// zoals Go (de DW-WDT kent alleen machten van twee). Een kick op een
/// gewapende DW-WDT herlaadt hem, dus de aai rond een flip is gewoon een
/// aai.
impl board::Watchdog for Rk3566 {
    type Armed = watchdog::Desc;

    const WD_TIMEOUT_MS: u64 = watchdog::TIMEOUT_MS;

    fn arm(&self, timeout_ms: u64) -> Result<watchdog::Desc, &'static str> {
        watchdog::arm(timeout_ms)
    }

    fn pet(&self) {
        watchdog::pet();
    }

    fn disarm(&self) -> bool {
        watchdog::off()
    }
}

/// De TSADC van de SoC (`tsadc`): het warmste van de twee kanalen (CPU en
/// GPU). De sensor gaf in Go nooit een conversie (06-08); `open` zegt in
/// één regel wat hij nu teruggeeft.
impl board::Thermal for Rk3566 {
    /// Brengt de sensor op (busy-waits van ~5 ms, één keer bij de boot) en
    /// meldt de eerste lezing.
    fn open_thermal(&self) {
        tsadc::open();
    }

    /// 0 = geen geldige code.
    fn temp_milli_c(&self) -> i32 {
        tsadc::temp_milli_c().unwrap_or(0)
    }
}

/// SCMI_CLK_CPU van de TF-A met vdd_cpu over i2c0 (`clock`). Zonder meting
/// geen rem: de knop gaat tot 1800 MHz.
impl board::ClockKnob for Rk3566 {
    type Knob = clock::RkKnob<clock::Hw>;
    type KnobError = clock::Error;

    const HAS_KNOB: bool = true;

    /// Een reden als de spanning niet terug te lezen is of de klok niet
    /// antwoordt: dan blijft alles waar U-Boot het liet.
    fn clock_knob(&self, mhz: Option<u32>) -> Option<Result<Self::Knob, clock::Error>> {
        Some(clock::knob(mhz))
    }
}

impl Board for Rk3566 {
    type Nic = Dwmac4;
    type Sleeper = cpu::idle::ArmSleeper;
    /// Geen: de SD- en eMMC-controller heeft nog geen driver.
    type Disk = NoDisk;

    const NAME: &'static str = "rk3566";
    const DTB_IN_X0: bool = true;
    /// De kern mapt alles boven 0x0880_0000 Device, de pool dus ook
    /// (`mmu`, de tamago-keuze); de ringen van een slot krijgen `Hardware`
    /// zodra de staart Normal gemapt is ([`Board::map_tail_normal`]).
    const SLOT_RINGS: Coherence = Coherence::Maintained;

    fn console(&self) -> fn(&[u8]) {
        console_write
    }

    fn firmware(&self) -> &'static str {
        "boot: Radxa Zero 3E, U-Boot booti at EL2, TF-A bl31 at EL3 (PSCI via SMC)"
    }

    /// De DTB uit x0, en de initrd die hij aanwijst, naar de heap. Wat de
    /// kern straks vraagt (DRAM, cores, de pool) staat daarna vast. Dan de
    /// DRBG van de kern uit het TRNG (`rng`), met of zonder DTB: het blok
    /// staat er toch niet in.
    fn discover(&self, dtb: u64) {
        take_fdt(dtb);
        rng::seed();
    }

    fn clock(&self) -> executor::Clock {
        cpu::idle::now
    }

    #[cfg(feature = "usb")]
    fn usb_hosts(&self) -> board::UsbHosts {
        usb::hosts()
    }

    /// De buffer uit het plan ([`FB_RAM`]): de eerste aanroep start de
    /// scanout naar HDMI.
    #[cfg(feature = "gui")]
    fn framebuffer(&self) -> Option<driver_fb::Desc> {
        display::framebuffer(cpu::idle::now)
    }

    /// WFI met de fysieke timer op de deadline: hetzelfde pad als QEMU virt
    /// (PPI 30 scherp in de GIC wekt de WFI; GEMETEN 29-09 op QEMU). Op dit
    /// silicium in Rust NOG NIET GEMETEN: de eerste HOPOS_TICK-regels zeggen
    /// het (loopt de tik door, dan wekt de timer; blijft hij staan, dan
    /// `Mode::Wfe`, de default die op elk bekend silicium wekt).
    fn sleeper(&self) -> Self::Sleeper {
        cpu::idle::ArmSleeper::new(cpu::idle::Mode::Wfi)
    }

    fn mem_total(&self) -> u64 {
        MEM_TOTAL.load(Relaxed)
    }

    fn cores(&self) -> usize {
        match CORES.load(Relaxed) {
            0 => CORES_DEFAULT,
            n => n,
        }
    }

    fn core_class(&self, _core: usize) -> CoreClass {
        // Vier identieke A55's in één DynamIQ-cluster: homogeen, dus de
        // beste klasse die dít board heeft.
        CoreClass::Big
    }

    fn plan(&self) -> Plan {
        Plan {
            kern_ram: KERN_RAM,
            dma: DMA,
            net_dma: NET_DMA,
        }
    }

    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        let mpidr = cpu::mpidr();
        // SAFETY: de reeks is de GICR van de RK3566, gemapt als Device.
        let rd = unsafe { driver_gicv3::find_redistributor(GICR, GICR_LEN, mpidr) }
            .ok_or(Error::Irq("no redistributor frame for the OS core"))?;
        // SAFETY: `rd` is een frame uit die reeks, voor deze core.
        unsafe { GIC.set_redistributor(rd) };
        GIC.init()
            .map_err(|_| Error::Irq("redistributor stays asleep"))?;
        cpu::irq::use_controller(&GIC);
        cpu::irq::enable(Line(TIMER_PPI), Some(cpu::idle::timer_off), None)
            .map_err(|_| Error::Irq("timer PPI refused"))?;
        // De OS-core: de EL2-timer en de kick van de app-cores (die alleen
        // telt).
        cpu::irq::enable(Line(HYP_TIMER_PPI), Some(cpu::idle::hyp_timer_off), None)
            .map_err(|_| Error::Irq("hyp timer PPI refused"))?;
        cpu::irq::enable(Line(KICK_SGI), Some(cpu::el2::count_kick), None)
            .map_err(|_| Error::Irq("kick SGI refused"))?;
        cpu::println!("irq: {}", GIC.describe());
        cpu::irq::unmask();
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        let pass = cpu::irq::global().dispatch();
        cpu::irq::unmask();
        // De kick telt nergens (hij staat in `os(kicks=)`).
        let timer = pass
            .claims(Line(TIMER_PPI))
            .saturating_add(pass.claims(Line(HYP_TIMER_PPI)));
        let nic = pass.claims(Line(GMAC1_INTID));
        let kick = pass.claims(Line(KICK_SGI));
        Dispatched {
            timer,
            nic,
            other: pass
                .claimed
                .saturating_sub(timer.saturating_add(nic).saturating_add(kick)),
        }
    }

    /// De ethernet-keten, en die is op dit board langer dan op alle andere:
    /// klokken, pinmux, GRF, PHY-reset, AXI-reset, DMA-reset, de PHY, de
    /// klokdeler, dan pas de ringen. Elke stap die kan mislukken meldt
    /// zichzelf mét het gemeten getal: een boot-cyclus kost hier een
    /// kaartwissel.
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        // 1-3. Klokken open, pinnen naar M1, RGMII met nul-delays.
        soc::gmac_clock_on();
        soc::gmac_pinmux();
        soc::gmac_set_rgmii();
        // SAFETY: GMAC1 is het DWMAC4-blok van de RK3566 (Device-gemapt), en
        // `gmac_clock_on` opende zijn pclk.
        let mut p = unsafe { Probe::new(GMAC1, CSR_100_150M, cpu::idle::now) };
        // 4. Leeft de MAC?
        let version = p.check().map_err(|e| {
            let (sel, gate, rst) = soc::gmac_clocks();
            cpu::println!(
                "net: {e} at {:#x} (clksel33 {sel:#010x} clkgate17 {gate:#010x} softrst14 {rst:#010x}) HOPOS_NIC_FAIL",
                GMAC1.0
            );
            Error::Nic("no DWMAC4 at GMAC1")
        })?;
        // 5. PHY uit reset (hij levert de RGMII-referentieklok), de
        //    AXI-reset pulsen, dan pas de DMA-softreset. Volgorde GEMETEN
        //    05-08.
        soc::gmac_phy_reset();
        soc::gmac_axi_reset();
        if let Err(e) = p.reset() {
            let (sel, gate, rst) = soc::gmac_clocks();
            cpu::println!(
                "net: {e} (clksel33 {sel:#010x} clkgate17 {gate:#010x} softrst14 {rst:#010x}) HOPOS_NIC_FAIL"
            );
            return Err(Error::Nic("DMA soft reset stuck"));
        }
        // 6. De PHY en de autonegotiatie.
        let (phy, link) = Self::bring_up_phy(&mut p)?;
        // 7. De klokdeler op de onderhandelde snelheid.
        soc::gmac_set_speed(link.mbps);
        // 8. De ringen in de NIC-DMA-regio.
        let mac = Self::node_mac();
        // SAFETY: NET_DMA is van deze driver alleen: onder 4 GB, buiten de
        // kern-RAM, Normal-NC gemapt met NET_BUF als Normal-WB-blok erin
        // (`mmu`), door niets anders uitgedeeld.
        let mut nic =
            unsafe { p.start(NET_DMA.base, NET_DMA.size, mac, link.mbps, link.full_duplex) }
                .map_err(|e| {
                    cpu::println!("net: {e} HOPOS_NIC_FAIL");
                    Error::Nic("dwmac4 start failed")
                })?;
        // 9. De lijn. Een lijn die niet aan wil, laat de NIC pollen.
        NIC_ACK.set(nic.irq_ack());
        if cpu::irq::enable(Line(GMAC1_INTID), Some(nic_ack), Some(&NIC_BELL)).is_ok() {
            nic.set_irq(&NIC_BELL);
        }
        cpu::println!(
            "net: dwmac4 at {:#x} version {version:#x}, PHY {phy}, link {link}, intid {GMAC1_INTID}, rings {}+{} in {:#x}",
            GMAC1.0,
            dwmac4::NUM_RX,
            dwmac4::NUM_TX,
            NET_DMA.base.0
        );
        cpu::println!("net: dwmac4 {}", nic.diag());
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// Geen schijf: de SD- en eMMC-controller (dw_mmc/sdhci) heeft nog geen
    /// driver in v3, dus altijd `Ok(None)`: de bestandscalls weigeren dan
    /// luid.
    fn probe_disk(&self) -> Result<Option<NoDisk>, Error> {
        Ok(None)
    }

    /// De fysieke index van de core waar dit draait.
    fn this_core(&self) -> usize {
        slots::core_of(cpu::mpidr())
    }

    /// De OS-core die de bootargs vragen (`hopos.oscore=N`), met een reden
    /// als de vraag niet kon: dan de boot-core (0). Homogene A55's, dus
    /// `big` is core 0 en `small` of `mid` valt terug.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        board::os_core(
            self.boot_param("hopos.oscore"),
            self.cores(),
            |c| self.core_class(c),
            0,
        )
    }

    /// De kick van de OS-core voor de rotatie van `cpu::el2`.
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell {
            sgi1r: driver_gicv3::sgi1r(cpu::mpidr(), KICK_SGI),
            sgir: 0,
            intid: KICK_SGI,
            pending: cpu::gicv3::hppir1,
        }
    }

    /// Stuurt de kick naar deze core zelf: de zelftest van het IPI-pad.
    fn kick_self(&self) {
        cpu::gicv3::sgi1r(driver_gicv3::sgi1r(cpu::mpidr(), KICK_SGI));
    }

    /// Het venster in het image, anders `hopos.cfg` uit de initrd.
    fn config(&self) -> &'static str {
        board::cfgwin::or(
            CFG.get()
                .and_then(|b| core::str::from_utf8(b).ok())
                .unwrap_or(""),
        )
    }

    /// De APPEND-regel van extlinux.conf.
    fn bootargs(&self) -> &'static str {
        DTB.fdt().and_then(|f| f.bootargs()).unwrap_or("")
    }

    fn map_tail_normal(&self, pa: u64, size: u64) -> Option<Result<(), board::TailError>> {
        Some(map_tail_normal(pa, size).map_err(board::TailError::Attr))
    }
}
