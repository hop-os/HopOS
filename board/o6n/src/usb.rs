//! De USB-invoer van de O6N: de tien native xHCI's van de Cix P1, als
//! platformapparaten in de DSDT (`PNP0D10`, XHC0..5 en USB0..3), niet op
//! PCIe (Go: `board/o6n/hop/usb.go` en `cmd/hopos/usb_o6n.go`). Welke aan
//! staan en host zijn, zegt de firmware in haar variabelen-RAM (`GNVA`,
//! `GNVL`); de vorm van de DSDT leest [`firmware`].
//!
//! De DMA-stukken hebben een vaste eigenaar per positie: tien gelijke
//! stukken van [`board_uefi::usb::USB_DMA`], ook voor de controllers die uit
//! staan, zodat een kern-flip met een ander aan/uit-patroon nooit het stuk
//! van een levende controller aan een ander geeft (Go, 18-09: "keep all ten
//! positions"). Een controller die de firmware liet lopen, zet de bring-up
//! van `usb` stil vóór er één stuk gewist wordt.
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak).
//!
//! Niet geport uit Go: de toets dat het variabelen-RAM in de ACPI-RAM van de
//! EFI-geheugenkaart ligt (type 9/10, geen MMIO); het UEFI-board geeft die
//! kaart niet door. De bytes worden hier alleen gelezen, uit een adres dat
//! de parser op de exacte `GETV`-vorm toetste.

/// De vorm van de DSDT (usb_firmware.rs).
#[path = "usb_firmware.rs"]
mod firmware;

use board::{Region, UsbHost, UsbHosts, UsbKind, usb_dma_slice};
use board_uefi::Uefi;
use board_uefi::usb::USB_DMA;
use cpu::println;
use dev::Pa;
use firmware::HOSTS;

/// Het langste variabelen-RAM dat de parser toelaat (`GNVL` past in een
/// byte).
const VARS_MAX: usize = 256;

const _: () = {
    // De NVMe van de O6N neemt het onderste deel van de schijf-helft.
    assert!(
        board_uefi::BLK_DMA.base.0 + driver_nvme::DMA_NEED <= USB_DMA.base.0,
        "the USB region overlaps the NVMe"
    );
};

/// De controllers die de firmware aanzette als host, elk met zijn
/// vaste stuk DMA. Elke weigering is één regel.
pub(crate) fn hosts(uefi: &Uefi) -> UsbHosts {
    let mut out = UsbHosts::new();
    let Some(dsdt) = uefi.acpi_table(b"DSDT") else {
        println!("usb: no DSDT from the firmware, no USB HOPOS_USB_NONE");
        return out;
    };
    let fw = match firmware::parse(dsdt) {
        Ok(fw) => fw,
        Err(e) => {
            println!("usb: {e}, no USB HOPOS_USB_NONE");
            return out;
        }
    };
    let mut vars = [0u8; VARS_MAX];
    let Some(n) = read_vars(fw.base, fw.size, &mut vars) else {
        println!(
            "usb: firmware variables {:#x}+{:#x} unreadable, no USB HOPOS_USB_NONE",
            fw.base, fw.size
        );
        return out;
    };
    for (i, h) in fw
        .enabled(vars.get(..n).unwrap_or_default())
        .iter()
        .enumerate()
    {
        if !h.enabled {
            println!(
                "usb: {}: disabled by the firmware, or a device-role port",
                h.name
            );
            continue;
        }
        if !board_uefi::map_device(h.base, h.size) {
            println!(
                "usb: {}: window {:#x}+{:#x} unmappable",
                h.name, h.base, h.size
            );
            continue;
        }
        let _ = out.push(UsbHost {
            name: h.name,
            kind: UsbKind::Xhci,
            regs: Region {
                base: Pa(h.base),
                size: h.size,
            },
            // De lijn staat in de `_CRS`; de driver pollt.
            irq: None,
            bus_off: 0,
            dma: usb_dma_slice(USB_DMA, i, HOSTS),
        });
    }
    println!(
        "usb: {} of {HOSTS} native xHCI hosts enabled by the firmware",
        out.len()
    );
    out
}

/// Leest het variabelen-RAM `[base, base + size)` in `buf`; de lengte, of
/// `None` als het niet past of niet te mappen is.
fn read_vars(base: u64, size: u64, buf: &mut [u8; VARS_MAX]) -> Option<usize> {
    let n = usize::try_from(size).ok().filter(|n| *n <= VARS_MAX)?;
    if !board_uefi::map_device(base, size) {
        return None;
    }
    for (i, b) in buf.iter_mut().take(n).enumerate() {
        *b = dev::read8(Pa(base.checked_add(i as u64)?));
    }
    Some(n)
}
