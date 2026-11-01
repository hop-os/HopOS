//! De HID-kant van de manager (feature `hid`, de gui-smaak): de decoders per
//! poort, de pollronde die de rapporten ophaalt, en het loslaten van wat
//! een apparaat ingedrukt hield. Media zonder gui bouwt dit niet: daar kijkt
//! de manager alleen voor de scan en een lopende bulk-transfer naar de bus.

use crate::{Ctl, Sink, Timer};
use driver_hid::{Events, Keyboard, Mouse};
use driver_xhci::PROTO_MOUSE;

/// Hoe vaak we de event-ring bekijken. Een boot-toetsenbord meldt zich elke
/// 8-10ms; 4ms is dus ruim binnen de aanslagsnelheid en kost per beurt een
/// handvol MMIO-reads. Een poll met een reden: docs/description.md §4.11.
pub const POLL_INTERVAL_NS: u64 = 4_000_000;

/// Hoeveel rapporten we per beurt van één apparaat ophalen. Eén apparaat kan
/// twee endpoints hebben (een combo-dongle levert toetsenbord én muis), dus
/// één per beurt zou de muis halveren; ongebrensd zou een ratelend apparaat
/// de andere poorten kunnen uithongeren.
const MAX_PER_POLL: usize = 4;

/// De rapportbuffer per beurt: een boot-rapport is 8 bytes, een muis met
/// wiel 4; wat een apparaat meer stuurt, lezen we niet.
const REPORT_BUF: usize = 16;

/// De decoders van één poort.
#[derive(Debug, Default)]
pub(crate) struct Decoders {
    pub(crate) kb: Keyboard,
    pub(crate) ms: Mouse,
}

/// De HID-kant van de manager: de hergebruikte buffer, zodat het pollpad
/// niet alloceert.
pub(crate) struct Hid {
    pub(crate) evs: Events,
}

impl Hid {
    pub(crate) fn new() -> Self {
        Hid { evs: Events::new() }
    }

    /// Laat alles los wat `d` ingedrukt hield. Geen opruimwerk maar een
    /// correctie: een toets die tijdens het uittrekken ingedrukt was, moet
    /// bij de display worden losgelaten, anders blijft hij daar voor altijd
    /// staan.
    pub(crate) fn release(&mut self, d: &mut Decoders, sink: &mut impl Sink) {
        self.evs.clear();
        d.kb.reset(&mut self.evs);
        d.ms.reset(&mut self.evs);
        emit(&mut self.evs, sink);
    }

    /// Haalt één ronde rapporten op.
    pub(crate) async fn poll(&mut self, ctls: &mut [Ctl], sink: &mut impl Sink, t: &impl Timer) {
        let mut buf = [0u8; REPORT_BUF];
        for c in ctls.iter_mut() {
            // Een Disable Slot zonder completion maakt ook de
            // command/event-state verdacht. Poll daarom geen enkel oud
            // handvat meer tussen die fout en de controllerreset in de
            // volgende scanronde.
            if c.hc.recovery_needed().is_some() {
                continue;
            }
            for k in c.known.iter_mut() {
                let Some(d) = k.dev else {
                    continue; // bekend niet-HID-apparaat
                };
                if d.is_mass_storage() {
                    continue; // geen invoer: deze poort levert bytes op verzoek
                }
                // Meerdere keren per beurt: één apparaat kan twee endpoints
                // hebben en `report` levert er één per aanroep.
                for _ in 0..MAX_PER_POLL {
                    let Some(r) = c.hc.report(&d, &mut buf, t).await else {
                        break;
                    };
                    let rep = buf.get(..r.len).unwrap_or_default();
                    if r.proto == PROTO_MOUSE {
                        k.hid.ms.decode(rep, &mut self.evs);
                    } else {
                        k.hid.kb.decode(rep, &mut self.evs);
                    }
                    emit(&mut self.evs, sink);
                }
            }
        }
    }
}

/// Geeft de gebeurtenissen aan de sink en leegt de buffer.
pub(crate) fn emit(evs: &mut Events, sink: &mut impl Sink) {
    for e in evs.iter() {
        sink.input(*e);
    }
    evs.clear();
}
