//! De Radxa Orion O6N (Cix P1 / CD8180: twaalf Armv9-cores in drie
//! clusters, 2x RTL8125/8126, M.2 NVMe): het primaire productiedoel van
//! HopOS, en in v3 het UEFI-board (`board-uefi`) plus wat de O6N eigen
//! heeft (`OLD/metal/board/o6n/hop`).
//!
//! Alles wat de firmware vertelt, komt langs de universele weg van
//! `board-uefi`: cores uit de MADT, RAM uit de memory-map, PCIe uit de MCFG,
//! de console uit de SPCR, CPU_ON via PSCI. Wat dit crate toevoegt, is de
//! kennis die geen ACPI-tabel draagt:
//!
//! - de NIC: de Realtek achter de eerste root-poort die er een heeft
//!   (`board_uefi::pcie`), en zijn INTx-lijn per root-poort ([`probe`]);
//! - de schijf: de NVMe via dezelfde zoektocht;
//! - de klok ([`clock`], [`cpc`]): de `_CPC`-fastchannels en de knop voor
//!   `driver-dvfs`;
//! - de core-klassen ([`class`]): small/mid/big, want de Cix-firmware vult de
//!   MADT-efficiëntieklasse niet;
//! - de thermometer ([`thermal`]): de SCP via SCMI.
//!
//! - de codec ([`codec`], feature `media`): de VPU uit de DSDT, stroom en
//!   klokken via SCMI, en de stroomcyclus als hij vastzit. De driver zelf
//!   (`media-mve`) importeert board niet en omgekeerd.
//!
//! USB hoort bij gui en staat hier niet.

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

pub mod class;
pub mod clock;
#[cfg(feature = "media")]
pub mod codec;
mod machine;
pub mod probe;
pub mod thermal;
// De tien native xHCI's uit de DSDT (usb.rs), alleen met de bouwsteen
// `usb` (de gui- of de media-smaak).
#[cfg(feature = "usb")]
mod usb;

pub use machine::{Cix, O6n, is_cix};

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = O6n;

// De slot- en flip-lijm van de binary leest het plan onder deze namen
// (`slots`, en `facts` voor het beeld van een PIE-kern), zoals bij de Pi's;
// op de O6N zijn het die van het UEFI-board.
pub use board_uefi::{facts, slots};

/// De XSDT-OEM-ID van de Cix-firmware: de runtime-toets dat dit werkelijk
/// een Cix P1 is vóór er board-kennis (mailbox-adressen) in MMIO gaat. Een
/// generieke UEFI-doos met dit image krijgt dan gewoon het UEFI-gedrag (de
/// Ampere draaide dit image op 19-09).
pub const OEM_ID: [u8; 6] = *b"CIXTEK";

/// De naam, voor de bootlog.
pub const NAME: &str = "o6n";
