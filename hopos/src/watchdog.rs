//! De node-watchdog: het beleid van `kern::watchdog` op de hardware van het
//! board, als taak op de OS-core (`OLD/metal/cmd/hopos/watchdog.go`,
//! `nodeCanary`, `armBootGuard`, `requestNodeReset`).
//!
//! Dit bezit de watchdog-taak en het levensbewijs: elke [`PET_EVERY`] kijkt
//! hij of de node gezond is (een verse verbinding door de eigen
//! accept-laag, en als Hop op deze node woont zijn heartbeat die loopt) en
//! geeft dat aan het beleid, dat dan wel of niet aait. Omdat de pets uit
//! een taak op de executor komen, stopt een bevroren executor ze vanzelf;
//! dat is de helft van het vangnet. De andere helft is het bewijs: een node
//! die draait maar doof is, aait niet meer.
//!
//! Het bewijs is de canary van Go (`nodeCanary`): elke ronde een NIEUWE
//! TCP-verbinding over de node-stack, met 3 s geduld, en alleen een
//! geslaagde handshake telt. De les is van 02-08: nieuwe verbindingen en
//! ICMP dood terwijl alle interne lussen kerngezond waren; een heartbeat
//! alleen ziet dat niet. Woont Hop hier, dan belt de kern Hop's agent op
//! zijn slot-adres (10.100.0.2:8080): de node-stack, de host-taak, de
//! switch-actor, Hop's ring, Hop's stack en Hop's accept-laag, heen en
//! terug. Zonder Hop belt hij zijn eigen system-poort op het uplink-adres
//! (de loopback in de stack, zoals Go's self-dial door locdev): de
//! accept-laag van de kern, die er altijd luistert. Een geslaagde dial is
//! [`STALE_NS`] lang vers, net als de heartbeat: een herstart van Hop is
//! een hapering, geen hang.
//!
//! Niet via de DNAT van de uplink: een dial van de node-stack naar het
//! eigen uplink-adres keert in de stack zelf om (loopback) en de switch
//! kent voor poort 0 geen haarspeld, dus zo'n verbinding komt nooit bij
//! Hop. Beperkingen, eerlijk genoteerd: de NIC, de pomp en de NAT-tabel
//! zelf zitten niet in het pad (Go miste de NIC-demux ook), en de dial naar
//! 10.100.0.2 loopt in de stack via de ARP van de LAN-gateway (een adres
//! buiten het subnet van de lease), net als elk antwoord van de kern aan
//! een app. Een dode router kost dan een reset, en zolang hij dood blijft
//! elke guard ([`kern::watchdog::GUARD_SECS`]) nog een: luid, en de node
//! komt vanzelf terug zodra het net er weer is.
//!
//! `hopos.wd=off` zet alles uit, op elk board dezelfde knop: voor een
//! UART-postmortem moet een bevroren node blijven staan.
//!
//! De hardware is van het board (`board::Watchdog`): de SBSA-watchdog uit
//! de GTDT op de UEFI-boards (`board_uefi::watchdog`; QEMU heeft er geen),
//! de PM-watchdog van de BCM-familie op de Pi's (`board_raspi::watchdog`,
//! direct MMIO; QEMU `raspi4b` modelleert alleen de reset en wordt dus niet
//! gewapend), de primaire van `/arm-io/wdt` op de Mac mini
//! (`board_apple::wdt`, 30 s), de DesignWare-WDT op de Radxa en de LicheeRV
//! (`driver_dwwdt`; de Radxa op TOP 15 = 89,5 s zoals Go), en op virt de
//! i6300esb op PCIe (`board_qemuvirt::wdt`, `driver_i6300esb`), die elke
//! QEMU-ring meekrijgt (O2 van 01-11: de watchdog is verplicht, ook op
//! QEMU, zodat een `fatal` daar net zo reset als op ijzer).

use crate::{BOARD, Machine};
use board::{Board, Watchdog};
use core::cell::Cell;
use core::fmt;
use core::net::Ipv4Addr;
use core::time::Duration;
use cpu::println;
use executor::Executor;
use kern::watchdog::{Event, Phase, Policy};
use sync::Local;

/// De aai-cadans: ruim onder de hardware-timeout (12 s gevraagd; op een
/// teller van 1 GHz loopt WOR vol en wordt het 8,6 s), zodat een trage
/// ronde geen reset is.
const PET_EVERY: Duration = Duration::from_secs(2);
/// Hoe oud de laatste heartbeat van Hop en de laatste geslaagde dial mogen
/// zijn voor ze als "stil" tellen. Hop slaat elke seconde en de dial gaat
/// elke ronde; vijftien seconden is een hapering, geen hang.
const STALE_NS: u64 = 15_000_000_000;
/// Het geduld van één dial (Go: `probe(3 * time.Second)`). Met
/// [`PET_EVERY`] erbij is een ronde hooguit 5 s, ruim onder de kleinste
/// hardware-timeout (8,6 s op een teller van 1 GHz).
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Een gevraagde reset (een verloren adres), van wie hem vroeg naar de
/// watchdog-taak. Beide op de executor van de OS-core.
static RESET: Local<Cell<Option<&'static str>>> = Local::new(Cell::new(None));

/// Een aai buiten de watchdog-taak om: vlak vóór de sprong van een flip en
/// meteen na een landing, zodat de teller van de vertrekkende kern (12 s op
/// de Pi) niet afloopt terwijl de nieuwe kern nog boot (30-09).
pub(crate) fn pet_now() {
    BOARD.pet_now();
}

/// Vraagt een gecontroleerde herstart via de watchdog (Go:
/// `requestNodeReset`, gehangen aan `hopnet.AddressLost`): de stack kan
/// niet van adres wisselen, dus HOP-leven = node-leven. De taak houdt zijn
/// pets in en het ijzer reset. Zonder gewapende watchdog blijft het bij de
/// melding. De DHCP-keeper in net.rs roept dit bij `HOPOS_DHCP_LOST`.
pub(crate) fn request_reset(reason: &'static str) {
    println!("node: reset requested - {reason} HOPOS_RESET_REQUEST");
    let r = RESET.get();
    if r.get().is_none() {
        r.set(Some(reason));
    }
}

/// De gewapende watchdog, van [`arm`] (in `kmain`, na `discover`) tot
/// [`start`] (de taak die aait zodra de executor draait).
pub(crate) struct Armed {
    p: Policy,
    hw: Hw,
}

/// Wapent de watchdog als eerste stap van de boot, in `kmain` meteen na
/// `discover` (docs/sequence-hopos.md §13.2): vanaf hier loopt de ene guard
/// van `kern::watchdog`, ook over de verhuizing naar de OS-core en de
/// synchrone rest van `setup` heen (de mount, de probe van de NIC). Na een
/// verhuizing wapent de OS-core hem nog eens: de teller loopt door. Die
/// moet dus binnen de timeout van de hardware vallen, want de pets komen
/// pas met de executor. Tot 01-11 wapende de taak pas in de eerste ronde
/// van de executor, en parkeerde een fout in de mount voorgoed.
///
/// `hopos.wd=off` zet hem uit (ook een die de vorige kern wapende) en geeft
/// `None`. Op een board zonder hardware weigert `arm` en zegt het beleid dat
/// de node onbewaakt is; de taak draait de canary dan toch, voor de console.
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn arm() -> Option<Armed> {
    if BOARD.boot_param("hopos.wd") == "off" {
        if BOARD.disarm() {
            println!("watchdog: disabled for a post-mortem (the previous kernel armed it)");
        }
        println!(
            "watchdog: not armed (hopos.wd=off) - node liveness is UNGUARDED, a frozen node stays up for a post-mortem HOPOS_WD_OFF"
        );
        return None;
    }
    let mut hw = Hw {
        armed: None,
        why: "not armed",
    };
    let (p, e) = Policy::start(&mut hw, cpu::idle::counter(), cpu::idle::freq());
    say(&e, &hw);
    Some(Armed { p, hw })
}

/// Spawnt de watchdog-taak op wat [`arm`] wapende.
///
/// `hop`: woont Hop op deze node, gestaged of meegekomen over een flip.
/// `setup` beslist dat één keer (main.rs, ook voor het token); tot 24-10
/// keek dit zelf naar `staged_role`, en na een flip met Hop ingebakken in
/// de kaart-kern (de M4 van 03-10) werd zijn hartslag dan niet bewaakt (F6).
#[inline(never)] // eigen frame, niet in dat van `setup` (main.rs)
pub(crate) fn start(exec: &'static Executor, armed: Option<Armed>, hop: bool) {
    let Some(a) = armed else {
        return;
    };
    if exec.spawn(run(exec, a, hop)).is_err() {
        cpu::fatal!("watchdog: task not spawned, nobody would pet");
    }
}

/// De heartbeat van Hop: de laatste waarde en wanneer hij veranderde.
struct Beat {
    last: u64,
    at: u64,
}

impl Beat {
    /// Leest de heartbeat van Hop; `true` als hij binnen [`STALE_NS`]
    /// bewoog.
    ///
    /// Zonder page (een herstart van Hop: de stop haalt hem uit de tabel,
    /// de nieuwe plaatsing zet er een verse in) of met een verse page die
    /// nog 0 slaat, telt de laatste geziene beweging. Sinds 24-10 gaat de
    /// page bij elke stop uit de tabel (F1); daarvoor bleef de oude staan en
    /// was de herstart een hapering binnen [`STALE_NS`], geen gemiste aai.
    /// Vóór de eerste slag is er geen beweging, dus geen bewijs.
    fn fresh(&mut self, now: u64) -> bool {
        let v = crate::clock::ctrl_page(crate::slots::HOP_SLOT).map_or(0, |page| {
            let at = page.add(abi::hopabi::CTRL_HEARTBEAT);
            dev::pull(at, 8);
            dev::read64(at)
        });
        if v != 0 && v != self.last {
            self.last = v;
            self.at = now;
        }
        self.last != 0 && now.saturating_sub(self.at) < STALE_NS
    }
}

/// De canary: wanneer de dial voor het laatst slaagde, en of de console de
/// laatste uitkomst al meldde.
struct Canary {
    ok_at: Option<u64>,
    ok: bool,
}

impl Canary {
    /// Eén dial (zie de moduledoc voor het doel); `true` als er binnen
    /// [`STALE_NS`] een slaagde. Eén regel per wissel: de eerste geslaagde
    /// en elke wissel daarna, niet elke ronde. Een mislukking vóór het
    /// eerste succes is de boot (Hop komt nog op) en zwijgt; de
    /// wachtregel van het beleid zegt dan genoeg.
    async fn probe(&mut self, exec: &'static Executor, hop: bool) -> bool {
        let Some(own) = crate::net::uplink_ip() else {
            return false;
        };
        let target = match abi::layout::Slot::new(crate::slots::HOP_SLOT) {
            Some(s) if hop => (
                Ipv4Addr::from(abi::layout::slot_ip4(s)),
                crate::slots::HOP_PORT,
            ),
            _ => (own, abi::systemapi::PORT),
        };
        let r = crate::net::dial(exec, target.0, target.1, DIAL_TIMEOUT).await;
        let now = exec.now();
        match r {
            Ok(()) => {
                if !self.ok {
                    println!(
                        "watchdog: self-dial {}:{} connected - a new connection gets through the accept path HOPOS_WD_CANARY_OK",
                        target.0, target.1
                    );
                }
                self.ok = true;
                self.ok_at = Some(now);
            }
            Err(e) => {
                if self.ok {
                    println!(
                        "watchdog: self-dial {}:{} failed ({e}) - no pet once the last connection is 15 s old HOPOS_WD_CANARY_FAIL",
                        target.0, target.1
                    );
                }
                self.ok = false;
            }
        }
        self.ok_at
            .is_some_and(|at| now.saturating_sub(at) < STALE_NS)
    }
}

/// De watchdog-taak.
async fn run(exec: &'static Executor, a: Armed, hop: bool) {
    let Armed { mut p, mut hw } = a;
    let mut beat = Beat {
        last: 0,
        at: exec.now(),
    };
    let mut canary = Canary {
        ok_at: None,
        ok: false,
    };
    loop {
        exec.after(PET_EVERY).await;
        if p.phase() == Phase::Withheld {
            // Niets meer te doen: de hardware reset.
            continue;
        }
        if let Some(r) = RESET.get().get() {
            p.request_reset(r);
        }
        // Ook zonder hardware (Idle): dan aait niemand, maar de console
        // toont het levensteken toch.
        let dialed = canary.probe(exec, hop).await;
        let alive = dialed && (!hop || beat.fresh(exec.now()));
        if let Some(e) = p.tick(&mut hw, cpu::idle::counter(), alive) {
            say(&e, &hw);
        }
    }
}

/// Eén regel per gebeurtenis, met de marker van de Go-kern.
fn say(e: &Event, hw: &Hw) {
    match e {
        Event::Unguarded => {
            println!("watchdog: {hw} - node liveness is UNGUARDED HOPOS_WD_NONE");
        }
        Event::Armed => println!(
            "watchdog: hardware reset armed ({hw}) - boot guard: blind pets until the node proves liveness, at most {} s HOPOS_WD_ARMED",
            kern::watchdog::GUARD_SECS
        ),
        Event::GuardExpired => println!(
            "watchdog: no liveness within {} s of the boot (a self-dial that connects, and Hop beating) - withholding pets HOPOS_BOOT_GUARD_EXPIRED",
            kern::watchdog::GUARD_SECS
        ),
        Event::Live => println!(
            "watchdog: liveness proven - pets now require a fresh self-dial and Hop beating HOPOS_CANARY_LIVE"
        ),
        Event::Miss(n) => println!(
            "watchdog: liveness check failed ({n} in a row) - withholding the pet; hardware reset follows unless the node recovers HOPOS_CANARY_MISS"
        ),
        Event::ResetRequested(r) => println!(
            "watchdog: reset requested ({r}) - withholding pets, hardware reset follows HOPOS_RESET_REQUESTED"
        ),
    }
}

/// De watchdog van het board; na `arm` wat er gewapend staat, of de reden
/// van falen (op een board zonder: "no watchdog on this board").
struct Hw {
    armed: Option<<Machine as Watchdog>::Armed>,
    why: &'static str,
}

impl kern::watchdog::Hardware for Hw {
    fn arm(&mut self) -> bool {
        match BOARD.arm(Machine::WD_TIMEOUT_MS) {
            Ok(d) => {
                self.armed = Some(d);
                true
            }
            Err(why) => {
                self.why = why;
                false
            }
        }
    }
    fn pet(&mut self) {
        BOARD.pet();
    }
}

impl fmt::Display for Hw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.armed {
            Some(d) => write!(f, "{d}"),
            None => f.write_str(self.why),
        }
    }
}
