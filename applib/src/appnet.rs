//! De netstack van een app: `leannet` over de frame-ringen van zijn slot,
//! met async handvatten ([`TcpListener`], [`TcpStream`], [`UdpSocket`]) en
//! de system-API-client over een echte verbinding.
//!
//! De port van Go's `appnet.Up`. De vorm is die van het handboek (§1.1): de
//! [`Stack`] is één actor-staat in een [`Net`], en precies één taak drijft
//! hem ([`Net::drive`]): frames uit de RX-ring naar `receive`,
//! `poll_transmit` naar de TX-ring, en slapen tot `next_timeout`, de
//! deurbel of een wek van de stack zelf (een write, een close). De
//! handvatten lenen de stack alleen binnen één poll: probeer, en bij
//! `WouldBlock` de waker van deze taak op het handvat registreren. Een
//! lening over een `.await` bestaat hier niet.
//!
//! Het interne net is deterministisch, dus daar wordt niets geresolved: het
//! slot-IP en de MAC komen uit het slotnummer (`abi::layout`), de gateway is
//! de kern (10.100.0.1) als statische buur, en de DNS-server staat in de env
//! (`DNS`, of Go's `HOP_DNS`; voor Hop zet de kern daar de server uit zijn
//! lease, `hopos/src/config.rs`).
//! Namen buiten het slot-LAN lost [`resolve`] op: één A-vraag per keer over
//! UDP naar die server ([`dns`]). Het bufferbudget is een achtste van de
//! RAM-declaratie, geklemd op 1 tot 16 MiB (`NET_BUDGET` in de env wint).
//!
//! Deadlines lopen op het timerwiel van de executor (`Exec::until`), niet in
//! de stack: een handvat met een deadline racet zijn op tegen een timer, en
//! wie verliest ruimt op met `Drop`.
//!
//! Een app die stopt, sluit eerst netjes ([`Net::shutdown`], via
//! `App::shutdown`): elk open handvat dicht, FIN na de data, en de pomp
//! draait door tot elke FIN bevestigd is of de grens verstrijkt. Daarvoor
//! houdt de [`Net`] een vaste tabel bij van wat de app open heeft.
//!
//! Dit module bezit de stack van de app (na [`up`], te vinden via [`net`]),
//! de tabel van open handvatten en de RX-bel; de ringen
//! zelf zijn van de [`Nic`] die de pomp-taak bezit.

use crate::app::App;
use crate::clock;
use crate::contract::{NET_MTU, NET_RING_DATA_CAP};
use crate::log;
use crate::net::{Nic, PUMP_EARLY, PUMP_TIMER, RxPoll, host_ip, mac_of, slot_ip, ws_shift_for};
use crate::rt::{EXEC, Exec};
use crate::sys::{self, ConnError};
use alloc::vec::Vec;
use core::cell::{Cell, OnceCell, RefCell};
use core::fmt;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::sync::atomic::Ordering::Relaxed;
use core::task::{Poll, Waker};
use core::time::Duration;
use leannet::{Config, ListenHandle, Stack, TcpHandle, Udp6Handle, UdpHandle};
use sync::{Local, Signal, yield_now};

pub mod dns;

pub use dns::DnsError;
pub use leannet::{Endpoint, Endpoint6, Error as StackError, Stats, TcpState};

/// Het adres van de kern op het slot-LAN (de gateway).
pub const HOST: [u8; 4] = abi::layout::HOST_IP4.to_be_bytes();

/// Het budget zonder env: een achtste van de RAM-declaratie, niet minder
/// dan dit. Een welcome-app van 16 MB buffert zo tot 2 MB (Go, 12-08).
pub const BUDGET_MIN: usize = 1 << 20;

/// En niet meer dan dit, ook niet op een dikke server-partitie.
pub const BUDGET_MAX: usize = 16 << 20;

/// Zoveel frames leest de pomp per ronde voor hij eerst zendt en de rest
/// een beurt geeft. Zonder die grens komen de ACK's pas als de zender zijn
/// hele venster kwijt is (20-09, een 1 Gbit-upload: 216.147 segmenten in,
/// 298 ACK's uit, 39 MB/s).
pub const RX_BATCH: usize = 16;

/// Zoveel frames zendt de pomp achter elkaar voor hij een beurt afgeeft.
pub const TX_BATCH: usize = 16;

/// Zoveel rondes van de executor geeft de pomp na een ontvangst hooguit aan
/// de taken die hij wekte, voor hij zendt ([`Net::readers`]). Een gewekte
/// taak draait in de ronde na de wek; staat de pomp in die ronde vóór hem,
/// dan is er één ronde meer nodig. Meer niet: een taak die altijd klaar is
/// (BURN, ~0,3 ms per ronde) houdt de ACK zo hooguit één ronde langer op
/// dan zijn eigen antwoord toch al kostte.
pub const READER_ROUNDS: u32 = 2;

/// De ruimte die één maximaal frame in een slot-ring inneemt: kop en
/// payload, op 8 afgerond.
const FRAME_ROOM: u64 = (abi::ring::REC_HDR + NET_MTU as u64 + 14).next_multiple_of(8);

/// De kortste slaap van de pomp. `next_timeout` geeft "nu" zolang er iets
/// in een rij ligt dat nog op een route wacht; zonder deze bodem spint de
/// pomp daar op.
pub const SPIN_GUARD: Duration = Duration::from_micros(50);

/// Hoe lang een dial naar de kern mag duren voor de system-client opgeeft.
pub const SYS_DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Hoe lang [`TcpStream::flush`] wacht als de stream geen deadline heeft.
/// Het slot-LAN is geheugen: een ACK van de kern komt in microseconden, dus
/// wat na 200 ms nog open staat, wacht op iets anders dan de draad.
pub const FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

/// Om de zoveel kijkt een wachtende flush of shutdown opnieuw. Wat hij
/// afwacht (een andere verbinding die stil wordt, een budget dat terugkomt)
/// wekt zijn eigen taak niet; een koud pad mag daarom pollen.
pub const SETTLE_TICK: Duration = Duration::from_millis(1);

/// Hoe lang één DNS-vraag op antwoord wacht voor de ene herhaling. Een
/// resolver op het LAN antwoordt in milliseconden, een koude recursieve
/// lookup bij de provider in honderden; drie seconden is ruim, en twee
/// pogingen houden een verloren datagram onder de zes seconden.
pub const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// Hoeveel DNS-vragen [`Net::resolve`] stuurt: de eerste en één herhaling.
pub const DNS_ATTEMPTS: u8 = 2;

/// De vloer van de tabel van open handvatten.
pub const OPEN_MIN: usize = 64;

/// Het plafond van die tabel. Een verbinding kost minstens 20 KiB budget
/// (de vloeren van leannet: 16 KiB RX, 4 KiB TX), dus het grootste budget
/// ([`BUDGET_MAX`]) draagt er ruim 800.
pub const OPEN_MAX: usize = 1024;

/// De maat van de tabel bij `budget`: één plek per 16 KiB budget, binnen
/// [`OPEN_MIN`]..[`OPEN_MAX`]. Meer verbindingen dan dat past toch niet.
#[must_use]
pub const fn open_slots_for(budget: usize) -> usize {
    let n = budget >> 14;
    if n < OPEN_MIN {
        OPEN_MIN
    } else if n > OPEN_MAX {
        OPEN_MAX
    } else {
        n
    }
}

/// Waarom een netwerk-op niet lukte.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// [`up`] is nog niet gedraaid.
    NotUp,
    /// [`up`] draaide al.
    AlreadyUp,
    /// Het slot is onbekend (0: een image buiten de kern om); zonder slot is
    /// er geen IP.
    NoSlot,
    /// De stack was al geleend (een op vanuit een waker of een `Drop`
    /// midden in een andere op); niets gedaan.
    Busy,
    /// De stack weigerde.
    Stack(StackError),
    /// De deadline van het handvat verstreek.
    Timeout,
    /// De frame-ringen liggen niet waar ze horen.
    Nic(abi::Error),
    /// De heap kon de framebuffer niet geven.
    OutOfMemory {
        /// Gevraagde bytes.
        bytes: usize,
    },
    /// De pomp-taak kon niet gespawnd worden.
    Spawn,
    /// De app sluit af ([`Net::shutdown`]); er gaat niets nieuws meer open.
    ShuttingDown,
    /// Een naam werd geen adres ([`resolve`]).
    Dns(DnsError),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUp => f.write_str("network is not up"),
            Self::AlreadyUp => f.write_str("network is already up"),
            Self::NoSlot => f.write_str("no slot number, so no address"),
            Self::Busy => f.write_str("stack busy (re-entrant use)"),
            Self::Stack(e) => write!(f, "stack: {e}"),
            Self::Timeout => f.write_str("deadline exceeded"),
            Self::Nic(e) => write!(f, "frame rings: {e}"),
            Self::OutOfMemory { bytes } => write!(f, "out of memory for {bytes} bytes"),
            Self::Spawn => f.write_str("cannot spawn the pump task"),
            Self::ShuttingDown => f.write_str("the app is shutting down"),
            Self::Dns(e) => write!(f, "{e}"),
        }
    }
}

impl From<StackError> for NetError {
    fn from(e: StackError) -> Self {
        Self::Stack(e)
    }
}

impl From<DnsError> for NetError {
    fn from(e: DnsError) -> Self {
        Self::Dns(e)
    }
}

/// Het resultaat van een netwerk-op.
pub type Result<T = (), E = NetError> = core::result::Result<T, E>;

/// De stack van deze app, gezet door [`up`].
static NET: Local<OnceCell<Net>> = Local::new(OnceCell::new());

/// De bel van de RX-pomp: de idle van de app-core belt hem als er RX ligt.
static BELL: Signal = Signal::new();

/// De stack van deze app, als [`up`] gedraaid heeft.
#[must_use]
pub fn net() -> Option<&'static Net> {
    NET.get().get()
}

/// Het IPv4-adres van `host` over de stack van deze app: een naam die al
/// een adres is meteen, anders een A-vraag aan de DNS-server uit de env
/// ([`Net::resolve`]).
pub async fn resolve(host: &str) -> Result<[u8; 4]> {
    if let Some(ip) = parse_ip4(host) {
        return Ok(ip);
    }
    net().ok_or(NetError::NotUp)?.resolve(host).await
}

/// Abonneert de stack van deze app op multicastgroep `group`
/// ([`Net::join_group`]).
pub fn join_group(group: [u8; 4]) -> Result {
    net().ok_or(NetError::NotUp)?.join_group(group)
}

/// Het budget: `env` (bytes, of met `k`/`m`) als die er is en klopt, anders
/// een achtste van `ram_size`, geklemd op [`BUDGET_MIN`]..[`BUDGET_MAX`].
#[must_use]
pub fn budget_for(ram_size: u64, env: Option<&str>) -> usize {
    if let Some(n) = env.and_then(parse_size).filter(|&n| n > 0) {
        return n;
    }
    usize::try_from(ram_size / 8)
        .unwrap_or(BUDGET_MAX)
        .clamp(BUDGET_MIN, BUDGET_MAX)
}

/// Een maat als `4194304`, `512k` of `4m`.
fn parse_size(s: &str) -> Option<usize> {
    let (num, shift) = match s.as_bytes().last()? {
        b'k' | b'K' => (s.get(..s.len() - 1)?, 10),
        b'm' | b'M' => (s.get(..s.len() - 1)?, 20),
        _ => (s, 0),
    };
    num.parse::<usize>().ok()?.checked_mul(1 << shift)
}

/// Een IPv4-adres als `10.100.0.1`.
#[must_use]
pub fn parse_ip4(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut ip {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(ip)
}

/// Een `ip:poort` als `10.100.0.3:9000`; poort 0 is geen adres.
#[must_use]
pub fn parse_addr(s: &str) -> Option<([u8; 4], u16)> {
    let (ip, port) = s.trim().rsplit_once(':')?;
    Some((parse_ip4(ip)?, port.parse().ok().filter(|&p| p != 0)?))
}

/// De stack-config van slot `slot` met budget `budget`: het slot-IP en de
/// MAC uit het netplan, de kern als gateway.
#[must_use]
pub fn slot_config(slot: u64, budget: usize) -> Config {
    Config {
        ip: slot_ip(slot),
        prefix: u8::try_from(abi::layout::NET_PREFIX).unwrap_or(24),
        mac: mac_of(slot).0,
        gw: host_ip(),
        budget,
        // Het venster dat wij adverteren is wat de kern in één burst in onze
        // RX-ring mag zetten. Groter dan de ring laat de switch met
        // tegendruk op ons wachten (03-09: 459 naar 155 MB/s), dus de ring
        // min een PAD aan de rand (hooguit één frame) en één frame speling.
        // Tot 01-10 was het de halve ring, maar het venster is de rem van
        // één stroom: zijn rondgang (zender, switch, lezer) bepaalt de
        // doorvoer, en twee stromen haalden samen het dubbele (O6N: één
        // stroom 1434, met dit venster 1749 MB/s). Veel verbindingen tegelijk
        // vullen de ring ook met de halve ring al (04-09: 128x rx-full bij
        // vier hameraars); TCP herstelt een drop.
        max_buf_per_conn: usize::try_from(NET_RING_DATA_CAP - 2 * FRAME_ROOM).unwrap_or(usize::MAX),
        adv_ws: ws_shift_for(budget as u64 / 4),
        mtu: NET_MTU,
        // Het slot-LAN is geheugen, geen draad: geen checksums. De kern-kant
        // moet hetzelfde zeggen, anders gooit hij onze frames weg.
        link_trusted: true,
        ..Config::default()
    }
}

/// Brengt de netstack van `app` op: de stack, de gateway als statische
/// buur en de pomp-taak op [`EXEC`]. Geeft de stack, die daarna ook via
/// [`net`] te vinden is.
pub fn up(app: &'static App) -> Result<&'static Net> {
    if net().is_some() {
        return Err(NetError::AlreadyUp);
    }
    if app.slot() == 0 {
        return Err(NetError::NoSlot);
    }
    let nic = Nic::open(app).map_err(NetError::Nic)?;
    let budget = budget_for(app.ram_size(), app.env("NET_BUDGET"));
    let exec: &'static Exec = EXEC.get();
    // Het geheim van de ISS (RFC 6528) en de eerste efemere poort: uit de
    // DRBG van de app (het zaad van de kern plus jitter, `rand`). Elke
    // levensduur een vers geheim, dus na een flip geen hergebruikt tupel.
    let secret = crate::rand::Rng::open(app).array::<16>();
    let dns = app.env("DNS").or(app.env("HOP_DNS")).and_then(parse_ip4);
    let fresh =
        Net::new(slot_config(app.slot(), budget), secret, exec, clock::now_ns)?.with_dns(dns);
    fresh.seed_neighbor(host_ip(), mac_of(0).0)?;
    let buf = frame_buf(fresh.frame_len())?;
    if NET.get().set(fresh).is_err() {
        return Err(NetError::AlreadyUp);
    }
    let Some(net) = net() else {
        return Err(NetError::NotUp);
    };
    let poll = RxPoll::parse(app.env("RXPOLL").unwrap_or(""));
    if let Err(e) = exec.spawn(net.drive(nic, buf, &BELL, poll)) {
        log!("appnet: cannot spawn the pump: {e} HOPOS_APPNET_SPAWN");
        return Err(NetError::Spawn);
    }
    let [a, b, c, d] = net.ip();
    // Hoe de frame-ringen kopiëren (zie `Nic::open`): op ijzer de eerste
    // vraag als de doorvoer van een app verandert.
    let rings = match crate::mmu::ring_coherence(app.ctrl().on_kern_hart()) {
        crate::ring::Coherence::Hardware => "wb",
        crate::ring::Coherence::Maintained => "maintained",
    };
    log!(
        "appnet: up ip={a}.{b}.{c}.{d} budget={budget} mtu={NET_MTU} rings={rings} HOPOS_APPNET_UP"
    );
    Ok(net)
}

/// De framebuffer van de pomp, één keer en faalbaar: de MTU van het
/// slot-LAN is 64 KiB, te groot voor de stack van een taak.
fn frame_buf(len: usize) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len)
        .map_err(|_| NetError::OutOfMemory { bytes: len })?;
    v.resize(len, 0);
    Ok(v)
}

/// Waarom de pomp wakker werd.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Wake {
    /// De RX-bel.
    Bell,
    /// De timer: poll-ronde of een deadline van de stack.
    Timer,
    /// De uitstelbare ronde van `lo`: de core bleef bezig, dus de slaper
    /// (en met hem de deurbel) kwam niet aan de beurt.
    Busy,
    /// De stack (een write, een close, een accept) of een loze wek.
    Stack,
}

/// Een netstack en de executor waarop zijn taken wachten.
///
/// De stack is een actor-staat: [`Net::drive`] pompt hem, de handvatten
/// lenen hem per poll. Een `RefCell` in een `Local` (na [`up`]) of, in een
/// test, in een eigen `&'static`.
pub struct Net {
    stack: RefCell<Stack>,
    exec: &'static Exec,
    now: fn() -> u64,
    ip: [u8; 4],
    frame_len: usize,
    dns: Option<[u8; 4]>,
    /// Telt de DNS-vragen, zodat twee vragen in dezelfde klok-tik toch een
    /// ander id krijgen.
    dns_seq: Cell<u16>,
    /// Wat de app open heeft, zodat [`Net::shutdown`] het kan sluiten: een
    /// vaste tabel, één keer gealloceerd, die nooit groeit. Een lening
    /// duurt één statement.
    open: RefCell<Vec<Option<Open>>>,
    /// Handvatten die niet in de tabel pasten.
    untracked: Cell<u64>,
    /// [`Net::shutdown`] is begonnen: niets nieuws meer open.
    closing: Cell<bool>,
    /// Pomp-rondes die eindigden met een lege `poll_transmit`: alles wat
    /// toen klaarstond, staat op de TX-ring. [`TcpStream::flush`] wacht op
    /// een ronde na zijn eigen writes.
    tx_rounds: Cell<u64>,
}

/// Een handvat dat de app open heeft.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Open {
    Tcp(TcpHandle),
    Listen(ListenHandle),
    Udp(UdpHandle),
    Udp6(Udp6Handle),
}

/// Wat [`Net::shutdown`] deed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Drain {
    /// Gesloten handvatten: verbindingen, listeners en UDP-sockets.
    pub closed: usize,
    /// Elke verbinding gaf haar buffers terug, dus elke FIN is bevestigd
    /// en elk datagram is de deur uit.
    pub drained: bool,
    /// Hoe lang het afscheid duurde, in microseconden.
    pub waited_us: u64,
    /// Handvatten die de tabel niet kon volgen; die bleven open.
    pub untracked: u64,
}

impl Net {
    /// Een stack met config `cfg` en ISS-geheim `secret`, met zijn timers op
    /// `exec` en zijn tijd uit `now` (dezelfde klok als die van `exec`).
    pub fn new(
        cfg: Config,
        secret: [u8; 16],
        exec: &'static Exec,
        now: fn() -> u64,
    ) -> Result<Self> {
        let stack = Stack::new(cfg, secret)?;
        let slots = open_slots_for(cfg.budget);
        let mut open = Vec::new();
        open.try_reserve_exact(slots)
            .map_err(|_| NetError::OutOfMemory {
                bytes: slots * core::mem::size_of::<Option<Open>>(),
            })?;
        open.resize(slots, None);
        Ok(Self {
            ip: cfg.ip,
            frame_len: stack.frame_len(),
            stack: RefCell::new(stack),
            exec,
            now,
            dns: None,
            dns_seq: Cell::new(0),
            open: RefCell::new(open),
            untracked: Cell::new(0),
            closing: Cell::new(false),
            tx_rounds: Cell::new(0),
        })
    }

    /// Zet de DNS-server die de app meekreeg.
    #[must_use]
    pub fn with_dns(mut self, dns: Option<[u8; 4]>) -> Self {
        self.dns = dns;
        self
    }

    /// Het eigen IPv4.
    #[must_use]
    pub const fn ip(&self) -> [u8; 4] {
        self.ip
    }

    /// De DNS-server uit de env, als die er was.
    #[must_use]
    pub const fn dns(&self) -> Option<[u8; 4]> {
        self.dns
    }

    /// De maat van een framebuffer voor deze stack (MTU plus kop).
    #[must_use]
    pub const fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// De tellers van de stack.
    pub fn stats(&self) -> Result<Stats> {
        self.with(|st| st.stats())
    }

    /// Nu, in nanoseconden op de klok van de stack.
    fn now(&self) -> u64 {
        (self.now)()
    }

    /// Leent de stack voor één synchrone op. De lening eindigt in deze
    /// functie; een tweede lener (een `Drop` midden in een op) krijgt
    /// [`NetError::Busy`] in plaats van een paniek.
    fn with<R>(&self, f: impl FnOnce(&mut Stack) -> R) -> Result<R> {
        let mut st = self.stack.try_borrow_mut().map_err(|_| NetError::Busy)?;
        Ok(f(&mut st))
    }

    /// Zet een statische buur (de gateway, of een vaste peer).
    pub fn seed_neighbor(&self, ip: [u8; 4], mac: [u8; 6]) -> Result {
        let now = self.now();
        self.with(|st| st.seed_neighbor(ip, mac, now))?
            .map_err(NetError::Stack)
    }

    /// De eigenaar-taak: hangt de deurbel aan en pompt de stack over `nic`
    /// met framebuffer `buf` ([`Net::frame_len`] bytes). Keert nooit terug.
    pub async fn drive(
        &'static self,
        mut nic: Nic,
        mut buf: Vec<u8>,
        bell: &'static Signal,
        poll: RxPoll,
    ) {
        nic.watch_rx(bell);
        self.pump(&mut nic, &mut buf, bell, poll).await;
    }

    /// De lus van [`Net::drive`], zonder de deurbel (die is van de idle van
    /// de app-core, en een test heeft er geen).
    async fn pump(
        &'static self,
        nic: &mut Nic,
        buf: &mut [u8],
        bell: &'static Signal,
        poll: RxPoll,
    ) {
        let mut d = poll.lo;
        let mut empty: u32 = 0;
        let mut corrupt_logged = false;
        loop {
            let got = self.ingest(nic, buf);
            let busy = got > 0 && got < RX_BATCH && self.readers().await;
            self.transmit(nic, buf).await;
            if busy {
                // De core blijft bezig na wat we net zonden: niet wachten
                // tot de idle-yield of `TURN_CAP` (OS-core, `net::TX_OWED`).
                crate::net::kick_owed();
            }
            if got == RX_BATCH {
                // Er ligt waarschijnlijk meer; eerst de rest een beurt.
                yield_now().await;
                continue;
            }
            if got > 0 {
                d = poll.lo;
                empty = 0;
                continue;
            }
            // Een dode ring is stil: niets meer te lezen en toch "pending".
            // Eén regel met de reden (de SMP-jacht van 03-09).
            if !corrupt_logged && let Some(why) = nic.rx_corruption() {
                log!("appnet: RX ring corrupt: {why} HOPOS_APPNET_RX_CORRUPT");
                corrupt_logged = true;
            }
            match self.idle(bell, self.deadline(d), poll.lo).await {
                Wake::Bell => {
                    PUMP_EARLY.fetch_add(1, Relaxed);
                }
                Wake::Timer => {
                    PUMP_TIMER.fetch_add(1, Relaxed);
                    empty = empty.saturating_add(1);
                    d = poll.next(d, empty);
                }
                // Geen lege ronde voor de verdubbeling: `d` is de slaap van
                // een stille core, en deze core was niet stil. Wel een kick
                // die nog openstaat: de core sliep al `lo` lang niet, dus
                // de idle-yield komt niet snel (`net::TX_OWED`).
                Wake::Busy => {
                    crate::net::kick_owed();
                }
                // De app praat: het antwoord hoort in het scherpe venster te
                // komen, dus terug naar `lo` (de `hold` van Go).
                Wake::Stack => {
                    d = poll.lo;
                    empty = 0;
                }
            }
        }
    }

    /// Na een ontvangst eerst de taken die hij wekte, dan pas zenden: wat
    /// de lezer terugschrijft, gaat zo in hetzelfde segment als de ACK (de
    /// stack stuurt een kale ACK alleen als er geen data ligt). Zenden we
    /// meteen, dan gaat de ACK alleen de deur uit, en op de OS-core is elk
    /// frame naar de buur een beurt voor die buur: een rondreis van `bench
    /// ping` was zes hops (vraag, ack, lege yield, antwoord, ack, lege
    /// yield) in plaats van twee (03-10, hop-cost5). Hooguit
    /// [`READER_ROUNDS`] rondes van de executor; staat er na de eerste
    /// niemand meer klaar, dan meteen. `true`: na de laatste ronde stond er
    /// nog een taak klaar, de core blijft bezig.
    async fn readers(&self) -> bool {
        for _ in 0..READER_ROUNDS {
            yield_now().await;
            if !self.exec.has_ready() {
                return false;
            }
        }
        true
    }

    /// Hooguit [`RX_BATCH`] frames uit de RX-ring de stack in; het aantal.
    /// Elk frame gaat vanuit de ring zelf de stack in (01-10: één kopie
    /// minder per byte op het ontvangstpad; de lezer van een stroom zat op
    /// het kritieke pad van elk venster). `buf` begrenst de framemaat.
    fn ingest(&self, nic: &mut Nic, buf: &[u8]) -> usize {
        let mut got = 0;
        while got < RX_BATCH {
            let now = self.now();
            // Een geweigerd frame telt de stack zelf (Stats); een fout per
            // frame loggen is een kapotte stack (leannet DESIGN, 11-08).
            let rx = nic.receive_with(buf.len(), |frame| {
                let _ = self.with(|st| st.receive(frame, now));
            });
            if rx.is_none() {
                break;
            }
            got += 1;
        }
        got
    }

    /// Alles wat de stack klaar heeft naar de TX-ring. De stack bouwt elk
    /// frame in de ring zelf; alleen als daar geen plaats is voor een vol
    /// frame, in `buf` met de tegendruk van [`Nic::transmit_wait`]. Een frame
    /// dat na die tegendruk nog niet kon, is weg en geteld
    /// (`net::TX_DROPS`); TCP hertransmitteert.
    async fn transmit(&self, nic: &mut Nic, buf: &mut [u8]) {
        let mut sent = 0;
        loop {
            let now = self.now();
            let direct = nic.try_transmit_with(buf.len(), |ring| {
                self.with(|st| st.poll_transmit(now, ring))
                    .ok()
                    .flatten()
                    .unwrap_or(0)
            });
            match direct {
                Ok(0) => {
                    self.tx_rounds.set(self.tx_rounds.get().wrapping_add(1));
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    let Ok(Some(n)) = self.with(|st| st.poll_transmit(now, buf)) else {
                        self.tx_rounds.set(self.tx_rounds.get().wrapping_add(1));
                        return;
                    };
                    if let Some(frame) = buf.get(..n) {
                        let _ = nic.transmit_wait(frame, self.now).await;
                    }
                }
            }
            sent += 1;
            if sent % TX_BATCH == 0 {
                yield_now().await;
            }
        }
    }

    /// Tot wanneer de pomp slaapt: de vroegste van de poll-ronde `d` en de
    /// timers van de stack, maar niet korter dan [`SPIN_GUARD`].
    fn deadline(&self, d: Duration) -> u64 {
        let now = self.now();
        let poll = now.saturating_add(nanos(d));
        let stack = self.with(|st| st.next_timeout(now)).ok().flatten();
        stack
            .map_or(poll, |t| t.min(poll))
            .max(now.saturating_add(nanos(SPIN_GUARD)))
    }

    /// Slaapt tot de bel, de timer op `deadline`, of een wek van de stack.
    ///
    /// Plus een uitstelbare ronde na `busy` (`RxPoll::lo`): de bel gaat
    /// alleen vanuit de slaper, en een app die blijft rekenen (BURN, de
    /// vitals-cpu) slaapt niet. Zonder deze ronde zag zijn pomp RX alleen
    /// op de timer, die na stilte oploopt tot `RxPoll::hi` (1 s). Hij wekt
    /// een slapende core nooit (`Exec::after_deferrable`): een stille app
    /// kost hij hooguit één pomp-ronde per wek die er toch al was.
    ///
    /// De stack wekt via de waker van deze taak en zegt niet dat hij het
    /// deed. Daarom: wie ons na de eerste poll opnieuw pollt zonder bel of
    /// timer, is de stack (of een loze wek, en dan kost dat één ronde).
    /// De waker gaat ná het leegpompen de stack in en zonder `.await`
    /// ertussen, dus er valt geen wek tussen wal en schip.
    async fn idle(&self, bell: &'static Signal, deadline: u64, busy: Duration) -> Wake {
        let mut timer = pin!(self.exec.until(deadline));
        let mut round = pin!(self.exec.after_deferrable(busy));
        let mut ring = pin!(bell.wait());
        let mut armed = false;
        poll_fn(|cx| {
            if ring.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Bell);
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Timer);
            }
            if round.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Busy);
            }
            if armed {
                return Poll::Ready(Wake::Stack);
            }
            armed = true;
            // Busy kan hier niet: niemand anders draait nu.
            let _ = self.with(|st| st.register_driver_waker(cx.waker()));
            Poll::Pending
        })
        .await
    }

    /// Eén readiness-vraag als future: `check` zegt of een op nu zonder
    /// `WouldBlock` zou slagen, en zo niet registreert `register` de waker
    /// van deze taak. Verbruikt niets; de `readable()`'s van de sockets zijn
    /// hierop één regel. `deadline` als bij [`Net::wait`].
    async fn ready(
        &self,
        deadline: Option<u64>,
        mut check: impl FnMut(&mut Stack) -> leannet::Result<bool>,
        register: impl FnMut(&mut Stack, &Waker) -> leannet::Result,
    ) -> Result<()> {
        self.wait(
            deadline,
            |st, _| match check(st) {
                Ok(true) => Ok(()),
                Ok(false) => Err(StackError::WouldBlock),
                Err(e) => Err(e),
            },
            register,
        )
        .await
    }

    /// Eén stack-op als future: `op` proberen, en bij `WouldBlock` de waker
    /// van deze taak via `register` op het handvat zetten en wachten. Op
    /// `deadline` (absolute nanoseconden) wint de timer met
    /// [`NetError::Timeout`].
    async fn wait<T>(
        &self,
        deadline: Option<u64>,
        mut op: impl FnMut(&mut Stack, u64) -> leannet::Result<T>,
        mut register: impl FnMut(&mut Stack, &Waker) -> leannet::Result,
    ) -> Result<T> {
        let mut timer = pin!(deadline.map(|d| self.exec.until(d)));
        poll_fn(|cx| {
            let now = self.now();
            if deadline.is_some_and(|d| now >= d) {
                return Poll::Ready(Err(NetError::Timeout));
            }
            let r = self.with(|st| match op(st, now) {
                Err(StackError::WouldBlock) => register(st, cx.waker()).map(|()| None),
                other => other.map(Some),
            });
            match r {
                Ok(Ok(Some(v))) => Poll::Ready(Ok(v)),
                Ok(Err(e)) => Poll::Ready(Err(NetError::Stack(e))),
                Err(e) => Poll::Ready(Err(e)),
                Ok(Ok(None)) => {
                    // De timer liep net af tussen de toets en hier: nog één
                    // ronde, dan ziet de toets bovenaan hem.
                    if let Some(t) = timer.as_mut().as_pin_mut()
                        && t.poll(cx).is_ready()
                    {
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// Opent een TCP-listener op `port` (0 kiest een efemere).
    pub fn tcp_listen(&'static self, port: u16) -> Result<TcpListener> {
        self.admit()?;
        let h = self.with(|st| st.tcp_listen(port))??;
        Ok(TcpListener {
            net: self,
            h,
            slot: self.track(Open::Listen(h)),
        })
    }

    /// Verbindt met `ip:port` en geeft na `timeout` op. Er is geen dial
    /// zonder termijn: wie verbindt, zegt hoe lang hij wacht (de dial van lean heeft altijd een deadline).
    pub async fn tcp_connect_timeout(
        &'static self,
        ip: [u8; 4],
        port: u16,
        timeout: Duration,
    ) -> Result<TcpStream> {
        let at = self.now().saturating_add(nanos(timeout));
        self.tcp_connect_until(ip, port, at).await
    }

    /// Verbindt met `ip:port` vóór `deadline`. Het handvat zit vanaf de SYN
    /// in een [`TcpStream`], zodat een afgebroken dial via `Drop` opruimt.
    async fn tcp_connect_until(
        &'static self,
        ip: [u8; 4],
        port: u16,
        deadline: u64,
    ) -> Result<TcpStream> {
        self.admit()?;
        let now = self.now();
        let h = self.with(|st| st.tcp_connect(ip, port, deadline, now))??;
        let s = self.stream(h);
        self.wait(
            Some(deadline),
            |st, now| st.tcp_poll_connect(h, now),
            |st, w| st.tcp_register_write_waker(h, w),
        )
        .await?;
        Ok(s)
    }

    /// Bindt UDP-poort `port` (0 kiest een efemere).
    pub fn udp_bind(&'static self, port: u16) -> Result<UdpSocket> {
        self.admit()?;
        let h = self.with(|st| st.udp_bind(port))??;
        Ok(UdpSocket {
            net: self,
            h,
            deadline: None,
            slot: self.track(Open::Udp(h)),
        })
    }

    /// Bindt IPv6-UDP en activeert NDP/router discovery voor deze slot-interface.
    pub fn udp6_bind(&'static self, port: u16) -> Result<Udp6Socket> {
        self.admit()?;
        let h = self.with(|st| st.udp6_bind(port, self.now()))??;
        Ok(Udp6Socket {
            net: self,
            h,
            deadline: None,
            slot: self.track(Open::Udp6(h)),
        })
    }

    /// Abonneert op ff02-multicast op de ene slot-interface.
    pub fn join_group6(&self, group: [u8; 16]) -> Result {
        self.with(|st| st.join_group6(group, self.now()))?
            .map_err(NetError::Stack)
    }

    /// Link-local en optioneel SLAAC-adres; activeert de baan op eerste gebruik.
    pub fn ipv6_addresses(&self) -> Result<([u8; 16], Option<[u8; 16]>)> {
        self.with(|st| {
            st.enable_ipv6(self.now())?;
            st.ipv6_addresses(self.now()).ok_or(StackError::StackClosed)
        })?
        .map_err(NetError::Stack)
    }

    /// Abonneert de stack op multicastgroep `group`, voor zijn hele
    /// levensduur; een tweede join van dezelfde groep doet niets.
    ///
    /// Waarom dit genoeg is: de switch van de kern floodt elk
    /// IP-multicastframe naar elk aangesloten slot (en de uplink,
    /// `net/src/switch.rs`, `forward`), dus het filter zit hier, in de
    /// stack: een UDP-socket op de poort van de groep hoort het datagram
    /// alleen na deze join. Alleen link-local groepen (224.0.0.0/24, zoals
    /// mDNS op 224.0.0.251); de rest weigert leannet met
    /// [`StackError::NotLinkLocalMulticast`], en na vier groepen met
    /// [`StackError::GroupsFull`].
    ///
    /// Een `leave_group` is er niet: leannet v3.0.0 en v3.1.1 kennen geen
    /// leave (de enige consument, mDNS, verlaat nooit), en een filter hier
    /// kan het niet namaken, want `recv_from` zegt niet aan welk adres een
    /// datagram gericht was.
    pub fn join_group(&self, group: [u8; 4]) -> Result {
        self.with(|st| st.join_group(group))?
            .map_err(NetError::Stack)
    }

    /// Het IPv4-adres van `host`: een naam die al een adres is meteen, anders
    /// een A-vraag aan de DNS-server uit de env ([`Net::dns`]).
    pub async fn resolve(&'static self, host: &str) -> Result<[u8; 4]> {
        if let Some(ip) = parse_ip4(host) {
            return Ok(ip);
        }
        let server = self.dns.ok_or(NetError::Dns(DnsError::NoServer))?;
        self.resolve_record(server, host, 1).await
    }
    /// Het AAAA-adres van een host, of het letterlijke IPv6-adres zelf.
    pub async fn resolve6(&'static self, host: &str) -> Result<[u8; 16]> {
        if let Ok(ip) = host.parse::<core::net::Ipv6Addr>() {
            return Ok(ip.octets());
        }
        let server = self.dns.ok_or(NetError::Dns(DnsError::NoServer))?;
        self.resolve6_via(server, host).await
    }
    /// AAAA via de geconfigureerde IPv4-DNS-server; DNS-transport en antwoordfamilie zijn onafhankelijk.
    pub async fn resolve6_via(&'static self, server: [u8; 4], host: &str) -> Result<[u8; 16]> {
        self.resolve_record(server, host, 28).await
    }
    /// Vraagt `server` om het record van soort `kind` (1 is A, 28 is AAAA)
    /// van `host`: één vraag, wachten tot [`DNS_TIMEOUT`], en na stilte één
    /// herhaling met een nieuw id.
    ///
    /// Een datagram van een ander adres, of met een verkeerd id of een
    /// andere vraag, telt niet en de wacht gaat door: een laat antwoord op
    /// de vorige poging of een gok van buiten maakt de vraag niet stuk. Een
    /// echt antwoord dat nee zegt (NXDOMAIN, geen A, kapot) is meteen de
    /// uitkomst; nog eens vragen verandert daar niets aan.
    async fn resolve_record<const N: usize>(
        &'static self,
        server: [u8; 4],
        host: &str,
        kind: u16,
    ) -> Result<[u8; N]> {
        let mut query = [0u8; dns::QUERY_MAX];
        let mut buf = [0u8; dns::UDP_MAX];
        let mut sock = self.udp_bind(0)?;
        let to = Endpoint {
            ip: server,
            port: dns::PORT,
        };
        for _ in 0..DNS_ATTEMPTS {
            let id = self.dns_id();
            let n = dns::encode_kind(id, host, &mut query, kind)?;
            sock.set_timeout(Some(DNS_TIMEOUT));
            sock.send_to(to, query.get(..n).unwrap_or_default()).await?;
            loop {
                let (n, from) = match sock.recv_from(&mut buf).await {
                    Ok(got) => got,
                    Err(NetError::Timeout) => break,
                    Err(e) => return Err(e),
                };
                if from != to {
                    continue;
                }
                match dns::parse_kind(id, host, buf.get(..n).unwrap_or_default(), kind) {
                    Ok(ip) => return Ok(ip),
                    Err(DnsError::BadId { .. } | DnsError::Mismatch) => {}
                    Err(e) => return Err(NetError::Dns(e)),
                }
            }
        }
        Err(NetError::Dns(DnsError::Timeout {
            attempts: DNS_ATTEMPTS,
        }))
    }

    /// Een id voor de volgende DNS-vraag: de klok en een teller door elkaar.
    /// Geen geheim (de app heeft geen entropiebron), maar anders per vraag,
    /// zodat een laat antwoord op de vorige poging niet telt.
    fn dns_id(&self) -> u16 {
        let seq = self.dns_seq.get().wrapping_add(1);
        self.dns_seq.set(seq);
        let t = self.now();
        let mixed = t ^ (t >> 16) ^ (t >> 32) ^ u64::from(seq).wrapping_mul(0x9e37);
        (mixed & 0xffff) as u16
    }

    /// Een system-API-client over deze stack: hij verbindt bij de eerste
    /// call met de kern ([`sys::ADDRESS`]) en zet zijn deadlines op de
    /// executor van de stack.
    #[must_use]
    pub fn system_client(&'static self) -> SystemClient {
        sys::Client::new(
            SysDial {
                net: self,
                first: None,
            },
            ExecTimer(self.exec),
        )
    }

    /// Als [`Net::system_client`], maar de eerste call gaat over `conn`, een
    /// al open verbinding naar de kern.
    #[must_use]
    pub fn system_client_over(&'static self, conn: TcpStream) -> SystemClient {
        sys::Client::new(
            SysDial {
                net: self,
                first: Some(conn),
            },
            ExecTimer(self.exec),
        )
    }

    /// Absolute deadline over `d`.
    fn at(&self, d: Option<Duration>) -> Option<u64> {
        d.map(|d| self.now().saturating_add(nanos(d)))
    }

    // ---- De tabel van open handvatten en het afscheid ----

    /// Weigert iets nieuws zodra [`Net::shutdown`] begon.
    fn admit(&self) -> Result {
        if self.closing.get() {
            return Err(NetError::ShuttingDown);
        }
        Ok(())
    }

    /// Een [`TcpStream`] over `h`, in de tabel.
    fn stream(&'static self, h: TcpHandle) -> TcpStream {
        TcpStream {
            net: self,
            h,
            deadline: None,
            slot: self.track(Open::Tcp(h)),
        }
    }

    /// Zet `o` in de tabel; de plek. Is de tabel vol, dan blijft het
    /// handvat werken maar sluit [`Net::shutdown`] het niet: één regel bij
    /// de eerste keer, daarna alleen de teller.
    fn track(&self, o: Open) -> Option<usize> {
        let slot = self.open.try_borrow_mut().ok().and_then(|mut t| {
            let i = t.iter().position(Option::is_none)?;
            *t.get_mut(i)? = Some(o);
            Some(i)
        });
        if slot.is_none() {
            let n = self.untracked.get();
            self.untracked.set(n.saturating_add(1));
            if n == 0 {
                let cap = self.open.try_borrow().map_or(0, |t| t.len());
                log!(
                    "appnet: open table full ({cap}), shutdown will not close this handle HOPOS_APPNET_UNTRACKED"
                );
            }
        }
        slot
    }

    /// Haalt `o` van plek `slot`, als het daar nog staat (een shutdown kan
    /// hem al weggehaald hebben).
    fn untrack(&self, slot: Option<usize>, o: Open) {
        let Some(i) = slot else {
            return;
        };
        if let Ok(mut t) = self.open.try_borrow_mut()
            && let Some(e) = t.get_mut(i)
            && *e == Some(o)
        {
            *e = None;
        }
    }

    /// Sluit alles uit de tabel: listeners (hun backlog gaat mee),
    /// verbindingen (FIN na de gebufferde data) en UDP-sockets. Het aantal
    /// dat de stack aannam.
    fn close_all(&self) -> usize {
        let now = self.now();
        let n = self.open.try_borrow().map_or(0, |t| t.len());
        let mut closed = 0;
        for i in 0..n {
            let taken = self
                .open
                .try_borrow_mut()
                .ok()
                .and_then(|mut t| t.get_mut(i).and_then(Option::take));
            let Some(o) = taken else {
                continue;
            };
            let ok = self.with(|st| match o {
                Open::Tcp(h) => st.tcp_close(h, now).is_ok(),
                Open::Listen(h) => {
                    st.tcp_listen_close(h);
                    true
                }
                Open::Udp6(h) => {
                    st.udp6_close(h);
                    true
                }
                Open::Udp(h) => {
                    st.udp_close(h);
                    true
                }
            });
            if ok == Ok(true) {
                closed += 1;
            }
        }
        closed
    }

    /// Is de stack leeg? Elke verbinding gaf haar buffers terug (leannet
    /// geeft het zendbudget pas terug als de FIN bevestigd is, of als de
    /// verbinding opgeruimd werd), en er ligt niets meer in een rij.
    fn drained(&self) -> bool {
        let now = self.now();
        self.with(|st| st.budget_free() == st.config().budget && st.next_timeout(now) != Some(now))
            .unwrap_or(false)
    }

    /// Het net-afscheid van een app die stopt: niets nieuws meer open, elk
    /// handvat uit de tabel dicht (FIN na de data), en dan wachten tot de
    /// stack leeg is ([`Drain::drained`]) of `limit` verstrijkt. De pomp draait intussen gewoon als eigen taak; hij zet
    /// de FIN's op de draad en neemt de ACK's aan.
    ///
    /// Waarom: `App::exit` parkeert de core, en wat dan nog in een
    /// zendbuffer staat, is weg. Gemeten 29-09: een logregel over de
    /// system-verbinding (`log_us=320`) die de kern nooit zag.
    pub async fn shutdown(&self, limit: Duration) -> Drain {
        let t0 = self.now();
        let deadline = t0.saturating_add(nanos(limit));
        self.closing.set(true);
        let closed = self.close_all();
        let mut drained = self.drained();
        while !drained && self.now() < deadline {
            let tick = self.now().saturating_add(nanos(SETTLE_TICK)).min(deadline);
            self.exec.until(tick).await;
            drained = self.drained();
        }
        Drain {
            closed,
            drained,
            waited_us: self.now().saturating_sub(t0) / 1000,
            untracked: self.untracked.get(),
        }
    }
}

/// Een duur in nanoseconden, verzadigd.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Een TCP-listener. Sluit in `Drop`.
pub struct TcpListener {
    net: &'static Net,
    h: ListenHandle,
    /// De plek in de tabel van open handvatten.
    slot: Option<usize>,
}

impl TcpListener {
    /// Luistert op `port` van de stack van deze app.
    pub fn bind(port: u16) -> Result<Self> {
        net().ok_or(NetError::NotUp)?.tcp_listen(port)
    }

    /// De poort waarop hij luistert.
    pub fn port(&self) -> Result<u16> {
        let h = self.h;
        Ok(self.net.with(|st| st.listen_port(h))??)
    }

    /// Wacht op de volgende verbinding.
    pub async fn accept(&self) -> Result<TcpStream> {
        let h = self.h;
        let c = self
            .net
            .wait(
                None,
                |st, _| st.tcp_accept(h),
                |st, w| st.listen_register_waker(h, w),
            )
            .await?;
        Ok(self.net.stream(c))
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let h = self.h;
        self.net.untrack(self.slot, Open::Listen(h));
        let _ = self.net.with(|st| st.tcp_listen_close(h));
    }
}

/// Een TCP-verbinding. `Drop` sluit hem (FIN na de gebufferde data).
pub struct TcpStream {
    net: &'static Net,
    h: TcpHandle,
    deadline: Option<u64>,
    /// De plek in de tabel van open handvatten.
    slot: Option<usize>,
}

impl TcpStream {
    /// Verbindt met `ip:port` en geeft na `timeout` op.
    pub async fn connect_timeout(ip: [u8; 4], port: u16, timeout: Duration) -> Result<Self> {
        net()
            .ok_or(NetError::NotUp)?
            .tcp_connect_timeout(ip, port, timeout)
            .await
    }

    /// Zet de deadline van elke volgende read en write, als tijdstip op de
    /// klok van de stack; `None` wist hem.
    pub fn set_deadline(&mut self, at: Option<u64>) {
        self.deadline = at;
    }

    /// Zet de deadline op `d` vanaf nu; `None` wist hem.
    pub fn set_timeout(&mut self, d: Option<Duration>) {
        self.deadline = self.net.at(d);
    }

    /// Leest hooguit `buf.len()` bytes; 0 is EOF (de FIN van de peer). Een
    /// reset is [`StackError::Reset`], nooit EOF.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, _| st.tcp_read(h, buf),
                |st, w| st.tcp_register_read_waker(h, w),
            )
            .await
    }

    /// Leest precies `buf.len()` bytes. EOF ervoor is
    /// [`StackError::Closed`]; de deadline van de stroom geldt voor het
    /// geheel.
    pub async fn read_exact(&mut self, mut buf: &mut [u8]) -> Result {
        while !buf.is_empty() {
            let n = self.read(buf).await?;
            if n == 0 {
                return Err(NetError::Stack(StackError::Closed));
            }
            buf = buf.get_mut(n..).unwrap_or_default();
        }
        Ok(())
    }

    /// Wacht tot een read niet zou blokkeren: er staan bytes klaar, of de
    /// peer sloot (de read geeft dan EOF of de fout). Verbruikt niets, zodat
    /// een eigenaar met één `select` op meerdere verbindingen kan wachten in
    /// plaats van ze rond te pollen met een lege read en een dutje (de les
    /// van de gedeelde core, 02-10). De deadline van de stroom geldt ook hier.
    pub async fn readable(&mut self) -> Result<()> {
        let h = self.h;
        self.net
            .ready(
                self.deadline,
                |st| st.tcp_readable(h),
                |st, w| st.tcp_register_read_waker(h, w),
            )
            .await
    }

    /// Schrijft een deel van `data` in de zendring; het aantal bytes.
    pub async fn write(&mut self, data: &[u8]) -> Result<usize> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, _| st.tcp_write(h, data),
                |st, w| st.tcp_register_write_waker(h, w),
            )
            .await
    }

    /// Schrijft heel `data`.
    pub async fn write_all(&mut self, mut data: &[u8]) -> Result {
        while !data.is_empty() {
            let n = self.write(data).await?;
            data = data.get(n..).unwrap_or_default();
        }
        Ok(())
    }

    /// Wacht tot alles wat de app op deze verbinding schreef, op de draad
    /// staat en bevestigd is. Op de deadline van de stream, of zonder
    /// deadline na [`FLUSH_TIMEOUT`], wint [`NetError::Timeout`]; een reset
    /// geeft de fout van de stack.
    ///
    /// leannet v3.0.0 laat per verbinding niet zien hoeveel er nog
    /// onbevestigd is. Deze flush wacht daarom op het sterkere, wél
    /// zichtbare: een pomp-ronde na de eigen writes die alles op de TX-ring
    /// zette, en een stack zonder één lopende timer (`next_timeout` leeg:
    /// geen hertransmissie, geen persist, geen rij). Dat bewijst dat ook
    /// deze verbinding alles bevestigd kreeg. De prijs: zolang een andere
    /// verbinding een timer heeft (een eigen onbevestigde write, of een
    /// sluitende verbinding in FIN-WAIT of TIME-WAIT), wacht de flush tot
    /// die stil is of tot zijn deadline. Te laat is hier beter dan te
    /// vroeg: een `Ok` liegt nooit.
    pub async fn flush(&mut self) -> Result {
        let (net, h) = (self.net, self.h);
        let deadline = self
            .deadline
            .unwrap_or_else(|| net.now().saturating_add(nanos(FLUSH_TIMEOUT)));
        let round = net.tx_rounds.get();
        loop {
            // Een tik, zodat een andere verbinding die stil wordt ook gezien
            // wordt: die wekt deze taak niet.
            let tick = net.now().saturating_add(nanos(SETTLE_TICK)).min(deadline);
            let r = net
                .wait(
                    Some(tick),
                    |st, now| {
                        st.tcp_state(h)?;
                        let on_wire = net.tx_rounds.get() != round;
                        if on_wire && st.next_timeout(now).is_none() {
                            Ok(())
                        } else {
                            Err(StackError::WouldBlock)
                        }
                    },
                    |st, w| st.tcp_register_write_waker(h, w),
                )
                .await;
            match r {
                Err(NetError::Timeout) if net.now() < deadline => {}
                other => return other,
            }
        }
    }

    /// Sluit de verbinding: FIN na de gebufferde data, en de pomp stuurt
    /// hem. Synchroon, want de stack wacht nergens op; wie de peer wil zien
    /// sluiten, leest tot EOF vóór hij dit doet, en wie wil weten dat de
    /// data aankwam, doet eerst [`TcpStream::flush`].
    pub fn close(self) -> Result {
        let (net, h) = (self.net, self.h);
        net.untrack(self.slot, Open::Tcp(h));
        // `Drop` zou nog een keer sluiten; dat is idempotent, maar zo zegt
        // de fout van deze close wat er gebeurde.
        core::mem::forget(self);
        let now = net.now();
        Ok(net.with(|st| st.tcp_close(h, now))??)
    }

    /// De toestand van de TCP-machine.
    pub fn state(&self) -> Result<TcpState> {
        let h = self.h;
        Ok(self.net.with(|st| st.tcp_state(h))??)
    }

    /// Het lokale eindpunt.
    pub fn local(&self) -> Result<Endpoint> {
        let h = self.h;
        Ok(self.net.with(|st| st.tcp_local(h))??)
    }

    /// Het externe eindpunt.
    pub fn remote(&self) -> Result<Endpoint> {
        let h = self.h;
        Ok(self.net.with(|st| st.tcp_remote(h))??)
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let (net, h) = (self.net, self.h);
        net.untrack(self.slot, Open::Tcp(h));
        let now = net.now();
        // Een handvat dat de stack al opruimde (een mislukte dial) geeft
        // `Closed`; dat is hier het goede einde.
        let _ = net.with(|st| st.tcp_close(h, now));
    }
}

/// Een UDP-socket. Sluit in `Drop`.
pub struct UdpSocket {
    net: &'static Net,
    h: UdpHandle,
    deadline: Option<u64>,
    /// De plek in de tabel van open handvatten.
    slot: Option<usize>,
}

impl UdpSocket {
    /// Bindt `port` op de stack van deze app.
    pub fn bind(port: u16) -> Result<Self> {
        net().ok_or(NetError::NotUp)?.udp_bind(port)
    }

    /// Zet de deadline van elke volgende send en recv; `None` wist hem.
    pub fn set_deadline(&mut self, at: Option<u64>) {
        self.deadline = at;
    }

    /// Zet de deadline op `d` vanaf nu; `None` wist hem.
    pub fn set_timeout(&mut self, d: Option<Duration>) {
        self.deadline = self.net.at(d);
    }

    /// Het lokale eindpunt.
    pub fn local(&self) -> Result<Endpoint> {
        let h = self.h;
        Ok(self.net.with(|st| st.udp_local(h))??)
    }

    /// Verstuurt één datagram naar `to`; wacht op een route (ARP) of op
    /// ruimte in de zendrij.
    pub async fn send_to(&self, to: Endpoint, data: &[u8]) -> Result<usize> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, now| st.udp_send_to(h, to, data, now),
                |st, w| st.udp_register_write_waker(h, w),
            )
            .await
    }

    /// Wacht op één datagram: lengte en afzender. Wat niet in `buf` past,
    /// valt weg (UDP).
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, Endpoint)> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, _| st.udp_recv_from(h, buf),
                |st, w| st.udp_register_read_waker(h, w),
            )
            .await
    }

    /// Wacht tot er een datagram klaarligt, zonder het te lezen: de
    /// tegenhanger van [`TcpStream::readable`] voor een eigenaar die op
    /// meerdere sockets tegelijk wacht.
    pub async fn readable(&self) -> Result<()> {
        let h = self.h;
        self.net
            .ready(
                self.deadline,
                |st| st.udp_readable(h),
                |st, w| st.udp_register_read_waker(h, w),
            )
            .await
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let h = self.h;
        self.net.untrack(self.slot, Open::Udp(h));
        let _ = self.net.with(|st| st.udp_close(h));
    }
}

/// Een IPv6-UDP-socket. Sluit in `Drop`.
pub struct Udp6Socket {
    net: &'static Net,
    h: Udp6Handle,
    deadline: Option<u64>,
    /// De plek in de tabel van open handvatten.
    slot: Option<usize>,
}

impl Udp6Socket {
    /// Bindt `port` op de stack van deze app.
    pub fn bind(port: u16) -> Result<Self> {
        net().ok_or(NetError::NotUp)?.udp6_bind(port)
    }

    /// Zet de deadline van elke volgende send en recv; `None` wist hem.
    pub fn set_deadline(&mut self, at: Option<u64>) {
        self.deadline = at;
    }

    /// Zet de deadline op `d` vanaf nu; `None` wist hem.
    pub fn set_timeout(&mut self, d: Option<Duration>) {
        self.deadline = self.net.at(d);
    }

    /// Het lokale eindpunt.
    pub fn local(&self) -> Result<Endpoint6> {
        let h = self.h;
        Ok(self.net.with(|st| st.udp6_local(h))??)
    }

    /// Verstuurt één datagram naar `to`; wacht op een route (NDP) of op
    /// ruimte in de zendrij.
    pub async fn send_to(&self, to: Endpoint6, data: &[u8]) -> Result<usize> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, now| st.udp6_send_to(h, to, data, now),
                |st, w| st.udp6_register_write_waker(h, w),
            )
            .await
    }

    /// Wacht op één datagram: lengte en afzender. Wat niet in `buf` past,
    /// valt weg (UDP).
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, Endpoint6)> {
        let h = self.h;
        self.net
            .wait(
                self.deadline,
                |st, _| st.udp6_recv_from(h, buf),
                |st, w| st.udp6_register_read_waker(h, w),
            )
            .await
    }

    /// Wacht tot er een datagram klaarligt, zonder het te lezen: de
    /// IPv6-tegenhanger van [`UdpSocket::readable`].
    pub async fn readable(&self) -> Result<()> {
        let h = self.h;
        self.net
            .ready(
                self.deadline,
                |st| st.udp6_readable(h),
                |st, w| st.udp6_register_read_waker(h, w),
            )
            .await
    }
}

impl Drop for Udp6Socket {
    fn drop(&mut self) {
        let h = self.h;
        self.net.untrack(self.slot, Open::Udp6(h));
        let _ = self.net.with(|st| st.udp6_close(h));
    }
}

// ---- De system-API over een echte verbinding ----

/// De system-client van een app met netstack.
pub type SystemClient =
    sys::Client<SysDial, ExecTimer<{ crate::rt::TASKS }, { crate::rt::TIMERS }>>;

/// Vertaalt een netfout naar de drie transportfouten van de client: wat
/// de client herhaalt (reset, dicht) tegen wat hij als weigering ziet.
fn conn_error(e: NetError) -> ConnError {
    match e {
        NetError::Stack(StackError::Reset) => ConnError::Reset,
        NetError::Stack(
            StackError::Refused { .. }
            | StackError::Unreachable { .. }
            | StackError::NoRoute { .. }
            | StackError::DeadlineExceeded,
        )
        | NetError::NotUp
        | NetError::NoSlot
        | NetError::Timeout => ConnError::Refused,
        _ => ConnError::Closed,
    }
}

impl sys::Conn for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, ConnError> {
        TcpStream::read(self, buf).await.map_err(conn_error)
    }

    async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, ConnError> {
        TcpStream::write(self, buf).await.map_err(conn_error)
    }
}

/// De dialer van de system-client: een verbinding naar [`sys::ADDRESS`],
/// of eerst de verbinding die de app al had.
pub struct SysDial {
    net: &'static Net,
    first: Option<TcpStream>,
}

impl sys::Dial for SysDial {
    type Conn = TcpStream;

    async fn dial(&mut self) -> core::result::Result<TcpStream, ConnError> {
        if let Some(c) = self.first.take() {
            return Ok(c);
        }
        let (ip, port) = sys::ADDRESS;
        self.net
            .tcp_connect_timeout(ip, port, SYS_DIAL_TIMEOUT)
            .await
            .map_err(conn_error)
    }
}

/// De timer van de system-client: het timerwiel van de executor van de app.
pub use executor::ExecTimer;

#[cfg(test)]
mod tests;
