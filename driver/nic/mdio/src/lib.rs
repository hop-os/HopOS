//! De gedeelde clause-22-PHY-laag van HopOS: de scan over de MDIO-bus, de
//! autonegotiatie, de link-decodering, en de eigenaardigheden van PHY-chips
//! die op meer dan één board zitten.
//!
//! Elke gigabit-MAC van HopOS hangt aan een externe PHY met dezelfde
//! clause-22-registers (BMCR, BMSR, ANAR, ANLPAR, GBCR, GBSR); alleen de
//! MDIO-master, het registerpad naar de bus, verschilt per MAC. Die master
//! is het trait [`Mdio`], en de MAC-driver levert hem (DWMAC4 op de RK3566,
//! GEM op de Pi 5, GENET op de Pi 4). De scan en de autonegotiatie staan
//! hier één keer (Go: `metal/driver/nic/mdio`).
//!
//! Deze crate bezit geen registers en geen tijd: de bus komt als trait. Hij
//! wacht ook niet: [`start_autoneg`] zet de autonegotiatie in gang en
//! [`link`] kijkt één keer; wie op de link wacht, is de taak (Linux:
//! `phy_state_machine` pollt de PHY, en `adjust_link` zet de MAC bij de
//! wissel). Daarom is hij `forbid(unsafe_code)` en volledig op de host te
//! toetsen met een PHY in RAM.
//!
//! Een PHY-eigenaardigheid hoort bij de PHY en niet bij een board: het
//! volgende board met dezelfde chip krijgt hem gratis ([`rtl8211f`]).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod rtl8211f;

#[cfg(test)]
mod tests;

use core::fmt;

/// Het aantal adressen op een clause-22-bus (vijf bits).
pub const PHY_ADDRS: u8 = 32;

/// De clause-22-registers die deze laag aanraakt (IEEE 802.3 §22.2.4).
pub mod reg {
    /// Basic Mode Control.
    pub const BMCR: u8 = 0;
    /// Basic Mode Status.
    pub const BMSR: u8 = 1;
    /// PHY-identifier, hoog woord.
    pub const ID1: u8 = 2;
    /// PHY-identifier, laag woord.
    pub const ID2: u8 = 3;
    /// Autonegotiation Advertisement.
    pub const ANAR: u8 = 4;
    /// Autonegotiation Link Partner Ability.
    pub const ANLPAR: u8 = 5;
    /// 1000BASE-T Control.
    pub const GBCR: u8 = 9;
    /// 1000BASE-T Status.
    pub const GBSR: u8 = 10;
}

/// BMCR: autonegotiatie aan.
pub const BMCR_AN_ENABLE: u16 = 1 << 12;
/// BMCR: autonegotiatie (her)starten.
pub const BMCR_AN_RESTART: u16 = 1 << 9;
/// BMSR: autonegotiatie klaar.
pub const BMSR_AN_COMPLETE: u16 = 1 << 5;
/// BMSR: link staat.
pub const BMSR_LINK: u16 = 1 << 2;
/// ANAR: 10/100 half en full duplex, selector 802.3.
pub const ANAR_10_100: u16 = 0x01E1;
/// GBCR: 1000BASE-T full duplex adverteren.
pub const GBCR_1000_FD: u16 = 1 << 9;
/// GBSR: de tegenpartij kan 1000BASE-T full duplex.
pub const GBSR_LP_1000_FD: u16 = 1 << 11;
/// ANLPAR: de tegenpartij kan 100BASE-TX full duplex.
pub const LPA_100_FD: u16 = 1 << 8;
/// ANLPAR: 100BASE-TX half duplex.
pub const LPA_100_HD: u16 = 1 << 7;
/// ANLPAR: 10BASE-T full duplex.
pub const LPA_10_FD: u16 = 1 << 6;

/// Waarom de PHY-laag weigert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// De MDIO-master bleef bezet: geen klok, of een bus die niemand
    /// terugtrekt. Dat is iets anders dan "geen PHY".
    Bus {
        /// Het PHY-adres van de transactie.
        phy: u8,
        /// Het register.
        reg: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bus { phy, reg } => {
                write!(f, "mdio: bus stuck on phy {phy} register {reg}")
            }
        }
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Een clause-22-managementpoort: één register lezen of schrijven op een
/// PHY-adres. De MAC-driver levert hem; een transactie die niet afrondt is
/// [`Error::Bus`].
pub trait Mdio {
    /// Leest register `reg` van de PHY op `phy`.
    fn read(&mut self, phy: u8, reg: u8) -> Result<u16>;

    /// Schrijft `val` in register `reg` van de PHY op `phy`.
    fn write(&mut self, phy: u8, reg: u8, val: u16) -> Result;

    /// Read-modify-write: wist `clear`, zet `set`, en laat de rest staan.
    /// De bits die we niet kennen, mogen niet weg.
    fn modify(&mut self, phy: u8, reg: u8, clear: u16, set: u16) -> Result {
        let v = self.read(phy, reg)?;
        self.write(phy, reg, (v & !clear) | set)
    }
}

/// Een gevonden PHY: zijn adres en zijn twee id-woorden.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Phy {
    /// Het adres waarop hij antwoordde.
    pub addr: u8,
    /// Register 2.
    pub id1: u16,
    /// Register 3.
    pub id2: u16,
}

/// De link na de autonegotiatie.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Link {
    /// De snelheid in Mbit/s: 10, 100 of 1000.
    pub mbps: u32,
    /// Full duplex?
    pub full_duplex: bool,
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Mbps {} duplex",
            self.mbps,
            if self.full_duplex { "full" } else { "half" }
        )
    }
}

/// Zoekt de eerste PHY op de bus. Een id van `0x0000` of `0xFFFF`, of een
/// transactie die niet afrondt, is een lege of afwezige poort.
///
/// Let op: veel PHY's antwoorden óók op adres 0 (broadcast). Voor lezen
/// maakt dat niet uit, voor schrijven wel; het board kiest daarom het adres
/// uit zijn device tree als dat hetzelfde id geeft ([`answers_as`]).
pub fn scan<M: Mdio + ?Sized>(m: &mut M) -> Option<Phy> {
    (0..PHY_ADDRS).find_map(|addr| {
        let id1 = m.read(addr, reg::ID1).ok()?;
        if id1 == 0 || id1 == 0xFFFF {
            return None;
        }
        let id2 = m.read(addr, reg::ID2).ok()?;
        Some(Phy { addr, id1, id2 })
    })
}

/// Antwoordt `addr` met hetzelfde id als `found`? Voor de keuze tussen het
/// broadcast-adres dat de scan vond en het adres uit de device tree.
pub fn answers_as<M: Mdio + ?Sized>(m: &mut M, addr: u8, found: &Phy) -> bool {
    matches!(
        (m.read(addr, reg::ID1), m.read(addr, reg::ID2)),
        (Ok(a), Ok(b)) if a == found.id1 && b == found.id2
    )
}

/// Start de autonegotiatie, zonder te wachten: adverteert 10/100 half en
/// full, met `gigabit` ook 1000 full, en herstart hem. De link komt later,
/// en [`link`] ziet hem.
///
/// Zonder `gigabit` (de ePHY van de SG2002 op de LicheeRV Nano) worden GBCR
/// en GBSR (register 9 en 10) niet aangeraakt: die bestaan op zo'n PHY niet,
/// en op bring-up-silicium schrijven we niets waarvan we de betekenis niet
/// kennen.
pub fn start_autoneg<M: Mdio + ?Sized>(m: &mut M, phy: u8, gigabit: bool) -> Result {
    m.write(phy, reg::ANAR, ANAR_10_100)?;
    if gigabit {
        m.write(phy, reg::GBCR, GBCR_1000_FD)?;
    }
    m.write(phy, reg::BMCR, BMCR_AN_ENABLE | BMCR_AN_RESTART)
}

/// Eén blik op de link (`genphy_update_link` plus `genphy_read_status`):
/// `None` zolang BMSR niet autonegotiatie klaar én link zegt, anders de
/// snelheid uit GBSR (1000 full, alleen met `gigabit`) en anders ANLPAR.
/// Het link-bit is latched-low: een link die even weg was, leest één keer
/// laag, en de volgende blik ziet hem.
pub fn link<M: Mdio + ?Sized>(m: &mut M, phy: u8, gigabit: bool) -> Result<Option<Link>> {
    let s = m.read(phy, reg::BMSR)?;
    if s & (BMSR_AN_COMPLETE | BMSR_LINK) != BMSR_AN_COMPLETE | BMSR_LINK {
        return Ok(None);
    }
    if gigabit && m.read(phy, reg::GBSR)? & GBSR_LP_1000_FD != 0 {
        return Ok(Some(Link {
            mbps: 1000,
            full_duplex: true,
        }));
    }
    Ok(Some(link_from_lpa(m.read(phy, reg::ANLPAR)?)))
}

/// De link uit ANLPAR: de beste gemeenschappelijke stand onder gigabit.
#[must_use]
pub fn link_from_lpa(l: u16) -> Link {
    let (mbps, full_duplex) = if l & LPA_100_FD != 0 {
        (100, true)
    } else if l & LPA_100_HD != 0 {
        (100, false)
    } else if l & LPA_10_FD != 0 {
        (10, true)
    } else {
        (10, false)
    };
    Link { mbps, full_duplex }
}
