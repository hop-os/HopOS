//! De USB-invoer van QEMU virt: de `qemu-xhci` op de PCIe-host van virt
//! (`image/qemu-run.sh` en `tools/qemu-test.sh` geven hem met `GUI=display`
//! mee, met `usb-kbd` en `usb-mouse` erachter).
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak); kaal linkt
//! het board geen regel xHCI. De PCIe-host zelf (pcie.rs) is er altijd,
//! voor de watchdog (wdt.rs).
//!
//! Waarom PCIe en niet de MMIO-bus: virt heeft geen sysbus-xHCI die een
//! gebruiker kan aanmaken (`nec-usb-xhci` en `qemu-xhci` zijn allebei
//! PCI), en de O6N en het UEFI-board vinden hun xHCI ook op PCIe. Het
//! verschil met die twee: hier boot geen firmware, dus niemand wees de BAR
//! toe; dat doen we zelf uit het 32-bit MMIO-venster van virt, zoals de Pi 5
//! het doet achter zijn eigen root-complex.

use crate::BLK_DMA;
// Het ECAM-venster en het stuk MMIO-venster van de xHCI: de watchdog
// krijgt het zijne (pcie.rs).
use crate::pcie::{ECAM, ECAM_LAST_BUS, MMIO};
use board::{Region, UsbHost, UsbHosts, UsbKind};
use cpu::println;
use dev::Pa;
use driver_pcie::{Bar, Config, Ecam, Function, MmioWindow};
/// De eerste SPI van de vier INTx-lijnen (`VIRT_PCIE`, SPI 3), als
/// INTID.
const INTX_BASE: u32 = 32 + 3;
/// Klasse 0x0c0330: serial bus, USB, xHCI.
const CLASS_XHCI: u32 = 0x0c_0330;

/// Het USB-DMA-stuk: de bovenste 2 MB van de schijf-helft van de
/// DMA-regio. virtio-blk gebruikt daar alleen `DMA_NEED` (ruim 1 MiB)
/// van, onderin; de 2 MB-korrel is die van `abi::layout::USB_DMA_SIZE`.
/// Normal-NC gemapt, buiten de kern-RAM, zoals de NIC-ringen.
pub(crate) const USB_DMA: Region = Region {
    base: Pa(BLK_DMA.base.0 + BLK_DMA.size - abi::layout::USB_DMA_SIZE),
    size: abi::layout::USB_DMA_SIZE,
};

const _: () = {
    assert!(BLK_DMA.base.0 + driver_virtioblk::DMA_NEED <= USB_DMA.base.0);
    assert!(USB_DMA.base.0.is_multiple_of(0x20_0000));
    assert!(USB_DMA.base.0 + USB_DMA.size == BLK_DMA.base.0 + BLK_DMA.size);
};

/// Zoekt de xHCI op bus 0, wijst zijn BAR toe en zet decode en
/// bus-master aan. Geen xHCI (een QEMU-regel zonder `-device
/// qemu-xhci`) is één regel en een lege lijst.
pub(crate) fn hosts() -> UsbHosts {
    let mut out = UsbHosts::new();
    // SAFETY: het ECAM-venster van virt ligt in de Device-gigabyte van
    // de identity map en blijft gemapt; QEMU legt er met
    // `highmem-ecam=off` bus 0 tot 15.
    let ecam = unsafe { Ecam::new(ECAM, 0, ECAM_LAST_BUS) };
    let Some(f) = driver_pcie::find(&ecam, 0, |f| f.class == CLASS_XHCI) else {
        println!("usb: no xHCI on the PCIe host of virt (no -device qemu-xhci) HOPOS_USB_NONE");
        return out;
    };
    let Some(regs) = bar0(&ecam, &f) else {
        return out;
    };
    f.enable(&ecam);
    let irq = f
        .intx_pin(&ecam)
        .map(|pin| INTX_BASE + u32::from(driver_pcie::swizzle(pin, f.bdf.dev)));
    println!(
        "usb: xHCI {:04x}:{:04x} at {} on PCIe, registers {:#x}+{:#x}, INTx {irq:?}",
        f.vendor, f.device, f.bdf, regs.base.0, regs.size
    );
    let _ = out.push(UsbHost {
        name: "qemu-xhci",
        kind: UsbKind::Xhci,
        regs,
        irq,
        bus_off: 0,
        dma: USB_DMA,
    });
    out
}

/// Wijst de BAR's van `f` toe uit [`MMIO`] en geeft BAR 0, het
/// xHCI-venster. Een fout is één regel.
fn bar0(ecam: &Ecam, f: &Function) -> Option<Region> {
    let mut win = MmioWindow::new(MMIO.base.0, MMIO.size);
    match f.assign_bars(ecam, &mut win) {
        Ok([Bar::Mem { addr, size, .. }, ..]) if addr != 0 => Some(Region {
            base: Pa(addr),
            size,
        }),
        Ok(_) => {
            println!(
                "usb: xHCI at {} has no memory BAR 0 (BAR0 {:#010x}) HOPOS_USB_NONE",
                f.bdf,
                ecam.read32(f.bdf, driver_pcie::reg::BAR0)
            );
            None
        }
        Err(e) => {
            println!("usb: xHCI at {}: {e} HOPOS_USB_NONE", f.bdf);
            None
        }
    }
}
