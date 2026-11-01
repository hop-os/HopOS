//! De USB-hosts van een board (`Board::usb_hosts`): alleen met de
//! bouwsteen `usb`, die de gui- en de media-smaak meetrekken. Kaal kent
//! het board-contract geen USB.

use crate::Region;
use bounded::BoundedVec;

/// Hoeveel USB-hostcontrollers een board kan aanbieden. De O6N meldt er in
/// zijn DSDT tot tien (de firmware-upgrade van zes naar tien, Go 18-09), de
/// rest één of twee; gelijk aan `usb::MAX_HOSTS`.
pub const MAX_USB_HOSTS: usize = 10;

/// Wat voor controller achter een [`UsbHost`] zit.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UsbKind {
    /// Een xHCI die klaarstaat: het venster is het capability-blok (PCIe,
    /// de RP1 van de Pi 5).
    Xhci,
    /// Een Synopsys DWC3-core: eerst in hostmodus zetten (de globale
    /// registers op +0xC100 van hetzelfde venster), daarna is het venster
    /// een xHCI (de RK3566).
    Dwc3,
}

/// Eén USB-hostcontroller zoals het board hem kent: het venster, de lijn,
/// het soort, en het stuk DMA-geheugen dat het board voor hem plande. Een
/// board dat hem achter PCIe heeft, heeft de link en de BAR al opgebracht
/// voor het hem noemt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct UsbHost {
    /// Komt in elke logregel van deze controller.
    pub name: &'static str,
    /// Het soort.
    pub kind: UsbKind,
    /// Het registervenster (het xHCI-capability-blok, of de DWC3-core).
    /// Device gemapt, voor altijd.
    pub regs: Region,
    /// De interruptlijn (GIC INTID), als het board hem weet. De driver
    /// pollt vandaag; de lijn staat hier voor de dag dat hij dat niet meer
    /// doet, en voor de logregel.
    pub irq: Option<u32>,
    /// Wat de controller bij een CPU-fysiek adres optelt: nul op een SoC,
    /// het inbound-venster van de root-complex achter PCIe (de RP1 van de
    /// Pi 5: 0x10_0000_0000).
    pub bus_off: u64,
    /// Het DMA-geheugen van deze controller alleen: Normal-NC en buiten
    /// elke RAM-declaratie, zoals de NIC-ringen.
    pub dma: Region,
}

/// Het `i`-de van `n` gelijke, op 4 KB gealigneerde stukken van `r`: de
/// verdeling van één USB-DMA-regio over meer controllers. Leeg voor een
/// `i` buiten `0..n`.
#[must_use]
pub const fn usb_dma_slice(r: Region, i: usize, n: usize) -> Region {
    if n == 0 || i >= n {
        return Region {
            base: r.base,
            size: 0,
        };
    }
    let span = (r.size / n as u64) & !0xfff;
    Region {
        base: r.base.add(i as u64 * span),
        size: span,
    }
}

/// De USB-hosts van een board: begrensd en zonder heap.
pub type UsbHosts = BoundedVec<UsbHost, MAX_USB_HOSTS>;

#[cfg(test)]
mod tests {
    use super::*;
    use dev::Pa;

    #[test]
    fn usb_dma_is_split_on_pages() {
        let r = Region {
            base: Pa(0x4fe0_0000),
            size: 0x20_0000,
        };
        assert_eq!(usb_dma_slice(r, 0, 1), r);
        let b = usb_dma_slice(r, 2, 3);
        assert_eq!(b.base, Pa(0x4fe0_0000 + 2 * 0xa_a000));
        assert_eq!(b.size, 0xa_a000);
        assert!(b.end().0 <= r.end().0);
        assert_eq!(usb_dma_slice(r, 3, 3).size, 0);
        assert_eq!(usb_dma_slice(r, 0, 0).size, 0);
    }
}
