//! De USB-invoer van de Pi 5: de twee xHCI's in de RP1 (Go:
//! `cmd/hopos/usb_rpi5.go`). De goedkoopste van de Pi's, en dat is geen
//! toeval: de PCIe-link naar de RP1 staat al voor de GEM (`probe_nic`,
//! `rp1_link`), dus hier blijft alleen het venster binnen de RP1 over.
//!
//! De RP1 heeft zijn cores al in hostmodus staan voor wij er zijn
//! (`generic-xhci` in de Linux-DT), dus hier geen DWC3-init zoals op de
//! Radxa. Is de link er niet (de NIC-probe faalde of draaide niet), dan
//! noemen we geen controller: een toegang op een dood PCIe-venster is op
//! de BCM2712 geen nette 0xffffffff maar een gok.

use crate::RP1;
use dev::Pa;

/// De eerste USB3-hostcontroller van de RP1 (datasheet §5; Linux-DT
/// `xhci@200000`).
pub(crate) const RP1_USB0: Pa = Pa(RP1 + 0x20_0000);
/// De tweede (`xhci@300000`).
pub(crate) const RP1_USB1: Pa = Pa(RP1 + 0x30_0000);

#[cfg(feature = "usb")]
pub(crate) use hosts::hosts;

/// De hosts zelf: alleen met de bouwsteen `usb` (de gui- of de
/// media-smaak). De halt hieronder is er altijd: een kale kern die een
/// gui-kern opvolgt, erft lopende DMA-masters.
#[cfg(feature = "usb")]
mod hosts {
    use super::{RP1_USB0, RP1_USB1};
    use crate::{RP1_BUS_OFF, rc_bare};
    use board::{Region, UsbHost, UsbHosts, UsbKind, usb_dma_slice};
    use board_raspi::usb::UsbCtx;

    /// Het venster van elk: 1 MB.
    const RP1_USB_SIZE: u64 = 0x10_0000;

    /// De twee controllers achter de RP1, met elk de helft van het DMA-stuk.
    ///
    /// `bus_off`: de RP1 is een PCIe-master; wat hij een adres noemt, gaat door
    /// het inbound-venster van de BCM2712-RC, en dat legt PCIe 0x10_0000_0000
    /// op DRAM 0 (dezelfde term als de GEM). Zonder die term zoekt de
    /// controller zijn ringen 64 GB naast het DRAM, en dat geeft geen
    /// foutmelding, alleen stilte (Go, 06-08).
    pub(crate) fn hosts(ctx: &UsbCtx) -> UsbHosts {
        let mut out = UsbHosts::new();
        let (phy, dl) = rc_bare(ctx.now).link_status();
        if !(phy && dl) {
            cpu::println!(
                "usb: the RP1 link is down (phy {phy}, dl {dl}): the NIC probe trains it, no USB HOPOS_USB_NONE"
            );
            return out;
        }
        for (i, (name, base)) in [("rp1-usb0", RP1_USB0), ("rp1-usb1", RP1_USB1)]
            .into_iter()
            .enumerate()
        {
            let _ = out.push(UsbHost {
                name,
                kind: UsbKind::Xhci,
                regs: Region {
                    base,
                    size: RP1_USB_SIZE,
                },
                // De lijnen lopen via de RP1-MSI-X en de MIP; de driver pollt,
                // dus die bedrading komt pas als hij dat niet meer doet.
                irq: None,
                bus_off: RP1_BUS_OFF,
                dma: usb_dma_slice(ctx.dma, i, 2),
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_rp1_windows_are_in_gigabyte_124() {
            assert_eq!(RP1_USB0.0, 0x1f_0020_0000);
            assert_eq!(RP1_USB1.0 >> 30, 124);
            let d = board_raspi::usb::USB_DMA;
            assert_eq!(usb_dma_slice(d, 1, 2).base.0, d.base.0 + d.size / 2);
        }
    }
}

/// USBCMD (de eerste operationele register, op CAPLENGTH): Run/Stop.
const USBCMD_RS: u32 = 1 << 0;
/// USBSTS (operationeel +4): HCHalted.
const USBSTS_HCH: u32 = 1 << 0;
/// Hoe lang een halt mag duren: xHCI 1.2 §5.4.1 zegt 16 ms na Run/Stop = 0.
const HALT_NS: u64 = 20_000_000;

/// Halteert de twee xHCI's van een RP1 die de vorige kern liet draaien
/// (Linux' kexec-weg: `usb_hcd_platform_shutdown`), vóór de link eronder
/// reset ([`crate::rp1_quiesce`]). Alleen met DL actief. Geeft hoeveel er
/// stilstaan.
pub(crate) fn halt_inherited(now: fn() -> u64) -> usize {
    [RP1_USB0, RP1_USB1]
        .into_iter()
        .filter(|&base| halt(base, now))
        .count()
}

/// Run/Stop eraf en wachten op HCHalted, hoogstens [`HALT_NS`]. Een
/// venster zonder controller (CAPLENGTH 0 of all-ones) telt niet.
fn halt(base: Pa, now: fn() -> u64) -> bool {
    let caplen = dev::read32(base) & 0xff;
    if caplen == 0 || caplen == 0xff {
        return false;
    }
    let op = base.add(u64::from(caplen));
    dev::write32(op, dev::read32(op) & !USBCMD_RS);
    dev::poll_until(now, HALT_NS, || dev::read32(op.add(4)) & USBSTS_HCH != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// 1 ms per lezing: de halt-termijn loopt in twintig lezingen af.
    fn ticking() -> u64 {
        NOW.fetch_add(1_000_000, std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn halt_clears_run_stop_and_waits_for_hchalted() {
        let mut blk = vec![0u64; 16];
        let base = Pa(blk.as_mut_ptr() as usize as u64);
        // Geen controller: CAPLENGTH 0.
        assert!(!halt(base, ticking));
        // CAPLENGTH 0x20, lopend (RS plus INTE), nog niet gehalteerd.
        dev::write32(base, 0x0100_0020);
        dev::write32(base.add(0x20), USBCMD_RS | (1 << 2));
        assert!(!halt(base, ticking), "HCHalted never came");
        assert_eq!(dev::read32(base.add(0x20)), 1 << 2, "only Run/Stop off");
        dev::write32(base.add(0x24), USBSTS_HCH);
        assert!(halt(base, ticking));
    }
}
