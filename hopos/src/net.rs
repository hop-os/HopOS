//! Het netwerkvlak van de kern-binary: de RX-pomp op de NIC, de switch-actor,
//! HOP's poort 0 met de node-stack (`leannet`), de DHCP-lease en de
//! system-listener (Go: `hopnet.Up`, `hopswitch.Up` en `ServeSystem` uit
//! `OLD/metal/cmd/hopos/main.go`).
//!
//! Eigendom (PORT.md §3 en §4):
//!
//! - de pomp bezit de NIC; frames gaan als waarde door de ingress- en
//!   egress-rij (`net::pump`);
//! - de switch bezit de poorten, de NAT en de switch-kant van elke ring; wie
//!   iets wil, stuurt een [`Command`] naar [`COMMANDS`];
//! - de host-taak bezit HOP's twee ringen van poort 0: eerst rauw voor DHCP,
//!   daarna in de `HostPort` met de node-stack erachter;
//! - de system-listener bezit de listen-socket en geeft elke toegelaten
//!   verbinding (handvat plus [`Admitted`]) als waarde aan een vrije taak uit
//!   een vaste pool van [`SYSTEM_WORKERS`]; die taak bezit haar buffers en
//!   haar [`Reply`], en sluit het handvat zelf;
//! - de node-stack zelf staat in [`STACK`], een `LocalCell`. Dat is een
//!   bewuste afwijking van "een actor met berichten" (PORT.md §3 zegt: een
//!   `net.Conn` wordt een handvat met een rij): `leannet` is sans-I/O en elke
//!   call is synchroon en kort, dus de lening leeft binnen één call en nooit
//!   over een `.await` (de lint weigert de rest). De rij per verbinding komt
//!   als meting zegt dat de korte leningen de host-taak hinderen.
//!
//! Wat hier niet staat: de lifecycle achter de system-API (die krijgt de
//! listener als [`SystemApi`] van `main`), SNTP en DNS.

use crate::clock::ExecTimer;
use abi::layout::{HOST_IP4, NET_MTU, NET_PREFIX, NET_RING_DATA_CAP};
use abi::systemapi::PORT;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::future::{Future, poll_fn};
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use cpu::println;
use dev::Pa;
use executor::{Clock, Executor, Sleeper};
use kern::slots::Reply;
use kern::system::{Admitted, Conn, End, MAX_HOP_CONNS, MAX_IO_CHUNK, MAX_PAYLOAD};
use leandhcp::{Action, Client, Instant, KeepAction, Keeper, Lease};
use leannet::{Endpoint, ListenHandle, Stack, TcpHandle, UdpHandle};
use net::host::{HostPort, HostStack};
use net::nat::{Neighbors, Uplink};
use net::plan::MAX_LAN_FRAME;
use net::pump::Pump;
use net::ring::{AbiTx, KIND_UPLINK, Reader as _, Writer as _};
use net::switch::{self, Ack, Command, Commands, Doorbell, Published, Switch, Wiring};
use net::{Egress, Ingress, Stats};
use netdev::Device;
use sync::mpsc::Mailbox;
use sync::{Doors, Either, LocalCell, Signal, select};

/// De leeskant van een ring zoals de switch en poort 0 hem zien.
type RingRx = AbiTx;
/// De schrijfkant.
type RingTx = abi::ring::Writer;

/// Hoe de kern zijn kant van elke frame-ring in de pool mapt
/// ([`abi::ring::Coherence`], `Board::SLOT_RINGS`): Normal write-back inner
/// shareable waar de kern de pool zo mapt (`cpu::boot::ATTR_NORMAL`: de
/// Pi's, de UEFI-boards, QEMU virt). Niet op Apple: daar mapt de kern de
/// pool Device (board/apple/src/mmu.rs, `dram_attr`). Niet op de Radxa: de
/// kern mapt daar alles boven 0x0880_0000 Device, de pool dus ook
/// (board/rk3566/src/mmu.rs, de tamago-keuze), en met de belofte las hij
/// langs de cache van de app heen (01-10, stempel AA: `HOPOS_NETRING_TX_CORRUPT`
/// en een corrupte RX-ring in slot 3, terwijl de Pi 4 met dezelfde kern en
/// apps foutloos draaide). Niet op RISC-V: de C906 is niet coherent met het
/// andere hart. Elke ring kopieert pas zonder onderhoud als de tegenpartij
/// hetzelfde belooft. De ringen van een slot krijgen op Apple en de Radxa
/// toch Hardware zodra de kooi de staart Normal mapt (`kooi::tail_rings`).
pub(crate) const RINGS: abi::ring::Coherence = <crate::Machine as board::Board>::SLOT_RINGS;

/// De belofte van de twee host-ringen van poort 0 (`Board::HOST_RINGS`).
/// Die liggen in de kern-heap (Normal write-back op elk ARM-board, ook op
/// Apple) en beide kanten zijn de kern, dus daar geldt [`RINGS`] niet, dat
/// over de pool gaat. Op Apple kostte het onderhoud elk frame van de
/// node-stack een veeg per cacheline in beide richtingen (GEMETEN 01-10 op
/// M10: het opslagpad haalde 930 MB/s uit gaten in het RAM van de kern, met
/// de OS-core vol bezig, tegen 4300 MB/s app naar app door dezelfde switch).
///
/// Ook op RISC-V: daar is [`RINGS`] `Maintained` omdat de app-harts niet
/// coherent zijn met de kern, maar beide kanten van poort 0 draaien in de
/// executor van de kern op zijn eigen hart, dus in één cache. Het onderhoud
/// was daar per frame `th.dcache.cipa`/`cpa` met `th.sync.is` op kop, staart
/// en payload voor niets (03-10, de hop op de C906).
const HOST_RINGS: abi::ring::Coherence = <crate::Machine as board::Board>::HOST_RINGS;

/// De meetlat van het netwerkvlak.
pub(crate) static STATS: Stats = Stats::new();
/// De brievenbus van de switch: `Attach`/`Detach` van de slots komen hier.
pub(crate) static COMMANDS: Commands<'static, RingRx, RingTx> = Mailbox::new();
/// De tabel voor de deur van de executor.
static PUBLISHED: Published<RingRx> = Published::new();
/// De slaper van de OS-core met de deur van de switch erom: elke idle-ronde
/// kijkt hij met kale loads in de TX-ringen en belt [`DOOR`] als er werk
/// ligt, zodat de kick van een app (SEV of HVC 6) de switch ook echt wekt
/// in plaats van zijn failsafe (`net::switch::Doorbell`, de les van 30-09).
pub(crate) fn doorbell<S: Sleeper>(inner: S) -> Doorbell<'static, RingRx, S> {
    Doorbell::new(inner, &PUBLISHED, &DOOR)
}
/// Pomp naar switch.
static INGRESS: Ingress = Ingress::new();
/// Switch naar pomp.
static EGRESS: Egress = Egress::new();
/// De deur van de switch: na elke schrijf in een TX-ring.
pub(crate) static DOOR: Signal = Signal::new();
/// De bel van de pomp: de switch zet hem na een ronde met egress-werk.
static PUMP_BELL: Signal = Signal::new();
/// De bel van de host-taak: de switch zet hem na een schrijf in poort 0, en
/// elke taak die de node-stack iets gaf (een write, een accept) ook.
static HOST_BELL: Signal = Signal::new();
/// De bevestiging van `SetUplink` na DHCP.
static UPLINK_ACK: Ack = Ack::new();
/// Het uplink-adres na de lease (big-endian als getal; 0 = nog geen lease).
/// Eén schrijver (de host-taak), gelezen door de plaatsing van Hop: die
/// zet het in `HOPOS_NODE_IP`, want de leader moet het endpoint zien dat
/// van buiten bereikbaar is, niet het slot-adres.
static UPLINK_IP: AtomicU32 = AtomicU32::new(0);
/// Het MAC-adres van de uplink (big-endian in de lage 48 bits; 0 = geen
/// NIC). Eén schrijver ([`start`]), gelezen door de plaatsing van Hop: de
/// standaardnaam van de node (`kern::nodecfg::default_node`).
static UPLINK_MAC: AtomicU64 = AtomicU64::new(0);
/// De DNS-server uit de lease (big-endian als getal; 0 = geen): Hop krijgt
/// hem in zijn env, want zonder resolver haalt hij niets op naam.
static UPLINK_DNS: AtomicU32 = AtomicU32::new(0);
/// Draait de switch-actor? Gezet na zijn spawn, nooit meer terug (de
/// switch stopt niet). Wie op een bevestiging van de switch wil wachten,
/// kijkt eerst hier.
static SWITCH_UP: AtomicBool = AtomicBool::new(false);
/// De node-stack, gezet door de host-taak na de lease. Zie de moduledoc
/// voor waarom dit een `LocalCell` is en geen actor.
static STACK: LocalCell<Option<Stack>> = LocalCell::cell(None);

/// De datacapaciteit van elk van de twee host-ringen: die van een slot-ring,
/// zodat een burst van een app en een jumboframe (64 KB, hoogstens de halve
/// ring) altijd passen.
const HOST_RING_DATA: u64 = NET_RING_DATA_CAP;

/// Hoe lang één DHCP-poging mag duren. QEMU's user-net antwoordt binnen een
/// milliseconde; een LAN-server binnen een seconde. Tien seconden is ruim
/// genoeg voor een trage router en kort genoeg om het in de log te zien.
const DHCP_TIMEOUT: Duration = Duration::from_secs(10);

/// De wacht tussen twee mislukte DHCP-pogingen.
const DHCP_RETRY: Duration = Duration::from_secs(5);

/// De proeflease na een verlopen lease: renew na 1 s, rebind na 2 s, op na
/// 60 s. Een keeper daarop rebindt elke ~70 s; de eerste ACK legt de echte
/// looptijd eroverheen.
const DHCP_PROBE: (u32, u32, u32) = (60, 1, 2);

/// Het bufferbudget van de node-stack: het plafond op wat de TCP-ringen
/// tegelijk vasthouden. De Go-kern nam 1/8 van het RAM-raam (30 MB op
/// QEMU). De kernheap heeft sinds 01-10 vrije lijsten (heap/src/lib.rs),
/// dus wat een gesloten verbinding teruggeeft, is weer bruikbaar; 8 MB
/// begrenst alleen wat tegelijk leeft.
const NET_BUDGET: usize = 8 << 20;

/// De antwoordbuffer van een system-verbinding: de kop plus één I/O-brok,
/// want een `read` antwoordt met tot [`MAX_IO_CHUNK`] bytes (Hop leest zijn
/// staat in brokken van 1 MiB). De callbuffer is één frame van de grootste
/// payload ([`MAX_PAYLOAD`]). Beide gaan per bestandscall als waarde naar
/// de hopfs-actor en terug (`kern::rpc`); er wordt niets per call
/// gealloceerd.
const OUT_BUF: usize = abi::hopabi::HDR_LEN + MAX_IO_CHUNK;

/// Het totaalplafond op gelijktijdige system-verbindingen: de poolgrootte
/// van de verbindingstaken (handboek §2: een verbinding is een taak uit een
/// vaste pool). Per slot laat `admit` er [`MAX_SYSTEM_CONNS`] toe (één plus
/// een herverbinding); de pool draagt één verbinding per app-slot van het
/// grootste board, plus [`MAX_HOP_CONNS`] voor Hop (zijn store-taak houdt
/// er blijvend één), plus één herverbinding. GEMETEN 01-10 op de M4: met
/// de oude 9 (de drie app-slots van QEMU virt maal twee, plus Hop) kreeg de
/// achtste app van acht geen verbinding ("connection closed") terwijl de
/// schijf er ruimte voor had. Elke taak houdt een callbuffer van
/// [`MAX_PAYLOAD`] (1 MiB plus 64 KiB) en een antwoordbuffer van 1 MiB
/// vast, dus 13 taken zijn ruim 27 MB heap.
pub(crate) const SYSTEM_WORKERS: usize = APP_SLOTS + MAX_HOP_CONNS as usize + 1;

/// De app-slots van het grootste board: de M4 (tien cores, één voor de kern).
const APP_SLOTS: usize = 9;

// Elke verbinding heeft hoogstens één bestandscall uitstaan; de brievenbus
// van de hopfs-actor draagt ze allemaal plus de committer en de flip.
const _: () = assert!(SYSTEM_WORKERS + 2 <= kern::rpc::FS_DEPTH);

/// Weigeringen van de listener die een eigen regel krijgen; daarna tellen
/// we alleen (handboek §6: falen is luid, en één keer).
const LOUD_REFUSALS: u64 = 3;

/// De DNS-server uit de lease, of `None` zolang er geen lease is.
pub(crate) fn uplink_dns() -> Option<Ipv4Addr> {
    match UPLINK_DNS.load(Relaxed) {
        0 => None,
        ip => Some(Ipv4Addr::from(ip)),
    }
}

/// Het MAC-adres van de uplink, of `None` zonder NIC.
pub(crate) fn uplink_mac() -> Option<[u8; 6]> {
    match UPLINK_MAC.load(Relaxed).to_be_bytes() {
        [_, _, 0, 0, 0, 0, 0, 0] => None,
        [_, _, m @ ..] => Some(m),
    }
}

/// Het uplink-adres na de lease, of `None` zolang er geen lease is.
pub(crate) fn uplink_ip() -> Option<Ipv4Addr> {
    match UPLINK_IP.load(Relaxed) {
        0 => None,
        ip => Some(Ipv4Addr::from(ip)),
    }
}

/// Zet poort `port` (`proto`) van de uplink door naar dezelfde poort in
/// `slot` en wacht op `ack`: de poorten van een start (Hop en elke
/// jobspec), door de lifecycle-actor (`kooi.rs`), die elke bevestiging
/// afwacht voor hij de volgende stuurt. Alleen met een draaiende switch
/// ([`switch_up`]): anders leest niemand de brievenbus en duurt de wacht
/// eeuwig.
pub(crate) async fn publish(
    ack: &'static Ack,
    proto: net::nat::Proto,
    slot: usize,
    port: u16,
) -> Result<(), net::Error> {
    let cmd = Command::Publish {
        proto,
        node_port: port,
        slot,
        slot_port: port,
        ack,
    };
    match sync::oneshot::call(&COMMANDS, ack, cmd).await {
        Ok(r) => r.map(|_| ()),
        Err(_) => Err(net::Error::Full("switch mailbox", switch::COMMANDS)),
    }
}

/// Draait de switch-actor? Zonder NIC niet, en dan beantwoordt niemand een
/// commando.
pub(crate) fn switch_up() -> bool {
    SWITCH_UP.load(Relaxed)
}

/// De vaste instellingen van het netwerkvlak, uit `main`.
pub(crate) struct Params {
    /// Het hoogste slotnummer van dit board.
    pub(crate) max_slots: usize,
    /// De klok van het board (monotone nanoseconden).
    pub(crate) now: Clock,
    /// De kick van slot `i` na een leeg-naar-niet-leeg-schrijf in zijn
    /// RX-ring (van de slot-lifecycle; zonder slots niets).
    pub(crate) slot_wake: fn(usize),
    /// Deelt de bewoner van slot `i` de OS-core? Dan wacht de switch niet
    /// op zijn RX-ring (`net::switch::Config::resident`).
    pub(crate) resident: fn(usize) -> bool,
}

/// Wat de system-listener van de lifecycle-kant krijgt: de system-API en de
/// lijm die `serve` vraagt, alle `static` (alle verbindingstaken lenen ze).
pub(crate) struct SystemApi {
    /// De system-API over de lifecycle-inbox en de servicers.
    pub(crate) system: &'static crate::KernSystem,
    /// Eén antwoordplek per verbindingstaak: een antwoord van de actor
    /// landt nooit bij een andere verbinding.
    pub(crate) replies: &'static [Reply; SYSTEM_WORKERS],
    /// Klok en flip.
    pub(crate) hooks: &'static crate::BootHooks,
    /// De console voor app-logregels.
    pub(crate) log: &'static crate::SystemLog,
}

/// Waarom het netwerkvlak niet opkwam.
#[derive(Debug)]
pub(crate) enum Error {
    /// Een boot-allocatie faalde (bytes).
    OutOfMemory(usize),
    /// Een ring kon niet op zijn backing (`abi::ring`).
    Ring(abi::Error),
    /// De switch of de host-poort weigerde.
    Net(net::Error),
    /// Een rij was al gesplitst: `start` twee keer.
    Twice,
    /// De executor nam een taak niet aan.
    Spawn(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory(n) => write!(f, "out of memory allocating {n} bytes"),
            Self::Ring(e) => write!(f, "host ring: {e}"),
            Self::Net(e) => write!(f, "{e}"),
            Self::Twice => f.write_str("net started twice"),
            Self::Spawn(what) => write!(f, "spawn {what} refused"),
        }
    }
}

/// Zet het netwerkvlak op en spawnt zijn taken, in de volgorde van het
/// handboek §4 (de pompen eerst): de RX-pomp, de switch, de flow-expiry, de
/// host-taak. De host-taak haalt de lease, zet de stack op en spawnt dan de
/// DHCP-keeper en de system-listener.
pub(crate) fn start<D: Device + 'static>(
    exec: &'static Executor,
    nic: D,
    p: Params,
    api: SystemApi,
) -> Result<(), Error> {
    let mac = nic.mac().0;
    let [a, b, c, d, e, f] = mac;
    UPLINK_MAC.store(u64::from_be_bytes([0, 0, a, b, c, d, e, f]), Relaxed);
    let (ing_tx, ing_rx) = INGRESS.split().ok_or(Error::Twice)?;
    let (eg_tx, eg_rx) = EGRESS.split().ok_or(Error::Twice)?;

    // Poort 0: twee ringen in de kern-heap. Ze zijn kern-intern (geen ABI,
    // geen app ziet ze), dus hoeven ze niet in een DMA-regio.
    let host_tx = ring_backing(HOST_RING_DATA)?; // HOP → switch
    let host_rx = ring_backing(HOST_RING_DATA)?; // switch → HOP
    let sw_tx = AbiTx::open(host_tx, HOST_RING_DATA, HOST_RINGS).map_err(Error::Ring)?;
    let sw_rx = RingTx::open_with(host_rx, HOST_RING_DATA, HOST_RINGS).map_err(Error::Ring)?;
    let hop_rx = AbiTx::open(host_rx, HOST_RING_DATA, HOST_RINGS).map_err(Error::Ring)?;
    let hop_tx = RingTx::open_with(host_tx, HOST_RING_DATA, HOST_RINGS).map_err(Error::Ring)?;

    let mut sw = Switch::new(
        switch::Config {
            max_slots: p.max_slots,
            now: p.now,
            log: log_line,
            slot_wake: p.slot_wake,
            resident: p.resident,
            neighbors: NEIGHBORS,
        },
        Wiring {
            commands: &COMMANDS,
            door: &DOOR,
            published: &PUBLISHED,
            stats: &STATS,
            host_bell: Some(&HOST_BELL),
            ingress: Some(ing_rx),
            egress: Some(eg_tx),
            pump_bell: Some(&PUMP_BELL),
        },
    )
    .map_err(Error::Net)?;
    sw.attach_host(sw_tx, sw_rx);

    // De stop van de switch ([`net::switch::Xoff`]): de pomp opent hem als
    // de NIC weer frames neemt.
    let mut pump = Pump::new(
        nic,
        ing_tx,
        eg_rx,
        &PUMP_BELL,
        &DOOR,
        PUBLISHED.xoff(),
        &STATS,
    );
    let irq = pump.nic().irq().is_some();
    exec.spawn(async move { pump.run(exec).await })
        .map_err(|_| Error::Spawn("pump"))?;
    println!(
        "net: pump on the nic ({}), uplink queues 2x{} HOPOS_NET_PUMP",
        if irq {
            "irq line, 10 ms guard"
        } else {
            "polled every 300 us"
        },
        net::UPLINK_QUEUE,
    );

    let mut sw_buf = boot_buf(MAX_LAN_FRAME)?;
    exec.spawn(async move { sw.run(exec, &mut sw_buf).await })
        .map_err(|_| Error::Spawn("switch"))?;
    SWITCH_UP.store(true, Relaxed);
    exec.spawn(switch::flow_expiry(exec, &COMMANDS))
        .map_err(|_| Error::Spawn("flow expiry"))?;
    println!(
        "net: switch up, ports 0..={}, host rings 2x{} bytes HOPOS_SWITCH_UP",
        p.max_slots, HOST_RING_DATA
    );

    let host_buf = boot_buf(MAX_LAN_FRAME)?;
    let node = Node {
        exec,
        mac,
        max_slots: p.max_slots,
        rx: hop_rx,
        tx: hop_tx,
        buf: host_buf,
    };
    exec.spawn(node.run(api))
        .map_err(|_| Error::Spawn("host"))?;
    Ok(())
}

/// De console als `LogFn` voor de switch.
fn log_line(args: fmt::Arguments<'_>) {
    println!("{args}");
}

/// Eén cacheline: de eenheid van de host-ringen. Kop, staart en rand delen
/// zo nooit een regel met ander heap-geheugen, en dat moet: `dev::pull`
/// invalideert hele regels.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([u8; 64]);

/// Een ring-backing met `data` bytes datacapaciteit in de kern-heap,
/// klaargezet met `abi::ring::init`. Hij leeft voor altijd: de heap geeft
/// hem nooit terug en niemand buiten `dev` raakt hem nog aan.
fn ring_backing(data: u64) -> Result<Pa, Error> {
    let bytes = usize::try_from(abi::ring::DATA_OFF + data).map_err(|_| Error::OutOfMemory(0))?;
    let lines = bytes.div_ceil(core::mem::size_of::<Line>());
    let mut v: Vec<Line> = Vec::new();
    v.try_reserve_exact(lines)
        .map_err(|_| Error::OutOfMemory(bytes))?;
    v.resize(lines, Line([0; 64]));
    // Uit de handen van Rust: vanaf hier is het geheugen van de ring, en de
    // ring praat alleen via `dev` met een adres (de kern-RAM is identity
    // gemapt, dus het virtuele adres is het fysieke).
    let raw = Box::into_raw(v.into_boxed_slice());
    let pa = Pa(raw.cast::<Line>().expose_provenance() as u64);
    abi::ring::init(pa, data).map_err(Error::Ring)?;
    Ok(pa)
}

/// Een framebuffer uit de heap (boot): te groot voor de stack van een taak.
fn boot_buf(n: usize) -> Result<Vec<u8>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Error::OutOfMemory(n))?;
    v.resize(n, 0);
    Ok(v)
}

// ---------------------------------------------------------------------------
// De host-taak: eerst DHCP rauw over de ringen, dan de stack in poort 0.
// ---------------------------------------------------------------------------

/// De host-taak met zijn eigendom: de twee ringen van poort 0 en zijn
/// framebuffer.
struct Node {
    exec: &'static Executor,
    mac: [u8; 6],
    max_slots: usize,
    rx: RingRx,
    tx: RingTx,
    buf: Vec<u8>,
}

impl Node {
    /// De levensloop: lease, uplink, stack, keeper en listener, en dan de
    /// lus van poort 0 tot de stop.
    async fn run(mut self, api: SystemApi) {
        let exec = self.exec;
        let lease = self.lease().await;
        let cidr = lease.cidr();
        let ip = u32::from(lease.ip);
        let mac = self.mac;

        // De switch moet het externe adres kennen vóór de stack praat: pas
        // dan scheidt de NAT gepubliceerde poorten en masquerade-flows van
        // verkeer voor de node zelf.
        match Uplink::new(ip, cidr.prefix, mac, u32::from(lease.gateway)) {
            Ok(uplink) => {
                let cmd = Command::SetUplink {
                    uplink,
                    ack: &UPLINK_ACK,
                };
                match sync::oneshot::call(&COMMANDS, &UPLINK_ACK, cmd).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => println!("net: switch refused the uplink: {e} HOPOS_NET_FAIL"),
                    Err(_) => println!("net: switch mailbox full, uplink not set HOPOS_NET_FAIL"),
                }
            }
            Err(e) => println!("net: uplink /{}: {e} HOPOS_NET_FAIL", cidr.prefix),
        }

        let stack = match Stack::new(stack_config(&lease, mac), iss_seed(exec, mac)) {
            Ok(s) => s,
            Err(e) => {
                println!("net: node stack: {e} HOPOS_NET_FAIL");
                return;
            }
        };
        *STACK.borrow_mut() = Some(stack);
        UPLINK_IP.store(ip, Relaxed);
        UPLINK_DNS.store(u32::from(lease.dns), Relaxed);
        println!(
            "net: {} (mac {}, gw {}) HOPOS_NET_UP",
            lease.ip,
            netdev::Mac(mac),
            lease.gateway
        );

        if exec.spawn(keep_lease(exec, mac, lease)).is_err() {
            println!("net: dhcp keeper not spawned, the lease will lapse HOPOS_DHCP_LOST");
            crate::watchdog::request_reset("dhcp keeper not spawned");
        }
        if exec.spawn(system_listener(exec, lease.ip, api)).is_err() {
            println!("system: listener not spawned HOPOS_SYSTEM_FAIL");
        }
        if crate::conport::enabled() {
            if exec.spawn(console_listener(exec, lease.ip)).is_err() {
                println!("console: listener not spawned, serial line only HOPOS_CONPORT_FAIL");
            }
        } else {
            println!(
                "console: tcp/{CONSOLE_PORT} off (hopos.console=1 or hopos.insecure=1 opens it), serial line only HOPOS_CONPORT_OFF"
            );
        }

        let Node {
            rx,
            tx,
            mut buf,
            max_slots,
            ..
        } = self;
        let node = NodeStack { exec };
        HostPort::new(node, rx, tx, &HOST_BELL, &DOOR, &STATS, mac, ip, max_slots)
            .run(exec, &mut buf)
            .await;
    }

    /// Haalt een lease, en blijft het proberen: een node zonder adres kan
    /// niets, maar de rest van de kern draait door.
    async fn lease(&mut self) -> Lease {
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            let t0 = self.exec.now();
            match self.dhcp().await {
                Ok(l) => {
                    let ms = self.exec.now().saturating_sub(t0) / 1_000_000;
                    println!(
                        "dhcp: lease {} from {}, dns {}, {} s, attempt {attempt}, {ms} ms HOPOS_DHCP_BOUND",
                        l.cidr(),
                        l.server,
                        l.dns,
                        l.lease_secs,
                    );
                    return l;
                }
                Err(e) => {
                    // Go: de eerste poging en daarna elke tiende, niet elke 15 s.
                    if attempt == 1 || attempt.is_multiple_of(10) {
                        println!("{e}, attempt {attempt}, retry in 5 s HOPOS_DHCP_FAIL");
                    }
                    self.exec.after(DHCP_RETRY).await;
                }
            }
        }
    }

    /// Eén DHCP-poging over de rauwe ringen van poort 0: zonder uplink-adres
    /// geeft de switch alles van de draad aan ons (`KIND_UPLINK`), en wat wij
    /// als `KIND_UPLINK` schrijven gaat de draad op. Dat is Go's
    /// `hopnet.Up`-volgorde: eerst de lease, dan de stack.
    async fn dhcp(&mut self) -> leandhcp::Result<Lease> {
        let exec = self.exec;
        let now = || Instant::from_duration(Duration::from_nanos(exec.now()));
        let mut c = Client::new(self.mac, now(), DHCP_TIMEOUT);
        loop {
            // Eén frame toevoeren en dan pollen, zoals de client vraagt.
            let fed = match self.rx.read_into(&mut self.buf) {
                Some((KIND_UPLINK, n)) => {
                    if let Some(f) = self.buf.get(..n) {
                        c.receive(f, now());
                    }
                    true
                }
                Some(_) => true, // Geen uplink-frame: niet voor de client.
                None => false,
            };
            let step = match c.poll(now())? {
                Action::Transmit(f) => Step::Sent(self.tx.write_notify(KIND_UPLINK, f).is_some()),
                Action::Wait(until) => Step::Wait(until),
                Action::Bound(l) => return Ok(l),
            };
            match step {
                Step::Sent(true) => DOOR.set(),
                Step::Sent(false) => c.transmit_failed(),
                // Lag er een frame, dan eerst de ring leeg.
                Step::Wait(_) if fed => {}
                Step::Wait(until) => {
                    let at = u64::try_from(until.as_duration().as_nanos()).unwrap_or(u64::MAX);
                    let _ = select(HOST_BELL.wait(), exec.until(at)).await;
                }
            }
        }
    }
}

/// Wat de DHCP-lus na een poll doet; los van de lening op de client.
enum Step {
    /// Het frame ging de ring in (of niet).
    Sent(bool),
    /// Wachten tot dit tijdstip of een frame.
    Wait(Instant),
}

/// De stack-instellingen uit de lease, zoals Go's `stackUp`: het adres van
/// de uplink, maar het device is ook het slot-LAN, en dáár gelden de
/// jumbo-MTU en de vertrouwde link (de ringen zijn geheugen).
fn stack_config(lease: &Lease, mac: [u8; 6]) -> leannet::Config {
    let prefix = u8::try_from(lease.cidr().prefix).unwrap_or(32);
    leannet::Config {
        ip: lease.ip.octets(),
        prefix,
        mac,
        gw: lease.gateway.octets(),
        budget: NET_BUDGET,
        // Het venster naar een app blijft onder de halve slot-ring, anders
        // loopt die vol zodra HOP sneller is dan de pomp van de app (Go,
        // 04-09: 128x rx-full bij vier hameraars).
        max_buf_per_conn: usize::try_from(NET_RING_DATA_CAP / 2).unwrap_or(0),
        adv_ws: ws_shift_for(NET_BUDGET / 4),
        mtu: NET_MTU,
        mtu_net: HOST_IP4.to_be_bytes(),
        mtu_prefix: u8::try_from(NET_PREFIX).unwrap_or(24),
        link_trusted: true,
    }
}

/// De kleinste window-scale-shift die een venster van `max` bytes draagt
/// (RFC 7323, plafond 14).
fn ws_shift_for(max: usize) -> u8 {
    let mut shift = 0u8;
    while shift < 14 && (0xffff_usize << shift) < max {
        shift += 1;
    }
    shift
}

/// De ISS-seed: niet kryptografisch, wel per boot anders. Er is nog geen
/// wandklok, dus de teller sinds boot plus de MAC (Go deed de boot-tijd).
fn iss_seed(exec: &Executor, mac: [u8; 6]) -> u32 {
    let m = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    // Afkappen is hier de bedoeling: alleen de lage bits variëren.
    (exec.now() as u32) ^ m.rotate_left(13)
}

// ---------------------------------------------------------------------------
// De node-stack achter poort 0.
// ---------------------------------------------------------------------------

/// De `HostStack` van poort 0 over de `leannet::Stack` in [`STACK`].
struct NodeStack {
    exec: &'static Executor,
}

impl HostStack for NodeStack {
    fn receive(&mut self, frame: &[u8]) {
        let now = self.exec.now();
        // Een frame dat de stack weigert (stack dicht) telt als host-drop;
        // wat hij zelf weggooit, telt hij zelf.
        if on_stack(|st| st.receive(frame, now)).is_err() {
            STATS.host_rx_drops.fetch_add(1, Relaxed);
        }
    }

    fn poll_transmit(&mut self, buf: &mut [u8]) -> Option<usize> {
        let now = self.exec.now();
        on_stack(|st| Ok(st.poll_transmit(now, buf))).ok().flatten()
    }

    fn poll_at(&self) -> Option<u64> {
        let now = self.exec.now();
        on_stack(|st| Ok(st.next_timeout(now))).ok().flatten()
    }
}

/// De neighbour-tabel van de node-stack voor de NAT van de switch: Linux
/// heeft er één, en de forwarding put eruit. Geen stack (nog niet, of een
/// lening die al loopt) is onbekend; een vraag of twijfel belt de host-taak,
/// die de ARP-vraag de draad op zet.
const NEIGHBORS: Neighbors = Neighbors {
    resolve: |dst, now| {
        let mac = on_stack(|st| Ok(st.neighbor(dst.to_be_bytes(), now)))
            .ok()
            .flatten();
        if mac.is_none() {
            HOST_BELL.set();
        }
        mac
    },
    probe: |dst, now| {
        let _ = io(|st| {
            st.probe_neighbor(dst.to_be_bytes(), now);
            Ok(())
        });
    },
    confirm: |src, mac, now| {
        let _ = on_stack(|st| {
            st.confirm_neighbor(src.to_be_bytes(), mac, now);
            Ok(())
        });
    },
};

/// Eén korte lening op de stack, zonder `.await`. Geen stack (nog niet, of
/// een lening die al loopt) is `StackClosed`: de aanroeper probeert het na
/// een bel opnieuw.
fn on_stack<T>(f: impl FnOnce(&mut Stack) -> leannet::Result<T>) -> leannet::Result<T> {
    let Ok(mut cell) = STACK.try_borrow_mut() else {
        return Err(leannet::Error::StackClosed);
    };
    match cell.as_mut() {
        Some(st) => f(st),
        None => Err(leannet::Error::StackClosed),
    }
}

/// Een socket-call van buiten de host-taak: na succes de bel van de
/// host-taak, want een write, read of close kan uitgaand werk maken (data,
/// een ACK, een vensterupdate, een FIN).
fn io<T>(f: impl FnOnce(&mut Stack) -> leannet::Result<T>) -> leannet::Result<T> {
    let r = on_stack(f);
    if r.is_ok() {
        HOST_BELL.set();
    }
    r
}

/// Probeert `op`; bij `WouldBlock` registreert `park` de waker van deze
/// taak op het handvat en is het `Pending`. Registreren en opnieuw kijken
/// gebeuren in dezelfde lening, dus een wek kan er niet tussendoor vallen.
fn poll_stack<T>(
    exec: &Executor,
    cx: &mut Context<'_>,
    op: impl FnOnce(&mut Stack, u64) -> leannet::Result<T>,
    park: impl FnOnce(&mut Stack, &Waker) -> leannet::Result,
) -> Poll<leannet::Result<T>> {
    let now = exec.now();
    let r = io(|st| match op(st, now) {
        Err(leannet::Error::WouldBlock) => {
            park(st, cx.waker())?;
            Err(leannet::Error::WouldBlock)
        }
        r => r,
    });
    match r {
        Err(leannet::Error::WouldBlock) => Poll::Pending,
        r => Poll::Ready(r),
    }
}

// ---------------------------------------------------------------------------
// De DHCP-keeper: de lease in leven houden over UDP op de stack.
// ---------------------------------------------------------------------------

/// De keeper-taak (Go: `keepLease`): RENEW op T1, REBIND op T2, luid als het
/// adres weg is. Zodra de stack de ringen bezit, spreekt de keeper UDP op
/// poort 68 en zendt de stack.
///
/// Een verlopen lease zonder antwoord is een netwerk zonder server, geen
/// adresconflict: het adres blijft in gebruik en een keeper op een proeflease
/// blijft rebinden tot een server het bevestigt. Alleen een NAK of een ander
/// adres kost het adres.
async fn keep_lease(exec: &'static Executor, mac: [u8; 6], lease: Lease) {
    let now = || Instant::from_duration(Duration::from_nanos(exec.now()));
    let h = match io(|st| st.udp_bind(leandhcp::CLIENT_PORT)) {
        Ok(h) => h,
        Err(e) => {
            println!(
                "dhcp: bind udp {}: {e}, the lease will lapse HOPOS_DHCP_LOST",
                leandhcp::CLIENT_PORT
            );
            crate::watchdog::request_reset("dhcp keeper could not bind");
            return;
        }
    };
    let mut k = Keeper::new(mac, lease, now());
    // Gezet na het verlopen: de mislukte pogingen zwijgen tot de eerste ACK.
    let mut held = false;
    let mut out = [0u8; leannet::UDP_MAX_PAYLOAD];
    let mut inb = [0u8; leannet::UDP_MAX_PAYLOAD];
    loop {
        let step = match k.poll(now()) {
            Ok(KeepAction::Send { to, payload }) => {
                let n = payload.len().min(out.len());
                if let (Some(d), Some(s)) = (out.get_mut(..n), payload.get(..n)) {
                    d.copy_from_slice(s);
                }
                Keep::Send(to, n)
            }
            Ok(KeepAction::Wait(until)) => Keep::Wait(until),
            Ok(KeepAction::Event(e)) => {
                if matches!(e, leandhcp::Event::Extended { .. }) {
                    held = false;
                }
                if !held {
                    println!("{e}");
                }
                continue;
            }
            Ok(KeepAction::Done) => {
                println!("dhcp: lease without end, nothing to keep");
                return;
            }
            Err(leandhcp::Error::Expired { ip }) => {
                // De keeper stopt na het verlopen; een nieuwe op de proeflease
                // rebindt verder (Go: keepLease na KeepAlive).
                if !held {
                    println!(
                        "dhcp: lease on {ip} expired without an answer, keeping the address and rebinding HOPOS_DHCP_RETRY"
                    );
                    held = true;
                }
                let (lease_secs, t1_secs, t2_secs) = DHCP_PROBE;
                let probe = Lease {
                    lease_secs,
                    t1_secs,
                    t2_secs,
                    ..*k.lease()
                };
                k = Keeper::new(mac, probe, now());
                continue;
            }
            Err(e) => {
                // Een NAK of een ander adres (Go hing `requestNodeReset` aan
                // `hopnet.AddressLost`): de stack kan niet van adres wisselen,
                // dus dat is het einde van deze node-levensduur. De watchdog
                // houdt zijn pets in en het ijzer reset; zonder gewapende
                // watchdog blijft het bij de melding.
                println!("{e} HOPOS_DHCP_LOST");
                crate::watchdog::request_reset("dhcp lease lost");
                return;
            }
        };
        match step {
            Keep::Send(to, n) => {
                let dst = Endpoint {
                    ip: to.octets(),
                    port: leandhcp::SERVER_PORT,
                };
                let t = exec.now();
                let data = out.get(..n).unwrap_or(&[]);
                if io(|st| st.udp_send_to(h, dst, data, t)).is_err() {
                    k.transmit_failed(now());
                }
            }
            Keep::Wait(until) => {
                let at = u64::try_from(until.as_duration().as_nanos()).unwrap_or(u64::MAX);
                let _ = select(udp_readable(exec, h), exec.until(at)).await;
                let t = exec.now();
                while let Ok((n, _)) = io(|st| st.udp_recv_from(h, &mut inb, t)) {
                    if let Some(p) = inb.get(..n) {
                        k.receive(p, now());
                    }
                }
            }
        }
    }
}

/// Wat de keeper na een poll doet; los van de lening op de keeper.
enum Keep {
    /// Zend `n` bytes uit de zendbuffer naar deze server.
    Send(Ipv4Addr, usize),
    /// Wachten tot dit tijdstip of een datagram.
    Wait(Instant),
}

/// Wacht tot er een datagram op `h` ligt.
fn udp_readable(exec: &'static Executor, h: UdpHandle) -> impl Future<Output = ()> {
    poll_fn(move |cx| {
        let r = poll_stack(
            exec,
            cx,
            |st, _| match st.udp_readable(h) {
                Ok(true) => Ok(()),
                Ok(false) => Err(leannet::Error::WouldBlock),
                Err(e) => Err(e),
            },
            |st, w| st.udp_register_read_waker(h, w),
        );
        r.map(|_| ())
    })
}

// ---------------------------------------------------------------------------
// De system-listener.
// ---------------------------------------------------------------------------

/// Eén toegelaten verbinding, zoals de listener hem aan een taak geeft: het
/// handvat, het bronadres en de toelating (die in haar `Drop` de plaats bij
/// het slot teruggeeft).
struct Job {
    h: TcpHandle,
    remote: u32,
    who: Admitted<'static>,
}

/// De deuren van de pool, één per verbindingstaak: de listener geeft een
/// verbinding aan de eerste vrije taak (en weigert als alle bezet zijn),
/// de taak meldt zich vrij na de verbinding. Kwamen er minder taken op
/// dan deuren, dan blijven de deuren zonder taak dicht
/// ([`Doors::staffed`]): een verbinding daar hing voorgoed, met haar
/// plaats bij het slot.
static DOORS: Doors<Job, SYSTEM_WORKERS> = Doors::new();

/// De listener op [`PORT`] (Go: `ServeSystem`): per verbinding `admit`
/// (het slot uit het bron-IP, een levende servicer, hooguit
/// [`MAX_SYSTEM_CONNS`]) en dan de verbinding naar een vrije taak uit de
/// pool. De listener zelf leest nooit van een verbinding, dus hij staat
/// altijd weer bij `accept`: een zwijgende peer houdt niemand op.
async fn system_listener(exec: &'static Executor, ip: Ipv4Addr, api: SystemApi) {
    let l = match io(|st| st.tcp_listen(PORT)) {
        Ok(l) => l,
        Err(e) => {
            println!("system: listen on {PORT}: {e} HOPOS_SYSTEM_FAIL");
            return;
        }
    };
    let workers = spawn_workers(exec, &api);
    DOORS.staffed(workers);
    if workers == 0 {
        println!("system: no connection tasks, listener closed HOPOS_SYSTEM_FAIL");
        io(|st| {
            st.tcp_listen_close(l);
            Ok(())
        })
        .ok();
        return;
    }
    println!(
        "system: listening on {ip}:{PORT} and {}:{PORT}, {workers} connection tasks HOPOS_SYSTEM_UP",
        Ipv4Addr::from(HOST_IP4)
    );
    let (mut served, mut refused, mut full) = (0u64, 0u64, 0u64);
    loop {
        let h = match accept(exec, l).await {
            Ok(h) => h,
            Err(e) => {
                println!("system: accept on {PORT}: {e}, listener closed HOPOS_SYSTEM_FAIL");
                return;
            }
        };
        let remote = io(|st| st.tcp_remote(h)).map_or(0, |ep| u32::from_be_bytes(ep.ip));
        if remote == u32::from(ip) {
            // De self-dial van de watchdog ([`dial`]): de handshake was het
            // bewijs. Geen weigerregel; die zijn voor wie van buiten belt.
            close(exec, h);
            continue;
        }
        let who = match api.system.admit(remote) {
            Ok(who) => who,
            Err(why) => {
                refused = refused.wrapping_add(1);
                if refused <= LOUD_REFUSALS {
                    println!(
                        "system: {} refused ({why}), {refused} so far HOPOS_SYSTEM_REFUSED",
                        Ipv4Addr::from(remote)
                    );
                }
                close(exec, h);
                continue;
            }
        };
        match DOORS.hand(Job { h, remote, who }) {
            Ok(()) => served = served.wrapping_add(1),
            Err(job) => {
                // Alle taken bezet: de toelating gaat met de job terug.
                full = full.wrapping_add(1);
                if full <= LOUD_REFUSALS {
                    println!(
                        "system: {} refused, all {workers} connection tasks busy, {full} so far, {served} served HOPOS_SYSTEM_FULL",
                        Ipv4Addr::from(job.remote)
                    );
                }
                close(exec, job.h);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// De console-listener.
// ---------------------------------------------------------------------------

/// De poort van de console over TCP (Go: `kern/conport`): `nc node 5555`.
const CONSOLE_PORT: u16 = 5555;

/// De listener op [`CONSOLE_PORT`]: per verbinding een lezersplaats
/// (`Ring::seat`) en een eigen taak die de ring afspeelt en volgt
/// (`kern::conport::stream`), dus de listener staat altijd weer bij
/// `accept`. Een fout is geen reden om de node te laten falen: de console
/// is gemak, geen functie.
async fn console_listener(exec: &'static Executor, ip: Ipv4Addr) {
    let l = match io(|st| st.tcp_listen(CONSOLE_PORT)) {
        Ok(l) => l,
        Err(e) => {
            println!(
                "console: tcp/{CONSOLE_PORT} unavailable ({e}), serial line only HOPOS_CONPORT_FAIL"
            );
            return;
        }
    };
    println!(
        "console: also on tcp/{CONSOLE_PORT} ({ip} and {}): nc, first the history, then live HOPOS_CONPORT_UP",
        Ipv4Addr::from(HOST_IP4)
    );
    loop {
        let h = match accept(exec, l).await {
            Ok(h) => h,
            Err(e) => {
                println!(
                    "console: accept on tcp/{CONSOLE_PORT}: {e}, port closed HOPOS_CONPORT_FAIL"
                );
                return;
            }
        };
        let remote = io(|st| st.tcp_remote(h)).map_or(0, |ep| u32::from_be_bytes(ep.ip));
        let Some(seat) = crate::conport::RING.seat() else {
            // Vol: één regel vóór de close (hij past in de lege zendring),
            // in plaats van stil vasthouden; de pool van de stack houdt plek
            // voor de rest van de node (Go, 11-08: negen browsertabs).
            println!(
                "console: {} refused, {} readers already attached HOPOS_CONPORT_FULL",
                Ipv4Addr::from(remote),
                kern::conport::MAX_READERS
            );
            let _ = io(|st| st.tcp_write(h, kern::conport::FULL, exec.now()));
            close(exec, h);
            continue;
        };
        let reader = async move {
            let (ring, mut conn) = (&crate::conport::RING, TcpConn { exec, h, remote });
            kern::conport::stream(ring, seat, &mut conn, &ExecTimer(exec), &mut [0u8; 1024]).await;
            close(exec, h);
            ring.leave(seat);
        };
        if exec.spawn(reader).is_err() {
            println!("console: reader task not spawned HOPOS_CONPORT_FAIL");
            close(exec, h);
            crate::conport::RING.leave(seat);
        }
    }
}

/// Spawnt de verbindingstaken, elk met haar eigen buffers (de heap geeft
/// ze eenmalig, als de listener na de lease opkomt) en haar eigen
/// antwoordplek. Geeft hoeveel er draaien: de eerste OOM of mislukte spawn
/// stopt de rij, en de listener sluit de deuren daarachter.
fn spawn_workers(exec: &'static Executor, api: &SystemApi) -> usize {
    let mut n = 0;
    for (i, reply) in api.replies.iter().enumerate() {
        let (Ok(buf), Ok(out)) = (boot_buf(MAX_PAYLOAD), boot_buf(OUT_BUF)) else {
            println!(
                "system: no call buffers ({MAX_PAYLOAD} bytes) for task {i} HOPOS_SYSTEM_FAIL"
            );
            break;
        };
        let w = Worker {
            exec,
            door: i,
            system: api.system,
            reply,
            hooks: api.hooks,
            log: api.log,
            buf,
            out,
        };
        if exec.spawn(w.run()).is_err() {
            println!("system: connection task {i} not spawned HOPOS_SYSTEM_FAIL");
            break;
        }
        n += 1;
    }
    n
}

/// Eén verbindingstaak met haar eigendom: buffers en antwoordplek. Ze dient
/// de ene verbinding na de andere, nooit twee tegelijk.
struct Worker {
    exec: &'static Executor,
    door: usize,
    system: &'static crate::KernSystem,
    reply: &'static Reply,
    hooks: &'static crate::BootHooks,
    log: &'static crate::SystemLog,
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl Worker {
    async fn run(mut self) {
        let timer = ExecTimer(self.exec);
        loop {
            let job = DOORS.take(self.door).await;
            let mut conn = TcpConn {
                exec: self.exec,
                h: job.h,
                remote: job.remote,
            };
            let end = self
                .system
                .serve(
                    &mut conn,
                    &job.who,
                    self.reply,
                    &timer,
                    &mut crate::DevMem,
                    self.hooks,
                    self.log,
                    &mut self.buf,
                    &mut self.out,
                )
                .await;
            let slot = job.who.slot();
            match end {
                End::Peer => println!("system: slot {slot} done"),
                End::Evicted => println!("system: slot {slot} closed: {end} HOPOS_SYSTEM_EVICTED"),
                End::Idle | End::Failed(_) => println!("system: slot {slot} closed: {end}"),
            }
            // Eerst het handvat dicht, dan de plaats terug (de `Drop` van
            // de toelating): een nieuwe verbinding van het slot vindt de
            // oude nooit nog half open.
            close(self.exec, job.h);
            drop(job);
            DOORS.free(self.door);
        }
    }
}

/// Sluit een handvat; een al gesloten handvat is geen fout.
fn close(exec: &Executor, h: TcpHandle) {
    let t = exec.now();
    let _ = io(|st| st.tcp_close(h, t));
}

/// De self-dial van de watchdog (Go: `probe` in `nodeCanary`, een
/// `net.DialTimeout` met 3 s): een nieuwe TCP-verbinding over de node-stack
/// naar `ip:port`, en na de handshake meteen weer dicht. Na `timeout` geeft
/// hij op, ook als de host-taak niet meer pompt en de stack dus niemand
/// wekt: dat is juist een doofheid die hij moet zien.
pub(crate) async fn dial(
    exec: &'static Executor,
    ip: Ipv4Addr,
    port: u16,
    timeout: Duration,
) -> leannet::Result {
    let now = exec.now();
    let deadline = now.saturating_add(u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX));
    let h = io(|st| st.tcp_connect(ip.octets(), port, Some(deadline), now))?;
    let connect = poll_fn(move |cx| {
        poll_stack(
            exec,
            cx,
            |st, now| st.tcp_poll_connect(h, now),
            |st, w| st.tcp_register_write_waker(h, w),
        )
    });
    let r = match select(connect, exec.until(deadline)).await {
        Either::Left(r) => r,
        // De klok won: nog één keer kijken geeft `DeadlineExceeded` en ruimt
        // de dial op.
        Either::Right(()) => io(|st| st.tcp_poll_connect(h, exec.now())),
    };
    if r.is_ok() {
        close(exec, h);
    }
    r
}

/// Wacht op de volgende verbinding van de listener.
fn accept(
    exec: &'static Executor,
    l: ListenHandle,
) -> impl Future<Output = leannet::Result<TcpHandle>> {
    poll_fn(move |cx| {
        poll_stack(
            exec,
            cx,
            |st, now| st.tcp_accept(l, now),
            |st, w| st.listen_register_waker(l, w),
        )
    })
}

/// Een TCP-verbinding op de node-stack als `kern::system::Conn`: een
/// handvat plus wakers, geen buffer van zichzelf.
struct TcpConn {
    exec: &'static Executor,
    h: TcpHandle,
    remote: u32,
}

impl Conn for TcpConn {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = kern::Result<usize>> {
        let (exec, h) = (self.exec, self.h);
        poll_fn(move |cx| {
            poll_stack(
                exec,
                cx,
                |st, now| st.tcp_read(h, buf, now),
                |st, w| st.tcp_register_read_waker(h, w),
            )
            .map(|r| r.map_err(|_| kern::Error::Conn))
        })
    }

    fn write(&mut self, buf: &[u8]) -> impl Future<Output = kern::Result<usize>> {
        let (exec, h) = (self.exec, self.h);
        poll_fn(move |cx| {
            poll_stack(
                exec,
                cx,
                |st, now| st.tcp_write(h, buf, now),
                |st, w| st.tcp_register_write_waker(h, w),
            )
            .map(|r| r.map_err(|_| kern::Error::Conn))
        })
    }

    fn remote_ip4(&self) -> u32 {
        self.remote
    }
}

// ---------------------------------------------------------------------------
// De input-listener (docs/gui.md): de USB-invoer naar de display-app.
// ---------------------------------------------------------------------------

/// De input-listener op `10.100.0.1:7879`, de node-kant van de switch zoals
/// de system-listener (Go: `usbin.listen` en `deliver.go`). Alleen in de
/// gui-smaak; kaal bestaat hij niet (handboek §7).
///
/// Eigendom: de taak bezit de listen-socket, de ene verbinding naar de
/// display-app en de ontvangkant van de invoerrij (in de
/// `gui_usbin::deliver::Deliverer`); de USB-taak (gui.rs) bezit de
/// zendkant. De houder van het glas leest hij per regel bij de grant
/// (`gui::glass_holder`), zonder die ooit vast te houden.
#[cfg(feature = "gui")]
pub(crate) mod input {
    use super::{accept, close, io, poll_stack};
    use abi::layout::HOST_IP4;
    use core::future::{Future, poll_fn};
    use core::net::Ipv4Addr;
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use core::time::Duration;
    use cpu::println;
    use executor::Executor;
    use gui_usbin::deliver::{
        self, ConnError, Deliverer, INPUT_PORT, InputConn, KEEPALIVE_NS, Lines, WRITE_DEADLINE_NS,
    };
    use leannet::TcpHandle;
    use sync::{Either, select};

    /// Regels die wegvielen omdat er geen display-app verbonden was: er
    /// kijkt dan niemand, en invoer bewaren levert alleen een lawine bij het
    /// aansluiten (Go deed hetzelfde). Een teller, geen fout.
    pub(crate) static DROPPED: AtomicU64 = AtomicU64::new(0);
    /// Regels (keepalives niet meegeteld) die de display-app kreeg.
    pub(crate) static SENT: AtomicU64 = AtomicU64::new(0);
    /// Verbindingen die niet van de houder van het glas kwamen.
    pub(crate) static REFUSED: AtomicU64 = AtomicU64::new(0);

    /// Hoe vaak de listener het opnieuw probeert zolang de node-stack er
    /// nog niet is (de USB-taak start vóór de DHCP-lease).
    const STACK_RETRY: Duration = Duration::from_millis(100);

    /// Weigeringen en drops die een eigen regel krijgen; daarna tellen.
    const LOUD: u64 = 3;

    /// De taak: luisteren zodra de node-stack er is, dan de ene verbinding
    /// van de houder bedienen. Een nieuwe verbinding van de houder verdringt
    /// de oude (een display die herstart, krijgt het toetsenbord terug
    /// zonder dat iemand de dode verbinding hoeft op te ruimen); zonder
    /// verbinding gaan de regels weg en tellen ze.
    pub(crate) async fn serve(exec: &'static Executor, mut d: Deliverer<'static>) {
        let l = loop {
            match io(|st| st.tcp_listen(INPUT_PORT)) {
                Ok(l) => break l,
                Err(leannet::Error::StackClosed) => exec.after(STACK_RETRY).await,
                Err(e) => {
                    println!("input: listen on {INPUT_PORT}: {e} HOPOS_INPUT_FAIL");
                    return;
                }
            }
        };
        println!(
            "input: listening on {}:{INPUT_PORT} for the holder of the glass HOPOS_INPUT_UP",
            Ipv4Addr::from(HOST_IP4)
        );
        let tick = || exec.after(Duration::from_nanos(KEEPALIVE_NS));
        let mut conn: Option<Stream> = None;
        loop {
            let got = match conn.as_mut() {
                None => match select(accept(exec, l), d.next(tick())).await {
                    Either::Left(r) => r,
                    Either::Right(lines) => {
                        drop_lines(&lines);
                        continue;
                    }
                },
                Some(c) => match select(d.serve(c, tick), accept(exec, l)).await {
                    Either::Left(e) => {
                        if let Some(c) = conn.take() {
                            println!(
                                "input: {} stream ended: {e}, {} lines sent HOPOS_INPUT_GONE",
                                Ipv4Addr::from(c.remote),
                                SENT.load(Relaxed)
                            );
                            close(exec, c.h);
                        }
                        continue;
                    }
                    Either::Right(r) => r,
                },
            };
            let h = match got {
                Ok(h) => h,
                Err(e) => {
                    println!(
                        "input: accept on {INPUT_PORT}: {e}, listener closed HOPOS_INPUT_FAIL"
                    );
                    return;
                }
            };
            let remote = io(|st| st.tcp_remote(h)).map_or(0, |ep| u32::from_be_bytes(ep.ip));
            if !deliver::allowed(remote, crate::gui::glass_holder()) {
                let n = REFUSED.fetch_add(1, Relaxed).wrapping_add(1);
                if n <= LOUD {
                    println!(
                        "input: {} refused, it does not hold the glass, {n} so far HOPOS_INPUT_REFUSED",
                        Ipv4Addr::from(remote)
                    );
                }
                close(exec, h);
                continue;
            }
            if let Some(old) = conn.take() {
                println!(
                    "input: {} replaced by {}, {} lines sent HOPOS_INPUT_GONE",
                    Ipv4Addr::from(old.remote),
                    Ipv4Addr::from(remote),
                    SENT.load(Relaxed)
                );
                close(exec, old.h);
            }
            println!(
                "input: {} connected (holder of the glass), {} lines dropped while nobody listened HOPOS_INPUT_CONN",
                Ipv4Addr::from(remote),
                DROPPED.load(Relaxed)
            );
            conn = Some(Stream { exec, h, remote });
        }
    }

    /// Telt de regels die niemand kreeg; een keepalive is geen invoer.
    fn drop_lines(lines: &Lines) {
        let n = lines.iter().filter(|l| *l != b"\n").count() as u64;
        if n == 0 {
            return;
        }
        let before = DROPPED.fetch_add(n, Relaxed);
        if before < LOUD {
            println!(
                "input: no display app connected, {} line(s) dropped (counted) HOPOS_INPUT_DROP",
                before.saturating_add(n)
            );
        }
    }

    /// De verbinding met de display-app op de node-stack: een handvat, geen
    /// buffer van zichzelf.
    struct Stream {
        exec: &'static Executor,
        h: TcpHandle,
        remote: u32,
    }

    impl InputConn for Stream {
        /// Eén regel binnen [`WRITE_DEADLINE_NS`]. Per regel opnieuw de
        /// houder: is het glas intussen van een ander slot (de display-app
        /// stopte, een nieuwe kreeg het), dan gaat de stroom dicht in plaats
        /// van andermans toetsen door te geven.
        fn write(&mut self, line: &[u8]) -> impl Future<Output = Result<(), ConnError>> {
            let (exec, h, remote) = (self.exec, self.h, self.remote);
            async move {
                if !deliver::allowed(remote, crate::gui::glass_holder()) {
                    return Err(ConnError::Closed);
                }
                let deadline = exec.after(Duration::from_nanos(WRITE_DEADLINE_NS));
                match select(write_all(exec, h, line), deadline).await {
                    Either::Left(Ok(())) => {
                        if line != b"\n" {
                            SENT.fetch_add(1, Relaxed);
                        }
                        Ok(())
                    }
                    Either::Left(Err(e)) => Err(e),
                    Either::Right(()) => Err(ConnError::Timeout),
                }
            }
        }

        fn remote_ip4(&self) -> u32 {
            self.remote
        }
    }

    /// Schrijft `buf` helemaal; de stack neemt wat in zijn venster past.
    async fn write_all(
        exec: &'static Executor,
        h: TcpHandle,
        mut buf: &[u8],
    ) -> Result<(), ConnError> {
        while !buf.is_empty() {
            let n = poll_fn(|cx| {
                poll_stack(
                    exec,
                    cx,
                    |st, now| st.tcp_write(h, buf, now),
                    |st, w| st.tcp_register_write_waker(h, w),
                )
            })
            .await
            .map_err(|_| ConnError::Closed)?;
            if n == 0 {
                return Err(ConnError::Closed);
            }
            buf = buf.get(n..).unwrap_or_default();
        }
        Ok(())
    }
}
