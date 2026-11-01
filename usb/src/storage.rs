//! Opslag op USB, gezien vanaf de rest van de node.
//!
//! De bus heeft één eigenaar (de taak van de [`Manager`]) en de xhci-driver
//! is daarom bewust `&mut self`. Een drive-driver die een sector wil, praat
//! dus niet met het apparaat maar met die eigenaar: hij stuurt een
//! [`BulkReq`] (in de binary via een `sync::mpsc::Mailbox<BulkReq, N>`
//! die de eigenaar-taak leegt met [`Manager::enqueue`]) en krijgt het
//! antwoord via [`Sink::bulk_done`], precies één keer per aangenomen
//! verzoek. De bytes zelf komen en gaan via [`Sink::bulk_out`] en
//! [`Sink::bulk_in`]: de binary houdt de buffer van elk verzoek vast (per
//! drive één, want de optical-driver serialiseert zijn commando's) en wekt de
//! wachtende drive met een `Signal` in `bulk_done`. Geen slot om de
//! controller heen: er is maar één taak die hem aanraakt, en dat is de hele
//! garantie.
//!
//! Wat Go's `Bulk` met zijn `gone`-kanaal was, is hier een [`BulkId`] met een
//! generatie plus [`Sink::storage_gone`]: een handvat van een drive die weg
//! is, krijgt [`BulkError::Gone`], ook als er op dezelfde poort intussen een
//! nieuwe drive zit.

use crate::{Ctl, Manager, Sink, Timer};
use core::fmt;
use driver_xhci::{BulkTd, Device};

/// Hoeveel verzoeken er per controller mogen wachten. Eén lopende transfer
/// per controller, en de optical-driver heeft er per drive één tegelijk; acht
/// is dus ruim voor elke drive die fysiek aan een controller past.
pub const QUEUE_DEPTH: usize = 8;

/// Hoe lang één bulk-transfer mag duren als de aanroeper zelf geen deadline
/// zet. Een optische drive is traag op een manier die niets met de bus te
/// maken heeft: na een disc-wissel spint hij op en kan een READ seconden
/// stil liggen. Tien seconden is ruim boven wat een drive nodig heeft.
pub const BULK_TIMEOUT_NS: u64 = 10_000_000_000;

/// Eén opslagapparaat als handvat voor andere taken. Samen met de naam van
/// de controller is `(host, port)` het fysieke adres, zodat `disc0` in een
/// jobspec terug te vinden is op een poort aan de achterkant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BulkId {
    /// De index van de controller in de manager.
    pub host: u8,
    /// De roothub-poort.
    pub port: u8,
    generation: u32,
}

impl BulkId {
    pub(crate) fn new(host: u8, port: u8, generation: u32) -> Self {
        Self {
            host,
            port,
            generation,
        }
    }
}

/// Wat de sink over een nieuw opslagapparaat hoort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkInfo {
    /// Het handvat.
    pub id: BulkId,
    /// idVendor.
    pub vendor_id: u16,
    /// idProduct.
    pub product_id: u16,
    /// De naam van de controller.
    pub host: &'static str,
    /// De roothub-poort.
    pub port: u8,
    /// Wat één OUT of IN in één keer kan dragen: de bouncebuffer van de
    /// controller. De optical-driver knipt zijn opdrachten hierop.
    pub max_transfer: usize,
}

/// Wat een verzoek doet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkOp {
    /// Bytes naar de BULK-OUT endpoint.
    Out,
    /// Tot `len` bytes van de BULK-IN endpoint; korter mag.
    In,
    /// De BOT-reset: commandostaat weg, beide endpoints vrij.
    Reset,
}

/// Eén verzoek aan de eigenaar van de bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkReq {
    /// Het apparaat.
    pub id: BulkId,
    /// Wat.
    pub op: BulkOp,
    /// Hoeveel bytes.
    pub len: usize,
    /// Wanneer de eigenaar opgeeft (monotone nanoseconden).
    pub deadline_ns: u64,
    /// Van de aanroeper: waarmee hij in de sink zijn buffer en zijn antwoord
    /// terugvindt.
    pub tag: u32,
}

impl BulkReq {
    /// Maakt een verzoek zoals Go's `Bulk.call` dat deed, aan de kant van de
    /// aanroeper. `Ok(None)`: een lege transfer, die hoeft niet naar de
    /// eigenaar. Een deadline die al verstreken is, bereikt de eigenaar niet
    /// eens. Zonder deadline geldt [`BULK_TIMEOUT_NS`] vanaf `now_ns`.
    pub fn new(
        id: BulkId,
        op: BulkOp,
        len: usize,
        deadline_ns: Option<u64>,
        now_ns: u64,
        tag: u32,
    ) -> Result<Option<Self>, BulkError> {
        if len == 0 && op != BulkOp::Reset {
            return Ok(None);
        }
        let deadline_ns = match deadline_ns {
            None => now_ns.saturating_add(BULK_TIMEOUT_NS),
            Some(d) if now_ns >= d => return Err(BulkError::DeadlineExceeded),
            Some(d) => d,
        };
        Ok(Some(Self {
            id,
            op,
            len,
            deadline_ns,
            tag,
        }))
    }
}

/// Waarom een verzoek niet lukte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkError {
    /// Het apparaat is er niet meer: uitgetrokken, of de controller moest
    /// zich herstellen en is daarbij elk apparaat vergeten. Een drive die
    /// terugkomt is een nieuwe [`BulkId`].
    Gone,
    /// De deadline was al voorbij voor het verzoek vertrok.
    DeadlineExceeded,
    /// De drive zweeg tot de deadline; alleen zijn endpoint ging terug naar
    /// nul, de controller en alles wat er verder aan hangt lopen door.
    TimedOut,
    /// De rij van deze controller is vol.
    Busy,
    /// De driver weigerde of de transfer faalde.
    Xhci(driver_xhci::Error),
}

impl fmt::Display for BulkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("usb: device is gone"),
            Self::DeadlineExceeded => f.write_str("usb: deadline exceeded"),
            Self::TimedOut => f.write_str("usb: bulk transfer timed out: deadline exceeded"),
            Self::Busy => write!(f, "usb: bulk queue full ({QUEUE_DEPTH})"),
            Self::Xhci(e) => e.fmt(f),
        }
    }
}

/// De ene bulk-transfer die op een controller loopt. Eén per controller,
/// want de bouncebuffer is per controller gedeeld: twee drives op dezelfde
/// controller wachten dus netjes op elkaar.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inflight {
    pub(crate) req: BulkReq,
    /// `None` alleen in de tests: een transfer die nooit de ring op ging.
    pub(crate) td: Option<BulkTd>,
}

impl<T: Timer> Manager<T> {
    /// Zet een verzoek achteraan de rij van zijn controller. Een verzoek
    /// voor een controller die er niet is, krijgt meteen
    /// [`BulkError::Gone`]; een volle rij [`BulkError::Busy`].
    pub fn enqueue(&mut self, req: BulkReq, sink: &mut impl Sink) {
        let Some(c) = self.ctls.get_mut(usize::from(req.id.host)) else {
            sink.bulk_done(&req, Err(BulkError::Gone));
            return;
        };
        if c.queue.push(req).is_err() {
            sink.bulk_done(&req, Err(BulkError::Busy));
        }
    }

    /// Het bulk-deel van één ronde: lopende transfers afmaken of laten
    /// verlopen, en daarna per vrije controller het volgende verzoek starten.
    pub async fn serve_bulk(&mut self, sink: &mut impl Sink) {
        let t = &self.timer;
        let now = t.now();
        let mut started = false;
        for c in self.ctls.iter_mut() {
            finish(c, now, sink, t).await;
            while c.busy.is_none()
                && let Some(r) = c.queue.remove(0)
            {
                started |= start(c, r, sink, t).await;
            }
        }
        if started {
            self.fine = crate::FINE_ROUNDS;
        }
    }
}

/// Maakt de lopende transfer van `c` af, of laat hem verlopen.
async fn finish(c: &mut Ctl, now: u64, sink: &mut impl Sink, t: &impl Timer) {
    let Some(f) = c.busy else {
        return;
    };
    let r = match f.td {
        Some(td) => c.hc.poll_bulk(&td, sink.bulk_in(&f.req), t).await,
        None => Ok(None),
    };
    let reply = match r {
        Ok(None) if now < f.req.deadline_ns => return,
        Ok(None) => {
            // De drive zwijgt. Alleen zijn endpoint gaat terug naar nul; de
            // controller en alles wat er verder aan hangt lopen door.
            if let Some(td) = f.td
                && let Err(e) = c.hc.abort_bulk(&td, t).await
            {
                sink.log(format_args!(
                    "usb: {} port {}: aborting a bulk transfer: {e}",
                    c.hc.name(),
                    f.req.id.port
                ));
            }
            Err(BulkError::TimedOut)
        }
        Ok(Some(n)) => Ok(n),
        Err(e) => Err(BulkError::Xhci(e)),
    };
    c.busy = None;
    sink.bulk_done(&f.req, reply);
}

/// Het apparaat achter `id`, als het er nog is.
fn device_of(c: &Ctl, id: BulkId) -> Option<Device> {
    c.known
        .iter()
        .find(|k| k.bulk == Some(id))
        .and_then(|k| k.dev)
}

/// Voert één verzoek uit. Een BOT-reset is een handvol control transfers en
/// commando's met hun eigen korte timeouts en loopt dus meteen door; een
/// datatransfer wordt alleen gestart en in latere rondes afgemaakt. Geeft
/// terug of er een transfer op de ring ging.
async fn start(c: &mut Ctl, r: BulkReq, sink: &mut impl Sink, t: &impl Timer) -> bool {
    let Some(d) = device_of(c, r.id) else {
        sink.bulk_done(&r, Err(BulkError::Gone));
        return false;
    };
    let td = match r.op {
        BulkOp::Reset => {
            let res = c.hc.reset_recovery(&d, t).await.map(|()| 0);
            sink.bulk_done(&r, res.map_err(BulkError::Xhci));
            return false;
        }
        BulkOp::In => c.hc.start_bulk_in(&d, r.len),
        BulkOp::Out => {
            let data = sink.bulk_out(&r);
            let data = data.get(..r.len).unwrap_or(data);
            c.hc.start_bulk_out(&d, data)
        }
    };
    match td {
        Ok(td) => {
            c.busy = Some(Inflight {
                req: r,
                td: Some(td),
            });
            true
        }
        Err(e) => {
            sink.bulk_done(&r, Err(BulkError::Xhci(e)));
            false
        }
    }
}

/// Meldt een apparaat af: de sink hoort dat het weg is, en elk verzoek dat
/// nog op hem wachtte krijgt [`BulkError::Gone`]. Vóór `detach` of een
/// controllerreset, want daarna kan er van zijn slot niets meer komen.
pub(crate) fn drop_bulk(c: &mut Ctl, id: BulkId, sink: &mut impl Sink) {
    sink.storage_gone(id);
    if let Some(f) = c.busy
        && f.req.id == id
    {
        c.busy = None;
        sink.bulk_done(&f.req, Err(BulkError::Gone));
    }
    let mut i = 0;
    while let Some(r) = c.queue.get(i).copied() {
        if r.id == id {
            c.queue.remove(i);
            sink.bulk_done(&r, Err(BulkError::Gone));
        } else {
            i += 1;
        }
    }
}
