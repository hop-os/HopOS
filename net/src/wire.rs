//! Frame-bytes: big-endian velden, IPv4-validatie en de incrementele
//! checksum-updates (RFC 1624) die elk NAT-pad deelt.
//!
//! Alles hier indexeert met `get`: frame-inhoud is onvertrouwd, en een
//! paniek op frame-inhoud velt de hele node, dus álle slots (de Go-kern had
//! er een `recover` voor; die bestaat hier niet, dus mag het niet kunnen).

/// De lengte van een Ethernet-kop.
pub(crate) const ETH_LEN: usize = 14;
/// Ethertype IPv4.
pub(crate) const ET_IPV4: u16 = 0x0800;
/// Ethertype ARP.
pub(crate) const ET_ARP: u16 = 0x0806;
/// Ethertype IPv6.
pub(crate) const ET_IPV6: u16 = 0x86dd;
/// IP-protocol TCP.
pub(crate) const PROTO_TCP: u8 = 6;
/// IP-protocol UDP.
pub(crate) const PROTO_UDP: u8 = 17;
/// IP-protocol ICMP.
pub(crate) const PROTO_ICMP: u8 = 1;

/// TCP-vlag FIN.
pub(crate) const TCP_FIN: u8 = 0x01;
/// TCP-vlag SYN.
pub(crate) const TCP_SYN: u8 = 0x02;
/// TCP-vlag RST.
pub(crate) const TCP_RST: u8 = 0x04;
/// TCP-vlag ACK.
pub(crate) const TCP_ACK: u8 = 0x10;

/// Leest een big-endian `u16` op `off`; 0 buiten bereik.
#[must_use]
pub(crate) fn be16(b: &[u8], off: usize) -> u16 {
    match b.get(off..off + 2) {
        Some(&[h, l]) => u16::from_be_bytes([h, l]),
        _ => 0,
    }
}

/// Leest een big-endian `u32` op `off`; 0 buiten bereik.
#[must_use]
pub(crate) fn be32(b: &[u8], off: usize) -> u32 {
    match b.get(off..off + 4) {
        Some(&[a, c, d, e]) => u32::from_be_bytes([a, c, d, e]),
        _ => 0,
    }
}

/// Schrijft een big-endian `u16` op `off`; stil buiten bereik.
pub(crate) fn put16(b: &mut [u8], off: usize, v: u16) {
    if let Some(d) = b.get_mut(off..off + 2) {
        d.copy_from_slice(&v.to_be_bytes());
    }
}

/// Schrijft een big-endian `u32` op `off`; stil buiten bereik.
pub(crate) fn put32(b: &mut [u8], off: usize, v: u32) {
    if let Some(d) = b.get_mut(off..off + 4) {
        d.copy_from_slice(&v.to_be_bytes());
    }
}

/// Kopieert zes bytes (een MAC) naar `off`; stil buiten bereik.
pub(crate) fn put_mac(b: &mut [u8], off: usize, mac: &[u8; 6]) {
    if let Some(d) = b.get_mut(off..off + 6) {
        d.copy_from_slice(mac);
    }
}

/// Leest zes bytes (een MAC) op `off`; nullen buiten bereik.
#[must_use]
pub(crate) fn mac_at(b: &[u8], off: usize) -> [u8; 6] {
    let mut m = [0u8; 6];
    if let Some(s) = b.get(off..off + 6) {
        m.copy_from_slice(s);
    }
    m
}

/// Eén byte op `off`; 0 buiten bereik.
#[must_use]
pub(crate) fn byte(b: &[u8], off: usize) -> u8 {
    b.get(off).copied().unwrap_or(0)
}

/// Werkt een internet-checksum (big-endian op `b[off..off+2]`) incrementeel
/// bij voor één veranderd 16-bit woord (RFC 1624: `HC' = ~(~HC + ~m + m')`).
pub(crate) fn fix_csum16(b: &mut [u8], off: usize, old: u16, new: u16) {
    let mut sum = u32::from(!be16(b, off));
    sum += u32::from(!old);
    sum += u32::from(new);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    // Na het vouwen past de som in 16 bits.
    put16(b, off, !((sum & 0xffff) as u16));
}

/// Idem voor een veranderd 32-bit woord (een IPv4-adres).
pub(crate) fn fix_csum32(b: &mut [u8], off: usize, old: u32, new: u32) {
    fix_csum16(b, off, (old >> 16) as u16, (new >> 16) as u16);
    fix_csum16(b, off, (old & 0xffff) as u16, (new & 0xffff) as u16);
}

/// De checksum-offset binnen de L4-kop: TCP 16, UDP 6.
const fn l4_csum_off(proto: u8) -> usize {
    if proto == PROTO_UDP { 6 } else { 16 }
}

/// Werkt poort (op `port_off` binnen de L4-kop: 0 = src, 2 = dst) en
/// checksum van een TCP/UDP-kop bij voor een IP- én poortwijziging. De
/// L4-kop begint op `l4` in `f`. UDP-checksum 0 blijft 0.
#[expect(
    clippy::too_many_arguments,
    reason = "de vorm van Go's rewriteL4 plus de offset"
)]
pub(crate) fn rewrite_l4(
    f: &mut [u8],
    l4: usize,
    proto: u8,
    port_off: usize,
    old_ip: u32,
    new_ip: u32,
    old_port: u16,
    new_port: u16,
) {
    let c = l4 + l4_csum_off(proto);
    put16(f, l4 + port_off, new_port);
    if proto == PROTO_UDP && be16(f, c) == 0 {
        return;
    }
    fix_csum32(f, c, old_ip, new_ip); // pseudo-header
    fix_csum16(f, c, old_port, new_port);
    // RFC 768: een berekende UDP-checksum van 0x0000 betekent "geen
    // checksum" en moet als 0xFFFF verzonden worden. De incrementele update
    // kan op 0 uitkomen; corrigeer dat (TCP en IP mogen 0x0000 wél houden).
    if proto == PROTO_UDP && be16(f, c) == 0 {
        put16(f, c, 0xffff);
    }
}

/// Werkt alleen de L4-checksum bij voor een gewijzigd IP in de
/// pseudo-header (de poorten blijven): het gateway-pad (Go: `gwFixL4`).
pub(crate) fn fix_l4_ip(f: &mut [u8], l4: usize, proto: u8, old_ip: u32, new_ip: u32) {
    let c = l4 + l4_csum_off(proto);
    if proto == PROTO_UDP && be16(f, c) == 0 {
        return;
    }
    fix_csum32(f, c, old_ip, new_ip);
    if proto == PROTO_UDP && be16(f, c) == 0 {
        put16(f, c, 0xffff);
    }
}

/// Rekent de L4-checksum van een TCP/UDP-frame helemaal uit, met de
/// pseudo-header. Voor een frame dat een app naar 10.100.0.1 stuurt: het
/// slot-LAN is vertrouwd (`link_trusted`), dus daar laat de app hem leeg,
/// zoals Linux' `CHECKSUM_PARTIAL`. Gaat zo'n frame door de hairpin naar
/// een buurslot, dan ziet die een afzender buiten het LAN en toetst hij;
/// een incrementele update van een lege checksum is dan rommel.
pub(crate) fn fill_l4_csum(f: &mut [u8], l4: usize, proto: u8) {
    let c = l4 + l4_csum_off(proto);
    let end = (ETH_LEN + usize::from(be16(f, ETH_LEN + 2))).min(f.len());
    if end < c + 2 {
        return;
    }
    put16(f, c, 0);
    let pseudo = f.get(ETH_LEN + 12..ETH_LEN + 20).map_or(0, sum_words);
    let seg = f.get(l4..end).map_or(0, sum_words);
    let len = u32::try_from(end - l4).unwrap_or(0);
    let mut v = !fold16(pseudo + u32::from(proto) + len + seg);
    if proto == PROTO_UDP && v == 0 {
        v = 0xffff;
    }
    put16(f, c, v);
}

/// De som van de 16-bit woorden van `b` (een oneven staart als hoge byte),
/// ongevouwen.
pub(crate) fn sum_words(b: &[u8]) -> u32 {
    let mut it = b.chunks_exact(2);
    let mut s: u32 = it
        .by_ref()
        .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
        .sum();
    if let [last] = it.remainder() {
        s += u32::from(*last) << 8;
    }
    s
}

/// Vouwt een som naar 16 bits (de enen-complementsom).
pub(crate) fn fold16(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    // Na het vouwen past de som in 16 bits.
    (sum & 0xffff) as u16
}

/// Een IPv4-frame met TCP/UDP en een volledige L4-kop: de offsets die elk
/// NAT-pad nodig heeft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ip4 {
    /// Offset van de L4-kop in het frame (`ETH_LEN + ihl`).
    pub(crate) l4: usize,
    /// Het IP-protocol.
    pub(crate) proto: u8,
}

/// De gedeelde fragmentregel: zowel een niet-nul offset als MF betekent dat
/// niet de volledige L4-datagram beschikbaar is. DF valt buiten het masker.
#[must_use]
pub(crate) fn ipv4_fragmented(f: &[u8]) -> bool {
    be16(f, ETH_LEN + 6) & 0x3fff != 0
}

/// Valideert een IPv4-frame met TCP/UDP en volledige L4-kop (Go: `ipv4L4`);
/// `None` voor al het andere (ARP, fragmenten, ICMP, afgekapt).
#[must_use]
pub(crate) fn ipv4_l4(f: &[u8]) -> Option<Ip4> {
    let (ihl, proto) = ipv4_head(f)?;
    // rewrite_l4 raakt bij TCP l4[16..18] (volledige 20-byte kop) en bij UDP
    // l4[6..8] (8-byte kop) aan; een te korte kop hier weigeren.
    let need = match proto {
        PROTO_TCP => 20,
        PROTO_UDP => 8,
        _ => return None,
    };
    if f.len() < ETH_LEN + ihl + need {
        return None;
    }
    Some(Ip4 {
        l4: ETH_LEN + ihl,
        proto,
    })
}

/// De IPv4-kop zonder L4-eisen: `(ihl, proto)` voor een niet-gefragmenteerd
/// IPv4-frame met een geldige IHL.
#[must_use]
pub(crate) fn ipv4_head(f: &[u8]) -> Option<(usize, u8)> {
    if f.len() < ETH_LEN + 20 || be16(f, 12) != ET_IPV4 {
        return None;
    }
    let v = byte(f, ETH_LEN);
    let ihl = usize::from(v & 0xf) * 4;
    if v >> 4 != 4 || ihl < 20 || f.len() < ETH_LEN + ihl || ipv4_fragmented(f) {
        return None;
    }
    Some((ihl, byte(f, ETH_LEN + 9)))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! De frame-bouwers van `nat_test.go`: `mkFrame`, `setTCPFlags`, de
    //! ontvanger-checks.
    use super::*;

    pub(crate) use super::{fold16, sum_words};

    /// De som over de hele IP-kop (checksum meegeteld) vouwt naar 0xFFFF.
    pub(crate) fn ip_valid(ip: &[u8]) -> bool {
        let ihl = usize::from(ip[0] & 0xf) * 4;
        fold16(sum_words(&ip[..ihl])) == 0xffff
    }

    /// Idem voor TCP/UDP inclusief pseudo-header; UDP-checksum 0 = "geen".
    pub(crate) fn l4_valid(ip: &[u8]) -> bool {
        let ihl = usize::from(ip[0] & 0xf) * 4;
        let proto = ip[9];
        let total = usize::from(be16(ip, 2));
        let l4 = &ip[ihl..total];
        if proto == PROTO_UDP && be16(l4, 6) == 0 {
            return true;
        }
        let sum = sum_words(&ip[12..20]) + u32::from(proto) + l4.len() as u32 + sum_words(l4);
        fold16(sum) == 0xffff
    }

    pub(crate) fn check_frame(f: &[u8], what: &str) {
        let ip = &f[ETH_LEN..];
        assert!(
            ip_valid(ip),
            "{what}: IP-checksum klopt niet na herschrijven"
        );
        assert!(
            l4_valid(ip),
            "{what}: L4-checksum klopt niet na herschrijven"
        );
    }

    /// Een geldig Ethernet+IPv4+TCP/UDP-frame met kloppende checksums.
    #[expect(clippy::too_many_arguments, reason = "de vorm van Go's mkFrame")]
    pub(crate) fn mk_frame(
        proto: u8,
        dst_mac: [u8; 6],
        src_mac: [u8; 6],
        src_ip: u32,
        dst_ip: u32,
        sport: u16,
        dport: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let l4_len = if proto == PROTO_UDP { 8 } else { 20 };
        let mut f = vec![0u8; ETH_LEN + 20 + l4_len + payload.len()];
        f[0..6].copy_from_slice(&dst_mac);
        f[6..12].copy_from_slice(&src_mac);
        put16(&mut f, 12, ET_IPV4);
        {
            let ip = &mut f[ETH_LEN..];
            ip[0] = 0x45;
            put16(ip, 2, (20 + l4_len + payload.len()) as u16);
            ip[8] = 64;
            ip[9] = proto;
            put32(ip, 12, src_ip);
            put32(ip, 16, dst_ip);
            let c = !fold16(sum_words(&ip[..20]));
            put16(ip, 10, c);
        }
        let ip = &mut f[ETH_LEN..];
        let (head, l4) = ip.split_at_mut(20);
        put16(l4, 0, sport);
        put16(l4, 2, dport);
        let csum_off = if proto == PROTO_TCP {
            l4[12] = 5 << 4;
            16
        } else {
            put16(l4, 4, (l4_len + payload.len()) as u16);
            6
        };
        l4[l4_len..].copy_from_slice(payload);
        let sum = sum_words(&head[12..20]) + u32::from(proto) + l4.len() as u32 + sum_words(l4);
        let mut c = !fold16(sum);
        if proto == PROTO_UDP && c == 0 {
            c = 0xffff;
        }
        put16(l4, csum_off, c);
        f
    }

    /// Zet de TCP-vlaggen en herberekent de volledige TCP-checksum.
    pub(crate) fn set_tcp_flags(f: &mut [u8], flags: u8) {
        let ip = &mut f[ETH_LEN..];
        let ihl = usize::from(ip[0] & 0x0f) * 4;
        let total = usize::from(be16(ip, 2));
        let (head, rest) = ip.split_at_mut(ihl);
        let l4 = &mut rest[..total - ihl];
        l4[13] = flags;
        put16(l4, 16, 0);
        let sum = sum_words(&head[12..20]) + u32::from(PROTO_TCP) + l4.len() as u32 + sum_words(l4);
        put16(l4, 16, !fold16(sum));
    }

    /// Een kaal Ethernet-frame (kop alleen).
    pub(crate) fn ether_frame(dst: [u8; 6], src: [u8; 6], et: u16) -> Vec<u8> {
        let mut f = vec![0u8; ETH_LEN];
        f[0..6].copy_from_slice(&dst);
        f[6..12].copy_from_slice(&src);
        put16(&mut f, 12, et);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    /// Een kleine deterministische bron voor de checksum-test (Go gebruikte
    /// `math/rand` met zaad 1; de eigenschap hangt niet van de bron af).
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    const NODE_IP: u32 = 0x0A00_020F;

    /// De incrementele update moet voor élke uitgangssituatie hetzelfde
    /// opleveren als volledig herrekenen: de ontvanger-check blijft waar.
    #[test]
    fn fix_csum_tegen_herberekening() {
        let mut rnd = XorShift(1);
        for i in 0..5000 {
            let mut h = [0u8; 20];
            for b in h.iter_mut() {
                *b = rnd.next() as u8;
            }
            h[0] = 0x45;
            put16(&mut h, 10, 0);
            let c = !fold16(sum_words(&h));
            put16(&mut h, 10, c);
            let old = be32(&h, 12);
            let nw = if i % 17 == 0 { old } else { rnd.next() as u32 };
            put32(&mut h, 12, nw);
            fix_csum32(&mut h, 10, old, nw);
            assert!(
                ip_valid(&h),
                "iteratie {i}: kop ongeldig na fix_csum32({old:#x}→{nw:#x})"
            );
        }
    }

    /// RFC 768: een update die op 0x0000 uitkomt moet bij UDP 0xFFFF worden.
    #[test]
    fn rewrite_l4_udp_nul_wordt_ffff() {
        let mut l4 = [0u8; 8];
        put16(&mut l4, 0, 5555);
        put16(&mut l4, 6, 0xffff);
        rewrite_l4(&mut l4, 0, PROTO_UDP, 0, NODE_IP, NODE_IP, 5555, 5555);
        assert_eq!(be16(&l4, 6), 0xffff);
    }

    /// UDP zonder checksum (0) blijft zonder checksum.
    #[test]
    fn rewrite_l4_udp_nul_blijft_nul() {
        let mut l4 = [0u8; 8];
        put16(&mut l4, 0, 5555);
        rewrite_l4(&mut l4, 0, PROTO_UDP, 0, 0x0A64_0002, NODE_IP, 5555, 20001);
        assert_eq!(be16(&l4, 6), 0);
        assert_eq!(be16(&l4, 0), 20001);
    }

    #[test]
    fn ipv4_l4_validatie() {
        let gw = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        let lan = [0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB];
        let valid = mk_frame(PROTO_TCP, gw, lan, 0x5DB8_D822, NODE_IP, 443, 5555, &[]);
        type Mut = fn(&mut Vec<u8>);
        let cases: [(&str, Mut, bool); 9] = [
            ("geldig TCP", |_| {}, true),
            ("te kort", |f| f.truncate(ETH_LEN + 10), false),
            ("geen IPv4-ethertype", |f| put16(f, 12, 0x0806), false),
            ("IPv6-versie", |f| f[ETH_LEN] = 0x65, false),
            ("ihl te klein", |f| f[ETH_LEN] = 0x44, false),
            ("fragment", |f| put16(f, ETH_LEN + 6, 0x00B9), false),
            (
                "eerste fragment met MF",
                |f| put16(f, ETH_LEN + 6, 0x2000),
                false,
            ),
            ("ICMP", |f| f[ETH_LEN + 9] = 1, false),
            ("TCP-kop afgekapt", |f| f.truncate(ETH_LEN + 20 + 12), false),
        ];
        for (name, m, ok) in cases {
            let mut f = valid.clone();
            m(&mut f);
            assert_eq!(ipv4_l4(&f).is_some(), ok, "{name}");
        }
        let u = mk_frame(PROTO_UDP, gw, lan, 0x5DB8_D822, NODE_IP, 53, 5555, &[]);
        assert_eq!(
            ipv4_l4(&u),
            Some(Ip4 {
                l4: ETH_LEN + 20,
                proto: PROTO_UDP
            })
        );
    }
}
