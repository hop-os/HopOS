//! Frame-niveau netwerk van een app: de [`Nic`] over de eigen frame-ringen
//! naar de L2-switch van de kern, de deurbel en de slaapstand van de RX-pomp.
//!
//! Het twee-methode-device (`netdev::Device`) waaraan in Go elke
//! stack-wissel hing (gVisor, lneto, leannet: elke wissel raakte alleen
//! `up.go`). De stack zelf staat in [`crate::appnet`] en gebruikt precies
//! dit; een app die rauwe frames wil ook.
//!
//! Het interne net is deterministisch: kern op 10.100.0.1, slot i op
//! 10.100.0.1 + i in een /16, MAC `02:00:00:00:<slot, zestien bits>`. Er
//! wordt niets geresolved; beide kanten leiden het uit het slotnummer af.
//!
//! Dit module bezit de TX-producer en de RX-consument van de app. De
//! deurbel-drempel op de control-page is van de idle ([`crate::sleep`]).

use crate::app::App;
use crate::contract::{NET_MTU, NET_RING_DATA_CAP};
use crate::ring::{Corrupt, Kind, Peek, Reader, Writer};
use crate::sleep::{self, RxDoor};
use abi::layout::{port_ip4, port_mac};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use netdev::{Device, Mac, TxError};
use sync::{Signal, yield_now};

/// De MTU van het slot-LAN (geen draad, geen bitfouten).
pub const MTU: usize = NET_MTU;

/// Hoe lang [`Nic::transmit_wait`] een volle TX-ring de tijd geeft: een
/// korte lokale burst krijgt tegendruk in plaats van stil verlies, en een
/// verdwenen switch blijft een gewone device-fout in plaats van een hang.
pub const TX_BACKPRESSURE: Duration = Duration::from_millis(10);

/// Transmit trof de ring vol en wachtte.
pub static TX_WAITS: AtomicU64 = AtomicU64::new(0);
/// Na [`TX_BACKPRESSURE`] alsnog opgegeven.
pub static TX_DROPS: AtomicU64 = AtomicU64::new(0);
/// De pomp werd vóór zijn timer gewekt (de bel).
pub static PUMP_EARLY: AtomicU64 = AtomicU64::new(0);
/// De poll-timer van de pomp liep af.
pub static PUMP_TIMER: AtomicU64 = AtomicU64::new(0);
/// Kicks naar de OS-core (HVC #6) na een leeg-naar-niet-leeg op de TX-ring.
/// Eén per burst, niet per frame: de switch van de kern leest de ring leeg
/// zodra hij wakker is.
///
/// GEMETEN 29-09, `tools/qemu-test.sh` (virt, 4 cores, zes appspike-runs per
/// kant): `dial_us` gemiddeld 3025 zonder en 2922 met de kick (binnen de
/// ruis: na de SYN wacht de app, en die idle-yield kickte al), `flush_us`
/// 1548 zonder en 1193 met (-23%: daar publiceert de app en rekent hij door
/// tot zijn flush). 3 à 4 kicks tot en met de dial.
pub static TX_KICKS: AtomicU64 = AtomicU64::new(0);

/// Een publicatie op de TX-ring die de kern nog niet hoorde: op het hart
/// van de kern (de OS-core) kickt de [`Nic`] niet bij elke burst. Daar is
/// de kick een yield naar nu, en dus een wissel naar de buur terwijl deze
/// bewoner zelf nog werk heeft (verder rekenen, tot zijn eigen idle komen):
/// hij bleef met wektijd 0 "aan de beurt" en kreeg later een beurt zonder
/// signaal, die hij met een lege yield teruggaf (03-10, hop-cost5). Het
/// principe: een bewoner van de OS-core houdt de core tot hij wacht (zijn
/// idle-yield, en die laat de kern het frame bezorgen, [`owed_by_yield`])
/// of tot `TURN_CAP`; een ander komt alleen aan de beurt met een signaal.
/// Blijft hij bezig (een taak die altijd klaar is, BURN), dan kickt de pomp
/// alsnog ([`kick_owed`]): dan wacht het frame niet op `TURN_CAP`.
static TX_OWED: AtomicBool = AtomicBool::new(false);

/// De kick naar de kern: SEV voor een kern in WFE, HVC #6 (riscv: de
/// kick-ecall) voor een kern die een bewoner draait of in WFI slaapt.
fn kick() {
    dev::notify();
    crate::arch::hvc_kick_os();
    TX_KICKS.fetch_add(1, Relaxed);
}

/// De bel naar de kern na een leeg-naar-niet-leeg op de outbox: dezelfde
/// kick als op de TX-ring, want de kern leest de outbox alleen op een kick
/// of na een beurt (O10, docs/sequence-hopos.md §13.3). Op het hart van de
/// kern de schuld ([`TX_OWED`]), zoals de [`Nic`] daar.
pub(crate) fn outbox_kick() {
    if crate::rt::app().is_some_and(|a| a.ctrl().on_kern_hart()) {
        TX_OWED.store(true, Relaxed);
    } else {
        kick();
    }
}

/// Betaalt een uitgestelde kick (zie [`TX_OWED`]): de app blijft bezig, of
/// slaapt zonder yield. `true` als er een kick was.
pub fn kick_owed() -> bool {
    let owed = TX_OWED.swap(false, Relaxed);
    if owed {
        kick();
    }
    owed
}

/// De idle-yield naar de kern: die bezorgt wat er op de TX-ring ligt, dus
/// een uitgestelde kick vervalt.
pub(crate) fn owed_by_yield() {
    TX_OWED.store(false, Relaxed);
}

/// Het interne IPv4 van slot `slot` (big-endian; een app kent zijn slot
/// als getal, 0 is de kern).
#[must_use]
pub const fn slot_ip(slot: u64) -> [u8; 4] {
    port_ip4(slot as usize).to_be_bytes()
}

/// Het adres van de kern op het slot-LAN: de gateway.
#[must_use]
pub const fn host_ip() -> [u8; 4] {
    slot_ip(0)
}

/// De MAC van slot `slot` (de kern is slot 0).
#[must_use]
pub const fn mac_of(slot: u64) -> Mac {
    Mac(port_mac(slot as usize))
}

/// De NIC van een app: de twee frame-ringen in zijn staart.
pub struct Nic {
    tx: Writer,
    rx: Reader,
    rx_peek: Peek,
    mac: Mac,
    /// Op het hart van de kern: de kick wacht op de idle-yield
    /// ([`TX_OWED`]).
    defer: bool,
}

impl Nic {
    /// Opent de frame-ringen van `app` met de belofte van deze kant
    /// ([`crate::mmu::ring_coherence`]); de ring kopieert zonder onderhoud
    /// zodra de kern hetzelfde belooft.
    pub fn open(app: &App) -> Result<Self, abi::Error> {
        let t = app.tail();
        let on_kern = app.ctrl().on_kern_hart();
        let c = crate::mmu::ring_coherence(on_kern);
        Ok(Self {
            tx: Writer::open_with(t.net_tx(), NET_RING_DATA_CAP, c)?,
            rx: Reader::open_with(t.net_rx(), NET_RING_DATA_CAP, c)?,
            rx_peek: Peek::new(t.net_rx(), NET_RING_DATA_CAP),
            mac: mac_of(app.slot()),
            defer: on_kern,
        })
    }

    /// Een NIC over willekeurige ringen (tests, een lokale lus); `rx_peek`
    /// kijkt naar dezelfde ring als `rx`.
    #[must_use]
    pub fn over(tx: Writer, rx: Reader, rx_peek: Peek, mac: Mac) -> Self {
        Self {
            tx,
            rx,
            rx_peek,
            mac,
            defer: false,
        }
    }

    /// Hangt de deurbel aan: de idle van de app-core wapent vanaf nu
    /// `CtrlRXDoor` en belt `bell` zodra er RX ligt. Alleen wie de RX-ring
    /// leegleest mag dit (zie [`crate::sleep`]).
    pub fn watch_rx(&self, bell: &'static Signal) {
        sleep::watch_rx(RxDoor {
            peek: self.rx_peek,
            bell,
        });
    }

    /// Eén poging. Vol is [`TxError::Full`], met een bel erbij: dat maakt de
    /// vol-naar-ruimte-race level-triggered zonder architectuurkennis.
    pub fn try_transmit(&mut self, frame: &[u8]) -> Result<(), TxError> {
        if frame.is_empty() || !self.tx.fits(frame.len()) {
            return Err(TxError::Size(frame.len()));
        }
        let r = self.tx.write(Kind::FRAME, frame).map(Some);
        self.sent(r, frame.len()).map(|_| ())
    }

    /// Als [`Nic::try_transmit`], maar `fill` bouwt het frame in de TX-ring
    /// zelf: hij krijgt `max` bytes en geeft de lengte van het frame (0 =
    /// niets te zenden, `Ok(0)`). Geen kopie uit een eigen buffer. Is er nu
    /// geen plaats voor `max` bytes, dan [`TxError::Full`] zonder `fill` te
    /// roepen: de aanroeper bouwt dan in zijn eigen buffer en wacht met
    /// [`Nic::transmit_wait`].
    pub fn try_transmit_with(
        &mut self,
        max: usize,
        fill: impl FnOnce(&mut [u8]) -> usize,
    ) -> Result<usize, TxError> {
        let mut len = 0;
        let r = self.tx.write_with(Kind::FRAME, max, |dst| {
            len = fill(dst).min(dst.len());
            len
        });
        self.sent(r, max).map(|sent| if sent { len } else { 0 })
    }

    /// De afloop van een schrijf in de TX-ring: de kick bij de overgang van
    /// leeg naar niet-leeg (op het hart van de kern de schuld, [`TX_OWED`]),
    /// de bel bij vol. `Ok(false)`: geen record.
    fn sent(&self, r: Result<Option<bool>, abi::Error>, len: usize) -> Result<bool, TxError> {
        match r {
            Ok(Some(was_empty)) => {
                if self.defer {
                    // De kern draait pas als wij de core teruggeven; dat doet
                    // de idle-yield, of de pomp als we bezig blijven.
                    TX_OWED.store(true, Relaxed);
                } else if was_empty {
                    // De SEV wekt een kern in WFE; de kick een kern die een
                    // bewoner draait of in WFI slaapt (Go: `dev.Notify`, dat
                    // op de M4 beide deed). Zonder kick hoorde de kern een
                    // app die na zijn publicatie blijft rekenen pas op zijn
                    // failsafe van 1 ms of op de idle-yield van de app.
                    kick();
                }
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(abi::Error::RingFull { .. }) => {
                // Op het hart van de kern leest niemand de ring leeg tot we
                // de core teruggeven: nu dus, anders wacht `transmit_wait`
                // zijn hele tegendruk voor niets.
                if self.defer {
                    TX_OWED.store(false, Relaxed);
                    kick();
                } else {
                    dev::notify();
                }
                Err(TxError::Full)
            }
            Err(abi::Error::RecordTooLarge { .. }) => Err(TxError::Size(len)),
            // Onmogelijke indexen: de switch beschrijft ze, en er valt niets
            // meer te herstellen tot de kern het slot herstart.
            Err(_) => Err(TxError::Dead),
        }
    }

    /// Zet een frame op de TX-ring en wacht bij een volle ring hooguit
    /// [`TX_BACKPRESSURE`] op ruimte, met een yield per poging. `now` is de
    /// klok in nanoseconden.
    pub async fn transmit_wait(&mut self, frame: &[u8], now: fn() -> u64) -> Result<(), TxError> {
        let budget = u64::try_from(TX_BACKPRESSURE.as_nanos()).unwrap_or(u64::MAX);
        let deadline = now().saturating_add(budget);
        loop {
            match self.try_transmit(frame) {
                Err(TxError::Full) => {
                    TX_WAITS.fetch_add(1, Relaxed);
                    if now() > deadline {
                        TX_DROPS.fetch_add(1, Relaxed);
                        return Err(TxError::Full);
                    }
                    yield_now().await;
                }
                other => return other,
            }
        }
    }

    /// Eén frame uit de RX-ring, in de ring zelf aan `f`: geen kopie naar
    /// een eigen buffer. `max` is het grootste frame dat we aannemen.
    /// Records van een ander type worden overgeslagen. De producer is de
    /// kern, en die schrijft een gepubliceerd record niet meer
    /// ([`abi::ring::Reader::read_with`]).
    pub fn receive_with<T>(&mut self, max: usize, mut f: impl FnMut(&[u8]) -> T) -> Option<T> {
        loop {
            let got = self
                .rx
                .read_with(max, |kind, frame| (kind == Kind::FRAME).then(|| f(frame)))?;
            if got.is_some() {
                return got;
            }
        }
    }

    /// De reden als de RX-ring corrupt verklaard is.
    #[must_use]
    pub fn rx_corruption(&self) -> Option<Corrupt> {
        self.rx.corrupt()
    }
}

impl Device for Nic {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), TxError> {
        self.try_transmit(frame)
    }

    /// Eén frame uit de RX-ring, rechtstreeks in `buf`: geen allocatie en
    /// geen extra kopie. Records van een ander type worden overgeslagen.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let rec = self.rx.read_into(buf)?;
            if rec.kind == Kind::FRAME {
                return Some(rec.payload.len());
            }
        }
    }

    fn mac(&self) -> Mac {
        self.mac
    }
}

/// De slaapstand van de RX-pomp: `lo` (de scherpe slaap), `hi` (de cap) en
/// `hold` (zoveel lege rondes blijft hij op `lo`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RxPoll {
    /// De slaap direct na verkeer.
    pub lo: Duration,
    /// Het plafond waarnaar hij verdubbelt als het stil is.
    pub hi: Duration,
    /// Lege rondes op `lo` voor hij gaat verdubbelen.
    pub hold: u32,
}

impl RxPoll {
    /// De default, "300us:1s:4". GEMETEN (schedbench 29-08): de deurbel
    /// draagt de latency (koud p50 0,8 ms bij een cap van tien seconden),
    /// dus de cap is alleen het vangnet voor een gedoofde bel. 1 s en niet
    /// meer: de heartbeat wekt elke app toch al, dus een grotere cap levert
    /// nul wekken minder op en begrenst een bel-storing op 1 s.
    pub const DEFAULT: Self = Self {
        lo: Duration::from_micros(300),
        hi: Duration::from_secs(1),
        hold: 4,
    };

    /// Leest de stand uit de env (`RXPOLL`): `""` is de default, `"300us"` een
    /// vaste slaap (het gedrag van vóór 29-08), `"300us:5ms"` NAPI-achtig
    /// verdubbelen, `"300us:5ms:8"` idem met acht lege rondes op `lo`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        if s.is_empty() {
            return Self::DEFAULT;
        }
        let mut p = Self {
            lo: Self::DEFAULT.lo,
            hi: Self::DEFAULT.lo,
            hold: 0,
        };
        let mut f = s.split(':');
        if let Some(d) = f.next().and_then(parse_duration).filter(|d| !d.is_zero()) {
            p.lo = d;
            p.hi = d;
        }
        if let Some(d) = f.next().and_then(parse_duration).filter(|&d| d >= p.lo) {
            p.hi = d;
        }
        if let Some(n) = f
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&n| n > 0)
        {
            p.hold = n;
        }
        p
    }

    /// De volgende slaap na `empty` lege rondes, vanaf `d`.
    #[must_use]
    pub fn next(&self, d: Duration, empty: u32) -> Duration {
        if empty > self.hold {
            d.saturating_mul(2).min(self.hi)
        } else {
            d
        }
    }
}

/// Een duur als `300us`, `5ms`, `1s` of `250ns`; de vormen die `RXPOLL`
/// gebruikt. `None` bij iets anders.
#[must_use]
pub fn parse_duration(s: &str) -> Option<Duration> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ns" => Some(Duration::from_nanos(n)),
        "us" | "\u{b5}s" => Some(Duration::from_micros(n)),
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        _ => None,
    }
}

/// De kleinste window-scale-shift waarmee een venster van `max_buf` bytes te
/// adverteren is (RFC 7323, plafond 14). Voor de stack-config.
#[must_use]
pub fn ws_shift_for(max_buf: u64) -> u8 {
    let mut shift = 0u8;
    while shift < 14 && (0xffffu64 << shift) < max_buf {
        shift += 1;
    }
    shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::tests::Backing;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    fn now_zero() -> u64 {
        0
    }

    // TestTransmitWachtKortOpRuimteInPlaatsVanDrop: een volle ring geeft
    // tegendruk, geen drop; zodra de consument ruimte maakt gaat het frame
    // er alsnog op.
    #[test]
    fn transmit_waits_briefly_for_room_instead_of_drop() {
        let txb = Backing::new(4096);
        let rxb = Backing::new(4096);
        let mut filler = Writer::open(txb.pa(), 4096).unwrap();
        let frame = [0xabu8; 1000];
        let mut filled = 0;
        while filler.write(Kind::FRAME, &frame).is_ok() {
            filled += 1;
        }
        assert!(filled >= 2, "testring vulde al na {filled} frames");
        // De echte producer pas nu: een schrijver houdt zijn eigen head bij
        // (`abi::ring::Writer`), dus hij opent na de vuller, zoals een app
        // de ring één keer opent en daarna de enige schrijver is.
        let tx = Writer::open(txb.pa(), 4096).unwrap();
        let rx = Reader::open(rxb.pa(), 4096).unwrap();

        let mut nic = Nic::over(tx, rx, Peek::new(rxb.pa(), 4096), mac_of(1));
        let mut fut = pin!(nic.transmit_wait(&frame, now_zero));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending); // vol: wachten
        let waits = TX_WAITS.load(Relaxed);
        assert!(waits >= 1);

        // De switch leest de ring leeg.
        let mut sw = Reader::open(txb.pa(), 4096).unwrap();
        let mut buf = [0u8; 1000];
        while sw.read_into(&mut buf).is_some() {}

        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        let rec = sw.read_into(&mut buf).unwrap();
        assert_eq!((rec.kind, rec.payload.len()), (Kind::FRAME, frame.len()));
    }

    #[test]
    fn transmit_gives_up_after_the_backpressure_window() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn now() -> u64 {
            CLOCK.fetch_add(4_000_000, Relaxed) // 4 ms per lees
        }
        let txb = Backing::new(256);
        let rxb = Backing::new(256);
        let mut filler = Writer::open(txb.pa(), 256).unwrap();
        while filler.write(Kind::FRAME, &[1; 100]).is_ok() {}
        // Na de vuller, zie hierboven.
        let tx = Writer::open(txb.pa(), 256).unwrap();
        let rx = Reader::open(rxb.pa(), 256).unwrap();
        let mut nic = Nic::over(tx, rx, Peek::new(rxb.pa(), 256), mac_of(1));
        let drops = TX_DROPS.load(Relaxed);
        let mut fut = pin!(nic.transmit_wait(&[1; 100], now));
        let mut cx = Context::from_waker(Waker::noop());
        let mut polls = 0;
        let r = loop {
            polls += 1;
            if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
                break r;
            }
        };
        assert_eq!(r, Err(TxError::Full));
        assert!(polls <= 4);
        assert!(TX_DROPS.load(Relaxed) > drops);
    }

    #[test]
    fn receive_reads_frames_and_refuses_bad_sizes() {
        let txb = Backing::new(4096);
        let rxb = Backing::new(4096);
        let mut switch = Writer::open(rxb.pa(), 4096).unwrap();
        let mut nic = Nic::over(
            Writer::open(txb.pa(), 4096).unwrap(),
            Reader::open(rxb.pa(), 4096).unwrap(),
            Peek::new(rxb.pa(), 4096),
            mac_of(2),
        );
        switch.write(Kind::FRAME, &[5; 60]).unwrap();
        let mut buf = [0u8; 1600];
        assert_eq!(nic.receive(&mut buf), Some(60));
        assert_eq!(nic.receive(&mut buf), None);
        assert_eq!(nic.transmit(&[]), Err(TxError::Size(0)));
        assert_eq!(nic.transmit(&[0; 3000]), Err(TxError::Size(3000)));
        assert_eq!(nic.mac(), Mac([2, 0, 0, 0, 0, 2]));
    }

    #[test]
    fn rx_poll_parses_every_form() {
        assert_eq!(RxPoll::parse(""), RxPoll::DEFAULT);
        let fixed = RxPoll::parse("300us");
        assert_eq!(
            (fixed.lo, fixed.hi, fixed.hold),
            (Duration::from_micros(300), Duration::from_micros(300), 0)
        );
        let napi = RxPoll::parse("300us:5ms");
        assert_eq!(
            (napi.lo, napi.hi, napi.hold),
            (Duration::from_micros(300), Duration::from_millis(5), 0)
        );
        let held = RxPoll::parse("1ms:1s:8");
        assert_eq!(
            (held.lo, held.hi, held.hold),
            (Duration::from_millis(1), Duration::from_secs(1), 8)
        );
        // Een hi onder lo telt niet; rommel valt terug op 300 µs vast.
        assert_eq!(RxPoll::parse("5ms:1ms").hi, Duration::from_millis(5));
        assert_eq!(RxPoll::parse("junk").lo, Duration::from_micros(300));
    }

    #[test]
    fn rx_poll_backs_off_after_hold_and_caps() {
        let p = RxPoll::DEFAULT;
        let mut d = p.lo;
        for empty in 1..=4 {
            d = p.next(d, empty);
        }
        assert_eq!(d, p.lo); // de eerste vier lege rondes blijven scherp
        d = p.next(d, 5);
        assert_eq!(d, Duration::from_micros(600));
        for empty in 6..40 {
            d = p.next(d, empty);
        }
        assert_eq!(d, p.hi);
    }

    #[test]
    fn net_plan_and_window_scale() {
        assert_eq!(slot_ip(2), [10, 100, 0, 3]);
        assert_eq!(host_ip(), [10, 100, 0, 1]);
        assert_eq!(mac_of(0), Mac([2, 0, 0, 0, 0, 0]));
        assert_eq!(ws_shift_for(0xffff), 0);
        assert_eq!(ws_shift_for(0x1_0000), 1);
        assert_eq!(ws_shift_for(1 << 20), 5);
        assert_eq!(ws_shift_for(u64::MAX), 14);
    }

    #[test]
    fn durations_in_the_rxpoll_forms() {
        assert_eq!(parse_duration("300us"), Some(Duration::from_micros(300)));
        assert_eq!(parse_duration("5ms"), Some(Duration::from_millis(5)));
        assert_eq!(parse_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("7ns"), Some(Duration::from_nanos(7)));
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("ms"), None);
        assert_eq!(parse_duration("5h"), None);
    }
}
