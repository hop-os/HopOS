//! HOP's poort 0: de node-stack aan de switch (Go: `hostDevice` in
//! `hopswitch.go`, en `locnet.go`, `internal.go` in `hopnet`).
//!
//! Het interne subnet (10.100.0.0/24) mag de draad nooit op: dat gaat hier
//! de switch in, via de statische 1:1-gateway-vertaling ([`crate::gw`]). De
//! rest gaat als [`KIND_UPLINK`] de host-TX-ring in; de switch zet hem in de
//! egress-rij van de pomp. Verkeer naar het eigen adres (de agent belt de
//! leader op het eigen externe IP) komt hier niet langs: leannet stuurt een
//! frame naar zijn eigen MAC zelf terug zijn eigen ingress in, en ARP't
//! nooit naar zijn eigen IP (`Stack::route`, `Stack::send_eth`).
//!
//! Zo voedt alleen de switch de uplink, en bestaan Go's `uplinkTxMu` en
//! `hostDevice.txMu` niet meer: elke ring heeft één producer, en die is een
//! taak.

use crate::Stats;
use crate::gw;
use crate::plan::MAX_LAN_FRAME;
use crate::plan::is_internal;
use crate::ring::{InPlace, KIND_FRAME, KIND_UPLINK, Reader, Writer};
use crate::wire::{ET_IPV4, ETH_LEN, be16, be32, put_mac};
use core::sync::atomic::Ordering::Relaxed;
use executor::Executor;
use sync::{Signal, select, yield_now};

/// Frames per richting per ronde, zodat de host-taak de rest niet
/// verhongert.
pub const HOST_BURST: usize = 32;

/// De node-netstack zoals poort 0 hem ziet. De stack zelf komt uit
/// `leannet`; dit is de naad.
pub trait HostStack {
    /// Eén frame de stack in.
    fn receive(&mut self, frame: &[u8]);

    /// Het volgende frame dat de stack wil zenden, in `buf`; `None` als er
    /// niets ligt.
    fn poll_transmit(&mut self, buf: &mut [u8]) -> Option<usize>;

    /// Het vroegste moment (nanoseconden op de klok van de executor) waarop
    /// de stack gepolld wil worden voor zijn timers (retransmit, keepalive);
    /// `None` = alleen op een bel. Wie buiten de host-taak iets aan de stack
    /// geeft (een socket-write), zet de bel van de host-taak.
    fn poll_at(&self) -> Option<u64> {
        None
    }
}

/// De host-kant van poort 0 plus de node-stack.
pub struct HostPort<'a, S, R, W> {
    stack: S,
    /// Switch → HOP (leeskant).
    rx: R,
    /// HOP → switch (schrijfkant).
    tx: W,
    /// De bel van deze taak: de switch zet hem na een leeg→niet-leeg-schrijf.
    bell: &'a Signal,
    /// De deur van de switch: na een schrijf in de TX-ring.
    door: &'a Signal,
    stats: &'a Stats,
    /// De MAC en het IP van de node-stack op de uplink.
    mac: [u8; 6],
    ip: u32,
}

impl<'a, S: HostStack, R: Reader, W: Writer> HostPort<'a, S, R, W> {
    /// Poort 0 voor `stack` met extern adres `ip`/`mac`.
    #[expect(
        clippy::too_many_arguments,
        reason = "de bedrading van poort 0, één keer bij boot"
    )]
    pub fn new(
        stack: S,
        rx: R,
        tx: W,
        bell: &'a Signal,
        door: &'a Signal,
        stats: &'a Stats,
        mac: [u8; 6],
        ip: u32,
    ) -> Self {
        Self {
            stack,
            rx,
            tx,
            bell,
            door,
            stats,
            mac,
            ip,
        }
    }

    /// De stack, voor een toets die hem van buiten de lus aanraakt.
    #[cfg(test)]
    pub fn stack(&mut self) -> &mut S {
        &mut self.stack
    }

    /// Het system-service-IP is een capability die uitsluitend uit een
    /// slotring mag komen. Een fysiek LAN-frame met zo'n bronadres is spoof.
    fn external_spoofs_internal(p: &[u8]) -> bool {
        p.len() >= ETH_LEN + 20 && be16(p, 12) == ET_IPV4 && is_internal(be32(p, ETH_LEN + 12))
    }

    /// Eén ronde: de host-RX-ring de stack in, dan wat de stack wil zenden
    /// de naad over. `buf` is de ene framebuffer van de taak (groot genoeg
    /// voor een LAN-jumbo). `true` = er was werk.
    pub fn pass(&mut self, buf: &mut [u8]) -> bool {
        let mut worked = false;
        for _ in 0..HOST_BURST {
            let Some((kind, n)) = self.rx.read_into(buf) else {
                break;
            };
            let Some(f) = buf.get_mut(..n) else { break };
            worked = true;
            match kind {
                // Op de LAN-ring is HOP 10.100.0.1 met MAC slot 0; de stack
                // houdt zijn echte adres. Vertalen op deze naad, zonder rij.
                KIND_FRAME if gw::to_host(f, self.ip) => {
                    put_mac(f, 0, &self.mac);
                    self.stack.receive(f);
                }
                KIND_UPLINK if !Self::external_spoofs_internal(f) => self.stack.receive(f),
                _ => {
                    self.stats.host_rx_drops.fetch_add(1, Relaxed);
                }
            }
        }
        let (sent, full) = self.send_burst();
        if sent {
            // De switch nú wekken: de bel via de deur van de executor werkt
            // alleen als HOP idle is, en onder verkeer is HOP dat niet.
            self.door.set();
        }
        // Vol: de switch moet eerst lezen, en dat kan pas als wij afgeven.
        worked | sent | full
    }

    /// Tot [`HOST_BURST`] frames van de stack de naad over, elk in de
    /// host-TX-ring zelf gebouwd: geen kopie uit de framebuffer (GEMETEN
    /// 01-10 op de M4, M17: 76 us per MiB van de kern naar een app). Zo zendt
    /// de app al (applib `try_transmit_with`). Geeft (iets gezonden, de ring
    /// was vol).
    fn send_burst(&mut self) -> (bool, bool) {
        let mut sent = false;
        for _ in 0..HOST_BURST {
            let mut refused = false;
            let (stack, ip) = (&mut self.stack, self.ip);
            let r = self.tx.write_in_place(MAX_LAN_FRAME, |p| {
                let n = stack.poll_transmit(p)?;
                let kind = classify(p.get_mut(..n)?, ip);
                refused = kind.is_none();
                Some((kind?, n))
            });
            match r {
                InPlace::Full => return (sent, true),
                InPlace::Written(_) => {}
                InPlace::Nothing if refused => {
                    self.stats.host_rx_drops.fetch_add(1, Relaxed);
                }
                InPlace::Nothing => return (sent, false),
            }
            sent = true;
        }
        (sent, false)
    }

    /// De lus van de host-taak; keert niet terug.
    pub async fn run<const T: usize, const M: usize>(
        &mut self,
        exec: &'static Executor<T, M>,
        buf: &mut [u8],
    ) {
        loop {
            if self.pass(buf) {
                yield_now().await;
                continue;
            }
            match self.stack.poll_at() {
                Some(at) => {
                    select(self.bell.wait(), exec.until(at)).await;
                }
                None => self.bell.wait().await,
            }
        }
    }
}

/// De soort waarmee een frame van de stack de host-TX-ring in gaat; een
/// intern frame wordt hier al vertaald. `None`: intern maar niet
/// vertaalbaar (fragment, vreemd slot). Dat gaat ook niet de draad op, want
/// dat zou het interne adresplan naar buiten lekken.
fn classify(f: &mut [u8], ip: u32) -> Option<u32> {
    if f.len() >= ETH_LEN + 20 && be16(f, 12) == ET_IPV4 && is_internal(be32(f, ETH_LEN + 16)) {
        return gw::from_host(f, ip).then_some(KIND_FRAME);
    }
    Some(KIND_UPLINK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{HOST_MAC, host_ip4, port_ip4, port_mac};
    use crate::ring::mem::{self, MemReader, MemWriter};
    use crate::wire::testutil::*;
    use crate::wire::{PROTO_TCP, mac_at};
    use std::collections::VecDeque;

    const IP: u32 = 0x0A00_020F;
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    #[derive(Default)]
    struct FakeStack {
        got: Vec<Vec<u8>>,
        out: VecDeque<Vec<u8>>,
    }

    impl HostStack for FakeStack {
        fn receive(&mut self, f: &[u8]) {
            self.got.push(f.to_vec());
        }
        fn poll_transmit(&mut self, buf: &mut [u8]) -> Option<usize> {
            let f = self.out.pop_front()?;
            buf[..f.len()].copy_from_slice(&f);
            Some(f.len())
        }
    }

    struct T {
        port: HostPort<'static, FakeStack, MemReader, MemWriter>,
        /// De switch-kant: leest de host-TX, schrijft de host-RX.
        sw_tx: MemReader,
        sw_rx: MemWriter,
        door: &'static Signal,
    }

    fn setup() -> T {
        let (sw_tx, tx) = mem::pair(256 << 10);
        let (rx, sw_rx) = mem::pair(256 << 10);
        let bell: &'static Signal = Box::leak(Box::new(Signal::new()));
        let door: &'static Signal = Box::leak(Box::new(Signal::new()));
        let stats: &'static Stats = Box::leak(Box::new(Stats::new()));
        let port = HostPort::new(FakeStack::default(), rx, tx, bell, door, stats, MAC, IP);
        T {
            port,
            sw_tx,
            sw_rx,
            door,
        }
    }

    fn pass(t: &mut T) -> bool {
        let mut buf = vec![0u8; crate::plan::MAX_LAN_FRAME];
        t.port.pass(&mut buf)
    }

    #[test]
    fn intern_verkeer_gaat_vertaald_de_switch_in() {
        let mut t = setup();
        let f = mk_frame(
            PROTO_TCP,
            [0xaa; 6],
            MAC,
            IP,
            port_ip4(3),
            8080,
            40000,
            b"x",
        );
        t.port.stack().out.push_back(f);
        assert!(pass(&mut t));
        assert!(t.door.is_set(), "switch niet gewekt");
        let (kind, got) = t.sw_tx.pop().unwrap();
        assert_eq!(kind, KIND_FRAME);
        assert_eq!(be32(&got, ETH_LEN + 12), host_ip4());
        assert_eq!((mac_at(&got, 0), mac_at(&got, 6)), (port_mac(3), HOST_MAC));
        check_frame(&got, "intern");
        // Een intern frame dat de vertaling weigert, lekt niet naar buiten.
        let bad = mk_frame(PROTO_TCP, [0xaa; 6], MAC, IP, host_ip4(), 8080, 40000, &[]);
        t.port.stack().out.push_back(bad);
        pass(&mut t);
        assert!(t.sw_tx.pop().is_none());
    }

    /// Een volle host-TX-ring (M18: de stack zendt in de ring zelf): de
    /// stack wordt dan niet gepolld, het frame blijft bij hem en gaat de
    /// volgende ronde, en de ronde telt als werk zodat de host-taak afgeeft.
    #[test]
    fn volle_ring_houdt_het_frame_bij_de_stack() {
        let mut t = setup();
        let f = mk_frame(PROTO_TCP, [0xaa; 6], MAC, IP, 0x0808_0808, 5555, 53, &[]);
        t.port.stack().out.push_back(f.clone());
        t.sw_tx.0.borrow_mut().refuse = 1;
        assert!(pass(&mut t), "een volle ring telde niet als werk");
        assert!(t.sw_tx.pop().is_none());
        assert_eq!(t.port.stack().out.len(), 1, "frame weg uit de stack");
        pass(&mut t);
        assert_eq!(t.sw_tx.pop(), Some((KIND_UPLINK, f)));
    }

    /// Extern verkeer en ARP gaan onvertaald als uplink de switch in; de
    /// naad kent geen eigen ARP- of zelfbelpad meer (dat doet leannet).
    #[test]
    fn extern_verkeer_en_arp_gaan_als_uplink_de_switch_in() {
        let mut t = setup();
        let f = mk_frame(PROTO_TCP, [0xaa; 6], MAC, IP, 0x0808_0808, 5555, 53, &[]);
        let mut arp = ether_frame([0xff; 6], MAC, 0x0806);
        arp.resize(ETH_LEN + 28, 0);
        t.port.stack().out.extend([f.clone(), arp.clone()]);
        pass(&mut t);
        assert_eq!(t.sw_tx.pop(), Some((KIND_UPLINK, f)));
        assert_eq!(t.sw_tx.pop(), Some((KIND_UPLINK, arp)));
    }

    #[test]
    fn rx_vertaalt_gateway_en_weigert_spoof() {
        let mut t = setup();
        let app = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(1),
            port_ip4(1),
            host_ip4(),
            5555,
            9080,
            b"hoi",
        );
        assert!(t.sw_rx.push(KIND_FRAME, &app));
        let ext = mk_frame(PROTO_TCP, MAC, [0xbb; 6], 0x0808_0808, IP, 53, 5555, &[]);
        assert!(t.sw_rx.push(KIND_UPLINK, &ext));
        let spoof = mk_frame(PROTO_TCP, MAC, [0xbb; 6], port_ip4(4), IP, 53, 5555, &[]);
        assert!(t.sw_rx.push(KIND_UPLINK, &spoof));
        pass(&mut t);
        let got = &t.port.stack().got;
        assert_eq!(got.len(), 2, "spoof met intern bronadres bereikte de stack");
        assert_eq!(be32(&got[0], ETH_LEN + 16), IP);
        assert_eq!(mac_at(&got[0], 0), MAC);
        check_frame(&got[0], "gateway naar host");
        assert_eq!(got[1], ext);
    }
}
