//! De USB-invoer van het UEFI-board: elke xHCI die de firmware op PCIe
//! configureerde (klasse 0x0c0330 op de MCFG-segmenten). Op QEMU onder
//! EDK2 is dat de `qemu-xhci` van `GUI=display`; de Altra kan dezelfde
//! weg nemen zodra haar board hem doorgeeft. De O6N heeft zijn xHCI's niet
//! op PCIe maar als platformapparaten in de DSDT (board/o6n/src/usb.rs);
//! wat hij hier leent, is [`USB_DMA`].
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak).
//!
//! De firmware wees de BAR toe; wij zetten alleen memory-decode en
//! bus-mastering aan (EDK2 laat bus-master bij ExitBootServices vaak uit).
//! Loopt de controller nog van de firmware, dan zet de bring-up van
//! `usb` hem eerst stil, vóór er één stuk DMA-geheugen gewist wordt.

use crate::BLK_DMA;
use board::{MAX_USB_HOSTS, Region, UsbHost, UsbHosts, UsbKind};
use cpu::println;
use dev::Pa;
use driver_pcie::{Bar, Ecam, Function};

/// Het USB-DMA-stuk: de bovenste 2 MB van de schijf-helft van de
/// DMA-regio (Normal-NC, buiten de kern-RAM). virtio-blk gebruikt daar
/// ruim 1 MiB van, de NVMe van de O6N en de Altra 4 MiB, allebei onderin;
/// de 2 MB-korrel is die van `abi::layout::USB_DMA_SIZE`.
pub const USB_DMA: Region = Region {
    base: Pa(BLK_DMA.base.0 + BLK_DMA.size - abi::layout::USB_DMA_SIZE),
    size: abi::layout::USB_DMA_SIZE,
};

const _: () = {
    assert!(BLK_DMA.base.0 + driver_virtioblk::DMA_NEED <= USB_DMA.base.0);
    assert!(USB_DMA.base.0.is_multiple_of(0x20_0000));
    assert!(USB_DMA.base.0 + USB_DMA.size == BLK_DMA.base.0 + BLK_DMA.size);
};

/// Klasse 0x0c0330: serial bus, USB, xHCI.
const CLASS_XHCI: u32 = 0x0c_0330;

/// De namen van de controllers in volgorde van de scan; de logregels
/// hebben een `&'static str` nodig.
const NAMES: [&str; MAX_USB_HOSTS] = [
    "xhci0", "xhci1", "xhci2", "xhci3", "xhci4", "xhci5", "xhci6", "xhci7", "xhci8", "xhci9",
];

/// Elke xHCI op de PCIe-segmenten van de firmware, met BAR 0 als
/// venster en een gelijk stuk van [`USB_DMA`]. Een functie zonder
/// bruikbare BAR is één regel.
#[must_use]
pub fn hosts() -> UsbHosts {
    let mut found: [Option<(Ecam, Function)>; MAX_USB_HOSTS] = [None; MAX_USB_HOSTS];
    let mut n = 0;
    for (e, _seg, start) in crate::ecams() {
        driver_pcie::walk(&e, start, |f| {
            if f.class == CLASS_XHCI
                && let Some(slot) = found.get_mut(n)
            {
                *slot = Some((e, *f));
                n += 1;
            }
            n < MAX_USB_HOSTS
        });
    }
    let mut out = UsbHosts::new();
    for (i, (e, f)) in found.iter().flatten().enumerate() {
        let Some(regs) = bar0(e, f) else {
            continue;
        };
        f.enable(e);
        let name = NAMES.get(i).copied().unwrap_or("xhci");
        println!(
            "usb: {name}: xHCI {:04x}:{:04x} at {} on PCIe, registers {:#x}+{:#x}",
            f.vendor, f.device, f.bdf, regs.base.0, regs.size
        );
        let _ = out.push(UsbHost {
            name,
            kind: UsbKind::Xhci,
            regs,
            irq: None,
            bus_off: 0,
            dma: board::usb_dma_slice(USB_DMA, i, n),
        });
    }
    if out.is_empty() {
        println!("usb: no xHCI on the PCIe segments of the firmware HOPOS_USB_NONE");
    }
    out
}

/// BAR 0 zoals de firmware hem toewees, en boven 1 TB in de
/// identiteitsmap gezet (de Altra legt BAR's daar).
fn bar0(e: &Ecam, f: &Function) -> Option<Region> {
    match f.bar(e, 0) {
        Ok(Bar::Mem { addr, size, .. }) if addr != 0 => {
            if !crate::map_device(addr, size) {
                println!(
                    "usb: xHCI at {}: window {addr:#x}+{size:#x} unmappable HOPOS_USB_NONE",
                    f.bdf
                );
                return None;
            }
            Some(Region {
                base: Pa(addr),
                size,
            })
        }
        Ok(_) => {
            println!(
                "usb: xHCI at {} has no memory BAR 0 from the firmware HOPOS_USB_NONE",
                f.bdf
            );
            None
        }
        Err(err) => {
            println!("usb: xHCI at {}: {err} HOPOS_USB_NONE", f.bdf);
            None
        }
    }
}
