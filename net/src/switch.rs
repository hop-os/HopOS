//! De switch-actor: HOP's interne L2-frame-switch over de per-slot
//! frame-ringen (Go: `hopswitch.go`, `gateway.go`).
//!
//! Elke app-core draait een eigen netstack over rauwe Ethernet-frames door
//! zijn frame-ringen; HOP kopieert die frames uitsluitend ring-naar-ring op
//! de dst-MAC. App-naar-app-verkeer raakt nooit een TCP-stack op core 0:
//! "Apps rekenen, HOP sjouwt data."
//!
//! HOP is poort 0 op hetzelfde LAN. ARP voor de gateway (10.100.0.1)
//! beantwoordt de switch zelf; verkeer naar buiten gaat door de NAT
//! ([`crate::nat`]) en de uplink-rij naar de pomp.
//!
//! Eigendom (handboek §1.1): [`Switch`] bezit de poorten, de NAT-tabel en de
//! switch-kant van elke ring als `&mut self`. Aan- en afmelden zijn berichten
//! ([`Command`]); de aanroeper wacht op zijn [`Ack`]. Zo is de actor vanzelf
//! de enige producer per RX-ring en de enige consument per TX-ring, zonder
//! slot. De deur van de executor leest alleen [`Published`], met kale loads.

use crate::nat::{Nat, NatIo, Neighbors, Proto, Uplink};
use crate::plan::{
    HOST_MAC, MAX_LAN_FRAME, UPLINK_MAX_FRAME, host_ip4, mac_port, port_ip4, port_mac,
};
use crate::ring::{InPlace, KIND_FRAME, KIND_UPLINK, Reader, Writer};
use crate::wire::{ET_ARP, ET_IPV4, ET_IPV6, ETH_LEN, be16, be32, byte, mac_at, put_mac, put16};
use crate::{Error, Frame, LogFn, Result, Stats, UPLINK_QUEUE};
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{
    AtomicBool,
    Ordering::{AcqRel, Relaxed, Release},
};
use core::time::Duration;
use executor::{Executor, Sleeper};
use sync::mpsc::Mailbox;
use sync::spsc::{Receiver, Sender};
use sync::{Either, LocalCell, Oneshot, Signal, select, yield_now};

/// Het aantal frames per poort per switch-ronde, zodat één drukke poort de
/// rest niet verhongert.
pub const MAX_BURST: usize = 64;

/// Zoveel plaatsen moet de egress-rij hebben voor de switch nog een record
/// uit een TX-ring leest: de bestemming weet hij pas na het lezen, en één
/// record maakt hoogstens twee uplink-frames (het frame en een ARP-vraag
/// naar een onbekende next-hop).
pub const EGRESS_HEADROOM: usize = 2;

/// Begrensde lokale backpressure: een korte TCP-burst verdwijnt niet stil,
/// en een niet-lezende poort kan de switch niet blokkeren.
const TX_BACKPRESSURE: u64 = 10_000_000;

/// De diepte van de brievenbus van de switch.
pub const COMMANDS: usize = 16;

/// De brievenbus van de switch.
pub type Commands<'a, R, W> = Mailbox<Command<'a, R, W>, COMMANDS>;

/// De bevestiging van een [`Command`]: de actor legt het resultaat erin,
/// de aanroeper wacht erop (`ack.recv().await`). Eén `Ack` per aanroeper,
/// herbruikbaar. Bij `Detach` betekent het antwoord: de switch raakt de
/// ringen gegarandeerd niet meer aan (ze zijn gedropt), dus een ring-herinit
/// mag.
pub type Ack = Oneshot<Result<u32>>;

/// Een bericht aan de switch-actor.
pub enum Command<'a, R, W> {
    /// Koppelt slot `slot` aan de switch met zijn twee ringen (door de
    /// slot-lifecycle, ná de ring-init). De switch wordt eigenaar.
    Attach {
        /// Het slot.
        slot: usize,
        /// De TX-ring (app → switch), leeskant.
        tx: R,
        /// De RX-ring (switch → app), schrijfkant.
        rx: W,
        /// Bevestiging.
        ack: &'a Ack,
    },
    /// Ontkoppelt slot `slot`; de ringen worden gedropt.
    Detach {
        /// Het slot.
        slot: usize,
        /// Bevestiging.
        ack: &'a Ack,
    },
    /// Publiceert node-poort naar slot-poort.
    Publish {
        /// Protocol.
        proto: Proto,
        /// De node-poort.
        node_port: u16,
        /// Het slot.
        slot: usize,
        /// De poort in het slot.
        slot_port: u16,
        /// Bevestiging.
        ack: &'a Ack,
    },
    /// Trekt de publicaties en flows van een slot in.
    UnpublishSlot {
        /// Het slot.
        slot: usize,
        /// Bevestiging.
        ack: &'a Ack,
    },
    /// Zet het externe adres (na DHCP).
    SetUplink {
        /// Het adres.
        uplink: Uplink,
        /// Bevestiging.
        ack: &'a Ack,
    },
    /// De tik van de flow-expiry-taak: veeg de verlopen flows.
    Sweep,
}

/// De poortentabel zoals de deur van de executor hem ziet: per poort het
/// probe-handvat van de TX-ring (plus één; 0 = geen poort), op poortnummer.
///
/// De actor schrijft, de deur leest; allebei op de executor van de OS-core
/// en nooit tegelijk: de deur draait tussen rondes, nooit tijdens de actor
/// (een [`LocalCell`], handboek §1.1). Na een `Detach` ziet geen lezer het
/// oude handvat nog. De tabel groeit met het hoogste slotnummer dat ooit
/// hing, zoals de fd-tabel van Linux: de kern deelt het laagste vrije
/// nummer uit, dus hij is zo lang als er ooit tegelijk slots waren.
pub struct Published<R> {
    tx: LocalCell<Vec<u64>>,
    /// De uplink staat stil ([`Xoff`]): dan is een volle TX-ring geen werk
    /// voor de switch, en belt de deur niet.
    xoff: Xoff,
    _ring: PhantomData<fn() -> R>,
}

/// Maakt plek `i` in `v` (met `none` erachter), faalbaar: een slot dat de
/// switch niet kan dragen, wordt geweigerd ("past niet").
fn reach<T>(v: &mut Vec<T>, i: usize, none: impl FnMut() -> T) -> Result {
    if i >= v.len() {
        let more = i + 1 - v.len();
        v.try_reserve(more)
            .map_err(|_| Error::OutOfMemory(more * core::mem::size_of::<T>()))?;
        v.resize_with(i + 1, none);
    }
    Ok(())
}

impl<R: Reader> Published<R> {
    /// Een lege tabel; voor een `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tx: LocalCell::cell(Vec::new()),
            xoff: Xoff::new(),
            _ring: PhantomData,
        }
    }

    /// De stop van de uplink-rij, voor de pomp (die hem weer opent).
    #[must_use]
    pub fn xoff(&self) -> &Xoff {
        &self.xoff
    }

    /// Ligt er werk in een TX-ring? Alleen kale loads (Go: `switchPending`,
    /// en zie [`Reader::probe`] voor de 1,7M-rondes/s-les van 04-09). Staat
    /// de uplink stil ([`Xoff`]), dan niet: de switch leest dan geen ring,
    /// en een deur die toch belde, liet de OS-core nooit slapen.
    #[must_use]
    pub fn pending(&self) -> bool {
        if self.xoff.is_stopped() {
            return false;
        }
        self.tx.borrow().iter().any(|&v| v != 0 && R::probe(v - 1))
    }

    /// Is de TX-ring van poort `i` zeker leeg, zonder cache-onderhoud gezien
    /// ([`Reader::quiet`])? Dan hoeft een ronde hem niet te openen.
    fn quiet(&self, i: usize) -> bool {
        let v = self.tx.borrow().get(i).copied().unwrap_or(0);
        v != 0 && R::quiet(v - 1)
    }

    /// De deur zelf: ligt er werk, dan de bel van de switch. `true` = er
    /// werd gebeld. De bel is één `set`, en die gaat alleen bij werk: een
    /// CAS in elke idle-ronde zou op de M4 de volgende WFE laten
    /// terugkeren (zie [`pending`](Self::pending)).
    pub fn ring(&self, door: &Signal) -> bool {
        let work = self.pending();
        if work {
            door.set();
        }
        work
    }

    fn set(&self, port: usize, handle: u64) -> Result {
        let mut t = self.tx.borrow_mut();
        reach(&mut t, port, || 0)?;
        if let Some(h) = t.get_mut(port) {
            *h = handle.wrapping_add(1);
        }
        Ok(())
    }

    fn clear(&self, port: usize) {
        if let Some(h) = self.tx.borrow_mut().get_mut(port) {
            *h = 0;
        }
    }
}

/// De stop van de uplink-rij: Linux' `netif_stop_queue` en
/// `netif_wake_queue` (`__QUEUE_STATE_DRV_XOFF`). Heeft de egress-rij geen
/// plaats, dan stopt de switch met lezen uit de TX-ringen van de slots en
/// de host in plaats van frames weg te gooien; de frames blijven daar, de
/// app wacht in `transmit_wait`, en het segment blijft in de zendbuffer van
/// zijn TCP. De pomp opent hem weer zodra hij een frame uit de rij nam, en
/// belt dan de deur van de switch.
///
/// Tot 24-10 gooide de switch een frame bij een volle rij weg, en de pomp
/// een frame bij een volle NIC-ring: elk gat kostte de TCP van de app een
/// go-back-N, soms een RTO van 200 ms (de ORION op de RTL8125, één stroom
/// naar een Mac over wifi: niet boven ~17 MB/s).
///
/// Eén vlag, geen protocol: wie hem zet, kijkt daarna nog één keer of er
/// toch plaats is (de volgorde van `netif_tx_stop_queue` en de hertoets in
/// een driver), dus een wek die net vóór de stop viel, gaat niet verloren.
pub struct Xoff {
    stopped: AtomicBool,
}

impl Xoff {
    /// Open; voor een `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
        }
    }

    /// Staat de rij stil?
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Relaxed)
    }

    /// Zet hem stil; `true` als hij liep (een nieuwe stop).
    pub(crate) fn stop(&self) -> bool {
        !self.stopped.swap(true, AcqRel)
    }

    /// Weer open, zonder bel (de switch zag zelf toch plaats).
    fn start(&self) {
        self.stopped.store(false, Release);
    }

    /// De wek van de pomp: stond hij stil, dan open en de bel van de
    /// switch. Een kale lees eerst, want de pomp roept dit na elke ronde
    /// met frames, en een CAS in een hete lus kost op sommige ARM-cores de
    /// volgende WFE (zie [`Published::pending`]).
    pub fn wake(&self, door: &Signal) {
        if self.stopped.load(Relaxed) && self.stopped.swap(false, AcqRel) {
            door.set();
        }
    }
}

impl Default for Xoff {
    fn default() -> Self {
        Self::new()
    }
}

/// De deur van de executor: de [`Sleeper`] van de OS-core, met vóór en
/// tijdens elke slaap de blik in de TX-ringen (Go: `pumpHook(switchPending,
/// notifySwitch)` in de idle-governor).
///
/// Waarom dit bestaat (gemeten 30-09): de kick van een app (SEV plus HVC 6)
/// haalt de OS-core wel uit zijn WFE of uit de beurt van Hop, maar niemand
/// zette daarna de bel van de switch: `ready()` van de executor kent alleen
/// taken. Elk frame van een app lag dan tot de failsafe van 1 ms (QEMU:
/// `switch by door 2/s, by failsafe 875/s` bij 5.561 kicks/s tijdens de
/// pull, 7,1 MB/s app naar app en een rtt van 2,2 ms; op ijzer ~1 ms per
/// handshake naar de kern). De deur maakt de kick weer een bel.
///
/// Hij wikkelt de slaper van het board: `ready()` wordt "een taak is klaar
/// óf er ligt werk in een TX-ring", en de slaper toetst dat al met de
/// interrupts dicht, vóór de beurt van een bewoner en tussen twee WFE's.
/// Zo blijft de verloren-wek-race dicht: een app die publiceert ná de
/// toets, zet het event-register (SEV) of kickt de OS-core uit zijn beurt.
pub struct Doorbell<'a, R, S> {
    inner: S,
    published: &'a Published<R>,
    door: &'a Signal,
    /// De andere deur van het interne LAN: de outboxen van de slots (de
    /// kern luidt hun servicers; `true` = er werd gebeld).
    outboxes: fn() -> bool,
}

impl<'a, R: Reader, S: Sleeper> Doorbell<'a, R, S> {
    /// De slaper `inner` met de deur over `published` naar `door`, en de
    /// deur van de outboxen.
    pub const fn new(
        inner: S,
        published: &'a Published<R>,
        door: &'a Signal,
        outboxes: fn() -> bool,
    ) -> Self {
        Self {
            inner,
            published,
            door,
            outboxes,
        }
    }
}

impl<R: Reader, S: Sleeper> Sleeper for Doorbell<'_, R, S> {
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        // Werk dat er al lag, ziet de slaper zelf: zijn eerste `ready()`
        // (met de maskers dicht) belt dan en hij keert meteen terug. Een
        // eigen blik hier ervoor was dezelfde blik twee keer (03-10).
        let (published, door, outboxes) = (self.published, self.door, self.outboxes);
        self.inner.sleep(now, until, &|| {
            ready() || published.ring(door) || outboxes()
        });
        // Terug uit een beurt van een bewoner van de OS-core (of een wek, de
        // kick van een app-core): wat hij schreef, meteen naar de switch en
        // de servicers, zonder eerst een lege ronde van de executor en een
        // tweede slaapvraag (03-10).
        published.ring(door);
        outboxes();
    }
}

impl<R: Reader> Default for Published<R> {
    fn default() -> Self {
        Self::new()
    }
}

/// Eén switch-poort: de ringen van een actief slot (of van HOP op poort 0).
struct Port<R, W> {
    tx: R,
    rx: W,
    /// Na één begrensde wacht droppen volgende frames totdat de consument
    /// aantoonbaar weer ruimte heeft gemaakt.
    rx_blocked: bool,
    /// De corrupt-verklaring van de TX-ring is al gemeld: één regel per
    /// leven van de poort, anders verzuipt de console.
    tx_warned: bool,
    /// Het eerste frame met een vreemd bronadres is al gemeld.
    src_warned: bool,
}

/// De vaste instellingen van de switch.
#[derive(Clone, Copy)]
pub struct Config {
    /// Monotone nanoseconden.
    pub now: fn() -> u64,
    /// De console.
    pub log: LogFn,
    /// De doelkick van slot `i` na een leeg→niet-leeg-overgang van zijn
    /// RX-ring (een IPI, of de wek van een app die naar EL2 yieldde).
    pub slot_wake: fn(usize),
    /// Woont de consument van slot `i` op déze core (een bewoner van de
    /// OS-core, zoals Hop)? Dan wacht [`write_rx`](Core::write_rx) niet op
    /// ruimte: die consument kan pas draaien als de executor afgeeft.
    pub resident: fn(usize) -> bool,
    /// De neighbour-tabel van de node-stack, voor de next-hops van de NAT.
    pub neighbors: Neighbors,
}

/// De gedeelde kanten van de switch: wat andere taken van hem zien.
pub struct Wiring<'a, R: Reader, W: Writer> {
    /// De brievenbus.
    pub commands: &'a Commands<'a, R, W>,
    /// De bel: zet hem na elke schrijf in een TX-ring (Go: `Kick`).
    pub door: &'a Signal,
    /// De tabel voor de deur van de executor.
    pub published: &'a Published<R>,
    /// De meetlat.
    pub stats: &'a Stats,
    /// De bel van de host-taak (poort 0).
    pub host_bell: Option<&'a Signal>,
    /// Van de pomp: ontvangen uplink-frames.
    pub ingress: Option<Receiver<'a, Frame, UPLINK_QUEUE>>,
    /// Naar de pomp: frames voor de draad.
    pub egress: Option<Sender<'a, Frame, UPLINK_QUEUE>>,
    /// De bel van de pomp, na een ronde met egress-werk.
    pub pump_bell: Option<&'a Signal>,
}

/// De poorten en de uitgangen: alles wat de NAT als [`NatIo`] ziet.
struct Core<'a, R, W> {
    /// De poorten op nummer (0 is de kern), zo lang als het hoogste slot
    /// dat ooit hing ([`Published`]).
    ports: Vec<Option<Port<R, W>>>,
    cfg: Config,
    stats: &'a Stats,
    host_bell: Option<&'a Signal>,
    egress: Option<Sender<'a, Frame, UPLINK_QUEUE>>,
    pump_bell: Option<&'a Signal>,
    egress_dirty: bool,
}

/// De switch-actor.
pub struct Switch<'a, R: Reader, W: Writer> {
    core: Core<'a, R, W>,
    nat: Nat,
    commands: &'a Commands<'a, R, W>,
    door: &'a Signal,
    published: &'a Published<R>,
    ingress: Option<Receiver<'a, Frame, UPLINK_QUEUE>>,
}

impl<'a, R: Reader, W: Writer> Switch<'a, R, W> {
    /// Een switch zonder poorten. Alloceert de NAT-tabellen (boot).
    pub fn new(cfg: Config, w: Wiring<'a, R, W>) -> Result<Self> {
        Ok(Self {
            core: Core {
                ports: Vec::new(),
                cfg,
                stats: w.stats,
                host_bell: w.host_bell,
                egress: w.egress,
                pump_bell: w.pump_bell,
                egress_dirty: false,
            },
            nat: Nat::new(crate::nat::masq_start((cfg.now)()))?,
            commands: w.commands,
            door: w.door,
            published: w.published,
            ingress: w.ingress,
        })
    }

    /// Hangt HOP's eigen ringen aan poort 0 (bij boot, vóór `run`).
    pub fn attach_host(&mut self, tx: R, rx: W) -> Result {
        self.plug(0, tx, rx)
    }

    /// Hangt ringen aan poort `i`; een oude poort daar wordt gedropt.
    fn plug(&mut self, i: usize, tx: R, rx: W) -> Result {
        reach(&mut self.core.ports, i, || None)?;
        self.published.set(i, tx.probe_handle())?;
        if let Some(p) = self.core.ports.get_mut(i) {
            *p = Some(Port {
                tx,
                rx,
                rx_blocked: false,
                tx_warned: false,
                src_warned: false,
            });
        }
        Ok(())
    }

    /// Koppelt slot `i` (de actor-kant van [`Command::Attach`]; vóór `run`
    /// ook direct te roepen). Een oude poort op dat slot wordt gedropt.
    pub fn attach(&mut self, i: usize, tx: R, rx: W) -> Result {
        if abi::layout::Slot::new(i).is_none() {
            return Err(Error::SlotRange(i));
        }
        self.plug(i, tx, rx)
    }

    /// Ontkoppelt slot `i`: eerst uit de gepubliceerde tabel, dan gedropt.
    pub fn detach(&mut self, i: usize) -> Result {
        if abi::layout::Slot::new(i).is_none() {
            return Err(Error::SlotRange(i));
        }
        self.published.clear(i);
        if let Some(p) = self.core.ports.get_mut(i) {
            *p = None;
        }
        Ok(())
    }

    /// De NAT, voor de toetsen.
    #[cfg(test)]
    pub fn nat(&mut self) -> &mut Nat {
        &mut self.nat
    }

    /// Hangt slot `i` aan de switch?
    #[cfg(test)]
    #[must_use]
    pub fn is_attached(&self, i: usize) -> bool {
        self.core.ports.get(i).is_some_and(Option::is_some)
    }

    /// Voert één commando uit.
    pub fn handle(&mut self, cmd: Command<'a, R, W>) {
        let now = (self.core.cfg.now)();
        match cmd {
            Command::Attach { slot, tx, rx, ack } => ack.put(self.attach(slot, tx, rx).map(|()| 0)),
            Command::Detach { slot, ack } => ack.put(self.detach(slot).map(|()| 0)),
            Command::Publish {
                proto,
                node_port,
                slot,
                slot_port,
                ack,
            } => ack.put(
                self.nat
                    .publish(proto, node_port, slot, slot_port)
                    .map(|()| 0),
            ),
            Command::UnpublishSlot { slot, ack } => {
                self.nat.unpublish_slot(slot);
                ack.put(Ok(0));
            }
            Command::SetUplink { uplink, ack } => {
                self.nat.set_uplink(uplink);
                ack.put(Ok(0));
            }
            Command::Sweep => self.nat.sweep(now),
        }
    }

    fn drain_commands(&mut self) -> bool {
        let mut worked = false;
        while let Some(cmd) = self.commands.try_recv() {
            self.handle(cmd);
            worked = true;
        }
        worked
    }

    /// Eén ronde: alle TX-ringen draineren (hoogstens [`MAX_BURST`] per
    /// poort) en per frame op dst-MAC bezorgen, dan de uplink-ingress. `buf`
    /// is de ene hergebruikte framebuffer: geen allocatie per frame.
    pub fn switch_pass(&mut self, buf: &mut [u8]) -> bool {
        let now = (self.core.cfg.now)();
        let mut worked = !self.published.quiet(0) && self.drain_host();
        self.warn_corrupt(0);
        // Een stille coherente ring niet openen: het lezen van kop en staart
        // kost daar per ring een `dc civac` en twee `dsb sy` (03-10, O6N).
        let published = self.published;
        'ports: for i in 1..self.core.ports.len() {
            if !self.core.attached(i) || published.quiet(i) {
                continue;
            }
            // Past een hele burst in de egress-rij, dan één blik per poort en
            // niet één per record: het pad van slot naar slot leest per
            // record anders twee indexen van een rij die het nooit raakt.
            let burst_fits = self.room_for(MAX_BURST);
            for _ in 0..MAX_BURST {
                // Geen plaats in de egress-rij: niets meer uit de TX-ringen
                // tot de pomp hem weer opent ([`Xoff`]). Ook verkeer van slot
                // naar slot wacht dan even, zoals Linux één rij per device
                // heeft; de uplink-ingress hieronder loopt door.
                if !burst_fits && !self.room() {
                    break 'ports;
                }
                match self.step_slot(i, buf, now) {
                    None => break,
                    Some(w) => worked |= w,
                }
            }
            self.warn_corrupt(i);
        }
        for _ in 0..MAX_BURST {
            let Some(mut fr) = self.ingress.as_mut().and_then(|r| r.try_recv()) else {
                break;
            };
            uplink_in(&mut self.core, &mut self.nat, fr.bytes_mut(), now);
            worked = true;
        }
        worked
    }

    /// Eén record van slot `i`: `None` als zijn ring leeg is, anders of het
    /// een frame was. Een unicast naar een ander slot gaat van ring naar
    /// ring ([`Core::relay`]); al het andere via `buf` en [`forward`], zoals
    /// altijd. De poort is zolang uit de tabel en komt terug vóór `forward`,
    /// want dat pad kan naar de afzender zelf schrijven (het ARP-antwoord
    /// van de gateway, een haarspeld door de NAT).
    fn step_slot(&mut self, i: usize, buf: &mut [u8], now: u64) -> Option<bool> {
        let mut port = self.core.ports.get_mut(i).and_then(Option::take)?;
        let mut slow = None;
        let rec = if port.tx.is_normal() {
            let core = &mut self.core;
            port.tx.read_in_place(MAX_LAN_FRAME, |kind, f| {
                if kind != KIND_FRAME {
                    return false;
                }
                if !core.relay(i, f)
                    && let Some(d) = buf.get_mut(..f.len())
                {
                    d.copy_from_slice(f);
                    slow = Some(f.len());
                }
                true
            })
        } else {
            port.tx.read_into(buf).map(|(kind, n)| {
                slow = (kind == KIND_FRAME).then_some(n);
                kind == KIND_FRAME
            })
        };
        if let Some(p) = self.core.ports.get_mut(i) {
            *p = Some(port);
        }
        if let Some(f) = slow.and_then(|n| buf.get_mut(..n)) {
            forward(&mut self.core, &mut self.nat, i, f, now);
        }
        rec
    }

    /// De TX-ring van poort 0 in de ring zelf: beide kanten zijn de kern,
    /// dus de kopie naar `buf` die een app-ring nodig heeft (een app kan
    /// haar record na de toets herschrijven) is hier loos werk. GEMETEN
    /// 01-10 op de M4 (M16): de switch kostte 238 us per MiB van de kern
    /// naar een app, twee kopieën van elk byte. De poort is zolang uit de
    /// tabel; een frame van poort 0 gaat nooit terug naar poort 0.
    fn drain_host(&mut self) -> bool {
        let Some(mut port) = self.core.ports.get_mut(0).and_then(Option::take) else {
            return false;
        };
        let mut worked = false;
        for _ in 0..MAX_BURST {
            if !self.room() {
                break;
            }
            let (core, nat) = (&mut self.core, &self.nat);
            let rec = port.tx.read_in_place(MAX_LAN_FRAME, |kind, f| match kind {
                // Van poort 0 is er geen NAT-staart: wat `deliver` laat
                // liggen (een frame aan de gateway zelf), vervalt.
                KIND_FRAME => {
                    deliver(core, nat, 0, f);
                    true
                }
                // Extern verkeer van HOP's eigen stack: de draad op.
                KIND_UPLINK => {
                    core.uplink_tx(f);
                    true
                }
                _ => false,
            });
            match rec {
                Some(w) => worked |= w,
                None => break,
            }
        }
        if let Some(p) = self.core.ports.get_mut(0) {
            *p = Some(port);
        }
        worked
    }

    /// Past `records` records in de egress-rij (elk [`EGRESS_HEADROOM`])?
    /// Een blik zonder stop: het snelle pad van [`Switch::switch_pass`].
    fn room_for(&self, records: usize) -> bool {
        self.core
            .egress
            .as_ref()
            .is_none_or(|e| e.free() >= records * EGRESS_HEADROOM)
    }

    /// Mag de switch nog een record uit een TX-ring lezen? Ja als de
    /// egress-rij [`EGRESS_HEADROOM`] plaatsen heeft (of er geen uplink
    /// is). Anders zet hij de uplink stil ([`Xoff`]) en kijkt hij nog één
    /// keer: nam de pomp er intussen een uit (een andere core), dan loopt
    /// hij weer, want diens wek kwam vóór de stop.
    fn room(&mut self) -> bool {
        let has = |c: &Core<'a, R, W>| {
            c.egress
                .as_ref()
                .is_none_or(|e| e.free() >= EGRESS_HEADROOM)
        };
        if has(&self.core) {
            return true;
        }
        let xoff = &self.published.xoff;
        let new = xoff.stop();
        if has(&self.core) {
            xoff.start();
            return true;
        }
        if new {
            self.core.stats.uplink_xoff.fetch_add(1, Relaxed);
        }
        false
    }

    fn warn_corrupt(&mut self, i: usize) {
        let log = self.core.cfg.log;
        if let Some(p) = self.core.ports.get_mut(i).and_then(Option::as_mut)
            && !p.tx_warned
            && let Some(why) = p.tx.corrupt()
        {
            p.tx_warned = true;
            // Uit de deur: een dode ring leest daar voor altijd als "werk"
            // (kop voorbij staart), en dan sliep de OS-core nooit meer.
            self.published.clear(i);
            log(format_args!("HOPOS_NETRING_TX_CORRUPT: slot {i}: {why}"));
        }
    }

    /// Eén doorbell per ronde, niet per frame (Go: `FlushUplinkTX`): de pomp
    /// wekken als er egress-werk ligt.
    fn flush_uplink(&mut self) {
        if self.core.egress_dirty {
            self.core.egress_dirty = false;
            if let Some(b) = self.core.pump_bell {
                b.set();
            }
        }
    }

    /// De lus: commando's, een switch-ronde, de uplink-doorbell, dan
    /// `yield_now` zodat de doelnetstack consumeert vóór een nieuwe volle
    /// burst. Niets te doen: wachten op de bel of een commando, verder
    /// niets (de failsafe van 1 ms is weg sinds de kick en de deur van de
    /// executor elke publicatie zien, docs/sequence-hopos.md §13.3). Keert
    /// niet terug.
    pub async fn run(&mut self, buf: &mut [u8]) {
        loop {
            let cmds = self.drain_commands();
            if self.switch_pass(buf) {
                self.flush_uplink();
                self.core.stats.work_by_door.fetch_add(1, Relaxed);
                // Nog werk in een TX-ring (een burst die de grens raakte, een
                // app die intussen schreef): na de anderen nog een ronde.
                // Anders meteen wachten: een lege tweede ronde was op de
                // OS-core de helft van de kern tussen twee beurten (03-10),
                // en wie daarna schrijft, belt de deur zelf (de pomp, de
                // host-poort, de [`Doorbell`] bij de volgende idle).
                if self.published.pending() || self.ingress.as_ref().is_some_and(|r| !r.is_empty())
                {
                    yield_now().await;
                    continue;
                }
            }
            self.flush_uplink();
            if cmds {
                continue;
            }
            let commands = self.commands;
            if let Either::Left(cmd) = select(commands.recv(), self.door.wait()).await {
                self.handle(cmd);
            }
        }
    }
}

/// De flow-expiry-taak: elke `FLOW_SWEEP_EVERY` een `Sweep` in de
/// brievenbus. De tabel blijft van de actor; deze taak is alleen de klok.
/// Vol = deze tik overslaan (de volgende komt, en een volle pool veegt zelf).
pub async fn flow_expiry<R: Reader, W: Writer, const T: usize, const M: usize>(
    exec: &'static Executor<T, M>,
    commands: &Commands<'_, R, W>,
) {
    let every = Duration::from_nanos(crate::nat::FLOW_SWEEP_EVERY);
    loop {
        exec.after(every).await;
        let _ = commands.try_send(Command::Sweep);
    }
}

impl<R: Reader, W: Writer> Core<'_, R, W> {
    fn port(&mut self, i: usize) -> Option<&mut Port<R, W>> {
        self.ports.get_mut(i).and_then(Option::as_mut)
    }

    fn attached(&self, i: usize) -> bool {
        self.ports.get(i).is_some_and(Option::is_some)
    }

    /// De enige producergrens voor LAN-RX (Go: `writeRXLocked`). Een korte
    /// lokale burst mag wachten tot de consument ruimte maakt; daarna geldt
    /// weer Ethernet: timeout = drop, TCP herstelt.
    ///
    /// Het wachten is een spin, geen `.await`: de consument van een slot-ring
    /// is een andere core, dus spinnen helpt, en de actor blijft zo één
    /// synchrone ronde. Poort 0 wacht niet: zijn consument is de host-taak op
    /// déze core, en die kan pas draaien als de actor afgeeft. Een bewoner
    /// van de OS-core ([`Config::resident`], Hop in slot 1) ook niet, om
    /// dezelfde reden: les van 30-09 (de soak), daar spinde de switch tot
    /// [`TX_BACKPRESSURE`] op een ring die niemand kon legen, met de hele
    /// OS-core stil. Voor hen: droppen en tellen, TCP herstelt.
    fn write_rx(&mut self, i: usize, kind: u32, p: &[u8]) {
        self.write_rx_by(i, |rx| match rx.write_notify(kind, p) {
            Some(notify) => InPlace::Written(notify),
            None => InPlace::Full,
        });
    }

    /// Een unicast van slot `src` naar een ander slot, van zijn TX-ring
    /// direct de RX-ring van het doel in: één kopie op de OS-core in plaats
    /// van twee. GEMETEN 01-10 op de M4 (M19): app naar app op een P-core
    /// (6100 MB/s) was de switch 130 van de 137 ms per 800 MiB bezig, met
    /// de kopie in `buf` en die eruit.
    ///
    /// De keuze valt op de bytes in de ring van de afzender, die hij nog kan
    /// herschrijven; de toets van [`deliver`] (bron-MAC en bron-IP van
    /// `src`, de doel-MAC) daarom op de kopie in de ring van het doel, vóór
    /// de publicatie. Wat daar niet klopt, komt nooit vrij. `false` = niet
    /// voor dit pad of de toets faalde: de aanroeper neemt de gewone weg
    /// (die toetst op zijn eigen kopie en meldt een vreemde bron).
    fn relay(&mut self, src: usize, f: &[u8]) -> bool {
        let n = f.len();
        if n < ETH_LEN || byte(f, 0) & 1 != 0 {
            return false;
        }
        let Some(dst) = mac_port(mac_at(f, 0)) else {
            return false;
        };
        if dst == 0 || dst == src {
            return false;
        }
        if !self.port(dst).is_some_and(|p| p.rx.is_normal()) {
            return false;
        }
        let (to, from) = (port_mac(dst), port_mac(src));
        self.write_rx_by(dst, |rx| {
            rx.write_in_place(n, |d| {
                let d = d.get_mut(..n)?;
                d.copy_from_slice(f);
                let ok = mac_at(d, 0) == to && mac_at(d, 6) == from && valid_source_ip(src, d);
                ok.then_some((KIND_FRAME, n))
            })
        })
    }

    /// De lus van [`write_rx`](Self::write_rx) rond één schrijfpoging:
    /// `attempt` schrijft het record of zegt vol; bij vol wachten of droppen
    /// zoals daar beschreven. `false` = `attempt` gaf [`InPlace::Nothing`]:
    /// er is niets geschreven en niets geteld.
    fn write_rx_by(&mut self, i: usize, mut attempt: impl FnMut(&mut W) -> InPlace) -> bool {
        let now = self.cfg.now;
        let deadline = now().saturating_add(TX_BACKPRESSURE);
        let mut woken = false;
        loop {
            let Some(port) = self.port(i) else {
                return true;
            };
            match attempt(&mut port.rx) {
                InPlace::Written(notify) => {
                    port.rx_blocked = false;
                    if notify {
                        self.wake(i);
                    }
                    return true;
                }
                InPlace::Nothing => return false,
                InPlace::Full => {}
            }
            if port.rx_blocked {
                self.stats.rx_drops.fetch_add(1, Relaxed);
                return true;
            }
            if !woken {
                self.stats.rx_full.fetch_add(1, Relaxed);
                woken = true;
                self.wake(i);
            }
            if i == 0 || (self.cfg.resident)(i) || now() > deadline {
                if let Some(port) = self.port(i) {
                    port.rx_blocked = true;
                }
                self.stats.rx_drops.fetch_add(1, Relaxed);
                return true;
            }
            core::hint::spin_loop();
        }
    }

    fn wake(&self, i: usize) {
        // Dedicated ARM-apps kunnen zelf in WFE staan; hetzelfde generieke
        // event wekt hen zonder dat de switch hun architectuur kent. Een app
        // die naar EL2 yieldde krijgt daarna de doelkick.
        dev::notify();
        if i == 0 {
            if let Some(b) = self.host_bell {
                b.set();
            }
        } else {
            (self.cfg.slot_wake)(i);
        }
    }

    /// Een frame de uplink op, via de egress-rij naar de pomp.
    fn uplink_tx(&mut self, f: &[u8]) {
        if f.len() > UPLINK_MAX_FRAME {
            // Een jumbo van het slot-LAN hoort hier nooit te komen (de stacks
            // klemmen de MSS per bestemming); de NIC-driver zou zijn
            // descriptor overlopen. Tellen en droppen.
            self.stats.nat_oversize.fetch_add(1, Relaxed);
            return;
        }
        let sent = match (self.egress.as_mut(), Frame::from_slice(f)) {
            (Some(tx), Some(fr)) => tx.try_send(fr).is_ok(),
            _ => false,
        };
        if sent {
            self.egress_dirty = true;
        } else {
            self.stats.uplink_tx_drops.fetch_add(1, Relaxed);
        }
    }
}

impl<R: Reader, W: Writer> NatIo for Core<'_, R, W> {
    fn deliver(&mut self, slot: usize, f: &[u8]) {
        if f.len() > MAX_LAN_FRAME || slot < 1 {
            return;
        }
        self.write_rx(slot, KIND_FRAME, f);
    }

    fn uplink_tx(&mut self, f: &[u8]) {
        Core::uplink_tx(self, f);
    }

    fn neighbor(&mut self, dst: u32, now: u64) -> Option<[u8; 6]> {
        (self.cfg.neighbors.resolve)(dst, now)
    }

    fn probe(&mut self, dst: u32, now: u64) {
        (self.cfg.neighbors.probe)(dst, now);
    }

    fn confirm(&mut self, src: u32, mac: [u8; 6], now: u64) {
        (self.cfg.neighbors.confirm)(src, mac, now);
    }

    fn stats(&self) -> &Stats {
        self.stats
    }

    fn log(&self, args: fmt::Arguments<'_>) {
        (self.cfg.log)(args);
    }
}

/// Is dit een IP-multicast-bestemming (01:00:5e IPv4, 33:33 IPv6)?
fn is_ip_multicast(p: &[u8]) -> bool {
    matches!(p, [0x01, 0x00, 0x5e, ..] | [0x33, 0x33, ..])
}

/// Bezorgt één frame op grond van de dst-MAC; meer switch is er niet.
/// Onbekende bestemming of volle ring = drop (Go: `forward`).
fn forward<R: Reader, W: Writer>(
    core: &mut Core<'_, R, W>,
    nat: &mut Nat,
    src: usize,
    p: &mut [u8],
    now: u64,
) {
    if deliver(core, nat, src, p) || src == 0 {
        return;
    }
    // Naar de gateway. Volgorde: eerst 10.100.0.1 zelf ("mijn node"), dan
    // het antwoord van een gepubliceerde poort, anders masquerade.
    if gateway_claim(core, nat, p) || nat.slot_reply(core, src, p, now) {
        return;
    }
    nat.outbound(core, src, p, now);
}

/// Het deel van [`forward`] dat het frame alleen leest: `true` = klaar
/// (bezorgd of gedropt), `false` = een frame van een slot naar de gateway,
/// voor de NAT.
fn deliver<R: Reader, W: Writer>(
    core: &mut Core<'_, R, W>,
    nat: &Nat,
    src: usize,
    p: &[u8],
) -> bool {
    if p.len() < ETH_LEN || p.len() > MAX_LAN_FRAME {
        return true;
    }
    // Bron-MAC-controle: een slot mag alleen zijn ÉIGEN MAC gebruiken. De
    // switch weet uit welke ring hij dit frame las en de nummering is
    // deterministisch, dus dit is een gratis feit, geen leertabel die te
    // vergiftigen valt. Zonder deze regel kan een slot ARP-antwoorden geven
    // namens een adres dat niet van hem is: precies de aanval die je niet
    // wilt op een node waar HOP toetsaanslagen rondstuurt. Poort 0 is HOP
    // zelf, de vertrouwde kant.
    if src >= 1 && (mac_at(p, 6) != port_mac(src) || !valid_source_ip(src, p)) {
        core.stats.slot_src_drops.fetch_add(1, Relaxed);
        let log = core.cfg.log;
        if let Some(port) = core.ports.get_mut(src).and_then(Option::as_mut)
            && !port.src_warned
        {
            port.src_warned = true;
            log(format_args!(
                "HOPOS_NETSWITCH_SRC_DROP: slot {src}: src mac {} (slot mac {}), ip ok {}, {} bytes, ethertype {:#06x}",
                netdev::Mac(mac_at(p, 6)),
                netdev::Mac(port_mac(src)),
                valid_source_ip(src, p),
                p.len(),
                be16(p, 12)
            ));
        }
        return true;
    }
    if byte(p, 0) & 1 != 0 {
        // Broadcast/multicast (ARP): iedereen behalve de bron.
        if arp_reply_gateway(core, src, p) {
            return true; // who-has de gateway? HOP antwoordt zelf
        }
        for i in 1..core.ports.len() {
            if i != src && core.attached(i) {
                core.write_rx(i, KIND_FRAME, p);
            }
        }
        // IP-multicast (mDNS, matter, NDP) gaat óók het LAN op: de scope is
        // de hele link. Broadcast en het interne ARP-verkeer blijven binnen
        // (dat lekt anders de 10.100-namen).
        if is_ip_multicast(p) && nat.uplink().is_some() {
            core.uplink_tx(p);
        }
        return true;
    }
    let Some(dst) = mac_port(mac_at(p, 0)) else {
        // Geen switch-MAC. IPv6-unicast van een slot naar een LAN-buur gaat
        // als écht L2-frame de NIC op, mét de slot-MAC als bron: v6 kent
        // geen NAT-pad, NDP heeft de slot al als buur geadverteerd. IPv4
        // kent alleen de switch-MAC's (NAT is de uitweg).
        if be16(p, 12) == ET_IPV6 && nat.uplink().is_some() {
            core.uplink_tx(p);
        }
        return true;
    };
    if dst == 0 {
        return false;
    }
    if dst != src && core.attached(dst) {
        core.write_rx(dst, KIND_FRAME, p);
    }
    true
}

/// Bindt L3 aan dezelfde slotidentiteit als de bron-MAC. Daardoor is het
/// remote IP van een verbinding met 10.100.0.1 een betrouwbare capability
/// voor precies dat slot: een app kan niet de volumes of credentials van een
/// buur lenen door diens adres als bron te schrijven.
fn valid_source_ip(src: usize, p: &[u8]) -> bool {
    let want = port_ip4(src);
    if p.len() >= ETH_LEN + 20 && be16(p, 12) == ET_IPV4 {
        return be32(p, ETH_LEN + 12) == want;
    }
    if p.len() >= ETH_LEN + 28 && be16(p, 12) == ET_ARP {
        return mac_at(p, ETH_LEN + 8) == port_mac(src) && be32(p, ETH_LEN + 14) == want;
    }
    true
}

/// Hoort dit gateway-frame bij HOP's poort 0? Ja voor IPv4 naar het
/// gateway-IP, behalve een gepubliceerde poort: die is op 10.100.0.1 net zo
/// bereikbaar als op het node-IP, zonder dat een app dat IP hoeft te kennen.
/// `true` = bezorgd.
fn gateway_claim<R: Reader, W: Writer>(core: &mut Core<'_, R, W>, nat: &Nat, p: &[u8]) -> bool {
    if !core.attached(0) || p.len() < ETH_LEN + 20 || be16(p, 12) != ET_IPV4 {
        return false;
    }
    if be32(p, ETH_LEN + 16) != host_ip4() {
        return false; // IPv4 naar elders: NAT-terrein
    }
    if nat.published_here(p) {
        return false; // een gepubliceerde poort: de hairpin, zoals op het node-IP
    }
    core.write_rx(0, KIND_FRAME, p);
    true
}

/// Beantwoordt een ARP-request voor de gateway namens HOP, in de RX-ring van
/// de vrager; `true` = afgehandeld. Andere ARP's (slot naar slot) worden
/// geflood; die beantwoordt het doelslot zelf.
fn arp_reply_gateway<R: Reader, W: Writer>(
    core: &mut Core<'_, R, W>,
    src: usize,
    p: &[u8],
) -> bool {
    if src < 1 || !core.attached(src) {
        return false;
    }
    if p.len() < ETH_LEN + 28 || be16(p, 12) != ET_ARP {
        return false;
    }
    let a = ETH_LEN;
    // Ethernet/IPv4-request (oper = 1) naar het gateway-IP?
    if be16(p, a) != 1 || be16(p, a + 2) != 0x0800 || be16(p, a + 6) != 1 {
        return false;
    }
    if be32(p, a + 24) != host_ip4() {
        return false;
    }
    let sha = mac_at(p, a + 8);
    let spa = be32(p, a + 14);
    let mut r = [0u8; ETH_LEN + 28];
    put_mac(&mut r, 0, &sha);
    put_mac(&mut r, 6, &HOST_MAC);
    put16(&mut r, 12, ET_ARP);
    put16(&mut r, a, 1);
    put16(&mut r, a + 2, 0x0800);
    r[a + 4] = 6;
    r[a + 5] = 4;
    put16(&mut r, a + 6, 2); // reply
    put_mac(&mut r, a + 8, &HOST_MAC);
    crate::wire::put32(&mut r, a + 14, host_ip4());
    put_mac(&mut r, a + 18, &sha);
    crate::wire::put32(&mut r, a + 24, spa);
    core.write_rx(src, KIND_FRAME, &r);
    true
}

/// Eén frame van de uplink-NIC (Go: de lus in `Uplink.Receive`): multicast
/// floodt naar álle slots, IPv6-unicast op een slot-MAC gaat naar dat slot,
/// de NAT claimt de rest die van een app is, en wat overblijft is voor HOP's
/// eigen stack (poort 0, soort [`KIND_UPLINK`]).
fn uplink_in<R: Reader, W: Writer>(
    core: &mut Core<'_, R, W>,
    nat: &mut Nat,
    f: &mut [u8],
    now: u64,
) {
    if f.len() >= ETH_LEN && is_ip_multicast(f) {
        // HOP's eigen stack joint geen groepen; de slot-stacks filteren zelf
        // op lidmaatschap, dus dit is een kopie per aangesloten slot.
        for i in 1..core.ports.len() {
            core.deliver(i, f);
        }
        return;
    }
    if f.len() >= ETH_LEN
        && be16(f, 12) == ET_IPV6
        && let Some(dst) = mac_port(mac_at(f, 0)).filter(|&d| d >= 1)
    {
        // De terugweg van het IPv6-L2-pad: een LAN-buur antwoordt de slot
        // rechtstreeks op de MAC die NDP adverteerde.
        core.deliver(dst, f);
        return;
    }
    // ARP claimt de NAT nooit: de neighbour-tabel is van de node-stack.
    if nat.inbound(core, f, now) {
        return;
    }
    core.write_rx(0, KIND_UPLINK, f);
}

#[cfg(test)]
mod tests;
