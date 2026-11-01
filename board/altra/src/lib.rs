//! De Ampere Altra (en AmpereOne): het UEFI-board (`board-uefi`) plus wat
//! deze machine eigen heeft (`OLD/metal/board/altra/hop`).
//!
//! Alles wat de firmware vertelt, komt langs de universele weg: cores uit
//! de MADT, RAM uit de memory-map (de ~300 GB bulk boven de vlakke 512 GB,
//! gemeten 15-07), PCIe uit de MCFG (tot acht segmenten), de console uit de
//! SPCR (16 TB hoog), CPU_ON via PSCI. Wat hier staat, is de kennis die geen
//! ACPI-tabel draagt:
//!
//! - de NIC: de Intel igb (I210) op deze machines, en QEMU's 82576 waarmee
//!   het pad getest wordt. Op MSI-X via de ITS, de route die Linux neemt,
//!   met een zelftest, en zonder lijn een fout ([`nic_irq_mode`]). Nooit INTx: de
//!   `_PRT`-INTx komt aan, maar hem aanzetten doodt de SoC (L83, 19-09,
//!   UART-bewijs);
//! - de schijf: de eerste NVMe, het hele device voor HopOS;
//! - de thermometer: de SMpro via PCC-kanaal 14;
//! - de klok is op servers firmware-domein: geen dvfs-beleid;
//! - de core-klassen: homogeen, alles "big".

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

mod machine;

pub use machine::{Altra, Ampere};

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Altra;

// De slot- en flip-lijm van de binary leest het plan onder deze namen
// (`slots`, en `facts` voor het beeld van een PIE-kern), zoals bij de Pi's;
// op de Altra zijn het die van het UEFI-board.
pub use board_uefi::{facts, slots};

use board::CoreClass;
use board_uefi::irq::Mode;
use driver_pcie::Function;

/// De naam, voor de bootlog.
pub const NAME: &str = "altra";

/// Hoe lang de igb op een link wacht (het Altra-recept sinds 13-07).
pub const LINK_TIMEOUT_NS: u64 = 8_000_000_000;

/// Deze machines zijn homogeen: alles is "big". De UEFI-laag laat de vraag
/// bewust aan het board; een verzonnen indeling maakt jobs onplaatsbaar.
#[must_use]
pub const fn core_class(_core: usize) -> CoreClass {
    CoreClass::Big
}

/// Wat `hopos.nicirq` op dit board mag: `msix` (de standaard, en ook wat
/// `auto` hier betekent). INTx, ook een opgegeven INTID, weigert
/// het board: de `_PRT`-INTx aanzetten doodde deze SoC (L83, 19-09), en
/// `auto` zou er na een mislukte MSI-X op terugvallen.
pub fn nic_irq_mode(m: Mode) -> Result<Mode, &'static str> {
    match m {
        Mode::Auto | Mode::Msix => Ok(Mode::Msix),
        Mode::Intx | Mode::Line(_) => Err("INTx kills this SoC (L83), hopos.nicirq is msix here"),
    }
}

/// Is `f` een NIC van dit board?
#[must_use]
pub fn is_nic(f: &Function) -> bool {
    driver_igb::supported(f.vendor, f.device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_uefi::pcie::{CLASS_NVME, first};
    use driver_pcie::{Bdf, Config};
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeCfg(RefCell<HashMap<Bdf, [u32; 64]>>);

    impl FakeCfg {
        fn add(&self, bdf: Bdf, vendor: u16, device: u16, class: u32, bar0: u64) {
            let mut s = [0u32; 64];
            s[0] = u32::from(vendor) | (u32::from(device) << 16);
            s[2] = class << 8;
            s[4] = (bar0 as u32 & !0xf) | 0x4;
            s[5] = (bar0 >> 32) as u32;
            self.0.borrow_mut().insert(bdf, s);
        }
    }

    impl Config for FakeCfg {
        fn read32(&self, bdf: Bdf, off: u16) -> u32 {
            self.0
                .borrow()
                .get(&bdf)
                .map_or(u32::MAX, |s| s[usize::from(off / 4) % 64])
        }
        fn write32(&self, bdf: Bdf, off: u16, v: u32) {
            if let Some(s) = self.0.borrow_mut().get_mut(&bdf) {
                s[usize::from(off / 4) % 64] = v;
            }
        }
        fn write16(&self, _: Bdf, _: u16, _: u16) {}
    }

    #[test]
    fn the_nic_is_msix_never_intx() {
        assert_eq!(nic_irq_mode(Mode::Msix), Ok(Mode::Msix));
        assert_eq!(nic_irq_mode(Mode::Auto), Ok(Mode::Msix));
        assert!(nic_irq_mode(Mode::Intx).is_err());
        assert!(nic_irq_mode(Mode::Line(166)).is_err());
        // Zonder regel in hopos.cfg is het msix.
        assert_eq!(Mode::parse("", Mode::Msix), (Mode::Msix, true));
    }

    #[test]
    fn igb_and_nvme_are_found_across_segments() {
        let seg0 = FakeCfg::default();
        seg0.add(
            Bdf::new(0, 1, 0).unwrap(),
            0x8086,
            0x1234,
            0x02_00_00,
            0x4000_0000,
        );
        let seg1 = FakeCfg::default();
        seg1.add(
            Bdf::new(0, 2, 0).unwrap(),
            0x8086,
            0x1533,
            0x02_00_00,
            0x3000_0010_0000,
        );
        seg1.add(
            Bdf::new(0, 3, 0).unwrap(),
            0x144d,
            0xa80a,
            CLASS_NVME,
            0x3000_0020_0000,
        );
        let nic = first([(&seg0, 0), (&seg1, 0)], 0, is_nic).unwrap();
        assert_eq!((nic.f.device, nic.bar), (0x1533, 0x3000_0010_0000));
        assert_eq!(nic.win, 1);
        let disk = first([(&seg0, 0), (&seg1, 0)], 0, |f| f.class == CLASS_NVME).unwrap();
        assert_eq!((disk.bar, disk.win), (0x3000_0020_0000, 1));
    }
}
