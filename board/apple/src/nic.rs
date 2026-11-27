//! De NIC achter de ethernet-poort, per chip: de Broadcom 57762 (tg3) op
//! de M4, de Realtek RTL8125 op de M6 (ChargerLAB-teardown van de A3534:
//! RTL8125CPA; één bron, de probe toetst vendor en device). Beide drivers
//! bestonden al: de Realtek is die van de Orion O6N (`RTL8125CP`, XID
//! 0x708), naar Linux' r8169.
//!
//! Wat per chip verschilt, staat hier; de PCIe-keten, de lijn op de AIC en
//! de pomp zijn gedeeld (`pcie.rs`, `irq.rs`, `lib.rs`).

use crate::pcie::Endpoint;
use crate::{NET_DMA, fwinfo};
use board::Error;
use cpu::println;
use dev::Pa;

#[cfg(feature = "t8152")]
pub use driver_rtl8126::{IrqAck, Rtl8126 as Nic};
#[cfg(not(feature = "t8152"))]
pub use driver_tg3::{IrqAck, Tg3 as Nic};

/// Het bufferblok van de driver binnen [`NET_DMA`]: Normal WB, de ringen
/// ervoor blijven NC; de driver veegt het zelf (`dev::push`/`dev::pull`).
#[cfg(not(feature = "t8152"))]
pub(crate) const BUF: (u64, u64) = (driver_tg3::BUF_OFF, driver_tg3::BUF_LEN);
#[cfg(feature = "t8152")]
pub(crate) const BUF: (u64, u64) = (driver_rtl8126::BUF_OFF, 2 << 20);

/// Wat de driver van de DMA-regio vraagt.
#[cfg(not(feature = "t8152"))]
pub(crate) const DMA_NEED: u64 = driver_tg3::DMA_NEED;
#[cfg(feature = "t8152")]
pub(crate) const DMA_NEED: u64 = driver_rtl8126::DMA_NEED;

/// Dwingt `set_irq` meteen een interrupt af? De tg3 wel (HOSTCC_MODE_NOW),
/// en daarmee toetst `irq::wire_nic` de lijn door aflevering. De Realtek
/// niet (r8169 kent er geen): zijn lijn gaat scherp zonder toets, en de
/// eerste RX zegt of hij klopt.
#[cfg(not(feature = "t8152"))]
pub(crate) const FORCED_IRQ: bool = true;
#[cfg(feature = "t8152")]
pub(crate) const FORCED_IRQ: bool = false;

/// De naam in de bootregels.
#[cfg(not(feature = "t8152"))]
pub(crate) const NAME: &str = "tg3";
#[cfg(feature = "t8152")]
pub(crate) const NAME: &str = "rtl8125";

/// De driver op het endpoint, met het MAC uit de ADT, en de
/// autonegotiatie aan zonder te wachten (de link-wacht is van de taak).
/// `Ok(None)` als het endpoint niet de NIC is die we kennen.
#[cfg(not(feature = "t8152"))]
pub(crate) fn bring_up(ep: &Endpoint) -> Result<Option<Nic>, Error> {
    use netdev::Mac;
    if !driver_tg3::drives(ep.f.vendor, ep.f.device) {
        println!("net: {} is not the tg3 we know", ep.f);
        return Ok(None);
    }
    let mac = fwinfo::nic_mac().ok_or(Error::Nic("no local-mac-address in the ADT"))?;
    // SAFETY: BAR0 is net toegewezen uit het 64-bit venster (onder 512 GB,
    // Device-gemapt) en `cfg` is de ECAM-config-space van deze functie;
    // NET_DMA is van deze driver alleen, Normal-NC met het bufferblok
    // Normal WB (`NET_BUF`, de driver veegt het zelf), en de DART staat
    // in bypass, dus een DMA-adres is een fysiek adres.
    let mut nic = unsafe {
        Nic::new(
            Pa(ep.bar0),
            Pa(ep.cfg),
            Mac(mac),
            NET_DMA.base,
            NET_DMA.size,
            cpu::idle::now,
        )
    }
    .map_err(|e| {
        println!("tg3: {e} HOPOS_TG3_FAIL");
        Error::Nic("tg3 init failed")
    })?;
    println!("tg3: {}", nic.describe());
    crate::serror_check("the tg3 init");
    // De autonegotiatie aan, zonder te wachten: de link-wacht is van de
    // taak (`netdev::Device::link`), en de tg3 zet dan zijn poortmodus.
    nic.start_link().map_err(|e| {
        println!("tg3: {e} HOPOS_TG3_FAIL");
        Error::Nic("tg3 PHY did not start")
    })?;
    println!("tg3: link pending (autoneg)");
    Ok(Some(nic))
}

/// De Realtek: BAR2 (BAR0 is de I/O-alias), het MAC uit de ADT vóór dat
/// van de chip (Linux `rtl_init_mac_address`: het platform eerst; na een
/// PERST kan de chip leeg zijn), dezelfde DMA-indeling als de tg3.
#[cfg(feature = "t8152")]
pub(crate) fn bring_up(ep: &Endpoint) -> Result<Option<Nic>, Error> {
    if !driver_rtl8126::supported(ep.f.vendor, ep.f.device) {
        println!(
            "net: {} ({:04x}:{:04x}) is not the Realtek we expect HOPOS_NIC_NONE",
            ep.f, ep.f.vendor, ep.f.device
        );
        return Ok(None);
    }
    if ep.bar2 == 0 {
        return Err(Error::Nic("rtl8125 without a memory BAR2"));
    }
    let mac = fwinfo::nic_mac();
    // SAFETY: BAR2 is net toegewezen uit het 64-bit venster (onder 512 GB,
    // Device-gemapt, ruim boven `MMIO_LEN`), memory-decode en bus-master
    // staan aan (`enumerate_nic`); NET_DMA is van deze driver alleen,
    // 2 MB-gealigneerd, Normal-NC met het bufferblok Normal WB, en de DART
    // staat in bypass.
    let mut nic = unsafe {
        Nic::with_mac(
            Pa(ep.bar2),
            NET_DMA.base,
            NET_DMA.size,
            cpu::idle::now,
            mac.map(netdev::Mac),
        )
    }
    .map_err(|e| {
        println!("net: {e} at {} bar2 {:#x} HOPOS_NIC_FAIL", ep.f, ep.bar2);
        Error::Nic("rtl8125 init failed")
    })?;
    println!(
        "net: {} xid {:#x} at {} bar2 {:#x}, mac from {}",
        nic.name(),
        nic.xid(),
        ep.f,
        ep.bar2,
        if mac.is_some() { "the ADT" } else { "the chip" }
    );
    crate::serror_check("the rtl8125 init");
    nic.start_link().map_err(|e| {
        println!("net: {} {e}", nic.name());
        Error::Nic("rtl8125 PHY did not start")
    })?;
    println!("net: {} link pending (autoneg)", nic.name());
    Ok(Some(nic))
}
