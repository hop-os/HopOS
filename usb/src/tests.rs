//! De Go-tests van `gui/usbin` (recovery, storage), plus de rondes die de
//! Rust-vorm erbij vraagt. De HID-kant alleen met `hid`
//! (`cargo test -p usb --features hid`, tools/gate.sh).

use super::*;
use crate::register::{HostSpec, PrepareError, Registry};
use crate::storage::{BULK_TIMEOUT_NS, Inflight, drop_bulk};
use core::future::{Future, ready};
use core::pin::pin;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use std::format;
use std::string::String;
use std::vec::Vec;

static NOW: AtomicU64 = AtomicU64::new(1_000_000_000);

fn clock() -> u64 {
    NOW.fetch_add(1_000, Relaxed)
}

/// De timer van de tests: de klok hierboven, en een slaap die de klok
/// vooruit zet en meteen klaar is.
struct Clock;

impl Timer for Clock {
    fn now(&self) -> u64 {
        clock()
    }
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        NOW.fetch_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX), Relaxed);
        ready(())
    }
}

/// Een sink die alles opschrijft.
#[derive(Default)]
struct Rec {
    #[cfg(feature = "hid")]
    events: Vec<Event>,
    logs: Vec<String>,
    gone: Vec<BulkId>,
    done: Vec<(BulkReq, Result<usize, BulkError>)>,
}

impl Sink for Rec {
    #[cfg(feature = "hid")]
    fn input(&mut self, e: Event) {
        self.events.push(e);
    }
    fn log(&mut self, args: fmt::Arguments<'_>) {
        self.logs.push(format!("{args}"));
    }
    fn storage_gone(&mut self, id: BulkId) {
        self.gone.push(id);
    }
    fn bulk_done(&mut self, req: &BulkReq, r: Result<usize, BulkError>) {
        self.done.push((*req, r));
    }
}

fn test_ctl() -> Ctl {
    Ctl::new(Hc::unbound("test"))
}

fn req(id: BulkId, tag: u32) -> BulkReq {
    BulkReq::new(id, BulkOp::In, 512, None, clock(), tag)
        .unwrap()
        .unwrap()
}

/// Pollt een future die zonder wachten klaar hoort te zijn.
fn run_ready<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("future was not ready"),
    }
}

// --- recovery_test.go: de HID-kant ----------------------------------------

#[cfg(feature = "hid")]
mod hid_side {
    use super::*;
    use crate::hid::{Hid, emit};
    use driver_hid::{Events, Kind};

    fn ev(kind: Kind, code: i32) -> Event {
        Event {
            kind,
            code,
            ..Event::default()
        }
    }

    // Go: TestForgetControllerReleasesInputAndInvalidatesAllHandles
    #[test]
    fn forget_controller_releases_input_and_invalidates_all_handles() {
        let mut rec = Rec::default();
        let mut hid = Hid::new();
        let mut c = test_ctl();
        let mut k = Known {
            num: 1,
            ..Known::default()
        };
        k.hid.kb.decode(&[0, 0, 0x04, 0, 0, 0, 0, 0], &mut hid.evs); // A ingedrukt
        k.hid.ms.decode(&[1, 0, 0], &mut hid.evs); // linkermuisknop ingedrukt
        hid.evs.clear();
        c.known.push(k).unwrap();

        // `dev` is bewust `None`: herstel mag geen detach doen op een handvat
        // dat door de aanstaande HCRST ongeldig wordt.
        forget_controller(&mut c, &mut hid, &mut rec);

        assert!(c.known.is_empty(), "oude handvatten bleven bekend");
        assert_eq!(
            rec.events,
            [ev(Kind::KeyUp, 65), ev(Kind::MouseUp, 0)],
            "release-events"
        );
    }

    // Go: TestEmitDropsBufferedEventsWithoutSink
    //
    // In Rust is er altijd een sink; wat blijft is dat `emit` de buffer leegt,
    // ook als de sink niets met de gebeurtenissen doet.
    #[test]
    fn emit_drops_buffered_events_without_sink() {
        struct Deaf;
        impl Sink for Deaf {
            fn input(&mut self, _: Event) {}
            fn log(&mut self, _: fmt::Arguments<'_>) {}
        }
        let mut evs = Events::new();
        evs.push(ev(Kind::KeyUp, 65)).unwrap();
        emit(&mut evs, &mut Deaf);
        assert!(
            evs.is_empty(),
            "eventbuffer hield {} events vast",
            evs.len()
        );
    }

    // Go: TestNonHIDPortDoesNotPollOrDetachAndRecoveryForgetsIt
    #[test]
    fn non_hid_port_does_not_poll_or_detach_and_recovery_forgets_it() {
        let mut rec = Rec::default();
        let mut m = Manager::new(Clock);
        let mut c = test_ctl();
        c.known
            .push(Known {
                num: 1,
                ..Known::default()
            })
            .unwrap();
        let _ = m.ctls.push(c);
        // Een `None`-apparaat is een bekend, niet-ondersteund apparaat dat zijn
        // slot al teruggaf.
        run_ready(m.hid.poll(&mut m.ctls, &mut rec, &Clock));
        let c = &mut m.ctls[0];
        let mut k = Known {
            num: 1,
            ..Known::default()
        };
        run_ready(release(c, &mut k, &mut m.hid, &mut rec, &Clock));
        assert_eq!(
            c.known.len(),
            1,
            "de poortmarkering van het niet-HID-apparaat verdween"
        );
        forget_controller(c, &mut m.hid, &mut rec);
        assert!(c.known.is_empty(), "herstel liet geen her-enumeratie toe");
        assert!(
            rec.events.is_empty(),
            "genegeerd apparaat gaf invoer: {:?}",
            rec.events
        );
    }
}

// --- storage_test.go --------------------------------------------------------

// Go: TestUitgetrokkenDriveBeantwoordtElkVerzoekMetErrGone
#[test]
fn uitgetrokken_drive_beantwoordt_elk_verzoek_met_err_gone() {
    let mut rec = Rec::default();
    let mut m = Manager::new(Clock);
    let _ = m.ctls.push(test_ctl());
    let (a, b) = (BulkId::new(0, 1, 1), BulkId::new(0, 2, 2));

    // a heeft een transfer lopen, b wacht erachter op dezelfde controller.
    let ra = req(a, 1);
    m.ctls[0].busy = Some(Inflight { req: ra, td: None });
    let rb = req(b, 2);
    m.enqueue(rb, &mut rec);

    drop_bulk(&mut m.ctls[0], a, &mut rec);

    assert_eq!(
        rec.done,
        [(ra, Err(BulkError::Gone))],
        "lopende transfer van a"
    );
    assert_eq!(rec.gone, [a], "a werd niet afgemeld");
    assert!(
        m.ctls[0].busy.is_none(),
        "de controller bleef bezet door een apparaat dat weg is"
    );
    // b is niet van a en moet gewoon blijven wachten op zijn beurt.
    assert_eq!(m.ctls[0].queue.as_slice(), &[rb], "rij na afmelden van a");

    // Een nieuw verzoek aan a krijgt Gone; b gaat door naar de ring (en
    // krijgt daar Gone omdat er geen echt apparaat achter zit).
    m.ctls[0].queue.clear();
    rec.done.clear();
    let ra2 = req(a, 3);
    m.enqueue(ra2, &mut rec);
    run_ready(m.serve_bulk(&mut rec));
    assert_eq!(
        rec.done,
        [(ra2, Err(BulkError::Gone))],
        "verzoek na afmelden"
    );

    rec.done.clear();
    m.enqueue(rb, &mut rec);
    drop_bulk(&mut m.ctls[0], b, &mut rec);
    assert_eq!(
        rec.done,
        [(rb, Err(BulkError::Gone))],
        "wachtend verzoek van b"
    );
    assert!(m.ctls[0].queue.is_empty());
}

// Go: TestControllerherstelMeldtOpslagAf
#[test]
fn controllerherstel_meldt_opslag_af() {
    let mut rec = Rec::default();
    let mut hid = hid::Hid::new();
    let mut c = test_ctl();
    let b = BulkId::new(0, 1, 7);
    c.known
        .push(Known {
            num: 1,
            bulk: Some(b),
            ..Known::default()
        })
        .unwrap();
    forget_controller(&mut c, &mut hid, &mut rec);
    assert_eq!(
        rec.gone,
        [b],
        "een controllerreset liet de drive gepubliceerd staan"
    );
}

// Go: TestVerlopenDeadlineBereiktDeEigenaarNiet
#[test]
fn verlopen_deadline_bereikt_de_eigenaar_niet() {
    let b = BulkId::new(0, 1, 1);
    let now = clock();
    assert_eq!(
        BulkReq::new(b, BulkOp::In, 512, Some(now - 1_000_000), now, 0),
        Err(BulkError::DeadlineExceeded)
    );
    // Zonder deadline geldt de standaardgrens.
    let r = BulkReq::new(b, BulkOp::In, 512, None, now, 0)
        .unwrap()
        .unwrap();
    assert_eq!(r.deadline_ns, now + BULK_TIMEOUT_NS);
}

// Go: TestLegeTransferIsGeenVerzoek
#[test]
fn lege_transfer_is_geen_verzoek() {
    let b = BulkId::new(0, 1, 1);
    assert_eq!(BulkReq::new(b, BulkOp::Out, 0, None, clock(), 0), Ok(None));
    assert_eq!(BulkReq::new(b, BulkOp::In, 0, None, clock(), 0), Ok(None));
    // Een reset heeft geen bytes en is wél een verzoek.
    assert!(matches!(
        BulkReq::new(b, BulkOp::Reset, 0, None, clock(), 0),
        Ok(Some(_))
    ));
}

/// Een transfer die zijn deadline haalt zonder antwoord, krijgt TimedOut en
/// maakt de controller vrij voor de volgende.
#[test]
fn verlopen_transfer_maakt_de_controller_vrij() {
    let mut rec = Rec::default();
    let mut m = Manager::new(Clock);
    let _ = m.ctls.push(test_ctl());
    let mut r = req(BulkId::new(0, 1, 1), 9);
    r.deadline_ns = 0;
    m.ctls[0].busy = Some(Inflight { req: r, td: None });
    run_ready(m.serve_bulk(&mut rec));
    assert_eq!(rec.done, [(r, Err(BulkError::TimedOut))]);
    assert!(m.ctls[0].busy.is_none());
}

#[test]
fn volle_rij_en_onbekende_controller() {
    let mut rec = Rec::default();
    let mut m = Manager::new(Clock);
    let r = req(BulkId::new(3, 1, 1), 1);
    m.enqueue(r, &mut rec);
    assert_eq!(rec.done, [(r, Err(BulkError::Gone))]);
    let _ = m.ctls.push(test_ctl());
    rec.done.clear();
    for t in 0..=storage::QUEUE_DEPTH as u32 {
        m.enqueue(req(BulkId::new(0, 1, 1), t), &mut rec);
    }
    assert_eq!(rec.done.len(), 1);
    assert_eq!(rec.done[0].1, Err(BulkError::Busy));
}

// --- rondes en registratie ----------------------------------------------------

#[test]
fn step_sleeps_until_the_next_poll_or_the_next_bulk_look() {
    let mut rec = Rec::default();
    let mut m = Manager::new(Clock);
    let w = run_ready(m.step(&mut rec));
    assert!(w > 0 && w <= POLL_INTERVAL_NS, "{w}");
    let _ = m.ctls.push(test_ctl());
    let r = req(BulkId::new(0, 1, 1), 1);
    m.ctls[0].busy = Some(Inflight { req: r, td: None });
    m.fine = 2;
    assert_eq!(run_ready(m.step(&mut rec)), FINE_STEP_NS);
    assert_eq!(run_ready(m.step(&mut rec)), FINE_STEP_NS);
    assert_eq!(run_ready(m.step(&mut rec)), COARSE_STEP_NS);
}

#[test]
fn bring_up_logs_every_failing_controller() {
    let mut reg = Registry::new();
    for (name, base, dma_size) in [
        ("nodma", Pa(0x1000), 0),
        ("pcie", Pa(0x2000), 0x10_0000),
        ("nobase", Pa(0), 0x10_0000),
        ("dead", Pa(0x1000), 0x10_0000),
    ] {
        reg.register(HostSpec {
            name,
            base,
            bus_off: 0,
            dma: Pa(0x10_0000),
            dma_size,
        })
        .unwrap();
    }
    let mut rec = Rec::default();
    let mut m = Manager::new(Clock);
    let live = run_ready(reg.bring_up(&mut m, &mut rec, async |h: &HostSpec| {
        if h.name == "pcie" {
            return Err(PrepareError {
                what: "PCIe link down",
                value: 0x1d,
            });
        }
        Ok(Hc::unbound(h.name))
    }));
    assert_eq!(live, 0);
    assert_eq!(m.hosts(), 0);
    assert_eq!(
        rec.logs,
        [
            "usb: nodma: this board planned no USB DMA region, skipped",
            "usb: pcie: PCIe link down (0x1d)",
            "usb: nobase: no register window, skipped",
            "usb: dead: xhci: CAPLENGTH 0x0 in raw word 0x00000000: no controller at 0x0 (0 = not clocked, 0xFF.. = dead bus)",
            "usb: no working controller on this node",
        ]
    );
    // Zonder aangemelde controllers gebeurt er niets, ook geen regel.
    rec.logs.clear();
    assert_eq!(
        run_ready(
            Registry::new().bring_up(&mut m, &mut rec, async |h: &HostSpec| Ok(Hc::unbound(
                h.name
            )))
        ),
        0
    );
    assert!(rec.logs.is_empty());
}
