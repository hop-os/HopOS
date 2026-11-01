//! De USB-invoer van de Pi 4: de VL805 achter de BCM2711-root-complex (Go:
//! `cmd/hopos/usb_rpi4.go`).
//!
//! Dit is de duurste van de Pi's, want op de Pi 4 IS alle USB die
//! PCIe-kaart: hier moet een root-complex op die voor niets anders bestaat
//! (op de Pi 5 staat de link al voor de GEM). Waarom dan toch: het bord
//! wordt nog verkocht en ligt bij iedere tinkerer in de la (Derek, 06-08).
//!
//! De stappen, elk met zijn regel:
//!
//! 1. het outbound-venster (CPU 0x6_0000_0000, 64 MB naar PCIe
//!    0xF800_0000, uit de Pi 4-DT) als Device in de niveau-1-tabel: de
//!    vaste tabel mapt alleen gigabyte 0 en 3;
//! 2. de RC op (`driver-brcmpcie`, BCM2711: geen RESCAL, gen 2, SCB0_SIZE
//!    naar het inbound-venster zoals Linux), de link, de VL805 (1106:3483)
//!    op bus 1 en zijn BAR 0 op 0xF800_0000, met de endpoint nog DICHT
//!    (geen memory-decode, geen bus-mastering), de stand van Linux op dit
//!    moment;
//! 3. de firmware-handshake (`board_raspi::usb::vl805_handshake`): een
//!    VL805 zonder eigen SPI-EEPROM (de latere Pi 4-revisies, de CM4)
//!    heeft na PERST# geen firmware tot de VideoCore hem laadt
//!    (`NOTIFY_XHCI_RESET`). Staat er al een versie in config 0x50, dan
//!    vragen we niets (zoals Linux); anders de notify, 1 ms stil, en dan
//!    om de 10 ms de versie tot hij er is, hoogstens een seconde. Koud
//!    (30-09) stond er na die ene lezing van 1 ms nog 0, gaf het board
//!    de host toch door, en kreeg de xHCI-driver een VL805 zonder
//!    firmware: een HCRST die nooit klaarde. Komt er geen versie, dan de
//!    keten nog één keer vanaf PERST# (wat de warme flips deden, en daar
//!    werkte het), en anders geen controller: de endpoint blijft dicht en
//!    niemand raakt het xHCI-venster aan;
//! 4. pas met een versie de endpoint open en de host naar de USB-taak.
//!
//! De SError van die koude boot (`vec 11` in de zelftest van de OS-core)
//! kwam niet van de xHCI-driver: die draait pas in de executor, ná de
//! zelftest. Hij stond dus al klaar uit deze keten (de kern draait op EL2
//! met PSTATE.A dicht, en de eerste ERET naar EL1 neemt hem). Welke stap
//! het was, is nergens gemeten; daarom kijkt het board na elke stap of er
//! een klaarstaat (ISR_EL1.A) en zegt bij de eerste welke stap hem gaf
//! (`HOPOS_USB_SERROR`).
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak): kaal linkt
//! de binary geen PCIe.

use crate::Bcm2711;
use board::{Region, UsbHost, UsbHosts, UsbKind};
use board_raspi::usb::{self, UsbCtx, Vl805};
use cpu::println;
use dev::Pa;
use driver_brcmpcie::{EpBar, InWin, OutWin, Rc};

/// De PCIe-root-complex (`brcm,bcm2711-pcie`, DT `pcie@7d500000` in de
/// 0x7e000000-view, in low-peripheral-mode op 0xFD50_0000).
const PCIE: Pa = Pa(0xFD50_0000);
/// Het outbound-venster: CPU 0x6_0000_0000 ziet PCIe 0xF800_0000.
const OUT_CPU: u64 = 0x6_0000_0000;
const OUT_PCIE: u64 = 0xF800_0000;
const OUT_SIZE: u64 = 0x0400_0000;
/// De DT-compatible van de RC.
const PCIE_COMPAT: &str = "brcm,bcm2711-pcie";
/// De VL805 zoals de config-read hem geeft: device << 16 | vendor.
const VL805_ID: u32 = 0x3483_1106;
/// Waar de VL805 zijn firmwareversie laat zien (Linux
/// `VL805_PCI_CONFIG_VERSION_OFFSET`): nul = niet geladen.
const VL805_VERSION: u64 = 0x50;
/// Het xHCI-venster van de VL805 (BAR 0): 4 KB.
const VL805_SIZE: u64 = 0x1000;
/// Hoe vaak de hele keten (PERST#, link, BAR's, notify) mag. De tweede
/// keer is wat de warme flips van 30-09 deden, en daar laadde de
/// VideoCore hem wel.
const ATTEMPTS: u32 = 2;

/// De VL805 op, of één regel waarom niet.
pub(crate) fn hosts(ctx: &UsbCtx) -> UsbHosts {
    let mut out = UsbHosts::new();
    // Alleen een RC die de DTB aanzet, bestaat. QEMU `raspi4b`
    // modelleert hem niet en haalt de node weg (`raspi4_modify_dtb`);
    // de eerste schrijf op RGR1 was daar een synchrone abort (gemeten
    // 30-09: ESR 0x96000010, FAR 0xfd509210). De echte DTB van de
    // Pi 4 heeft de node zonder `status`, dus aan (getoetst tegen
    // bcm2711-rpi-4-b.dtb), net als de GENET en de RNG200.
    if board_raspi::device_enabled(PCIE_COMPAT) != Some(true) {
        println!("usb: vl805: no enabled {PCIE_COMPAT} in the DTB, PCIe not touched");
        return out;
    }
    if !usb::map_device_gb::<Bcm2711>(OUT_CPU >> 30) {
        println!(
            "usb: vl805: gigabyte {} for the PCIe window cannot be mapped HOPOS_USB_NONE",
            OUT_CPU >> 30
        );
        return out;
    }
    let Some(rc) = up_with_firmware(ctx.now) else {
        // De endpoint blijft dicht (geen memory-decode): niemand leest
        // het xHCI-venster van een VL805 zonder firmware (de HCRST die
        // koud nooit klaarde).
        return out;
    };
    rc.open_endpoint();
    println!(
        "usb: vl805 on PCIe (status {:#x}), xHCI window at {OUT_CPU:#x} (bus {OUT_PCIE:#x})",
        rc.status()
    );
    let _ = out.push(UsbHost {
        name: "vl805",
        kind: UsbKind::Xhci,
        regs: Region {
            base: Pa(OUT_CPU),
            size: VL805_SIZE,
        },
        // De INTx van de RC is nog niet bedraad (zoals de GENET); de
        // driver pollt.
        irq: None,
        // De dma-ranges van dit bord leggen PCIe 0 op DRAM 0: wat de
        // VL805 een adres noemt, is het fysieke adres (het tweede
        // inbound-venster hieronder).
        bus_off: 0,
        dma: ctx.dma,
    });
    out
}

/// De keten zoals Linux hem loopt (`brcm_pcie_setup`, `pci_host_probe`,
/// dan `xhci_pci_common_probe` met de reset van de firmware): de RC op
/// en de link, de BAR's toegewezen met de endpoint nog dicht, dan de
/// handshake, en pas met een draaiende firmware de endpoint open (de
/// aanroeper). Lukt de handshake niet, dan de hele keten nog één keer
/// vanaf PERST#. Geeft de RC met een VL805 die een versie meldt.
fn up_with_firmware(now: fn() -> u64) -> Option<Rc> {
    let rc = rc(now);
    let bars = [
        // BAR 0 laag: de xHCI-registers; hoog: 64-bit BAR, bovenhelft nul.
        EpBar {
            off: 0x10,
            val: OUT_PCIE as u32,
        },
        EpBar { off: 0x14, val: 0 },
    ];
    // Een SError van vóór deze keten is niet van de VL805.
    let mut seen = false;
    serror_after(&mut seen, "the boot, before any PCIe access");
    for attempt in 1..=ATTEMPTS {
        // SAFETY: geen RESCAL op de BCM2711 (0), de RC staat op PCIE.
        if let Err(e) = unsafe { rc.bring_up_closed(0, VL805_ID, &bars) } {
            println!("usb: vl805: bcm2711 pcie: {e} (attempt {attempt}) HOPOS_USB_NONE");
            return None;
        }
        serror_after(&mut seen, "the PCIe bring-up");
        if firmware(&rc, now, attempt, &mut seen) {
            return Some(rc);
        }
    }
    println!(
        "usb: vl805: no firmware after {ATTEMPTS} attempts, the xHCI registers stay untouched HOPOS_USB_VL805"
    );
    None
}

/// De handshake van één poging, met één regel over de uitkomst. Geeft
/// of de firmware draait.
fn firmware(rc: &Rc, now: fn() -> u64, attempt: u32, seen: &mut bool) -> bool {
    let mut version = || rc.cfg_read32(1, 0, 0, VL805_VERSION);
    let mut notify = || usb::notify_xhci_reset(usb::vl805_dev_addr(1, 0, 0));
    let mut after = |step| serror_after(seen, step);
    match usb::vl805_handshake(&mut version, &mut notify, &mut after, now, usb::VL805_WAIT) {
        Vl805::Running { version } => {
            println!("usb: vl805 firmware {version:#x} already loaded (attempt {attempt})");
            true
        }
        Vl805::Loaded {
            version,
            waited_ns,
            reply,
        } => {
            println!(
                "usb: vl805 firmware {version:#x} loaded by the VideoCore after {} us (attempt {attempt}, reply {reply:#x})",
                waited_ns / 1000
            );
            true
        }
        Vl805::Refused(e) => {
            println!("usb: vl805 firmware handshake: {e} (attempt {attempt}) HOPOS_USB_VL805");
            false
        }
        Vl805::Silent {
            last,
            waited_ns,
            reply,
        } => {
            println!(
                "usb: vl805 firmware still {last:#x} {} us after NOTIFY_XHCI_RESET (attempt {attempt}, reply {reply:#x}, MISC_CTRL {:#x}, status {:#x}) HOPOS_USB_VL805",
                waited_ns / 1000,
                rc.misc_ctrl(),
                rc.status()
            );
            false
        }
    }
}

/// Kijkt na een stap van de keten of er een SError klaarstaat
/// ([`usb::serror_pending`]) en zegt bij de eerste welke stap het was
/// (`seen` onthoudt dat hij er was). Hij blijft staan tot de eerste
/// stap naar EL1: de regel is de diagnose, niet de genezing.
fn serror_after(seen: &mut bool, step: &str) {
    if !*seen && usb::serror_pending() {
        *seen = true;
        println!(
            "usb: vl805: SError pending after {step}; the first EL1 entry will take it HOPOS_USB_SERROR"
        );
    }
}

/// De RC met het adresplan van de Pi 4. Inbound: op de BCM2711 is RC_BAR2
/// hét DRAM-venster en hoort BAR1 uit te staan (`pcie-brcmstb.c`); de
/// lege eerste regel schrijft size-encoding 0 in BAR1. 4 GB en niet het
/// hele DRAM: de enige DMA hier is die van de xHCI naar
/// `board_raspi::usb::USB_DMA`, en die ligt laag. Dat venster (en
/// SCB0_SIZE ernaar, `driver-brcmpcie`) is ook de weg waarlangs de VL805
/// zijn firmware haalt als de VideoCore hem laadt.
fn rc(now: fn() -> u64) -> Rc {
    let inb = [
        Some(InWin {
            pcie: 0,
            cpu: 0,
            size: 0,
        }),
        Some(InWin {
            pcie: 0,
            cpu: 0,
            size: 0x1_0000_0000,
        }),
        None,
        None,
    ];
    let out = OutWin {
        cpu: OUT_CPU,
        pcie: OUT_PCIE,
        size: OUT_SIZE,
    };
    // SAFETY: PCIE is het RC-blok van de BCM2711 in het Device-venster
    // van gigabyte 3 (de vaste tabel); de SW_INIT-bank bestaat op deze
    // SoC niet (de reset zit in RGR1) en wordt niet aangeraakt.
    unsafe {
        Rc::new(
            driver_brcmpcie::Soc::Bcm2711,
            PCIE,
            Pa(0),
            0,
            2, // de VL805 is een gen2 x1-endpoint
            out,
            inb,
            now,
        )
    }
}
