//! Welke Apple-chip dit image is: de vaste getallen die niet uit de boom
//! mogen of kunnen komen, per chip één tabel, gekozen bij het bouwen
//! (feature `t8152`; zonder is het de t8132 van de M4).
//!
//! Waarom bij het bouwen en niet uit de ADT: de eerste regel op de console
//! komt vóór de MMU en vóór de boom (head.rs, Go 29-08), en de recepten
//! die alleen op de M4 gemeten zijn (de p-states, de SMC, de vaste
//! PCIe-adressen) horen op een andere chip niet stil mee te lopen. De ADT
//! zegt bij de boot welke chip het écht is (`/chosen` `chip-id`); wijkt die
//! af, dan zegt de bootregel het luid (`HOPOS_APPLE_CHIP_MISMATCH`).
//!
//! Afgekeken van m1n1 (`src/soc.h`, `src/smp.c`, `src/pmgr.c`, PR #679
//! "Add basic T8152 (M6) support", gemerged 27-09-2026): de vroege UART van
//! de t8152 op 0x3_3120_0000, de CPU-start op dezelfde PMGR-offset als de
//! t8112-familie, en de PMGR-functies onder `/arm-io/pmgr-child`
//! ([`crate::fwinfo::pmgr_path`]). De dockchannel van de t8152 staat daar
//! niet: die komt uit `HOPOS_APPLE_DOCK` (build.rs), en tot dan uit de ADT
//! zodra die gelezen is ([`crate::console::adopt_adt`]).

include!(concat!(env!("OUT_DIR"), "/dock.rs"));

/// De vaste getallen van één chip.
pub struct Chip {
    /// De SoC ("t8132").
    pub soc: &'static str,
    /// `/chosen` `chip-id` zoals de ADT hem draagt.
    pub chip_id: u32,
    /// De naam van het board voor de kern (`Board::NAME`).
    pub board: &'static str,
    /// De dockchannel voor de eerste regel; 0 = onbekend (dan alleen
    /// [`Chip::uart0`] tot de ADT gelezen is).
    pub dock: u64,
    /// De vroege Samsung-UART (m1n1 `EARLY_UART_BASE`).
    pub uart0: u64,
    /// Zijn de recepten van de M4 hier gemeten: de p-state-tune en zijn
    /// wachter, de SMC, de ANS. Op een nieuwe chip staan ze standaard dicht
    /// (`hopos.disk=on`, `hopos.pstate=E,P`, `hopos.smc=1` zetten ze aan).
    pub measured: bool,
    /// De ethernet-poort, als die op deze chip gemeten is; `None` = de
    /// probe meldt wat de ADT over de APCIe zegt en start geen NIC
    /// ([`crate::pcie::survey`]).
    pub eth: Option<Eth>,
}

/// De vaste adressen van de ethernet-poort (zie `pcie.rs` voor de herkomst
/// van elk getal op de M4).
pub struct Eth {
    /// De ECAM van `/arm-io/apcie` (reg[0]).
    pub ecam: u64,
    /// Het 64-bit prefetch-venster, PCI-adres == CPU-adres.
    pub mmio: u64,
    /// De maat van dat venster.
    pub mmio_size: u64,
    /// Het poortnummer (`pci-bridgeN`), en het busnummer erachter.
    pub port: u8,
    /// Het poortregisterblok.
    pub port_base: u64,
    /// De PERST#-GPIO van de poort (ADT `function-perst`).
    pub perst_pin: u64,
    /// De DART van de poort.
    pub dart: u64,
    /// `/arm-io/gpio0`.
    pub gpio: u64,
}

/// De Mac mini M4: alles GEMETEN 28-08 tot 19-09 (pcie.rs, console.rs).
#[cfg(not(feature = "t8152"))]
pub const CHIP: Chip = Chip {
    soc: "t8132",
    chip_id: 0x8132,
    board: "apple-m4",
    dock: match DOCK_BUILD {
        Some(d) => d,
        None => 0x3_8812_8000,
    },
    uart0: 0x3_ad20_0000,
    measured: true,
    eth: Some(Eth {
        ecam: 0x1c_b000_0000,
        mmio: 0xb_c000_0000,
        mmio_size: 0x2000_0000,
        port: 2,
        port_base: 0x4_9202_8000,
        perst_pin: 165,
        dart: 0x4_9200_0000,
        gpio: 0x3_9a00_0000,
    }),
};

/// De Mac mini M6 (Mac18,5): nog niets gemeten. De UART van m1n1, de
/// dockchannel van het bouwen, de ethernet-poort na de eerste boot (de
/// survey van de probe vult deze tabel).
#[cfg(feature = "t8152")]
pub const CHIP: Chip = Chip {
    soc: "t8152",
    chip_id: 0x8152,
    board: "apple-m6",
    dock: match DOCK_BUILD {
        Some(d) => d,
        None => 0,
    },
    uart0: 0x3_3120_0000,
    measured: false,
    eth: None,
};

const _: () = {
    assert!(CHIP.uart0 != 0 && CHIP.uart0 < 512 << 30);
    assert!(CHIP.dock < 512 << 30);
};
