//! De USB-invoer van de Radxa Zero 3E: twee DWC3-cores, elk eerst in
//! hostmodus en dan als xHCI (Go: `cmd/hopos/usb_rk3566.go`). De hostmodus
//! zelf is van `driver-dwc3`, die de binary vóór de xHCI draait
//! ([`board::UsbKind::Dwc3`]).
//!
//! Dit bezit ook de klokken, de reset en de PHY-poort van usbdrd30. U-Boot
//! laat usbhost30 geklokt achter en usbdrd30 niet (30-09 op ijzer: "GSNPSID
//! names no DWC3 core (0 = not clocked)"). Go zette die NIET aan
//! (`cmd/hopos/usb_rk3566.go`: de bits waren ongeverifieerd); nu staan ze
//! nagelezen in Linux master (30-09): clk-rk3568.c zet `ACLK_USB3OTG0`,
//! `CLK_USB3OTG0_REF` en `CLK_USB3OTG0_SUSPEND` in `CLKGATE_CON(10)` bit 8,
//! 9 en 10 (die van usbhost30 op 12, 13 en 14), rk3568-cru.h geeft
//! `SRST_USB3OTG0` = 148 (bank 9, bit 4), en phy-rockchip-inno-usb2.c zet de
//! OTG-poort van usb2phy0 aan met `phy_sus` = 0 op offset 0 van zijn GRF
//! (`usb2phy0_grf`, syscon@fdca0000 in de DTB van U-Boot). De volgorde is die
//! van `dwc3_probe` (klokken, reset los) en `rockchip_usb2phy_power_on`
//! (`phy_sus` uit, 1,5 tot 2 ms voor de UTMI-klok). De PHY-reset zelf
//! (`SRST_USB2PHY0_POR`) blijft af: die deelt usb2phy0 met usbhost30.
//!
//! Eén meting als vangrail tegen een verkeerde tabel: usbhost30 draait, dus
//! zijn drie gates en aclk_pipe en pclk_pipe (bit 0 en 1) moeten open lezen. Lezen ze dicht, dan klopt de kaart niet
//! en raken we usbdrd30 niet aan (luid).
//!
//! En één ding dat met klokken niet op te lossen is: usbdrd30 is de USB-C
//! van de Zero 3E, en dat is ook de voedingsingang. Hostmodus levert daar
//! geen VBUS: de DT kent de boost van de PMIC (`OTG_SWITCH` van de RK817)
//! wel, maar hangt hem aan geen poort, en hij zit achter I2C, dat v3 niet
//! heeft. Een apparaat dat van de bus leeft, heeft op die poort een hub met
//! eigen voeding nodig. De regel zegt het.
//!
//! Alleen met de bouwsteen `usb` (de gui- of de media-smaak).

use crate::soc::{CRU, delay_us, hiword};
use dev::Pa;

/// usbdrd30: de OTG-core (de USB-C van de Radxa).
pub const USBDRD30: Pa = Pa(0xFCC0_0000);
/// usbhost30: de hostcore (de USB-A).
pub const USBHOST30: Pa = Pa(0xFD00_0000);
/// Het venster van een DWC3-core: 1 MB (de xHCI onderin, de globale
/// registers op +0xC100).
pub const DWC3_SIZE: u64 = 0x10_0000;

const _: () = {
    // Beide in de Device-MMIO van de SoC (vanaf 0xF000_0000, `mmu`).
    assert!(USBDRD30.0 >= 0xF000_0000 && USBHOST30.0 >= 0xF000_0000);
};

pub(crate) use imp::hosts;

/// `CLKGATE_CON(10)`: de gates van beide DWC3's (actief-laag, 1 = uit).
const CLKGATE10: u64 = 0x300 + 10 * 4;
/// usbdrd30 (OTG0): aclk, ref, suspend.
const OTG0_GATES: [u32; 3] = [8, 9, 10];
/// Wat usbhost30 nodig heeft en dus open moet lezen: aclk_pipe en pclk_pipe
/// (bit 0 en 1, ook de ouders van usbdrd30 en de klok van de PIPE-GRF) en
/// zijn eigen aclk, ref en suspend (12, 13, 14). Alleen gelezen, als
/// vangrail.
const HOST_GATES: [u32; 5] = [0, 1, 12, 13, 14];
/// `SOFTRST_CON(9)`: SRST_USB3OTG0 = 148, bit 4.
const SOFTRST9: u64 = 0x400 + 9 * 4;
const SRST_OTG0: u32 = 4;
/// De GRF van usb2phy0 (`rockchip,usbgrf` van `usb2phy@fe8a0000`).
pub const USB2PHY0_GRF: Pa = Pa(0xFDCA_0000);
/// `phy_sus` van de OTG-poort: [8:0] op offset 0; 0 = aan, 0x1d1 = suspend.
const OTG_PHY_SUS: u64 = 0x0000;
const PHY_SUS_MASK: u32 = 0x1FF;
/// De PIPE-GRF (`pipegrf`, syscon@fdc50000) en daarin `USB3OTG0_CON1`
/// (combphy: `u3otg0_port_en`, 0x1100 = USB3-poort aan, 0x0181 = alleen
/// USB2). De RK3566 heeft geen combphy0, dus Linux schrijft hem op dit
/// silicium nooit; wij lezen hem alleen, voor de regel.
pub const PIPE_GRF: Pa = Pa(0xFDC5_0000);
const USB3OTG0_CON1: u64 = 0x0104;

const _: () = {
    // Beide GRF's in de Device-MMIO van de SoC (vanaf 0xF000_0000, `mmu`).
    assert!(USB2PHY0_GRF.0 >= 0xF000_0000 && PIPE_GRF.0 >= 0xF000_0000);
};

/// De schrijf die gates `bits` opent: alleen maskerbits, waarde 0.
#[must_use]
pub fn gates_open_word(bits: &[u32]) -> u32 {
    bits.iter().fold(0, |w, &b| w | hiword(0, 1, b))
}

/// Staan alle gates `bits` open in de gelezen waarde `v`?
#[must_use]
pub fn gates_open(v: u32, bits: &[u32]) -> bool {
    bits.iter().all(|&b| v & (1 << b) == 0)
}

/// Wat [`power_otg`] las en deed, voor één regel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OtgPower {
    /// `CLKGATE_CON(10)` ervoor en erna.
    pub gates: (u32, u32),
    /// `SOFTRST_CON(9)` ervoor.
    pub reset: u32,
    /// `phy_sus` van de OTG-poort ervoor en erna.
    pub phy_sus: (u32, u32),
    /// `USB3OTG0_CON1` van de PIPE-GRF (0 = niet gelezen).
    pub pipe: u32,
    /// Klopte de vangrail (de gates van usbhost30 open), en is er dus
    /// geschreven?
    pub done: bool,
}

/// Klokt usbdrd30, haalt hem uit reset en zet de OTG-poort van usb2phy0
/// aan (de moduledoc zegt waar elk bit vandaan komt). Busy-waits van ~2 ms,
/// één keer bij de boot.
pub fn power_otg() -> OtgPower {
    let gates0 = dev::read32(CRU.add(CLKGATE10));
    let reset = dev::read32(CRU.add(SOFTRST9));
    let sus0 = dev::read32(USB2PHY0_GRF.add(OTG_PHY_SUS)) & PHY_SUS_MASK;
    if !gates_open(gates0, &HOST_GATES) {
        return OtgPower {
            gates: (gates0, gates0),
            reset,
            phy_sus: (sus0, sus0),
            pipe: 0,
            done: false,
        };
    }
    // Pas nu: de PIPE-GRF hangt aan pclk_pipe, en die staat dus open.
    let pipe = dev::read32(PIPE_GRF.add(USB3OTG0_CON1));
    dev::write32(CRU.add(CLKGATE10), gates_open_word(&OTG0_GATES));
    dev::mb();
    dev::write32(CRU.add(SOFTRST9), hiword(0, 1, SRST_OTG0));
    dev::write32(USB2PHY0_GRF.add(OTG_PHY_SUS), hiword(0, PHY_SUS_MASK, 0));
    dev::mb();
    delay_us(2000);
    OtgPower {
        gates: (gates0, dev::read32(CRU.add(CLKGATE10))),
        reset,
        phy_sus: (
            sus0,
            dev::read32(USB2PHY0_GRF.add(OTG_PHY_SUS)) & PHY_SUS_MASK,
        ),
        pipe,
        done: true,
    }
}

mod imp {
    use super::{DWC3_SIZE, USBDRD30, USBHOST30};
    use crate::USB_DMA;
    use board::{Region, UsbHost, UsbHosts, UsbKind, usb_dma_slice};

    /// Klokt usbdrd30 ([`super::power_otg`]) en zegt wat er stond; de
    /// DWC3-bring-up van de USB-taak leest daarna zelf GSNPSID.
    fn power_otg() {
        let p = super::power_otg();
        if !p.done {
            cpu::println!(
                "usb: usbdrd30 NOT clocked: usbhost30 runs while its gates or the PIPE clocks read closed (clkgate10 {:#x}, bits 0, 1, 12..14), so the gate map from clk-rk3568.c does not fit this silicon HOPOS_USB_OTG_REFUSED",
                p.gates.0
            );
            return;
        }
        cpu::println!(
            "usb: usbdrd30 clocked (clkgate10 {:#x} -> {:#x}, softrst9 was {:#x}, otg phy_sus {:#x} -> {:#x}, pipe-grf usb3otg0_con1 {:#x}); it is the USB-C power input: host mode without VBUS, a bus-powered device there needs a powered hub",
            p.gates.0,
            p.gates.1,
            p.reset,
            p.phy_sus.0,
            p.phy_sus.1,
            p.pipe
        );
    }

    /// De twee cores, usbdrd30 eerst geklokt.
    pub(crate) fn hosts() -> UsbHosts {
        power_otg();
        list()
    }

    /// De twee cores met elk de helft van het DMA-stuk. `bus_off` nul: de
    /// cores hangen rechtstreeks op de geheugenbus, dus wat de CPU een adres
    /// noemt, noemt de controller ook zo.
    pub(super) fn list() -> UsbHosts {
        let mut out = UsbHosts::new();
        for (i, (name, base)) in [("usbdrd30", USBDRD30), ("usbhost30", USBHOST30)]
            .into_iter()
            .enumerate()
        {
            let _ = out.push(UsbHost {
                name,
                kind: UsbKind::Dwc3,
                regs: Region {
                    base,
                    size: DWC3_SIZE,
                },
                // De GIC-lijnen staan in de DT; de driver pollt, dus die
                // bedrading komt pas als hij dat niet meer doet.
                irq: None,
                bus_off: 0,
                dma: usb_dma_slice(USB_DMA, i, 2),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::USB_DMA;
    use board::UsbKind;

    #[test]
    fn the_otg0_gates_are_clk_rk3568s() {
        // Bit 8, 9, 10 open: alleen de maskerbits 24, 25, 26.
        assert_eq!(gates_open_word(&OTG0_GATES), 0x0700_0000);
        // De vangrail: usbhost30 en de PIPE-klokken open, usbdrd30 dicht.
        assert!(gates_open(0x0700, &HOST_GATES));
        assert!(!gates_open(0x0700, &OTG0_GATES));
        assert!(!gates_open(0x1000, &HOST_GATES));
        assert!(!gates_open(0x0002, &HOST_GATES));
        // SRST_USB3OTG0 = 148 = bank 9, bit 4.
        assert_eq!(SOFTRST9, 0x400 + (148 / 16) * 4);
        assert_eq!(SRST_OTG0, 148 % 16);
    }

    #[test]
    fn two_cores_split_the_usb_region() {
        // De lijst, niet `hosts`: die klokt usbdrd30 (MMIO).
        let h = imp::list();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].dma.base, USB_DMA.base);
        assert_eq!(h[1].dma.base.0, USB_DMA.base.0 + USB_DMA.size / 2);
        assert!(h.iter().all(|u| u.kind == UsbKind::Dwc3));
    }
}
