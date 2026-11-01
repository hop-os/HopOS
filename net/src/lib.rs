//! Het netwerkvlak van de kern: de switch (L2 over de frame-ringen), NAT, de
//! host-poort en de RX-pomp.
//!
//! De vorm (rustdoc/README.md §1, PORT.md §3 en §4):
//!
//! - [`switch::Switch`] is één actor. Hij bezit de poorten, de NAT-tabel en
//!   de switch-kant van elke ring; `Attach`/`Detach` zijn berichten
//!   ([`switch::Command`]). Hij is de enige producer van elke RX-ring en de
//!   enige consument van elke TX-ring, en de enige die de uplink voedt.
//! - [`switch::Published::pending`] leest een gepubliceerde tabel met kale
//!   loads, voor de deur van de executor (Go: `switchPending`).
//! - [`pump`] drijft de NIC: RX de switch in, TX van de switch de draad op,
//!   via twee eigen rijen.
//! - [`host`] is HOP's poort 0: de node-stack achter de [`host::HostStack`]
//!   -trait, met de gateway-vertaling van het interne subnet op de naad.
//!
//! Wat hier niet staat: de node-stack zelf (die komt uit `leannet`), DHCP en
//! SNTP.

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
#![forbid(unsafe_code)]

extern crate alloc;

mod flows;
pub mod gw;
pub mod host;
pub mod nat;
pub mod nodemac;
pub mod plan;
pub mod pump;
pub mod ring;
pub mod switch;
mod wire;

pub use flows::{MAX_FLOWS, MAX_FLOWS_PER_SLOT};

use core::fmt;
use core::sync::atomic::AtomicU64;

/// Wat er in het netwerkvlak mis kan gaan.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Error {
    /// Een tabel kon bij boot niet gealloceerd worden (bytes).
    OutOfMemory(usize),
    /// Een slotnummer buiten 1..=max.
    SlotRange(usize),
    /// Poort 0 is geen poort.
    PortZero,
    /// De node-poort staat al gepubliceerd, door dit slot.
    AlreadyPublished {
        /// De node-poort.
        port: u16,
        /// Het slot dat hem heeft.
        slot: usize,
    },
    /// Een begrensde tabel zit vol: welke, en zijn plafond.
    Full(&'static str, usize),
    /// Een prefixlengte boven 32.
    Prefix(u32),
    /// De brievenbus van de switch zit vol; probeer het na een ronde opnieuw.
    Busy,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory(n) => write!(f, "out of memory allocating {n} bytes"),
            Self::SlotRange(s) => write!(f, "slot {s} out of range"),
            Self::PortZero => f.write_str("port 0"),
            Self::AlreadyPublished { port, slot } => {
                write!(f, "port {port} already published by slot {slot}")
            }
            Self::Full(what, cap) => write!(f, "{what} full ({cap})"),
            Self::Prefix(p) => write!(f, "prefix /{p} out of range"),
            Self::Busy => f.write_str("switch mailbox full"),
        }
    }
}

/// Het resultaat van dit vlak.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Een logregel-schrijver: de kern geeft de console; deze crate heeft er
/// geen. Engels, één regel, met marker en getallen (handboek §10).
pub type LogFn = fn(fmt::Arguments<'_>);

/// De meetlat van het netwerkvlak: atomics, zodat elke taak en elke core ze
/// mag lezen zonder de actor te storen.
#[derive(Default)]
pub struct Stats {
    /// Switch-rondes mét werk na een bel.
    pub work_by_door: AtomicU64,
    /// Een RX-ring zat vol (backpressure begonnen).
    pub rx_full: AtomicU64,
    /// Frames gedropt op een volle RX-ring.
    pub rx_drops: AtomicU64,
    /// Een frame groter dan de uplink draagt.
    pub nat_oversize: AtomicU64,
    /// Uitgaand naar een onbekende buur zonder gateway: gedropt.
    pub nat_no_route: AtomicU64,
    /// Geen flow voor een uitgaand pakket (plafond, RST zonder flow,
    /// adoptievenster, uitgeputte poortpool). Zonder deze twee is "de SYN
    /// kwam nooit aan" niet te scheiden van "de server antwoordde niet"
    /// (het beeld dat op 20-09 op drie boards als client opdook).
    pub nat_flow_full: AtomicU64,
    /// Inbound van buiten dat een masq-flow raakte en naar het slot ging:
    /// de SYN-ACK's en de rest van elke uitgaande verbinding. Samen met
    /// `nat_in_unmatched` scheidt dit "de SYN-ACK kwam nooit aan" van "hij
    /// kwam aan en het slot deed er niets mee" (de Pi 5 aan het LAN, 30-09:
    /// 5 van 15 connects naar buiten, met noroute en flowfull op nul).
    pub nat_reply_in: AtomicU64,
    /// Inbound aan het node-IP dat geen flow en geen publicatie raakte: dat
    /// ging naar de node-stack (DHCP, en alles wat verdwaald is).
    pub nat_in_unmatched: AtomicU64,
    /// Uplink-frames die de pomp niet in de ingress-rij kwijt kon.
    pub uplink_rx_drops: AtomicU64,
    /// Frames die de switch niet in de egress-rij kwijt kon. Sinds de
    /// stop van de rij ([`switch::Xoff`], 24-10) alleen nog een record dat
    /// meer dan zijn marge aan uplink-frames maakte; hoort 0 te zijn.
    pub uplink_tx_drops: AtomicU64,
    /// Hoe vaak de switch stopte met lezen omdat de egress-rij vol was
    /// (`netif_stop_queue`); de frames bleven in de TX-ringen van de slots.
    pub uplink_xoff: AtomicU64,
    /// Lege rondes van de RX-pomp: hoe vaak HOP keek en niets vond. Gepold
    /// ~3.333/s bij stilte, op de interrupt ~100/s (de vangrail).
    pub rx_idle: AtomicU64,
    /// Frames die de NIC weigerde met een echte fout (geen volle ring):
    /// geteld en weg.
    pub nic_tx_errors: AtomicU64,
    /// Hoe vaak de pomp een frame vasthield omdat de TX-ring van de NIC
    /// vol was, ook na een flush; de volgende ronde probeert hij hem eerst.
    pub nic_tx_held: AtomicU64,
    /// De meetlat van de NIC-driver (`netdev::Stats`), na elke ronde van de
    /// pomp overgenomen: afgekeurde RX-descriptors.
    pub nic_rx_bad: AtomicU64,
    /// Idem: `transmit` op een volle ring.
    pub nic_tx_full: AtomicU64,
    /// Idem: doorbells naar de NIC.
    pub nic_doorbells: AtomicU64,
    /// Frames van de node-stack die niet in de host-TX-ring pasten.
    pub host_tx_drops: AtomicU64,
    /// Frames op de host-naad gedropt: een fysiek LAN-frame met een intern
    /// bronadres (spoof), of een intern frame dat de gateway-vertaling
    /// weigerde.
    pub host_rx_drops: AtomicU64,
    /// Frames van een slot met een vreemd bronadres (MAC of IP niet van
    /// dat slot): gedropt vóór alles. De M4 (01-10) bereikte de kern
    /// nooit; dit scheidt "de switch zag het frame niet" van "hij wees het
    /// af".
    pub slot_src_drops: AtomicU64,
}

impl Stats {
    /// Nul overal; voor een `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            work_by_door: AtomicU64::new(0),
            rx_full: AtomicU64::new(0),
            rx_drops: AtomicU64::new(0),
            nat_oversize: AtomicU64::new(0),
            nat_no_route: AtomicU64::new(0),
            nat_flow_full: AtomicU64::new(0),
            nat_reply_in: AtomicU64::new(0),
            nat_in_unmatched: AtomicU64::new(0),
            uplink_rx_drops: AtomicU64::new(0),
            uplink_tx_drops: AtomicU64::new(0),
            uplink_xoff: AtomicU64::new(0),
            rx_idle: AtomicU64::new(0),
            nic_tx_errors: AtomicU64::new(0),
            nic_tx_held: AtomicU64::new(0),
            nic_rx_bad: AtomicU64::new(0),
            nic_tx_full: AtomicU64::new(0),
            nic_doorbells: AtomicU64::new(0),
            host_tx_drops: AtomicU64::new(0),
            host_rx_drops: AtomicU64::new(0),
            slot_src_drops: AtomicU64::new(0),
        }
    }
}

/// Eén uplink-frame als waarde: zo reist het door de rijen tussen pomp en
/// switch (handboek §1.2: verplaats, deel niet).
#[derive(Clone)]
pub struct Frame {
    len: u16,
    data: [u8; netdev::MAX_FRAME],
}

impl Frame {
    /// Een leeg frame.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            len: 0,
            data: [0; netdev::MAX_FRAME],
        }
    }

    /// Een frame met een kopie van `p`; `None` als het niet past.
    #[must_use]
    pub fn from_slice(p: &[u8]) -> Option<Self> {
        let mut f = Self::new();
        f.set(p).then_some(f)
    }

    /// Vervangt de inhoud; `false` als `p` niet past.
    pub fn set(&mut self, p: &[u8]) -> bool {
        let (Some(d), Ok(n)) = (self.data.get_mut(..p.len()), u16::try_from(p.len())) else {
            return false;
        };
        d.copy_from_slice(p);
        self.len = n;
        true
    }

    /// De hele buffer, om in te ontvangen; zet daarna [`set_len`](Self::set_len).
    pub fn buf_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Zet de lengte na een ontvangst in [`buf_mut`](Self::buf_mut).
    pub fn set_len(&mut self, n: usize) {
        self.len = u16::try_from(n.min(netdev::MAX_FRAME)).unwrap_or(0);
    }

    /// De inhoud.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.data.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// De inhoud, veranderlijk.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.data
            .get_mut(..usize::from(self.len))
            .unwrap_or(&mut [])
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

/// De diepte van de twee uplink-rijen (pomp → switch en switch → pomp): de
/// burst van de switch (64 per poort) plus marge.
pub const UPLINK_QUEUE: usize = 128;

/// De rij waarin de pomp ontvangen uplink-frames aan de switch geeft.
pub type Ingress = sync::spsc::Channel<Frame, UPLINK_QUEUE>;
/// De rij waarin de switch frames voor de draad aan de pomp geeft.
pub type Egress = sync::spsc::Channel<Frame, UPLINK_QUEUE>;
