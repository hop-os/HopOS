//! NAT tussen de externe NIC en het interne net: twee richtingen, geen
//! tunnel (Go: `nat.go`, `handoff.go`).
//!
//! - Poort-publicatie (DNAT): een vaste bestemming node-IP:poort naar
//!   slot-IP:poort. Stateloos: per pakket alleen koppen herschrijven,
//!   checksums incrementeel (RFC 1624). [`Nat::inbound`] (extern naar slot)
//!   en `Nat::slot_reply` (slot-antwoord naar extern; Go: `natFromSlot`).
//! - Uitgaand (masquerade/PAT): een app belt naar buiten; HOP herschrijft
//!   bron slot-IP:poort naar node-IP:node-poort en houdt een kleine
//!   conntrack bij zodat het antwoord terugvindt. TCP én UDP (DNS, QUIC).
//!   Nooit TCP-terminatie op core 0: HOP herschrijft alleen koppen.
//! - Hairpin (Dereks model 27-07: poorten publiceren áltijd naar buiten, DNS
//!   kiest de host, en is dat je eigen node dan is de switch de sluiproute):
//!   een app die het node-IP belt op een gepubliceerde poort wordt intern
//!   omgelegd, DNAT en masquerade in één, en ring-in bezorgd; er gaat geen
//!   byte de NIC uit.
//!
//! De L2-next-hop komt uit de neighbour-tabel van de node-stack
//! ([`Neighbors`], leannet), zoals Linux één neighbour-tabel heeft waar ook
//! de forwarding uit put. De NAT houdt geen eigen ARP-cache: de bugjachten
//! van 20-08 (de Brother) en 30-09 (de vergiftigde gateway) zaten allebei in
//! die tweede cache.
//!
//! Alle NAT-staat is van de switch-actor en wordt alleen vanuit zijn lus
//! aangeraakt: er is geen moment waarop de tabel half-gemuteerd is, en een
//! snapshot voor de kern-flip is een gewone lees-ronde.

use crate::flows::{FKey, Flow, FlowTable, Id, MAX_FLOWS_PER_SLOT, RKey};
use crate::plan::{HOST_MAC, port_ip4, port_mac};
use crate::wire::{
    ETH_LEN, PROTO_TCP, PROTO_UDP, TCP_ACK, TCP_FIN, TCP_RST, TCP_SYN, be16, be32, byte,
    fix_csum32, ipv4_l4, mac_at, put_mac, put32, rewrite_l4,
};
use crate::{Error, Stats};
use bounded::BoundedVec;
use core::fmt;
use core::sync::atomic::Ordering::Relaxed;

const SEC: u64 = 1_000_000_000;

/// Het masquerade-poortbereik (PAT). Bewust disjunct van het efemere bereik
/// van HOP's eigen stack (die deelt sequentieel uit in [49152, 65535]), dus
/// de masq stopt op 49152: anders kan een inbound antwoord op HOP's eigen
/// DNS/S3-poort per ongeluk een masquerade-flow naar dezelfde peer matchen.
/// 29k poorten blijft ruim boven het conntrack-plafond.
pub const MASQ_BASE: u16 = 20000;
/// Het einde (exclusief) van het masquerade-bereik.
pub const MASQ_END: u16 = 49152;

/// Idle-timeouts. Keepalives van een langlopende tunnel (~30-90 s) blijven
/// ruim binnen de TCP-timeout.
const TCP_IDLE: u64 = 300 * SEC;
const UDP_IDLE: u64 = 60 * SEC;
/// Na een FIN in beide richtingen is alleen de afsluitende ACK/retransmit
/// nog onderweg: een conservatieve minuut. Een eenzijdige FIN kan een
/// legitieme half-close zijn en houdt daarom gewoon de TCP-timeout.
const TCP_CLOSING_IDLE: u64 = 60 * SEC;

/// Echte wall-clock-expiry zonder scan per pakket. De kortste TTL is een
/// minuut; twee vegen per TTL houden het terugwinnen tijdig zonder core 0
/// elke seconde door 4096 entries te laten lopen.
pub const FLOW_SWEEP_EVERY: u64 = 30 * SEC;
const FLOW_LOG_EVERY: u64 = 30 * SEC;

/// Het plafond van de publicatietabel.
pub const MAX_PUBS: usize = 512;

/// Een transportprotocol dat gepubliceerd kan worden. De Go-API nam een
/// string en weigerde "icmp" bij runtime; hier kan het niet eens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

impl Proto {
    const fn num(self) -> u8 {
        match self {
            Self::Tcp => PROTO_TCP,
            Self::Udp => PROTO_UDP,
        }
    }
}

/// Het externe adres van de node: IP, prefix, de MAC van de NIC en de
/// gateway uit de lease (0 = geen: dan is alleen het subnet bereikbaar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uplink {
    /// Het node-IP.
    pub ip: u32,
    /// Het subnetmasker, afgeleid uit de prefix.
    pub mask: u32,
    /// De MAC van de uplink-NIC.
    pub mac: [u8; 6],
    /// De gateway (de next-hop voor alles buiten het subnet), 0 = geen.
    pub gateway: u32,
}

impl Uplink {
    /// Het uplink-adres uit IP, prefixlengte, MAC en gateway.
    pub fn new(ip: u32, prefix: u32, mac: [u8; 6], gateway: u32) -> Result<Self, Error> {
        if prefix > 32 {
            return Err(Error::Prefix(prefix));
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Self {
            ip,
            mask,
            mac,
            gateway,
        })
    }
}

/// Eén gepubliceerde poort. De conventie is dat de app hetzelfde
/// poortnummer bindt als de node-poort, maar de vertaling kan het aan als
/// ze verschillen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pub {
    proto: u8,
    node_port: u16,
    slot: u16,
    slot_port: u16,
}

/// De neighbour-tabel van de node-stack, waar de NAT zijn next-hops uit
/// haalt (Linux: `neigh_resolve_output` uit dezelfde tabel als de eigen
/// verbindingen). Tijd is de klok van de switch.
#[derive(Clone, Copy)]
pub struct Neighbors {
    /// De MAC van de next-hop naar een bestemming: de gateway buiten het
    /// subnet, anders de bestemming zelf. `None` = onbekend, en dan loopt
    /// er een vraag.
    pub resolve: fn(dst: u32, now: u64) -> Option<[u8; 6]>,
    /// Twijfel aan de bekende next-hop van een bestemming: de stack vraagt
    /// hem opnieuw, hoogstens één keer per seconde, en houdt zijn MAC
    /// intussen.
    pub probe: fn(dst: u32, now: u64),
    /// Een frame aan ons van `src` met bron-MAC `mac` hoorde bij een flow of
    /// een publicatie: de hint die de next-hop vers houdt (Linux:
    /// `neigh_confirm`).
    pub confirm: fn(src: u32, mac: [u8; 6], now: u64),
}

/// Wat de NAT van zijn omgeving nodig heeft: bezorgen in een slot-ring, een
/// frame de uplink op, de neighbour-tabel, de tellers, en een logregel.
pub(crate) trait NatIo {
    /// Legt een (al herschreven) frame in de RX-ring van slot `slot`. Niet
    /// aangesloten of vol = drop, zoals echt Ethernet.
    fn deliver(&mut self, slot: usize, f: &[u8]);
    /// Zet een frame op de uplink.
    fn uplink_tx(&mut self, f: &[u8]);
    /// [`Neighbors::resolve`].
    fn neighbor(&mut self, dst: u32, now: u64) -> Option<[u8; 6]>;
    /// [`Neighbors::probe`].
    fn probe(&mut self, dst: u32, now: u64);
    /// [`Neighbors::confirm`].
    fn confirm(&mut self, src: u32, mac: [u8; 6], now: u64);
    /// De gedeelde tellers.
    fn stats(&self) -> &Stats;
    /// Eén logregel.
    fn log(&self, args: fmt::Arguments<'_>);
}

/// De NAT-staat: publicaties en conntrack.
pub struct Nat {
    pubs: BoundedVec<Pub, MAX_PUBS>,
    uplink: Option<Uplink>,
    flows: FlowTable,
    masq_next: u16,
    next_sweep: u64,
    next_full_log: u64,
    /// Eén grens voor de regel van een vol slot, voor alle slots samen: een
    /// dader die de zijne vult, gijzelt zo de console niet.
    next_slot_full_log: u64,
}

/// Waar de masquerade begint: een plek in het bereik die per boot verschilt
/// (de klok bij de start), zoals Linux zijn bronpoorten spreidt (RFC 6056).
/// Een kern na een flip geeft zo niet meteen de node-poorten van de vorige
/// opnieuw uit aan een peer die het oude tupel nog open heeft: de slirp van
/// QEMU antwoordde dan een nieuwe SYN met een RST (01-11).
#[must_use]
pub fn masq_start(seed: u64) -> u16 {
    let span = u64::from(MASQ_END - MASQ_BASE);
    MASQ_BASE + u16::try_from(seed % span).unwrap_or(0)
}

impl Nat {
    /// Een lege NAT die zijn masquerade-poorten vanaf `first` uitdeelt;
    /// alloceert zijn tabellen één keer (boot).
    pub fn new(first: u16) -> Result<Self, Error> {
        Ok(Self {
            pubs: BoundedVec::new(),
            uplink: None,
            flows: FlowTable::new()?,
            masq_next: first,
            next_sweep: 0,
            next_full_log: 0,
            next_slot_full_log: 0,
        })
    }

    /// Registreert het externe adres (Go: `WrapUplink`). Zonder uplink claimt
    /// de NAT niets.
    pub fn set_uplink(&mut self, u: Uplink) {
        self.uplink = Some(u);
    }

    /// Het externe adres, als het er is.
    #[must_use]
    pub fn uplink(&self) -> Option<Uplink> {
        self.uplink
    }

    fn on_subnet(&self, ip: u32) -> bool {
        self.uplink.is_some_and(|u| ip & u.mask == u.ip & u.mask)
    }

    // --- Publicaties ------------------------------------------------------

    /// Routeert node-IP:`node_port` naar `slot`:`slot_port`. Fout bij een al
    /// gepubliceerde poort. De publicatie leeft tot [`unpublish_slot`].
    ///
    /// [`unpublish_slot`]: Self::unpublish_slot
    pub fn publish(
        &mut self,
        proto: Proto,
        node_port: u16,
        slot: usize,
        slot_port: u16,
    ) -> Result<(), Error> {
        let Some(s) = slot_word(slot) else {
            return Err(Error::SlotRange(slot));
        };
        if node_port == 0 || slot_port == 0 {
            return Err(Error::PortZero);
        }
        if let Some(e) = self.pub_by_node_port(proto.num(), node_port) {
            return Err(Error::AlreadyPublished {
                port: node_port,
                slot: usize::from(e.slot),
            });
        }
        self.pubs
            .push(Pub {
                proto: proto.num(),
                node_port,
                slot: s,
                slot_port,
            })
            .map_err(|_| Error::Full("publications", MAX_PUBS))
    }

    fn pub_by_node_port(&self, proto: u8, node_port: u16) -> Option<Pub> {
        self.pubs
            .iter()
            .find(|p| p.proto == proto && p.node_port == node_port)
            .copied()
    }

    /// Trekt alle publicaties van slot `i` in en ruimt zijn flows op. Een
    /// hairpin-flow staat op naam van de CLIENT, maar hoort net zo hard bij
    /// de dienst in `dst_ip`: stoppen van óf client óf dienst trekt de
    /// mapping in, anders erft een nieuwe huurder van het dienstslot een
    /// verouderde omgekeerde route.
    pub fn unpublish_slot(&mut self, i: usize) {
        self.pubs.retain(|p| usize::from(p.slot) != i);
        let target = if slot_word(i).is_some() {
            port_ip4(i)
        } else {
            0
        };
        for id in 0..crate::flows::MAX_FLOWS as Id {
            let hit = self
                .flows
                .get(id)
                .is_some_and(|f| usize::from(f.slot) == i || (target != 0 && f.dst_ip == target));
            if hit {
                self.flows.remove(id);
            }
        }
    }

    // --- De vier herschrijfpaden ------------------------------------------

    /// Een frame van de externe NIC; `true` = geclaimd (Go: `natInbound`).
    /// Probeert een masquerade-antwoord en anders DNAT. Een geclaimd frame
    /// is de hint voor de neighbour-tabel; wat niet geclaimd wordt, ziet de
    /// node-stack zelf.
    pub(crate) fn inbound(&mut self, io: &mut impl NatIo, f: &mut [u8], now: u64) -> bool {
        let Some(ip) = ipv4_l4(f) else { return false };
        let Some(u) = self.uplink else { return false };
        if be32(f, ETH_LEN + 16) != u.ip {
            return false; // niet van ons: de node-stack mag hem hebben
        }
        // Vóór het herschrijven: de bezorging zet de MAC's van het slot.
        let (src_ip, src_mac) = (be32(f, ETH_LEN + 12), mac_at(f, 6));
        let claimed = if self.reply_in(io, f, ip.l4, ip.proto, u, now) {
            io.stats().nat_reply_in.fetch_add(1, Relaxed);
            true
        } else {
            self.dnat_in(io, f, ip.l4, ip.proto, u)
        };
        if !claimed {
            io.stats().nat_in_unmatched.fetch_add(1, Relaxed);
            return false;
        }
        io.confirm(src_ip, src_mac, now);
        true
    }

    /// De gedeelde staart van elk pad richting een app: bestemming
    /// herschrijven naar het slot, MAC's zetten, bezorgen.
    #[expect(clippy::too_many_arguments, reason = "Go's dnatToSlotLocked, plat")]
    fn dnat_to_slot(
        io: &mut impl NatIo,
        slot: usize,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        old_ip: u32,
        new_ip: u32,
        old_port: u16,
        new_port: u16,
    ) {
        put32(f, ETH_LEN + 16, new_ip);
        fix_csum32(f, ETH_LEN + 10, old_ip, new_ip);
        rewrite_l4(f, l4, proto, 2, old_ip, new_ip, old_port, new_port);
        put_mac(f, 0, &port_mac(slot));
        put_mac(f, 6, &HOST_MAC);
        io.deliver(slot, f);
    }

    /// Herschrijft de bron naar het node-IP (SNAT).
    fn snat_src(
        u: Uplink,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        old_ip: u32,
        old_port: u16,
        new_port: u16,
    ) {
        put32(f, ETH_LEN + 12, u.ip);
        fix_csum32(f, ETH_LEN + 10, old_ip, u.ip);
        rewrite_l4(f, l4, proto, 0, old_ip, u.ip, old_port, new_port);
    }

    /// Zet de MAC's en stuurt het frame de externe NIC uit.
    fn tx_uplink(io: &mut impl NatIo, u: Uplink, f: &mut [u8], next_hop: [u8; 6]) {
        put_mac(f, 0, &next_hop);
        put_mac(f, 6, &u.mac);
        io.uplink_tx(f);
    }

    fn reply_in(
        &mut self,
        io: &mut impl NatIo,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        u: Uplink,
        now: u64,
    ) -> bool {
        let peer_ip = be32(f, ETH_LEN + 12);
        let peer_port = be16(f, l4);
        let node_port = be16(f, l4 + 2);
        let Some(id) = self
            .flows
            .by_rev(&RKey(proto, node_port, peer_ip, peer_port))
        else {
            return false;
        };
        if self.expired(id, now) {
            self.flows.remove(id);
            return false; // eventueel dezelfde node-poort als publicatie: DNAT mag
        }
        self.note_tcp_flags(id, byte(f, l4 + 13), true);
        let Some(fl) = self.touch(id, now) else {
            return false;
        };
        Self::dnat_to_slot(
            io,
            usize::from(fl.slot),
            f,
            l4,
            proto,
            u.ip,
            fl.slot_ip,
            node_port,
            fl.slot_port,
        );
        true
    }

    fn dnat_in(
        &mut self,
        io: &mut impl NatIo,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        u: Uplink,
    ) -> bool {
        let dport = be16(f, l4 + 2);
        let Some(m) = self.pub_by_node_port(proto, dport) else {
            return false;
        };
        let slot = usize::from(m.slot);
        Self::dnat_to_slot(
            io,
            slot,
            f,
            l4,
            proto,
            u.ip,
            port_ip4(slot),
            dport,
            m.slot_port,
        );
        true
    }

    /// Een frame van slot `src` richting de gateway; `true` = geclaimd (Go:
    /// `natFromSlot`). Het antwoordpad van een gepubliceerde poort.
    pub(crate) fn slot_reply(
        &mut self,
        io: &mut impl NatIo,
        src: usize,
        f: &mut [u8],
        now: u64,
    ) -> bool {
        let Some(ip) = ipv4_l4(f) else { return false };
        let Some(u) = self.uplink else { return false };
        let sport = be16(f, ip.l4);
        let Some(m) = self
            .pubs
            .iter()
            .find(|p| p.proto == ip.proto && usize::from(p.slot) == src && p.slot_port == sport)
            .copied()
        else {
            return false;
        };
        // Een échte reply begint nooit met een kale SYN: dit pad is stateloos
        // en matcht alleen (proto, slot, poort), dus een app die zélf naar
        // buiten belt en toevallig zijn eigen listen-poort als bronpoort
        // krijgt, werd hier als "antwoord" geclaimd en stil ge-SNAT. UDP kent
        // geen handshake en blijft op de poort-match staan.
        let flags = byte(f, ip.l4 + 13);
        if ip.proto == PROTO_TCP && flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
            return false;
        }
        let dst = be32(f, ETH_LEN + 16);
        // Draagt de reply het node-IP als bestemming, dan was de client een
        // buurslot (hairpin): terugvertalen via de conntrack en ring-in.
        if dst == u.ip {
            return self.hairpin_back(io, f, ip.l4, ip.proto, m, u, now);
        }
        let Some(next_hop) = io.neighbor(dst, now) else {
            return true; // next-hop onbekend: drop, de vraag loopt
        };
        let src_ip = be32(f, ETH_LEN + 12);
        Self::snat_src(u, f, ip.l4, ip.proto, src_ip, sport, m.node_port);
        Self::tx_uplink(io, u, f, next_hop);
        true
    }

    /// Een app belt naar buiten (Go: `natOutbound`). Masquerade, dan de
    /// externe NIC uit. `true` = afgehandeld, ook als het gedropt is.
    pub(crate) fn outbound(
        &mut self,
        io: &mut impl NatIo,
        src: usize,
        f: &mut [u8],
        now: u64,
    ) -> bool {
        let Some(ip) = ipv4_l4(f) else { return false };
        let Some(u) = self.uplink else { return false };
        let dst_ip = be32(f, ETH_LEN + 16);
        let slot_ip = port_ip4(src);
        let sport = be16(f, ip.l4);
        let dport = be16(f, ip.l4 + 2);
        let flags = byte(f, ip.l4 + 13);
        let proto = ip.proto;

        // Het node-IP zelf: hairpin, nooit de NIC uit en nooit de
        // neighbour-tabel in.
        if dst_ip == u.ip {
            return self.hairpin_out(io, src, f, ip.l4, proto, slot_ip, sport, dport, u, now);
        }

        // Onbekend: de node-stack vraagt het net nu, de next-hop zelf (een
        // host in het eigen net, Altra 14-07) of de gateway (Pi 5, 30-09).
        let hop = io.neighbor(dst_ip, now);
        let known = hop.is_some();
        // Een host in het eigen net gaat intussen via de gateway mee
        // (Brother-jacht 20-08): er bestaan hosts die broadcast-ARP niet
        // beantwoorden (wifi-powersave, mesh). Relayt de router, dan
        // antwoordt de host óns rechtstreeks en is hij alsnog geleerd.
        let hop = match hop {
            None if self.on_subnet(dst_ip) && u.gateway != 0 => io.neighbor(u.gateway, now),
            hop => hop,
        };
        let Some(next_hop) = hop else {
            io.stats().nat_no_route.fetch_add(1, Relaxed);
            return true; // geen enkel spoor: drop, retransmit volgt
        };
        let Some((id, created)) =
            self.flow_for_packet(io, proto, src, slot_ip, sport, dst_ip, dport, flags, now)
        else {
            io.stats().nat_flow_full.fetch_add(1, Relaxed);
            return true; // pool vol: drop
        };
        // Een tweede kale SYN op dezelfde levende flow is de retransmit van
        // de TCP-stack: de eerste SYN was de gratis unicast-probe. Twijfel
        // dan aan de next-hop (31-08): de stack vraagt hem opnieuw, hoogstens
        // één keer per seconde, en houdt zijn MAC tot het antwoord. Een buur
        // of gateway die stil van MAC wisselde, is zo binnen een seconde
        // terug.
        if known && !created && proto == PROTO_TCP && flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
            io.probe(dst_ip, now);
        }
        let reap = self.note_tcp_flags(id, flags, false);
        let Some(np) = self.flows.get(id).map(|f| f.node_port) else {
            return true;
        };
        Self::snat_src(u, f, ip.l4, proto, slot_ip, sport, np);
        Self::tx_uplink(io, u, f, next_hop);
        if reap {
            self.flows.remove(id);
        }
        true
    }

    /// Een app belt het node-IP: de gepubliceerde poort van een buurslot.
    /// DNAT en masquerade in één beweging, dan ring-in bij de dienst. Niets
    /// gepubliceerd op die poort = drop, zoals een dichte poort.
    #[expect(clippy::too_many_arguments, reason = "Go's hairpinOutLocked, plat")]
    fn hairpin_out(
        &mut self,
        io: &mut impl NatIo,
        src: usize,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        slot_ip: u32,
        sport: u16,
        dport: u16,
        u: Uplink,
        now: u64,
    ) -> bool {
        let Some(m) = self.pub_by_node_port(proto, dport) else {
            return true;
        };
        let srv = usize::from(m.slot);
        let srv_ip = port_ip4(srv);
        let flags = byte(f, l4 + 13);
        let Some((id, _)) = self.flow_for_packet(
            io,
            proto,
            src,
            slot_ip,
            sport,
            srv_ip,
            m.slot_port,
            flags,
            now,
        ) else {
            io.stats().nat_flow_full.fetch_add(1, Relaxed);
            return true;
        };
        let reap = self.note_tcp_flags(id, flags, false);
        let Some(np) = self.flows.get(id).map(|f| f.node_port) else {
            return true;
        };
        Self::snat_src(u, f, l4, proto, slot_ip, sport, np);
        Self::dnat_to_slot(io, srv, f, l4, proto, u.ip, srv_ip, dport, m.slot_port);
        if reap {
            self.flows.remove(id);
        }
        true
    }

    /// De reply van de dienst op een hairpin-verbinding: de conntrack wijst
    /// de beller aan; beide kanten terugschrijven en ring-in bezorgen. Geen
    /// flow = drop.
    #[expect(clippy::too_many_arguments, reason = "Go's hairpinBackLocked, plat")]
    fn hairpin_back(
        &mut self,
        io: &mut impl NatIo,
        f: &mut [u8],
        l4: usize,
        proto: u8,
        m: Pub,
        u: Uplink,
        now: u64,
    ) -> bool {
        let srv_ip = port_ip4(usize::from(m.slot));
        let np = be16(f, l4 + 2);
        let Some(id) = self.flows.by_rev(&RKey(proto, np, srv_ip, m.slot_port)) else {
            return true;
        };
        if self.expired(id, now) {
            self.flows.remove(id);
            return true;
        }
        self.note_tcp_flags(id, byte(f, l4 + 13), true);
        let Some(fl) = self.touch(id, now) else {
            return true;
        };
        // Eerst de src-helft, dan de dst-helft (die ook bezorgt): disjuncte
        // velden en incrementele checksums, dus de volgorde is vrij.
        Self::snat_src(u, f, l4, proto, srv_ip, m.slot_port, m.node_port);
        Self::dnat_to_slot(
            io,
            usize::from(fl.slot),
            f,
            l4,
            proto,
            u.ip,
            fl.slot_ip,
            np,
            fl.slot_port,
        );
        true
    }

    // --- Conntrack --------------------------------------------------------

    /// Maakt voor gewone uitgaande pakketten zo nodig een mapping. Een RST
    /// mag alleen een bestaande sluiten: zonder mapping ontbreekt het
    /// node-poortnummer dat de peer kent. Geeft `(flow, nieuw aangemaakt)`.
    #[expect(clippy::too_many_arguments, reason = "de 5-tupel plus context")]
    fn flow_for_packet(
        &mut self,
        io: &mut impl NatIo,
        proto: u8,
        slot: usize,
        slot_ip: u32,
        slot_port: u16,
        dst_ip: u32,
        dst_port: u16,
        flags: u8,
        now: u64,
    ) -> Option<(Id, bool)> {
        let k = FKey(proto, slot_ip, dst_ip, slot_port, dst_port);
        if proto == PROTO_TCP && flags & TCP_RST != 0 {
            return self.lookup_flow(&k, now).map(|id| (id, false));
        }
        self.flow_for(io, proto, slot, slot_ip, slot_port, dst_ip, dst_port, now)
    }

    /// Vindt of maakt de conntrack-entry van een uitgaande flow; `None` als
    /// de pool vol is. Een volle pool veegt hoogstens eens per
    /// `FLOW_SWEEP_EVERY` en logt hoogstens eens per `FLOW_LOG_EVERY`, zodat
    /// een dader niet via het weigerpad core 0 kan gijzelen.
    #[expect(clippy::too_many_arguments, reason = "de 5-tupel plus context")]
    pub(crate) fn flow_for(
        &mut self,
        io: &mut impl NatIo,
        proto: u8,
        slot: usize,
        slot_ip: u32,
        slot_port: u16,
        dst_ip: u32,
        dst_port: u16,
        now: u64,
    ) -> Option<(Id, bool)> {
        let k = FKey(proto, slot_ip, dst_ip, slot_port, dst_port);
        if let Some(id) = self.lookup_flow(&k, now) {
            return Some((id, false));
        }
        let s = slot_word(slot)?;
        if self.flows.is_full() || self.flows.count(s) >= MAX_FLOWS_PER_SLOT {
            self.maybe_sweep(now);
            // Als de veeg niets opleverde: recycle de oudste al gesloten flow
            // van dit slot. Een flow met beide FIN's houdt zijn plek alleen
            // nog vast voor late retransmits; een nieuwe verbinding heeft meer
            // recht op de tabel dan een dode. GEMETEN 21-09 op de Pi 5 (L83
            // p51): 200 korte verbindingen per storm vullen de 512 van een
            // slot sneller dan die minuut ze vrijgeeft, en dan dropte dit pad
            // de SYN; in zes van twaalf rondes, met `nat_flowfull` als bewijs.
            if (self.flows.is_full() || self.flows.count(s) >= MAX_FLOWS_PER_SLOT)
                && let Some(old) = self.oldest_closed(s)
            {
                self.flows.remove(old);
            }
        }
        if self.flows.is_full() {
            if now >= self.next_full_log {
                io.log(format_args!(
                    "HOPOS_MASQ_FULL: conntrack full ({}), new outbound flows dropped",
                    crate::flows::MAX_FLOWS
                ));
                self.next_full_log = now.saturating_add(FLOW_LOG_EVERY);
            }
            return None;
        }
        let n = self.flows.count(s);
        if n >= MAX_FLOWS_PER_SLOT {
            if now >= self.next_slot_full_log {
                io.log(format_args!(
                    "HOPOS_MASQ_SLOT_FULL: slot {slot} has {n} flows (max {MAX_FLOWS_PER_SLOT}), new outbound flow dropped"
                ));
                self.next_slot_full_log = now.saturating_add(FLOW_LOG_EVERY);
            }
            return None;
        }
        let np = self.alloc_port(proto, dst_ip, dst_port)?;
        let id = self.flows.insert(Flow {
            proto,
            slot: s,
            fin_fwd: false,
            fin_rev: false,
            slot_ip,
            dst_ip,
            slot_port,
            dst_port,
            node_port: np,
            seen: now,
        })?;
        Some((id, true))
    }

    /// Vindt een verse mapping, werkt zijn idle-klok bij en ruimt een
    /// verlopen mapping op.
    fn lookup_flow(&mut self, k: &FKey, now: u64) -> Option<Id> {
        let id = self.flows.by_fwd(k)?;
        if self.expired(id, now) {
            self.flows.remove(id);
            return None;
        }
        self.touch(id, now)?;
        Some(id)
    }

    fn touch(&mut self, id: Id, now: u64) -> Option<Flow> {
        let fl = self.flows.get_mut(id)?;
        fl.seen = now;
        Some(*fl)
    }

    /// De oudste al gesloten flow van `slot`, uit een volle doorloop van de
    /// slab: alleen bij een vol slot, één keer per nieuwe flow, en dan is
    /// zeker weten goedkoper dan een SYN die 1 s op zijn RTO wacht. GEMETEN
    /// 03-10 op de Pi 4: een steekproef van 64 vond in een slab vol flows
    /// van Hop af en toe geen gesloten flow van het slot terwijl het er
    /// honderden had (hairpin-storm, de derde run binnen een minuut: 180
    /// conn/s, p99 1002 ms, HOPOS_MASQ_SLOT_FULL).
    fn oldest_closed(&self, slot: u16) -> Option<Id> {
        self.flows
            .ids()
            .filter_map(|id| self.flows.get(id).map(|fl| (id, fl)))
            .filter(|(_, fl)| fl.slot == slot && fl.fin_fwd && fl.fin_rev)
            .min_by_key(|(_, fl)| fl.seen)
            .map(|(id, _)| id)
    }

    /// Kiest een vrije node-poort: rollend door het masquerade-bereik, niet
    /// botsend met een lopende flow naar dezelfde peer, noch met een
    /// gepubliceerde poort (die is voor DNAT).
    fn alloc_port(&mut self, proto: u8, dst_ip: u32, dst_port: u16) -> Option<u16> {
        let flows = &self.flows;
        let pubs = &self.pubs;
        alloc_port_by(&mut self.masq_next, |p| {
            flows.by_rev(&RKey(proto, p, dst_ip, dst_port)).is_some()
                || pubs.iter().any(|q| q.proto == proto && q.node_port == p)
        })
    }

    /// Verwerkt alleen de terminale hints waarvoor geen volledige
    /// TCP-state-machine nodig is. Een RST van de slotkant mag na het
    /// doorsturen meteen weg; een inbound RST niet, want zonder
    /// sequence-tracking weet de NAT niet of de ontvanger hem accepteert.
    /// FIN wordt per richting onthouden. Geeft "opruimen na doorsturen".
    fn note_tcp_flags(&mut self, id: Id, flags: u8, reverse: bool) -> bool {
        let Some(fl) = self.flows.get_mut(id) else {
            return false;
        };
        if fl.proto != PROTO_TCP {
            return false;
        }
        if flags & TCP_RST != 0 {
            if reverse {
                // Een inbound RST: de flow blijft voor late pakketten
                // (zonder sequence-tracking weet de NAT niet of de
                // ontvanger hem accepteert), maar telt als gesloten in
                // beide richtingen: de sluit-TTL geldt en de recycler mag
                // hem bij een volle tabel nemen. Zonder dit bleef een
                // RST-gesloten flow TCP_IDLE (300 s) staan en liep de
                // tabel van 512 per slot in een storm vol (Pi 5 en Pi 4,
                // 30-09: HOPOS_MASQ_SLOT_FULL, de SYN pas na 1 s).
                fl.fin_fwd = true;
                fl.fin_rev = true;
                return false;
            }
            return true;
        }
        if flags & TCP_FIN != 0 {
            if reverse {
                fl.fin_rev = true;
            } else {
                fl.fin_fwd = true;
            }
        }
        false
    }

    fn expired(&self, id: Id, now: u64) -> bool {
        self.flows.get(id).is_some_and(|fl| flow_expired(fl, now))
    }

    /// Verwijdert alle verlopen flows.
    pub(crate) fn sweep_expired(&mut self, now: u64) {
        for id in 0..crate::flows::MAX_FLOWS as Id {
            if self.expired(id, now) {
                self.flows.remove(id);
            }
        }
    }

    /// De veeg van de flow-expiry-tik: maakt de idle-timeouts echt, ook
    /// onder de quota en zonder een nieuwe allocatie.
    pub fn sweep(&mut self, now: u64) {
        self.sweep_expired(now);
        self.next_sweep = now.saturating_add(FLOW_SWEEP_EVERY);
    }

    /// Een drukgedreven veeg, begrensd in tempo.
    fn maybe_sweep(&mut self, now: u64) {
        if self.next_sweep != 0 && now < self.next_sweep {
            return;
        }
        self.sweep(now);
    }
}

/// Het slotnummer als woord, als het een slot is (1..=`SLOT_MAX`, de
/// zestien bits van het slot-LAN).
fn slot_word(slot: usize) -> Option<u16> {
    abi::layout::Slot::new(slot).and_then(|s| u16::try_from(s.get()).ok())
}

fn flow_idle(fl: &Flow) -> u64 {
    if fl.proto != PROTO_TCP {
        UDP_IDLE
    } else if fl.fin_fwd && fl.fin_rev {
        TCP_CLOSING_IDLE
    } else {
        TCP_IDLE
    }
}

fn flow_expired(fl: &Flow, now: u64) -> bool {
    now.saturating_sub(fl.seen) > flow_idle(fl)
}

/// De poortkeuze los van de tabellen, zodat de uitputting te toetsen is
/// zonder 29k flows (de conntrack draagt er maar 4096).
fn alloc_port_by(next: &mut u16, busy: impl Fn(u16) -> bool) -> Option<u16> {
    for _ in MASQ_BASE..MASQ_END {
        let p = *next;
        *next = if p + 1 >= MASQ_END { MASQ_BASE } else { p + 1 };
        if !busy(p) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests;
