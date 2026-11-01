//! De orchestrator-mechanismen van HopOS: slots en partities, de kooi, de
//! system-API, hopfs, de kern-flip en de console.
//!
//! Deze crate BEZIT de boekhouding van de node: welke partitie van wie is,
//! welke core welke bewoner draagt, welke servicer bij welk slot hoort, de
//! boom van hopfs. Hij bezit GEEN ijzer: alles wat per architectuur of board
//! anders is (een kooi bouwen, een core aanzetten, een blok lezen, een
//! TCP-verbinding) komt binnen als trait, zodat de logica op de
//! ontwikkelmachine test met nep-implementaties en `cpu`, `board` en `net` de
//! echte leveren:
//!
//! | Trait | Wie levert hem | Module |
//! | --- | --- | --- |
//! | [`cage::Cage`] | `cpu` (stage-2 op ARM, PMP op RISC-V) | [`cage`] |
//! | [`cage::Cores`] | het board (PSCI, klassen small/mid/big) | [`cage`] |
//! | [`cage::Timer`] | de executor | [`cage`] |
//! | [`cage::PhysMem`] | `dev` via het board | [`cage`] |
//! | [`hopfs::BlockIo`] | de blok-driver achter een `blkdev::Queue` (virtio-blk op QEMU, NVMe op ijzer) | [`hopfs`] |
//! | [`system::Conn`] | `leannet` | [`system`] |
//!
//! De specificatie is de Go-kern in `OLD/metal/kern`; de regels E1 tot E9
//! uit `OLD/docs/lifecycle.md` staan hier als typen ([`partmem`]) en tests.

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

pub mod cage;
#[cfg(feature = "media")]
pub mod codecabi;
pub mod conport;
pub mod deviceabi;
mod error;
pub mod grants;
pub mod hopfs;
pub mod kernflip;
pub mod nodecfg;
pub mod partmem;
pub mod pool;
pub mod rpc;
pub mod slots;
pub mod store;
pub mod system;
pub mod watchdog;

#[cfg(test)]
mod testutil;

pub use error::{Error, Result};

/// De harde kooi-bovengrens: het hoogste slotnummer dat de kern ooit kent
/// (128, de Ampere Altra); een board zet zijn eigen, lagere `max_slots` bij
/// boot.
pub use abi::layout::SLOT_CAP;

mod slotvec;
pub use slotvec::SlotVec;

/// De hoogste logische core die de kern ooit kent.
pub const CORE_CAP: usize = 256;

/// De blokkorrel van de partities: 2 MiB, omdat de stage-2-partitieblokken
/// 2 MiB zijn.
pub const GRAIN: u64 = 2 << 20;

/// Een slotnummer (= kooinummer), 1 tot en met [`abi::layout::SLOT_MAX`]
/// (de zestien bits van het slot-LAN): het type van het contract, want kern
/// en app tellen dezelfde slots.
pub use abi::layout::Slot;

/// Een logisch corenummer (1 tot en met [`CORE_CAP`]); het fysieke nummer
/// komt uit [`cage::Cores::phys`].
///
/// De nummers zijn logisch en aaneengesloten: een afspraak van de kern, geen
/// uitspraak over het silicium (Apple's HOP-core zit midden in de cpu-lijst,
/// een RISC-V-board levert hart-ID's met gaten).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Core(u16);

impl Core {
    /// De OS-core: logische core 0, waar de kern zelf woont en die hij deelt
    /// met Hop en met de sharegroups die hem mogen delen (PORT.md beslissing
    /// 2). Geen app-core: [`Core::new`] geeft hem nooit, zodat een getal van
    /// buiten hem niet kan aanwijzen; alleen de plaatsing kiest hem
    /// ([`crate::pool::Places::share_os_core`]).
    pub const OS: Core = Core(0);

    /// Core `c`, of `None` buiten `1..=CORE_CAP`.
    #[must_use]
    pub const fn new(c: usize) -> Option<Core> {
        if c >= 1 && c <= CORE_CAP {
            // INVARIANT: bereik net getoetst; CORE_CAP past in een u16.
            Some(Core(c as u16))
        } else {
            None
        }
    }

    /// Het nummer.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// De core-run van een kooi: deze core en de `span - 1` erna. Anders dan
    /// `(c..c + span).filter_map(Core::new)` laat dit [`Core::OS`] niet
    /// weg: een stop die de OS-core oversloeg, zag een bewoner daar meteen
    /// als stil.
    pub fn run(self, span: usize) -> impl Iterator<Item = Core> {
        core::iter::once(self)
            .chain((self.get() + 1..self.get() + span.max(1)).filter_map(Core::new))
    }
}

impl core::fmt::Display for Core {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Een fysiek bereik `[base, base + size)`: dat van het contract (een
/// partitie, een venster en het PA-plan zijn hetzelfde begrip).
pub use abi::Region;

/// Rondt `n` op naar de [`GRAIN`], of `None` bij omloop.
#[must_use]
pub const fn align_grain(n: u64) -> Option<u64> {
    match n.checked_add(GRAIN - 1) {
        Some(v) => Some(v & !(GRAIN - 1)),
        None => None,
    }
}

/// Schrijft `e` in `buf`, afgekapt; geeft de lengte. De fouttekst van een
/// antwoord (de system-API, hopfs, de codec-dienst).
pub(crate) fn fmt_into(buf: &mut [u8], e: &impl core::fmt::Display) -> usize {
    struct W<'b>(&'b mut [u8], usize);
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for b in s.bytes() {
                if let Some(x) = self.0.get_mut(self.1) {
                    *x = b;
                    self.1 += 1;
                }
            }
            Ok(())
        }
    }
    let mut w = W(buf, 0);
    let _ = core::fmt::write(&mut w, format_args!("{e}"));
    w.1
}
