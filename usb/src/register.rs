//! De naad tussen board en dienst: de controllers zoals een board ze
//! aanbiedt, en het opbrengen ervan.
//!
//! De boards zelf kennen deze crate niet, dus de bedrading gebeurt in de
//! binary: die zet in een [`Registry`] wat
//! het board met `Board::usb_hosts` als xHCI aanbiedt (Go's
//! `usb_<board>.go` met `Register`) en roept daarna [`Registry::bring_up`].
//! Een `Registry` is gewone boot-staat in de binary, geen globale lijst:
//! eenmalige initialisatie gebeurt vóór de eerste `spawn`.
//!
//! Wat hier sinds 29-09 niet meer staat: de `Prepare`-haak per controller.
//! Een board brengt zijn PCIe-link en zijn BAR's zelf op voordat het een
//! venster noemt (het weet dan het adres al), en de enige stap die bij de
//! controller zelf hoort (de DWC3-core van de RK3566 in hostmodus) doet de
//! binary in zijn `make`, met het venster erbij. Een kale `fn()` zonder dat
//! venster kon de O6N-hosts alleen via gedeelde staat voorbereiden (de open
//! vraag van docs/gui.md).

use crate::{MAX_HOSTS, Manager, Sink, Timer};
use bounded::{BoundedVec, Full};
use core::fmt;
use dev::Pa;
use driver_xhci::Hc;

/// Waarom de voorbereiding van een controller faalde.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrepareError {
    /// Wat er misging, in het Engels.
    pub what: &'static str,
    /// Het getal erbij (een register, een adres); 0 als er geen is.
    pub value: u64,
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:#x})", self.what, self.value)
    }
}

/// Eén controller zoals een board hem aanbiedt.
#[derive(Clone, Copy, Debug)]
pub struct HostSpec {
    /// Komt in élke logregel van deze controller. Een node met drie
    /// controllers moet leesbaar zijn.
    pub name: &'static str,
    /// Het xHCI-capabilityvenster. Vast op een SoC, een BAR achter PCIe;
    /// nul = het board vond hem niet (de regel zegt het).
    pub base: Pa,
    /// Wat de controller bij een CPU-fysiek adres optelt (zie
    /// `driver_xhci::Hc`): nul op een SoC, 0x10_0000_0000 achter de RP1 van
    /// de Pi 5.
    pub bus_off: u64,
    /// Het stuk DMA-geheugen van déze controller (Normal-NC, buiten elke
    /// RAM-declaratie). Elke controller krijgt zijn eigen stuk, want ze
    /// draaien tegelijk en delen niets; maat nul = het board plande er geen.
    pub dma: Pa,
    /// De maat van dat stuk.
    pub dma_size: u64,
}

/// De controllers van deze node, in registratievolgorde.
#[derive(Debug, Default)]
pub struct Registry {
    hosts: BoundedVec<HostSpec, MAX_HOSTS>,
}

impl Registry {
    /// Een lege lijst.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hosts: BoundedVec::new(),
        }
    }

    /// Meldt een controller aan. Alleen bij boot, vóór [`Registry::bring_up`].
    pub fn register(&mut self, h: HostSpec) -> Result<(), Full<HostSpec>> {
        self.hosts.push(h)
    }

    /// De aangemelde controllers.
    #[must_use]
    pub fn hosts(&self) -> &[HostSpec] {
        &self.hosts
    }

    /// Brengt alle aangemelde controllers op en geeft terug hoeveel er
    /// draaien. Geen harde eis: een node zonder werkende USB draait door,
    /// alleen typ je er niet op. Elke controller die het niet doet is één
    /// logregel, en die regel ís de meting, want dit pad is per bord anders
    /// bedraad.
    ///
    /// `make` maakt de [`Hc`] voor een venster: de eventuele voorbereiding
    /// (de DWC3 in hostmodus, die zelf 225 ms op de timer slaapt) en de
    /// `unsafe Hc::new`, die het adres vertrouwt en dus van de binary is.
    ///
    /// Twee rondes. Eerst gaat élke controller die er is naar halt (probe
    /// en reset), en pas daarna wist de start van de eerste zijn stuk
    /// DMA-geheugen: een controller die de firmware liet lopen, of de vorige
    /// kern na een flip met een andere verdeling van de regio, mag niet
    /// schrijven in een stuk dat een ander net opbouwt (Go deed dit alleen
    /// voor de O6N, in een `prepare` met gedeelde staat, 18-09).
    ///
    /// Geeft dit nul, dan trekt de binary het invoeradres van de fb-grant
    /// weer in: een display die het veld niet ziet, weet dat er niets te
    /// bellen valt.
    pub async fn bring_up<T: Timer>(
        &self,
        mgr: &mut Manager<T>,
        sink: &mut impl Sink,
        mut make: impl AsyncFnMut(&HostSpec) -> Result<Hc, PrepareError>,
    ) -> usize {
        if self.hosts.is_empty() {
            return 0;
        }
        let mut made: BoundedVec<(HostSpec, Hc), MAX_HOSTS> = BoundedVec::new();
        for h in self.hosts.iter() {
            if h.dma_size == 0 {
                sink.log(format_args!(
                    "usb: {}: this board planned no USB DMA region, skipped",
                    h.name
                ));
                continue;
            }
            if h.base.0 == 0 {
                sink.log(format_args!("usb: {}: no register window, skipped", h.name));
                continue;
            }
            let mut hc = match make(h).await {
                Ok(hc) => hc,
                Err(e) => {
                    sink.log(format_args!("usb: {}: {e}", h.name));
                    continue;
                }
            };
            // Stil zetten; een fout hier meldt `add` hieronder met zijn
            // eigen woorden.
            if hc.probe().is_ok() {
                let _ = hc.reset(&mgr.timer).await;
            }
            // Vol kan niet: `made` is zo groot als `hosts`.
            let _ = made.push((*h, hc));
        }
        let mut live = 0;
        while let Some((h, hc)) = made.remove(0) {
            if let Err(e) = mgr.add(hc, h.dma, h.dma_size, sink).await {
                sink.log(format_args!("usb: {}: {e}", h.name));
                continue;
            }
            live += 1;
        }
        if live == 0 {
            sink.log(format_args!("usb: no working controller on this node"));
        }
        live
    }
}
