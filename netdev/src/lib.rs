//! Het contract tussen een NIC-driver en de rest van de kern.
//!
//! Een driver levert frames en neemt frames aan; wie hem drijft (de
//! RX-pomp en de switch in `net`, of de app-netstack in `applib`) kent
//! alleen dit. De trait is met opzet klein: batching van doorbells
//! (`flush`) en het wek-signaal van de interrupt zijn de twee dingen die
//! de Go-kern na meting nodig bleek te hebben (de RX-lus: één doorbell per
//! burst, en de 10 ms-vangrail op een verloren flank).
//!
//! Met een lijn is het ritme dat van NAPI, en het is voor elke driver
//! hetzelfde: de dispatch van het board roept [`IrqAck::ack`] vóór de EOI
//! (de lijn valt, het masker gaat dicht), luidt de bel, en de pomp leest de
//! ring tot [`Device::receive`] `None` geeft. Pas dán gaat de lijn weer open
//! (`rearm`), in `receive` zelf, gevolgd door nog één blik op de ring, zodat
//! een frame dat tussen de lege lees en het openen binnenkwam niet tot de
//! vangrail blijft liggen (`napi_complete_done`, dan de irq aan, dan nog
//! eens kijken).
//!
//! Op dezelfde lijn zit de TX-completion: een descriptor die de NIC
//! vrijgeeft, trekt de lijn zoals een ontvangen frame, en de ack en de
//! rearm nemen hem mee. Zo wekt een volle ring die leegloopt de pomp
//! (Linux: de TX-clean in de NAPI-poll, `netif_wake_queue`). De blik na het
//! openen doet de pomp voor TX zelf: hij zendt ná de RX-ronde.

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

use core::cell::Cell;
use core::fmt;
use sync::{Local, Signal};

/// De MTU van het interne net en van de uplink.
pub const MTU: usize = 1500;

/// De grootste frame die een driver hoeft te dragen: MTU plus de
/// Ethernet-kop en een VLAN-tag, naar boven afgerond op een cacheline.
pub const MAX_FRAME: usize = 1536;

/// Een MAC-adres.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Mac(pub [u8; 6]);

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}

/// Waarom een frame niet verzonden is.
#[derive(Debug, PartialEq, Eq)]
pub enum TxError {
    /// De TX-ring zit vol; de TX-completion op de lijn wekt de pomp voor
    /// een nieuwe poging.
    Full,
    /// Het frame past niet (langer dan [`MAX_FRAME`] of leeg).
    Size(usize),
    /// Het device is weg of stuk; niets helpt meer.
    Dead,
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("tx ring full"),
            Self::Size(n) => write!(f, "frame of {n} bytes does not fit"),
            Self::Dead => f.write_str("device gone"),
        }
    }
}

/// De meetlat van een driver, voor de tik van de kern (`nic(...)`).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    /// RX-descriptors die afgekeurd en zonder kopie teruggegeven zijn:
    /// foutframe, gesplitst frame, onmogelijke lengte.
    pub rx_bad: u64,
    /// Keren dat `transmit` een volle ring vond.
    pub tx_full: u64,
    /// Doorbells naar de NIC.
    pub doorbells: u64,
}

/// Het interrupt-pad van een NIC, los van de ringen: wat de dispatch van
/// het board vóór de EOI roept. `Copy`, zodat het board hem naast de
/// driver houdt (de pomp bezit de driver zelf).
pub trait IrqAck: Copy {
    /// Laat de lijn los en houdt hem dicht tot de driver hem in
    /// [`Device::receive`] heropent. Wat dat per chip is (masker, status,
    /// mailbox), staat bij de driver.
    fn ack(&self);
}

/// De ack van de NIC voor de dispatch van het board: gezet vóór de lijn
/// scherp gaat, gelezen in de dispatch. Alleen de executor van de
/// kern-core raakt hem aan (zie [`Local`]).
pub struct AckSlot<A>(Local<Cell<Option<A>>>);

impl<A: IrqAck> AckSlot<A> {
    /// Een lege; voor een `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self(Local::new(Cell::new(None)))
    }

    /// Zet de ack; vóór de lijn bij de controller scherp gaat.
    pub fn set(&self, ack: A) {
        self.0.get().set(Some(ack));
    }

    /// De ack, als die gezet is (uit de dispatch).
    pub fn ack(&self) {
        if let Some(a) = self.0.get().get() {
            a.ack();
        }
    }
}

impl<A: IrqAck> Default for AckSlot<A> {
    fn default() -> Self {
        Self::new()
    }
}

/// Een NIC zoals de kern hem ziet.
pub trait Device {
    /// Zet één frame op de TX-ring. De doorbell mag wachten tot [`flush`].
    ///
    /// [`flush`]: Device::flush
    fn transmit(&mut self, frame: &[u8]) -> Result<(), TxError>;

    /// Haalt één ontvangen frame op in `buf`; `None` als de RX-ring leeg is.
    /// Met een lijn heropent de driver hem bij `None` en kijkt hij daarna
    /// nog één keer (zie de crate-tekst).
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize>;

    /// Publiceert uitgestelde doorbells (TX en RX): één keer per burst,
    /// niet per frame.
    fn flush(&mut self) {}

    /// Het MAC-adres.
    fn mac(&self) -> Mac;

    /// Het wek-signaal van de NIC-interrupt, als het board er een bedraadt.
    /// `None` = pollen, met de microslaap uit de Go-kern.
    fn irq(&self) -> Option<&'static Signal> {
        None
    }

    /// De meetlat; nul voor een driver zonder.
    fn stats(&self) -> Stats {
        Stats::default()
    }
}
