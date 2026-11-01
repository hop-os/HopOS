//! De invoerdienst van HopOS: bezit de USB-controllers, houdt bij wat er in-
//! en uitgeplugd wordt, en levert toetsaanslagen en muisbewegingen af bij
//! één ontvanger. De Rust-vorm van `OLD/metal/gui/usbin`.
//!
//! WAAROM HOP DE CONTROLLER BEZIT EN NIET EEN APP. Het gui-ontwerp had hier
//! een DeviceGrant staan: de app krijgt het registerblok in zijn kooi en
//! bedient zijn eigen apparaat. Voor GPIO of I²C is dat prima. Voor xHCI
//! niet, en het verschil is DMA: een xHCI-controller is een bus-master die
//! descriptors leest en schrijft op adressen die HIJ krijgt aangereikt. De
//! stage-2 begrenst wat de CPU van een app mag zien, maar niet wat een
//! apparaat namens die app doet; daar is een IOMMU voor nodig, en op dit
//! silicium staat die aantoonbaar uit. Een DeviceGrant op een DMA-capabel
//! blok is dus effectief het hele geheugen. Daarom: HOP leest de rapporten en
//! stuurt de gebeurtenissen door.
//!
//! Wat deze crate BEZIT, in drie delen:
//!
//! - De [`Manager`]: één eigenaar-struct over nul of meer [`Hc`]'s. Scannen,
//!   HID-rapporten ophalen en bulk-verzoeken bedienen gebeurt allemaal in
//!   zijn [`Manager::step`], op één taak. Dat vervangt Go's `Run`-goroutine
//!   met zijn `select` en timer: de binary draait de lus (zie
//!   [`Manager::step`] voor de vorm) en geeft de klok en de slaap
//!   ([`Timer`]). Elke wachtende hardwarestap (een poortreset, een
//!   commando) slaapt op die timer, dus een insteek houdt de executor niet
//!   vast.
//! - De [`deliver`]-logica: de begrensde rij naar de display, de cursor, het
//!   samenvoegen van muisbewegingen en de JSON-regels, zonder socket. De
//!   verbinding zelf (listen op 7879, accept, schrijven met deadline) is van
//!   de binary, via de trait [`deliver::InputConn`].
//! - De [`storage`]-kant: opslagapparaten als handvat ([`BulkId`]) met een
//!   verzoekenrij en één lopende transfer per controller.
//!
//! En [`register`]: de lijst controllers zoals een board ze aanbiedt, en het
//! opbrengen ervan.
//!
//! Wat NIET van deze crate is: de board-voorbereiding van een controller
//! (PCIe-link, de DWC3-core van de RK3566 in hostmodus: dat doet de
//! `prepare` van een [`register::HostSpec`]) en het aanmaken van een [`Hc`]
//! (dat is `unsafe`, want het vertrouwt een registervenster; de binary doet
//! het met het adres uit de layout).

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

use bounded::BoundedVec;
use core::fmt;
use dev::Pa;
use driver_hid::{Event, Events, Keyboard, Mouse};
use driver_xhci::{Device, Hc, PROTO_MOUSE, Poison};

pub use driver_xhci::Timer;

pub mod deliver;
pub mod register;
pub mod storage;

pub use storage::{BulkError, BulkId, BulkInfo, BulkOp, BulkReq};

use storage::Inflight;

/// Hoe vaak we de event-ring bekijken. Een boot-toetsenbord meldt zich elke
/// 8-10ms; 4ms is dus ruim binnen de aanslagsnelheid en kost per beurt een
/// handvol MMIO-reads. Een poll met een reden: docs/description.md §4.11.
pub const POLL_INTERVAL_NS: u64 = 4_000_000;

/// Hoe vaak we naar in- en uitpluggen kijken. Een halve seconde wachten op
/// een toetsenbord dat je net insteekt is niet merkbaar, en het houdt de
/// poortregisters uit de hete lus.
pub const SCAN_INTERVAL_NS: u64 = 500_000_000;

/// Hoe vaak de eigenaar na het starten van een bulk-transfer op 100µs-afstand
/// kijkt voor hij terugvalt op een milliseconde. Een toetsenbord kan een
/// milliseconde missen, bulk-opslag niet: één SCSI-opdracht is drie
/// transfers, en bij 64KB per opdracht kost een vaste milliseconde per wacht
/// al meer dan de drive zelf. GEMETEN 22-09: een Blu-ray door de share kwam
/// zo op 2,7 MB/s. Twintig rondes van 100µs dekken het normale geval; wat
/// daarna nog wacht is een trage drive, en dáár is een milliseconde precies
/// goed.
pub const FINE_ROUNDS: u32 = 20;
const FINE_STEP_NS: u64 = 100_000;
const COARSE_STEP_NS: u64 = 1_000_000;

/// Hoeveel rapporten we per beurt van één apparaat ophalen. Eén apparaat kan
/// twee endpoints hebben (een combo-dongle levert toetsenbord én muis), dus
/// één per beurt zou de muis halveren; ongebrensd zou een ratelend apparaat
/// de andere poorten kunnen uithongeren.
pub const MAX_PER_POLL: usize = 4;

/// Hoeveel controllers één node kan hebben. De O6N biedt er tot tien aan
/// (de firmware-upgrade van zes naar tien), de rest één of twee.
pub const MAX_HOSTS: usize = 10;

/// Hoeveel bezette poorten we per controller bijhouden: de slots
/// ([`driver_xhci::MAX_DEVICES`]) plus de bekende niet-HID-apparaten die
/// geen slot houden.
pub const MAX_KNOWN: usize = 32;

/// De rapportbuffer per beurt: een boot-rapport is 8 bytes, een muis met
/// wiel 4; wat een apparaat meer stuurt, lezen we niet.
const REPORT_BUF: usize = 16;

/// Wat de manager van buiten nodig heeft: waar de gebeurtenissen heen gaan,
/// waar de logregels heen gaan, en de opslagkant. Wordt aangeroepen op de
/// taak die de bus bezit, dus mag blokkeren noch panieken.
pub trait Sink {
    /// Eén invoergebeurtenis. De binary zet hem in de rij van de deliverer
    /// ([`deliver::InputTx`], met `try_send`); vol is weggooien.
    fn input(&mut self, e: Event);

    /// Eén logregel, Engels, met de getallen erin.
    fn log(&mut self, args: fmt::Arguments<'_>);

    /// Een nieuw opslagapparaat. Eén haak en geen lijst: er is één eigenaar
    /// van de bus, en die deelt het apparaat uit aan wie het hebben wil.
    fn storage_attached(&mut self, info: &BulkInfo) {
        let _ = info;
    }

    /// Het apparaat is weg (uitgetrokken, of de controller herstelde zich en
    /// vergat alles). Wie het ergens publiceerde, haalt het daar weg.
    fn storage_gone(&mut self, id: BulkId) {
        let _ = id;
    }

    /// De bytes van een OUT-verzoek.
    fn bulk_out(&mut self, req: &BulkReq) -> &[u8] {
        let _ = req;
        &[]
    }

    /// Waar de bytes van een IN-verzoek heen gaan.
    fn bulk_in(&mut self, req: &BulkReq) -> &mut [u8] {
        let _ = req;
        &mut []
    }

    /// Het antwoord op een verzoek: precies één keer per aangenomen
    /// verzoek, ook als het apparaat intussen verdween.
    fn bulk_done(&mut self, req: &BulkReq, r: Result<usize, BulkError>) {
        let _ = (req, r);
    }
}

/// Waarom de manager een controller niet in beheer nam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// De driver weigerde.
    Xhci(driver_xhci::Error),
    /// Er zijn al [`MAX_HOSTS`] controllers.
    TooManyHosts,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Xhci(e) => e.fmt(f),
            Self::TooManyHosts => write!(f, "usb: more than {MAX_HOSTS} controllers"),
        }
    }
}

impl From<driver_xhci::Error> for Error {
    fn from(e: driver_xhci::Error) -> Self {
        Self::Xhci(e)
    }
}

/// De `Result` van deze crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// Eén bezette roothub-poort met zijn apparaat en decoders.
#[derive(Debug, Default)]
struct Known {
    num: u8,
    /// `None`: een bekend niet-HID-apparaat (Attach gaf zijn slot al terug).
    dev: Option<Device>,
    /// Opslag: het handvat dat de rest van de node van dit apparaat heeft.
    bulk: Option<BulkId>,
    kb: Keyboard,
    ms: Mouse,
}

/// Eén controller met wat de manager erover weet.
struct Ctl {
    hc: Hc,
    known: BoundedVec<Known, MAX_KNOWN>,
    /// Wachtende bulk-verzoeken van deze controller.
    queue: BoundedVec<BulkReq, { storage::QUEUE_DEPTH }>,
    /// De ene lopende transfer: de bouncebuffer is per controller gedeeld.
    busy: Option<Inflight>,
}

impl Ctl {
    fn new(hc: Hc) -> Self {
        Self {
            hc,
            known: BoundedVec::new(),
            queue: BoundedVec::new(),
            busy: None,
        }
    }
}

/// De invoerdienst: bedient nul of meer controllers. Alles hierin is van
/// één taak; [`Manager::add`] hoort vóór de eerste [`Manager::step`], en wie
/// daarna iets van de bus wil, stuurt een [`BulkReq`] via
/// [`Manager::enqueue`].
pub struct Manager<T: Timer> {
    ctls: BoundedVec<Ctl, MAX_HOSTS>,
    /// Hergebruikte buffer: het pollpad alloceert niet.
    evs: Events,
    /// De klok en de slaap van de eigenaar-taak.
    timer: T,
    next_scan: u64,
    next_poll: u64,
    /// Resterende fijnmazige rondes ([`FINE_ROUNDS`]).
    fine: u32,
    /// De volgende generatie voor een [`BulkId`].
    next_bulk: u32,
}

impl<T: Timer> Manager<T> {
    /// Een lege invoerdienst op de klok en de slaap van `timer`.
    #[must_use]
    pub fn new(timer: T) -> Self {
        Self {
            ctls: BoundedVec::new(),
            evs: Events::new(),
            timer,
            next_scan: 0,
            next_poll: 0,
            fine: 0,
            next_bulk: 1,
        }
    }

    /// Het aantal controllers in beheer.
    #[must_use]
    pub fn hosts(&self) -> usize {
        self.ctls.len()
    }

    /// Halteert elke controller (`Hc::stop`): vóór een kern-flip, zodat
    /// geen DMA-master op de bus door de link-reset van de nieuwe kern heen
    /// schrijft (de Pi 5, 30-09: vijf flips vanuit een kern met koud
    /// gestarte xHCI's eindigden met een RP1 die niets meer naar de host
    /// kreeg). Daarna is de dienst klaar; de structuren blijven staan.
    pub async fn stop_all(&mut self) {
        let t = &self.timer;
        for c in self.ctls.iter_mut() {
            c.hc.stop(t).await;
        }
    }

    /// Neemt een controller in beheer: probe, reset, structuren opzetten in
    /// `[dma, dma+size)`, poortvoeding aan. Een controller die niet
    /// antwoordt is geen fatale fout: een board mag meer controllers
    /// aanbieden dan er fysiek bedraad zijn, en de melding is dan de meting.
    pub async fn add(&mut self, mut hc: Hc, dma: Pa, size: u64, sink: &mut impl Sink) -> Result {
        if self.ctls.is_full() {
            return Err(Error::TooManyHosts);
        }
        let t = &self.timer;
        hc.probe()?;
        let (ver, slots, ports, ctx64) = hc.info();
        sink.log(format_args!(
            "usb: {} xHCI {:x}.{:x}, {slots} slots, {ports} ports, {}-byte contexts",
            hc.name(),
            ver >> 8,
            ver & 0xFF,
            if ctx64 { 64 } else { 32 }
        ));
        hc.reset(t).await?;
        hc.start(dma, size, t).await?;
        hc.power_on(t).await;
        // De rauwe poortstand, één regel. Dit is de meting die op ijzer telt:
        // een controller die netjes opkomt maar op géén poort CCS meldt, is
        // een controller die niet aan de fysieke connector hangt, en dat is
        // iets heel anders dan een driver die stukgaat. Zonder deze regel
        // lijken die twee identiek, namelijk stil.
        sink.log(format_args!("usb: {} PORTSC{}", hc.name(), PortsLine(&hc)));
        let _ = self.ctls.push(Ctl::new(hc));
        Ok(())
    }

    /// Eén ronde van de eigenaar: scannen en pollen als hun tijd er is, en de
    /// bulk-transfers bedienen. Geeft terug hoeveel nanoseconden de taak mag
    /// slapen tot de volgende ronde: de volgende pollronde, of (zolang er een
    /// bulk-transfer loopt) de volgende blik op de event-ring.
    ///
    /// De vorm van de taak in de binary (Go's `Run`):
    ///
    /// ```ignore
    /// let mut wait = 0;
    /// loop {
    ///     if let Either::Left(req) = select(REQS.recv(), after(wait)).await {
    ///         mgr.enqueue(req, &mut sink);
    ///     }
    ///     wait = mgr.step(&mut sink).await;
    /// }
    /// ```
    pub async fn step(&mut self, sink: &mut impl Sink) -> u64 {
        let now = self.timer.now();
        if now >= self.next_scan {
            self.scan(sink).await;
            self.next_scan = self.timer.now().saturating_add(SCAN_INTERVAL_NS);
        }
        if self.timer.now() >= self.next_poll {
            self.poll(sink).await;
            self.next_poll = self.timer.now().saturating_add(POLL_INTERVAL_NS);
        }
        self.serve_bulk(sink).await;
        let mut wait = self.next_poll.saturating_sub(self.timer.now());
        if self.ctls.iter().any(|c| c.busy.is_some()) {
            let step = if self.fine > 0 {
                self.fine -= 1;
                FINE_STEP_NS
            } else {
                COARSE_STEP_NS
            };
            wait = wait.min(step);
        }
        wait
    }

    /// Kijkt welke poorten er bij zijn gekomen en welke leeg zijn geraakt.
    pub async fn scan(&mut self, sink: &mut impl Sink) {
        let (mut next_bulk, evs, t) = (self.next_bulk, &mut self.evs, &self.timer);
        for (host, c) in self.ctls.iter_mut().enumerate() {
            if let Some(cause) = c.hc.recovery_needed() {
                // HCRST maakt elk bestaand handvat ongeldig, ook als maar één
                // slot de oorspronkelijke fout gaf. Vergeet ze daarom zonder
                // Disable Slot te sturen, maar laat eerst alle lokaal
                // onthouden toetsen en muisknoppen los. Na het herstel ziet
                // de normale scan aangesloten apparaten in dezelfde ronde
                // opnieuw.
                forget_controller(c, evs, sink);
                if let Err(e) = c.hc.recover(t).await {
                    sink.log(format_args!(
                        "usb: {}: controller recovery after {cause} failed: {e}",
                        c.hc.name()
                    ));
                    continue;
                }
                sink.log(format_args!(
                    "usb: {}: controller recovered after {cause}; rescanning ports",
                    c.hc.name()
                ));
            }
            for n in 1..=c.hc.num_ports() {
                scan_port(c, host as u8, n, &mut next_bulk, evs, sink, t).await;
            }
        }
        self.next_bulk = next_bulk;
    }

    /// Haalt één ronde rapporten op.
    pub async fn poll(&mut self, sink: &mut impl Sink) {
        let (evs, t) = (&mut self.evs, &self.timer);
        let mut buf = [0u8; REPORT_BUF];
        for c in self.ctls.iter_mut() {
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
                        k.ms.decode(rep, evs);
                    } else {
                        k.kb.decode(rep, evs);
                    }
                    emit(evs, sink);
                }
            }
        }
    }
}

/// Eén poort in de scan.
async fn scan_port(
    c: &mut Ctl,
    host: u8,
    n: u8,
    next_bulk: &mut u32,
    evs: &mut Events,
    sink: &mut impl Sink,
    t: &impl Timer,
) {
    let p = c.hc.port(n);
    let have = c.known.iter().position(|k| k.num == n);
    match (p.connected, have) {
        (true, None) => {
            attach_port(c, host, n, next_bulk, sink, t).await;
            c.hc.clear_changes(n);
        }
        (false, Some(i)) => {
            if let Some(mut k) = c.known.remove(i) {
                match k.dev {
                    Some(d) => {
                        sink.log(format_args!("usb: {} port {n}: {d} unplugged", c.hc.name()))
                    }
                    None => sink.log(format_args!(
                        "usb: {} port {n}: device unplugged",
                        c.hc.name()
                    )),
                }
                release(c, &mut k, evs, sink, t).await;
            }
            c.hc.clear_changes(n);
        }
        _ => c.hc.clear_changes(n),
    }
}

/// Een net aangesloten poort enumereren en boeken.
async fn attach_port(
    c: &mut Ctl,
    host: u8,
    n: u8,
    next_bulk: &mut u32,
    sink: &mut impl Sink,
    t: &impl Timer,
) {
    let name = c.hc.name();
    let d = match c.hc.attach(n, t).await {
        Ok(d) => d,
        Err(e) => {
            let d = c.hc.diagnostic();
            sink.log(format_args!(
                "usb: {name} port {n}: {e}; sts={:x} crcr={:x} cmdpa={:x} event={:x}",
                d.usbsts, d.crcr, d.cmd_bus, d.trb0[3]
            ));
            if matches!(
                e,
                driver_xhci::Error::Poisoned(Poison::CommandTimeout { .. })
            ) {
                // Zwijgt de command ring, dan is de vraag waar de controller
                // zijn events heen schrijft: ERSTBA en ERDP zoals hij ze
                // teruggeeft, naast waar onze ringen liggen.
                let [t0, t1, t2, t3] = d.trb0;
                sink.log(format_args!(
                    "usb: {name} port {n}: event ring: erdp={:x} (ring {:x}) erstba={:x} (erst {:x}) trb0={t0:08x} {t1:08x} {t2:08x} {t3:08x}",
                    d.erdp, d.evt_bus, d.erstba, d.erst_bus
                ));
            }
            return;
        }
    };
    let mut k = Known {
        num: n,
        dev: d,
        ..Known::default()
    };
    match d {
        None => {
            // Wel iets, maar geen boot-HID en geen opslag. Geen fout: een
            // dongle in de poort is gewoon niets voor deze stack. Onthouden
            // tot uittrekken of controllerherstel: telkens opnieuw
            // enumereren levert geen invoer op.
            sink.log(format_args!(
                "usb: {name} port {n}: device is not a boot-HID, ignored"
            ));
        }
        Some(d) if d.is_mass_storage() => {
            // Opslag hoort niet bij invoer, maar de bus heeft één eigenaar
            // en dat is deze scanner. Het apparaat blijft dus hier in de
            // boekhouding, zodat uittrekken gezien wordt, en wie er wél iets
            // mee doet krijgt een handvat via de sink.
            sink.log(format_args!(
                "usb: {name} port {n}: mass storage {:04x}:{:04x}",
                d.vendor_id, d.product_id
            ));
            let id = BulkId::new(host, n, *next_bulk);
            *next_bulk = next_bulk.wrapping_add(1).max(1);
            k.bulk = Some(id);
            sink.storage_attached(&BulkInfo {
                id,
                vendor_id: d.vendor_id,
                product_id: d.product_id,
                host: name,
                port: n,
                max_transfer: c.hc.max_transfer(),
            });
        }
        Some(d) => {
            sink.log(format_args!("usb: {name} port {n}: {d}"));
            for (i, _) in d.protos().iter().enumerate() {
                if d.boot_assumed(i) {
                    sink.log(format_args!(
                        "usb: {name} slot {} iface {i}: SET_PROTOCOL(boot) refused, assuming boot protocol",
                        d.slot
                    ));
                }
            }
        }
    }
    if let Err(bounded::Full(k)) = c.known.push(k) {
        // Meer bezette poorten dan we bijhouden: niet stil laten hangen met
        // een slot dat niemand ooit teruggeeft.
        sink.log(format_args!(
            "usb: {name} port {n}: more than {MAX_KNOWN} ports in use, device released"
        ));
        if let Some(d) = k.dev
            && let Err(e) = c.hc.detach(&d, t).await
        {
            sink.log(format_args!("usb: {name}: detach failed: {e}"));
        }
    }
}

/// Laat alles los wat dit apparaat vast hield. De reset van de decoders is
/// geen opruimwerk maar een correctie: een toets die tijdens het uittrekken
/// ingedrukt was, moet bij de display worden losgelaten, anders blijft hij
/// daar voor altijd staan.
async fn release(
    c: &mut Ctl,
    k: &mut Known,
    evs: &mut Events,
    sink: &mut impl Sink,
    t: &impl Timer,
) {
    let Some(d) = k.dev else {
        return; // bekend niet-HID-apparaat; Attach gaf zijn slot al terug
    };
    if let Some(id) = k.bulk {
        storage::drop_bulk(c, id, sink);
    }
    evs.clear();
    append_reset(k, evs);
    emit(evs, sink);
    if let Err(e) = c.hc.detach(&d, t).await {
        sink.log(format_args!(
            "usb: {}: detach could not confirm the controller slot: {e}",
            c.hc.name()
        ));
    }
}

/// Laat decoderstaat los en vergeet alle handvatten na een ownership-fout.
/// Roept bewust geen `detach` aan: de daaropvolgende HCRST is juist de
/// hardware-operatie die alle slots atomair vrijmaakt, en oude handvatten
/// zijn daarna niet meer geldig.
fn forget_controller(c: &mut Ctl, evs: &mut Events, sink: &mut impl Sink) {
    let mut known = core::mem::take(&mut c.known);
    for k in known.iter_mut() {
        evs.clear();
        append_reset(k, evs);
        emit(evs, sink);
        if let Some(id) = k.bulk {
            storage::drop_bulk(c, id, sink);
        }
    }
}

fn append_reset(k: &mut Known, evs: &mut Events) {
    k.kb.reset(evs);
    k.ms.reset(evs);
}

fn emit(evs: &mut Events, sink: &mut impl Sink) {
    for e in evs.iter() {
        sink.input(*e);
    }
    evs.clear();
}

/// De poortstand als één logregel: ` 1:000002a0 2:00001203(low-speed)`.
struct PortsLine<'a>(&'a Hc);

impl fmt::Display for PortsLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for n in 1..=self.0.num_ports() {
            let p = self.0.port(n);
            write!(f, " {}:{:08x}", p.num, p.raw)?;
            if p.connected {
                write!(f, "({})", p.speed)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
