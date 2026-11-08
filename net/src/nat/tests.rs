//! De NAT-tests, naam voor naam uit `nat_test.go`, `adoption_test.go` en
//! `hairpin_test.go`. De switch-lus draait hier niet; bezorgde frames landen
//! in een rij per slot van de test-omgeving (Go: `testSlotRing`), frames voor
//! de uplink in `sent` (Go: `fakeNIC`). De next-hops komen uit een echte
//! node-stack (leannet), zoals in de kern: zijn ARP-vragen gaan meteen de
//! uplink op, alsof de host-taak tussendoor draaide, en wat de NAT niet
//! claimt, krijgt hij via [`feed`].

use super::*;
use crate::plan::{HOST_MAC, host_ip4};
use crate::wire::testutil::*;
use crate::wire::{ET_ARP, PROTO_TCP, PROTO_UDP, TCP_ACK, TCP_FIN, TCP_RST, TCP_SYN, put16};
use std::cell::RefCell;
use std::collections::VecDeque;

const NODE_IP: u32 = 0x0A00_020F; // 10.0.2.15/24
const GW_IP: u32 = 0x0A00_0202; // 10.0.2.2, de gateway uit de lease
const EXT_IP: u32 = 0x5DB8_D822; // 93.184.216.34 (off-subnet)
const LAN_IP: u32 = 0x0A00_0263; // 10.0.2.99 (on-subnet)
const GW_MAC0: [u8; 6] = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
const LAN_MAC0: [u8; 6] = [0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB];
const NIC_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const T0: u64 = 10_000 * SEC;

/// Hoe lang een opgeloste neighbor in leannet geldt.
const NEIGH_TTL: u64 = 120 * SEC;

pub(crate) struct TestIo {
    pub(crate) stats: Stats,
    pub(crate) attached: Vec<bool>,
    pub(crate) rings: Vec<VecDeque<Vec<u8>>>,
    pub(crate) sent: Vec<Vec<u8>>,
    pub(crate) logs: RefCell<Vec<String>>,
    /// De node-stack van poort 0, met de enige neighbour-tabel.
    pub(crate) stack: leannet::Stack,
}

impl TestIo {
    fn new() -> Self {
        let cfg = leannet::Config {
            ip: NODE_IP.to_be_bytes(),
            prefix: 24,
            mac: NIC_MAC,
            gw: GW_IP.to_be_bytes(),
            budget: 1 << 20,
            ..leannet::Config::default()
        };
        Self {
            stats: Stats::new(),
            attached: vec![false; 256],
            rings: vec![VecDeque::new(); 256],
            sent: Vec::new(),
            logs: RefCell::new(Vec::new()),
            stack: leannet::Stack::new(cfg, [1; 16]).unwrap(),
        }
    }
    /// De host-taak: wat de node-stack wil zenden, de uplink op.
    fn pump(&mut self, now: u64) {
        let mut buf = vec![0u8; self.stack.frame_len()];
        while let Some(n) = self.stack.poll_transmit(now, &mut buf) {
            self.sent.push(buf[..n].to_vec());
        }
    }
    /// Go: `testSlotRing`, zonder de leesfunctie.
    fn attach(&mut self, i: usize) {
        self.attached[i] = true;
    }
    fn read(&mut self, i: usize) -> Option<Vec<u8>> {
        self.rings[i].pop_front()
    }
}

impl NatIo for TestIo {
    fn deliver(&mut self, slot: usize, f: &[u8]) {
        if self.attached.get(slot).copied().unwrap_or(false) {
            self.rings[slot].push_back(f.to_vec());
        }
    }
    fn uplink_tx(&mut self, f: &[u8]) {
        self.sent.push(f.to_vec());
    }
    fn neighbor(&mut self, dst: u32, now: u64) -> Option<[u8; 6]> {
        let mac = self.stack.neighbor(dst.to_be_bytes(), now);
        self.pump(now);
        mac
    }
    fn probe(&mut self, dst: u32, now: u64) {
        self.stack.probe_neighbor(dst.to_be_bytes(), now);
        self.pump(now);
    }
    fn confirm(&mut self, src: u32, mac: [u8; 6], now: u64) {
        self.stack.confirm_neighbor(src.to_be_bytes(), mac, now);
    }
    fn stats(&self) -> &Stats {
        &self.stats
    }
    fn log(&self, args: fmt::Arguments<'_>) {
        self.logs.borrow_mut().push(args.to_string());
    }
}

/// Go: `resetNAT` plus `setUplink`.
fn setup() -> (Nat, TestIo) {
    let mut nat = Nat::new(MASQ_BASE).unwrap();
    nat.set_uplink(Uplink::new(NODE_IP, 24, NIC_MAC, GW_IP).unwrap());
    (nat, TestIo::new())
}

/// Een frame van de uplink zoals `uplink_in` het verdeelt: wat de NAT niet
/// claimt (ARP, verkeer voor de node zelf), gaat de node-stack in.
fn feed(nat: &mut Nat, io: &mut TestIo, f: &[u8], now: u64) {
    let mut f = f.to_vec();
    if !nat.inbound(io, &mut f, now) {
        io.stack.receive(&f, now).unwrap();
    }
}

/// Laat de node-stack `ip` oplossen: zijn vraag, het antwoord van `mac`.
/// Leegt `sent`.
fn leer(io: &mut TestIo, ip: u32, mac: [u8; 6], now: u64) {
    assert_eq!(io.neighbor(ip, now), None, "{ip:#x} was al bekend");
    io.stack.receive(&arp_reply(mac, ip), now).unwrap();
    assert_eq!(io.neighbor(ip, now), Some(mac));
    io.sent.clear();
}

/// De gateway bekend, zoals seconden na elke echte boot.
fn leer_gateway(io: &mut TestIo) {
    leer(io, GW_IP, GW_MAC0, T0);
}

/// Een TCP-segment van slot 1 naar `dst`:`dport`.
fn seg(dst: u32, sport: u16, dport: u16, flags: u8) -> Vec<u8> {
    let mut s = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        port_ip4(1),
        dst,
        sport,
        dport,
        &[],
    );
    set_tcp_flags(&mut s, flags);
    s
}

/// Een kale SYN van slot 1.
fn syn(dst: u32, sport: u16, dport: u16) -> Vec<u8> {
    seg(dst, sport, dport, TCP_SYN)
}

#[expect(clippy::too_many_arguments, reason = "Go's flowFor, plat")]
fn flow_for(
    nat: &mut Nat,
    io: &mut TestIo,
    proto: u8,
    slot: usize,
    sport: u16,
    dst: u32,
    dport: u16,
    now: u64,
) -> Option<Id> {
    nat.flow_for(io, proto, slot, port_ip4(slot), sport, dst, dport, now)
        .map(|(id, _)| id)
}

fn age(nat: &mut Nat, id: Id, seen: u64) {
    nat.flows.get_mut(id).unwrap().seen = seen;
}

fn l4(f: &[u8]) -> &[u8] {
    &f[ETH_LEN + 20..]
}

#[test]
fn publish_validatie() {
    let (mut nat, _) = setup();
    // "icmp" bestaat niet meer: `Proto` kent alleen tcp en udp.
    assert!(
        nat.publish(Proto::Tcp, 80, 0, 80).is_err(),
        "slot 0 geaccepteerd"
    );
    assert!(
        nat.publish(Proto::Tcp, 80, abi::layout::SLOT_MAX + 1, 80)
            .is_err(),
        "slot buiten bereik"
    );
    assert_eq!(nat.publish(Proto::Tcp, 0, 1, 80), Err(Error::PortZero));
    nat.publish(Proto::Tcp, 80, 1, 80).unwrap();
    assert_eq!(
        nat.publish(Proto::Tcp, 80, 2, 80),
        Err(Error::AlreadyPublished { port: 80, slot: 1 })
    );
    nat.publish(Proto::Udp, 80, 2, 80).unwrap();
}

/// Het volledige masquerade-pad: app belt uit, antwoord komt terug.
#[test]
fn masquerade_uit_en_terug() {
    let (mut nat, mut io) = setup();
    leer_gateway(&mut io);
    let payload = b"GET / HTTP/1.1";
    let slot_ip = port_ip4(1);
    let mut out = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        slot_ip,
        EXT_IP,
        5555,
        443,
        payload,
    );
    assert!(nat.outbound(&mut io, 1, &mut out, T0));
    assert_eq!(io.sent.len(), 1);
    let sent = io.sent[0].clone();
    assert_eq!(
        be32(&sent, ETH_LEN + 12),
        NODE_IP,
        "bron-IP niet gemasqueradeerd"
    );
    let masq = be16(l4(&sent), 0);
    assert!((MASQ_BASE..MASQ_END).contains(&masq));
    assert_eq!(sent[0..6], GW_MAC0);
    assert_eq!(sent[6..12], NIC_MAC);
    assert!(sent.ends_with(payload), "payload beschadigd");
    check_frame(&sent, "uitgaand");

    // Zelfde 5-tupel opnieuw: zelfde flow, zelfde masq-poort.
    let mut out2 = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        slot_ip,
        EXT_IP,
        5555,
        443,
        &[],
    );
    nat.outbound(&mut io, 1, &mut out2, T0);
    assert_eq!(be16(l4(&io.sent[1]), 0), masq);

    // Het antwoord: geclaimd en rechtstreeks in slot 1 bezorgd.
    io.attach(1);
    let mut reply = mk_frame(
        PROTO_TCP,
        NIC_MAC,
        GW_MAC0,
        EXT_IP,
        NODE_IP,
        443,
        masq,
        b"HTTP/1.1 200 OK",
    );
    assert!(nat.inbound(&mut io, &mut reply, T0));
    let inj = io.read(1).expect("antwoord niet bezorgd");
    assert_eq!(be32(&inj, ETH_LEN + 16), slot_ip);
    assert_eq!(be16(l4(&inj), 2), 5555);
    assert_eq!(inj[0..6], port_mac(1));
    assert_eq!(inj[6..12], HOST_MAC);
    check_frame(&inj, "antwoord");
}

/// Het DNAT-pad: gepubliceerde poort in, slot-antwoord uit (SNAT).
#[test]
fn dnat_in_en_slot_antwoord_uit() {
    let (mut nat, mut io) = setup();
    nat.publish(Proto::Tcp, 8080, 1, 9090).unwrap();
    io.attach(1);
    let mut inf = mk_frame(
        PROTO_TCP, NIC_MAC, LAN_MAC0, LAN_IP, NODE_IP, 1234, 8080, b"hallo",
    );
    assert!(nat.inbound(&mut io, &mut inf, T0));
    let inj = io.read(1).expect("DNAT-frame niet bezorgd");
    assert_eq!(be32(&inj, ETH_LEN + 16), port_ip4(1));
    assert_eq!(be16(l4(&inj), 2), 9090);
    check_frame(&inj, "DNAT-in");

    let mut los = mk_frame(
        PROTO_TCP,
        NIC_MAC,
        LAN_MAC0,
        LAN_IP,
        NODE_IP,
        1234,
        8081,
        &[],
    );
    assert!(
        !nat.inbound(&mut io, &mut los, T0),
        "niet-gepubliceerde poort geclaimd"
    );

    let mut uit = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        port_ip4(1),
        LAN_IP,
        9090,
        1234,
        b"antwoord",
    );
    assert!(nat.slot_reply(&mut io, 1, &mut uit, T0));
    assert_eq!(io.sent.len(), 1);
    let sent = &io.sent[0];
    assert_eq!(be32(sent, ETH_LEN + 12), NODE_IP);
    assert_eq!(be16(l4(sent), 0), 8080);
    assert_eq!(
        sent[0..6],
        LAN_MAC0,
        "dst-MAC hoort de geleerde neighbor te zijn"
    );
    check_frame(sent, "SNAT-uit");

    let mut vreemd = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        port_ip4(1),
        LAN_IP,
        7777,
        1234,
        &[],
    );
    assert!(
        !nat.slot_reply(&mut io, 1, &mut vreemd, T0),
        "slot-frame zonder publicatie geclaimd"
    );
}

#[test]
fn alloc_port_slaat_bezet_over() {
    let (mut nat, mut io) = setup();
    // De eerstvolgende twee kandidaten bezet: één door een flow naar dezelfde
    // peer, één door een publicatie.
    nat.flows
        .insert(Flow {
            proto: PROTO_TCP,
            slot: 2,
            fin_fwd: false,
            fin_rev: false,
            via_gw: false,
            slot_ip: port_ip4(2),
            dst_ip: EXT_IP,
            slot_port: 1,
            dst_port: 443,
            node_port: MASQ_BASE,
            seen: T0,
        })
        .unwrap();
    nat.publish(Proto::Tcp, MASQ_BASE + 1, 1, 80).unwrap();
    assert_eq!(nat.alloc_port(PROTO_TCP, EXT_IP, 443), Some(MASQ_BASE + 2));
    // Naar een andere peer mag alles, tot hij rondgaat.
    for _ in MASQ_BASE..MASQ_END {
        assert!(
            nat.alloc_port(PROTO_TCP, EXT_IP, 444).is_some(),
            "onterecht uitgeput"
        );
    }
    let _ = &mut io;
}

/// Alles bezet: geen poort. (Go vulde 29k reverse-entries; de conntrack
/// draagt er hier 4096, dus de uitputting toetst de keuze los van de tabel.)
#[test]
fn alloc_port_uitputting() {
    let mut next = MASQ_BASE;
    assert_eq!(alloc_port_by(&mut next, |_| true), None);
    assert_eq!(next, MASQ_BASE, "één volle ronde en weer terug op de basis");
}

#[test]
fn sweep_expired() {
    let (mut nat, mut io) = setup();
    let now = T0;
    for (proto, sport, leeftijd) in [
        (PROTO_TCP, 1001, TCP_IDLE + SEC), // verlopen
        (PROTO_TCP, 1002, TCP_IDLE - SEC), // vers genoeg
        (PROTO_UDP, 1003, UDP_IDLE + SEC), // verlopen (kortere timeout)
        (PROTO_UDP, 1004, TCP_IDLE - SEC), // ouder dan UDP_IDLE: verlopen
    ] {
        let id = flow_for(&mut nat, &mut io, proto, 1, sport, EXT_IP, 443, now).unwrap();
        age(&mut nat, id, now - leeftijd);
    }
    nat.sweep_expired(now);
    assert_eq!(nat.flows.len(), 1);
    assert!(
        nat.flows
            .by_fwd(&FKey(PROTO_TCP, port_ip4(1), EXT_IP, 1002, 443))
            .is_some()
    );
}

#[test]
fn sweep_na_verkeerspiek_houdt_de_overlever() {
    let (mut nat, mut io) = setup();
    let now = T0;
    let mut keep = None;
    for i in 0..128u16 {
        let id = flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1000 + i, EXT_IP, 443, now).unwrap();
        if i == 127 {
            keep = Some(id);
        } else {
            age(&mut nat, id, now - TCP_IDLE - SEC);
        }
    }
    assert_eq!(nat.flows.len(), 128);
    nat.sweep_expired(now);
    assert_eq!((nat.flows.len(), nat.flows.count(1)), (1, 1));
    let keep = *nat.flows.get(keep.unwrap()).unwrap();
    assert!(
        nat.flows.by_rev(&keep.rkey()).is_some(),
        "omgekeerde mapping van de overlever verloren"
    );
}

#[test]
fn flow_lookup_vervangt_verlopen_entry() {
    let (mut nat, mut io) = setup();
    let now = T0;
    let old = flow_for(&mut nat, &mut io, PROTO_UDP, 1, 1001, EXT_IP, 443, now).unwrap();
    let old_port = nat.flows.get(old).unwrap().node_port;
    age(&mut nat, old, now - UDP_IDLE - SEC);
    let (rep, created) = nat
        .flow_for(&mut io, PROTO_UDP, 1, port_ip4(1), 1001, EXT_IP, 443, now)
        .unwrap();
    assert!(created, "exacte lookup gaf een verlopen flow terug");
    assert_ne!(nat.flows.get(rep).unwrap().node_port, old_port);
    assert!(
        nat.flows
            .by_rev(&RKey(PROTO_UDP, old_port, EXT_IP, 443))
            .is_none()
    );
    assert_eq!((nat.flows.len(), nat.flows.count(1)), (1, 1));
}

/// Een verlopen reverse match mag de node-poort niet blijven kapen.
#[test]
fn reverse_lookup_verwijdert_expired_en_valt_terug_op_dnat() {
    let (mut nat, mut io) = setup();
    let now = T0;
    let id = flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1001, EXT_IP, 443, now).unwrap();
    age(&mut nat, id, now - TCP_IDLE - SEC);
    let np = nat.flows.get(id).unwrap().node_port;
    nat.publish(Proto::Tcp, np, 2, 8080).unwrap();
    io.attach(2);
    let mut inf = mk_frame(
        PROTO_TCP, NIC_MAC, GW_MAC0, EXT_IP, NODE_IP, 443, np, b"nieuw",
    );
    assert!(
        nat.inbound(&mut io, &mut inf, now),
        "viel niet terug op DNAT"
    );
    let got = io.read(2).expect("niet bij het gepubliceerde slot");
    assert_eq!(
        (be32(&got, ETH_LEN + 16), be16(l4(&got), 2)),
        (port_ip4(2), 8080)
    );
    check_frame(&got, "DNAT na stale reverse lookup");
    assert_eq!((nat.flows.len(), nat.flows.count(1)), (0, 0));
}

#[test]
fn tcp_fin_verkort_timeout_pas_na_beide_richtingen() {
    let (mut nat, mut io) = setup();
    let base = 1_000 * SEC;
    let id = flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1001, EXT_IP, 443, base).unwrap();
    nat.note_tcp_flags(id, TCP_FIN, false);
    nat.sweep_expired(base + TCP_CLOSING_IDLE + 1);
    assert_eq!(
        nat.flows.len(),
        1,
        "eenzijdige FIN ruimde een half-close op"
    );
    nat.note_tcp_flags(id, TCP_FIN, true);
    nat.sweep_expired(base + TCP_CLOSING_IDLE + 1);
    assert_eq!((nat.flows.len(), nat.flows.count(1)), (0, 0));
}

#[test]
fn tcp_inbound_rst_telt_als_gesloten() {
    let (mut nat, mut io) = setup();
    let base = 1_000 * SEC;
    let id = flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1001, EXT_IP, 443, base).unwrap();
    assert!(
        !nat.note_tcp_flags(id, TCP_RST | TCP_ACK, true),
        "een inbound RST ruimt niet meteen op"
    );
    nat.sweep_expired(base + TCP_CLOSING_IDLE + 1);
    assert_eq!(
        (nat.flows.len(), nat.flows.count(1)),
        (0, 0),
        "een RST-gesloten flow verliep niet op de sluit-TTL"
    );
}

#[test]
fn tcp_rst_bezorging_en_veilige_reclaim() {
    // Outbound zonder flow.
    {
        let (mut nat, mut io) = setup();
        leer_gateway(&mut io);
        let before = nat.masq_next;
        let mut rst = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(1),
            port_ip4(1),
            EXT_IP,
            5555,
            443,
            &[],
        );
        set_tcp_flags(&mut rst, TCP_RST | TCP_ACK);
        assert!(
            nat.outbound(&mut io, 1, &mut rst, T0),
            "flow-loze RST niet afgehandeld"
        );
        assert!(io.sent.is_empty(), "flow-loze RST toch vertaald");
        assert_eq!((nat.flows.len(), nat.masq_next), (0, before));
    }
    // Inbound.
    {
        let (mut nat, mut io) = setup();
        leer_gateway(&mut io);
        let slot_ip = port_ip4(1);
        let mut out = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(1),
            slot_ip,
            EXT_IP,
            5555,
            443,
            &[],
        );
        nat.outbound(&mut io, 1, &mut out, T0);
        let id = nat
            .flows
            .by_fwd(&FKey(PROTO_TCP, slot_ip, EXT_IP, 5555, 443))
            .unwrap();
        let np = nat.flows.get(id).unwrap().node_port;
        io.attach(1);
        let mut rst = mk_frame(PROTO_TCP, NIC_MAC, GW_MAC0, EXT_IP, NODE_IP, 443, np, &[]);
        set_tcp_flags(&mut rst, TCP_RST | TCP_ACK);
        assert!(nat.inbound(&mut io, &mut rst, T0));
        let got = io.read(1).expect("RST vóór aflevering weggegooid");
        assert_ne!(got[ETH_LEN + 20 + 13] & TCP_RST, 0);
        check_frame(&got, "inbound RST");
        assert_eq!(
            (nat.flows.len(), nat.flows.count(1)),
            (1, 1),
            "inbound RST ruimde onveilig vroeg op"
        );
    }
    // Outbound.
    {
        let (mut nat, mut io) = setup();
        leer_gateway(&mut io);
        let slot_ip = port_ip4(1);
        let mut first = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(1),
            slot_ip,
            EXT_IP,
            5555,
            443,
            &[],
        );
        nat.outbound(&mut io, 1, &mut first, T0);
        let mut rst = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(1),
            slot_ip,
            EXT_IP,
            5555,
            443,
            &[],
        );
        set_tcp_flags(&mut rst, TCP_RST | TCP_ACK);
        nat.outbound(&mut io, 1, &mut rst, T0);
        assert_eq!(io.sent.len(), 2);
        assert_ne!(io.sent[1][ETH_LEN + 20 + 13] & TCP_RST, 0);
        check_frame(&io.sent[1], "outbound RST");
        assert_eq!(
            (nat.flows.len(), nat.flows.count(1)),
            (0, 0),
            "outbound RST gaf de lease niet vrij"
        );
    }
}

#[test]
fn vol_slot_rejectpad_is_rate_limited() {
    let (mut nat, mut io) = setup();
    let now = T0;
    let mut oldest = None;
    for i in 0..MAX_FLOWS_PER_SLOT as u16 {
        let id = flow_for(&mut nat, &mut io, PROTO_UDP, 1, 1000 + i, EXT_IP, 443, now)
            .unwrap_or_else(|| panic!("flow {i} vóór het quotum geweigerd"));
        oldest.get_or_insert(id);
    }
    assert!(flow_for(&mut nat, &mut io, PROTO_UDP, 1, 60000, EXT_IP, 443, now).is_none());
    let (first_sweep, first_log) = (nat.next_sweep, nat.next_slot_full_log);
    assert!(
        first_sweep != 0 && first_log != 0,
        "eerste reject zette de cadans niet"
    );
    assert_eq!(io.logs.borrow().len(), 1);

    age(&mut nat, oldest.unwrap(), now - UDP_IDLE - SEC);
    assert!(flow_for(&mut nat, &mut io, PROTO_UDP, 1, 60001, EXT_IP, 443, now).is_none());
    assert!(
        nat.flows
            .by_fwd(&FKey(PROTO_UDP, port_ip4(1), EXT_IP, 1000, 443))
            .is_some(),
        "tweede reject veegde opnieuw vóór de cadans"
    );
    assert_eq!(
        (nat.next_sweep, nat.next_slot_full_log),
        (first_sweep, first_log)
    );
    assert_eq!(io.logs.borrow().len(), 1, "herhaalde reject logde opnieuw");

    nat.next_sweep = 0; // de volgende goedkope cadans
    assert!(flow_for(&mut nat, &mut io, PROTO_UDP, 1, 60002, EXT_IP, 443, now).is_some());
    assert_eq!(
        (nat.flows.len(), nat.flows.count(1)),
        (MAX_FLOWS_PER_SLOT, MAX_FLOWS_PER_SLOT)
    );
}

#[test]
fn unpublish_slot_ruimt_op() {
    let (mut nat, mut io) = setup();
    nat.publish(Proto::Tcp, 8080, 1, 8080).unwrap();
    nat.publish(Proto::Tcp, 8081, 2, 8081).unwrap();
    flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1001, EXT_IP, 443, T0).unwrap();
    flow_for(&mut nat, &mut io, PROTO_TCP, 2, 1002, EXT_IP, 443, T0).unwrap();
    flow_for(&mut nat, &mut io, PROTO_TCP, 2, 1003, port_ip4(1), 8080, T0).unwrap(); // hairpin naar 1
    nat.unpublish_slot(1);
    assert_eq!(nat.pubs.len(), 1);
    assert_eq!(nat.pubs[0].slot, 2);
    assert_eq!(nat.flows.len(), 1);
    for id in nat.flows.ids() {
        let fl = nat.flows.get(id).unwrap();
        assert!(
            fl.slot == 2 && fl.dst_ip != port_ip4(1),
            "verkeerde flow overleefde"
        );
    }
    assert_eq!((nat.flows.count(1), nat.flows.count(2)), (0, 1));
}

/// Go toetste hier dat de backing-array na een publicatiepiek loslaat. De
/// tabel is hier vast begrensd en houdt niets vast; wat overblijft is dat
/// precies de juiste publicatie overleeft.
#[test]
fn unpublish_slot_compacteert_publicatiepiek() {
    let (mut nat, _) = setup();
    for p in 1000u16..1128 {
        nat.publish(Proto::Tcp, p, 1, p).unwrap();
    }
    nat.publish(Proto::Tcp, 9000, 2, 9000).unwrap();
    nat.unpublish_slot(1);
    assert_eq!(nat.pubs.len(), 1);
    assert_eq!((nat.pubs[0].slot, nat.pubs[0].node_port), (2, 9000));
}

/// Zonder next-hop: geclaimd maar gedropt, en het enige dat de NIC op mag
/// is de ARP-vraag van de node-stack naar de gateway (30-09: zonder die
/// vraag kwam er op een echt LAN nooit iets van buiten, dus werd de gateway
/// nooit geleerd). De retransmit na het antwoord gaat naar de gateway.
#[test]
fn off_subnet_vraagt_de_gateway() {
    let (mut nat, mut io) = setup();
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), T0));
    assert_eq!(io.sent.len(), 1, "alleen de ARP-vraag naar de gateway");
    assert!(is_bcast_arp(&io.sent[0]));
    assert_eq!(
        be32(&io.sent[0], 38),
        GW_IP,
        "de vraag hoort naar de gateway te gaan"
    );
    assert_eq!(nat.flows.len(), 0, "drop hoort geen flow achter te laten");
    assert_eq!(io.stats.nat_no_route.load(Relaxed), 1);

    feed(&mut nat, &mut io, &arp_reply(GW_MAC0, GW_IP), T0);
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), T0 + SEC));
    assert_eq!(io.sent.len(), 2, "de retransmit gaat de NIC op");
    assert_eq!(io.sent[1][0..6], GW_MAC0, "niet naar de gateway-MAC");
    assert_eq!(nat.flows.len(), 1);
}

/// De vergiftigde gateway van 30-09: op de Pi 5 aan het LAN liepen 11 van
/// 14 connects naar buiten op de deadline met `noroute` en `flowfull` nul,
/// want de NAT leerde de gateway-MAC uit elk frame met een bron buiten het
/// subnet. Nu zet alleen ARP van de gateway hem: geen broadcast, geen
/// link-local bron, geen ARP-probe, geen DHCP van een buurman, en ook geen
/// geclaimd antwoord op een flow met een vreemde bron-MAC.
#[test]
fn een_buurman_vergiftigt_de_gateway_niet() {
    let (mut nat, mut io) = setup();
    leer_gateway(&mut io);
    io.attach(1);
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), T0));
    let np = be16(l4(&io.sent[0]), 0);
    let rogue = [0x02, 0xBA, 0xD0, 0x00, 0x00, 0x77];
    let ll = 0xA9FE_0707; // 169.254.7.7
    let mut probe = arp_reply(rogue, 0); // RFC 5227: spa 0.0.0.0
    put16(&mut probe, 20, 1);
    put32(&mut probe, 38, LAN_IP);
    for f in [
        // SSDP als broadcast, een ander subnet op hetzelfde L2, een
        // link-local bron die ons unicast aanspreekt, DHCP en een ARP-probe.
        mk_frame(
            PROTO_UDP,
            [0xff; 6],
            rogue,
            ll,
            0xFFFF_FFFF,
            1900,
            1900,
            &[1],
        ),
        mk_frame(
            PROTO_UDP,
            LAN_MAC0,
            rogue,
            0xC0A8_0005,
            LAN_IP,
            5353,
            5353,
            &[1],
        ),
        mk_frame(PROTO_UDP, NIC_MAC, rogue, ll, NODE_IP, 5353, 5353, &[1]),
        mk_frame(PROTO_UDP, [0xff; 6], rogue, 0, 0xFFFF_FFFF, 68, 67, &[1]),
        probe,
        // Het antwoord op de flow zelf, maar met de MAC van een buurman.
        mk_frame(PROTO_TCP, NIC_MAC, rogue, EXT_IP, NODE_IP, 443, np, &[]),
    ] {
        feed(&mut nat, &mut io, &f, T0);
    }
    assert!(
        io.read(1).is_some(),
        "het antwoord op de flow kwam niet aan"
    );
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5556, 443), T0));
    assert_eq!(io.sent.len(), 2, "een vraag waar geen twijfel was");
    assert_eq!(io.sent[1][0..6], GW_MAC0, "gateway-MAC vergiftigd");
}

/// Een gateway die van MAC wisselt (een nieuwe router, een failover): met
/// een aankondiging volgt de NAT meteen; stil volgt hij op de twijfel van
/// de SYN-retransmit, binnen een seconde, en tot het antwoord blijft de oude
/// MAC in gebruik (31-08, en het herstel van 30-09).
#[test]
fn een_gateway_die_van_mac_wisselt() {
    let (mut nat, mut io) = setup();
    leer_gateway(&mut io);
    let gw1 = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x02];
    let mut garp = arp_reply(gw1, GW_IP);
    put32(&mut garp, 38, GW_IP); // gratuitous: tpa = spa
    feed(&mut nat, &mut io, &garp, T0);
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), T0));
    assert_eq!(io.sent.len(), 1);
    assert_eq!(io.sent[0][0..6], gw1, "de aankondiging werd niet gevolgd");

    // Stil terug naar de oude MAC: de retransmit twijfelt.
    let t = T0 + SEC;
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), t));
    assert_eq!(io.sent.len(), 3, "een ARP-vraag naar de gateway en de SYN");
    assert!(is_bcast_arp(&io.sent[1]) && be32(&io.sent[1], 38) == GW_IP);
    assert_eq!(io.sent[2][0..6], gw1, "tot het antwoord de bekende MAC");
    feed(&mut nat, &mut io, &arp_reply(GW_MAC0, GW_IP), t);
    assert!(nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), t));
    assert_eq!(io.sent.len(), 4, "binnen de seconde geen tweede vraag");
    assert_eq!(io.sent[3][0..6], GW_MAC0, "de SYN volgt het antwoord niet");
}

fn arp_reply(from_mac: [u8; 6], from_ip: u32) -> Vec<u8> {
    let mut r = vec![0u8; 42];
    r[0..6].copy_from_slice(&NIC_MAC);
    r[6..12].copy_from_slice(&from_mac);
    put16(&mut r, 12, ET_ARP);
    put16(&mut r, 14, 1);
    put16(&mut r, 16, 0x0800);
    r[18] = 6;
    r[19] = 4;
    put16(&mut r, 20, 2);
    r[22..28].copy_from_slice(&from_mac);
    put32(&mut r, 28, from_ip);
    r[32..38].copy_from_slice(&NIC_MAC);
    put32(&mut r, 38, NODE_IP);
    r
}

fn is_bcast_arp(f: &[u8]) -> bool {
    be16(f, 12) == ET_ARP && f[0..6] == [0xff; 6]
}

/// First-contact naar een host in het eigen net (Altra 14-07: de eerste
/// loader-golf dropte al zijn SYNs): de ARP-vraag én de SYN alvast via de
/// gateway (de Brother, 20-08: er bestaan hosts die broadcast-ARP niet
/// beantwoorden). Zijn SYN-ACK komt rechtstreeks, en dan kent de tabel hem:
/// het volgende frame gaat niet meer om.
#[test]
fn een_host_in_het_eigen_net() {
    let (mut nat, mut io) = setup();
    leer_gateway(&mut io);
    io.attach(1);
    assert!(nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 8000), T0));
    assert_eq!(io.sent.len(), 2, "verwacht ARP-request + SYN-via-gateway");
    let arp = &io.sent[0];
    assert!(is_bcast_arp(arp), "geen broadcast-ARP");
    assert_eq!((be16(arp, 20), be32(arp, 38)), (1, LAN_IP));
    assert_eq!((be32(arp, 28), mac_at(arp, 22)), (NODE_IP, NIC_MAC));
    assert_eq!(io.sent[1][0..6], GW_MAC0);
    let np = be16(l4(&io.sent[1]), 0);

    // Eén vraag tegelijk: de retransmit gaat weer om, zonder tweede vraag.
    nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 8000), T0);
    assert_eq!(io.sent.len(), 3);
    assert!(!is_bcast_arp(&io.sent[2]));
    assert_eq!(io.sent[2][0..6], GW_MAC0);

    // De host zwijgt op ARP, maar antwoordt de SYN die de router doorgaf.
    let mut synack = mk_frame(PROTO_TCP, NIC_MAC, LAN_MAC0, LAN_IP, NODE_IP, 8000, np, &[]);
    set_tcp_flags(&mut synack, TCP_SYN | TCP_ACK);
    feed(&mut nat, &mut io, &synack, T0);
    assert!(io.read(1).is_some(), "de SYN-ACK kwam niet aan");
    nat.outbound(&mut io, 1, &mut seg(LAN_IP, 5555, 8000, TCP_ACK), T0);
    assert_eq!(io.sent.len(), 4);
    assert_eq!(io.sent[3][0..6], LAN_MAC0, "nog steeds om via de gateway");
}

/// Een bekende buur die stil van MAC wisselde (de Brother die zwierf, 20-08
/// en 31-08): de eerste SYN is de gratis unicast-probe, de retransmit
/// twijfelt (één vraag per seconde, de oude MAC blijft tot het antwoord),
/// en het antwoord zet de nieuwe.
#[test]
fn syn_retry_twijfelt_aan_een_bekende_buur() {
    let (mut nat, mut io) = setup();
    leer(&mut io, LAN_IP, LAN_MAC0, T0);
    nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 631), T0);
    assert_eq!(io.sent.len(), 1);
    assert_eq!(io.sent[0][0..6], LAN_MAC0);
    nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 631), T0);
    assert_eq!(io.sent.len(), 3, "retry hoort ARP + SYN te sturen");
    assert!(is_bcast_arp(&io.sent[1]) && be32(&io.sent[1], 38) == LAN_IP);
    assert_eq!(io.sent[2][0..6], LAN_MAC0);
    nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 631), T0);
    assert_eq!(io.sent.len(), 4);
    assert_eq!(
        be16(&io.sent[3], 12),
        0x0800,
        "ARP-rate-limit liet een storm door"
    );
    let moved = [0x66, 0x77, 0x88, 0x99, 0xAA, 0xCC];
    feed(&mut nat, &mut io, &arp_reply(moved, LAN_IP), T0);
    nat.outbound(&mut io, 1, &mut syn(LAN_IP, 5555, 631), T0);
    assert_eq!(io.sent.last().unwrap()[0..6], moved);
}

/// Een levende flow houdt zijn next-hop vers (Linux: `neigh_confirm`): de
/// antwoorden van buiten verversen de gateway, dus na het verloop van de
/// tabel (120 s) vraagt geen frame van de flow opnieuw. Zonder antwoorden
/// verloopt hij wel, en dan vraagt het volgende frame.
#[test]
fn antwoorden_houden_de_next_hop_vers() {
    let (mut nat, mut io) = setup();
    leer_gateway(&mut io);
    io.attach(1);
    nat.outbound(&mut io, 1, &mut syn(EXT_IP, 5555, 443), T0);
    let np = be16(l4(&io.sent[0]), 0);
    let t = T0 + NEIGH_TTL - SEC;
    let reply = mk_frame(PROTO_TCP, NIC_MAC, GW_MAC0, EXT_IP, NODE_IP, 443, np, &[]);
    feed(&mut nat, &mut io, &reply, t);
    let later = T0 + NEIGH_TTL + SEC;
    nat.outbound(&mut io, 1, &mut seg(EXT_IP, 5555, 443, TCP_ACK), later);
    assert_eq!(io.sent.len(), 2, "een vraag ondanks het verkeer");
    assert_eq!(io.sent[1][0..6], GW_MAC0);

    let stil = t + NEIGH_TTL + SEC;
    nat.outbound(&mut io, 1, &mut seg(EXT_IP, 5555, 443, TCP_ACK), stil);
    assert_eq!(io.sent.len(), 3, "verlopen: alleen de vraag");
    assert!(is_bcast_arp(&io.sent[2]) && be32(&io.sent[2], 38) == GW_IP);
}

// --- hairpin_test.go ---------------------------------------------------------

/// Slot 2 belt node-IP:7878 (gepubliceerd door slot 1): heen en terug.
#[test]
fn hairpin_heen_en_terug() {
    let (mut nat, mut io) = setup();
    io.attach(1);
    io.attach(2);
    nat.publish(Proto::Tcp, 7878, 1, 7878).unwrap();
    let (cli, srv) = (port_ip4(2), port_ip4(1));
    let mut heen = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(2),
        cli,
        NODE_IP,
        5555,
        7878,
        b"hoi",
    );
    assert!(nat.outbound(&mut io, 2, &mut heen, T0));
    assert!(io.sent.is_empty(), "hairpin-frame ging de NIC uit");
    let got = io.read(1).expect("niets bezorgd bij de dienst");
    check_frame(&got, "heen");
    assert_eq!((be32(&got, ETH_LEN + 16), be16(l4(&got), 2)), (srv, 7878));
    assert_eq!(be32(&got, ETH_LEN + 12), NODE_IP);
    let masq = be16(l4(&got), 0);
    assert!((MASQ_BASE..MASQ_END).contains(&masq));
    assert!(got.ends_with(b"hoi"));

    let mut terug = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        srv,
        NODE_IP,
        7878,
        masq,
        b"dag",
    );
    assert!(nat.slot_reply(&mut io, 1, &mut terug, T0));
    assert!(io.sent.is_empty(), "reply ging de NIC uit");
    let got = io.read(2).expect("niets bezorgd bij de beller");
    check_frame(&got, "terug");
    assert_eq!(
        (be32(&got, ETH_LEN + 12), be16(l4(&got), 0)),
        (NODE_IP, 7878)
    );
    assert_eq!((be32(&got, ETH_LEN + 16), be16(l4(&got), 2)), (cli, 5555));
    assert!(got.ends_with(b"dag"));
}

#[test]
fn hairpin_udp() {
    let (mut nat, mut io) = setup();
    io.attach(1);
    io.attach(2);
    nat.publish(Proto::Udp, 5353, 1, 5353).unwrap();
    let mut heen = mk_frame(
        PROTO_UDP,
        HOST_MAC,
        port_mac(2),
        port_ip4(2),
        NODE_IP,
        6666,
        5353,
        b"vraag",
    );
    nat.outbound(&mut io, 2, &mut heen, T0);
    let got = io.read(1).expect("heen: niets bij de dienst");
    check_frame(&got, "udp heen");
    let masq = be16(l4(&got), 0);
    let mut terug = mk_frame(
        PROTO_UDP,
        HOST_MAC,
        port_mac(1),
        port_ip4(1),
        NODE_IP,
        5353,
        masq,
        b"antwoord",
    );
    nat.slot_reply(&mut io, 1, &mut terug, T0);
    let got = io.read(2).expect("terug: niets bij de beller");
    check_frame(&got, "udp terug");
    assert!(io.sent.is_empty());
}

#[test]
fn hairpin_ongepubliceerd_dropt() {
    let (mut nat, mut io) = setup();
    io.attach(1);
    let mut f = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(2),
        port_ip4(2),
        NODE_IP,
        5555,
        8080,
        &[],
    );
    assert!(nat.outbound(&mut io, 2, &mut f, T0));
    assert!(io.sent.is_empty() && io.read(1).is_none());
}

#[test]
fn hairpin_reply_zonder_flow_dropt() {
    let (mut nat, mut io) = setup();
    io.attach(2);
    nat.publish(Proto::Tcp, 7878, 1, 7878).unwrap();
    let mut f = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(1),
        port_ip4(1),
        NODE_IP,
        7878,
        20001,
        &[],
    );
    assert!(nat.slot_reply(&mut io, 1, &mut f, T0));
    assert!(io.sent.is_empty() && io.read(2).is_none());
    let _ = host_ip4();
}

/// De Pi 4-storm (03-10): slot 3 bestormt zijn eigen gepubliceerde poort
/// via het node-IP, 8 verbindingen tegelijk, 600 achter elkaar (drie
/// rondes binnen de sluit-TTL), terwijl Hop in slot 1 de laagste plekken
/// van de slab vasthoudt. Boven de 512 moet de recycler elke SYN een
/// gesloten flow van slot 3 geven; vóór de fix vond de steekproef na de
/// wrap telkens Hops flows en vielen 64 SYNs (op de node: 1 s RTO).
#[test]
fn hairpin_storm_recyclet_achter_buurflows() {
    let (mut nat, mut io) = setup();
    io.attach(3);
    nat.publish(Proto::Tcp, 8090, 3, 8090).unwrap();
    for i in 0..60 {
        flow_for(&mut nat, &mut io, PROTO_TCP, 1, 1000 + i, EXT_IP, 443, T0).unwrap();
    }
    let ip = port_ip4(3);
    // Per verbinding: client-poort, masq-poort, fase van de handshake.
    let mut act: Vec<(u16, u16, u8)> = Vec::new();
    let (mut started, mut done, mut now) = (0u16, 0, T0);
    while done < 600 {
        while act.len() < 8 && started < 600 {
            act.push((50000 + started, 0, 0));
            started += 1;
        }
        now += 100_000;
        let mut k = 0;
        while let Some(&(cport, masq, fase)) = act.get(k) {
            let (sport, dport, fl) = match fase {
                0 => (cport, 8090, TCP_SYN),
                1 => (8090, masq, TCP_SYN | TCP_ACK),
                3 => (cport, 8090, TCP_FIN | TCP_ACK),
                4 => (8090, masq, TCP_FIN | TCP_ACK),
                _ => (cport, 8090, TCP_ACK),
            };
            let mut f = mk_frame(
                PROTO_TCP,
                HOST_MAC,
                port_mac(3),
                ip,
                NODE_IP,
                sport,
                dport,
                &[],
            );
            set_tcp_flags(&mut f, fl);
            if sport == 8090 {
                nat.slot_reply(&mut io, 3, &mut f, now);
            } else {
                nat.outbound(&mut io, 3, &mut f, now);
            }
            let got = io
                .read(3)
                .unwrap_or_else(|| panic!("verbinding {cport}: fase {fase} gedropt"));
            if fase == 5 {
                act.remove(k);
                done += 1;
                continue;
            }
            let masq = if fase == 0 { be16(l4(&got), 0) } else { masq };
            act[k] = (cport, masq, fase + 1);
            k += 1;
        }
    }
    assert_eq!(nat.flows.count(3), MAX_FLOWS_PER_SLOT);
    assert_eq!(io.stats.nat_flow_full.load(Relaxed), 0);
}

/// Run 3 van de Pi 4-storm (03-10): slot 3 op zijn maximum met 504
/// gesloten en 8 lopende hairpin-flows, tussen 300 flows van Hop (slot 1)
/// die de slab domineren. De nieuwe SYN moet slagen en de oudste gesloten
/// flow van slot 3 nemen, waar die ook in de slab staat.
#[test]
fn vol_slot_neemt_altijd_de_oudste_gesloten_flow() {
    let (mut nat, mut io) = setup();
    io.attach(3);
    nat.publish(Proto::Tcp, 8090, 3, 8090).unwrap();
    let srv = port_ip4(3);
    let mut hop = 1000;
    let mut buren = |nat: &mut Nat, io: &mut TestIo, n: u16| {
        for _ in 0..n {
            flow_for(nat, io, PROTO_TCP, 1, hop, EXT_IP, 443, T0).unwrap();
            hop += 1;
        }
    };
    buren(&mut nat, &mut io, 150);
    let mut oudste = None;
    for i in 0..MAX_FLOWS_PER_SLOT as u16 {
        if i == 256 {
            buren(&mut nat, &mut io, 150);
        }
        let id = flow_for(&mut nat, &mut io, PROTO_TCP, 3, 50000 + i, srv, 8090, T0).unwrap();
        let fl = nat.flows.get_mut(id).unwrap();
        if i >= 8 {
            (fl.fin_fwd, fl.fin_rev) = (true, true);
            fl.seen = T0 + u64::from(i) * SEC / 100;
        }
        if i == 8 {
            oudste = Some(fl.fkey());
        }
    }
    let mut syn = mk_frame(
        PROTO_TCP,
        HOST_MAC,
        port_mac(3),
        srv,
        NODE_IP,
        60000,
        8090,
        &[],
    );
    set_tcp_flags(&mut syn, TCP_SYN);
    let now = T0 + 20 * SEC;
    nat.next_sweep = now + SEC; // geen veeg: de recycler moet het doen
    assert!(nat.outbound(&mut io, 3, &mut syn, now));
    assert!(io.read(3).is_some(), "de SYN viel bij een vol slot");
    assert_eq!(nat.flows.count(3), MAX_FLOWS_PER_SLOT);
    assert!(
        nat.flows.by_fwd(&oudste.unwrap()).is_none(),
        "niet de oudste gesloten flow gerecycled"
    );
    assert_eq!(io.stats.nat_flow_full.load(Relaxed), 0);
    assert!(io.logs.borrow().is_empty());
}

/// De eerste masquerade-poort ligt per boot ergens anders, en altijd in
/// het bereik.
#[test]
fn the_first_masquerade_port_moves_with_the_clock_and_stays_in_range() {
    for seed in [0, 1, 29_151, 29_152, u64::MAX, 1_790_639_999_991_823_008] {
        let p = masq_start(seed);
        assert!((MASQ_BASE..MASQ_END).contains(&p), "{seed}: {p}");
    }
    assert_ne!(masq_start(5), masq_start(6));
}
