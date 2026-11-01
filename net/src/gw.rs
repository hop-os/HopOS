//! De 1:1-vertaling tussen het interne gateway-adres (10.100.0.1) en het
//! échte stack-adres van HOP (Go: `gwnat.go`).
//!
//! Sinds de netstack-flip (09-08) draait HOP één stack met één NIC op zijn
//! externe IP; het interne adres is geen tweede NIC maar een statische
//! herschrijving op de gateway-naad: apps blijven 10.100.0.1 zien, de stack
//! ziet zijn eigen IP.
//!
//! Geen conntrack: de mapping is 1:1 op IP-niveau, poorten blijven
//! onaangeraakt, dus beide richtingen zijn stateloos en het pad kan niet
//! vollopen. ICMP heeft geen pseudo-header, dus daar volstaat de
//! IP-checksum; fragmenten weigeren we (het interne net heeft één MTU).

use crate::plan::{HOST_MAC, host_ip4, ip4_port, port_mac};
use crate::wire::{
    ETH_LEN, PROTO_ICMP, PROTO_TCP, PROTO_UDP, be32, fix_csum32, fix_l4_ip, ipv4_head, put_mac,
    put32,
};

/// Een intern-vertaalbaar IPv4-frame: TCP/UDP met volledige L4-kop, of ICMP.
/// Geeft `(l4-offset, proto)`.
fn gw_parse(f: &[u8]) -> Option<(usize, u8)> {
    let (ihl, proto) = ipv4_head(f)?;
    let need = match proto {
        PROTO_TCP => 20,
        PROTO_UDP => 8,
        PROTO_ICMP => 0,
        _ => return None,
    };
    if f.len() < ETH_LEN + ihl + need {
        return None;
    }
    Some((ETH_LEN + ihl, proto))
}

/// Herschrijft een frame van een app richting HOP (dst 10.100.0.1) naar
/// HOP's stack-adres: dst-IP → `host_ip`, plus IP- en L4-checksums. `false`
/// = geen vertaalbaar frame voor het gateway-adres; het frame is dan niet
/// aangeraakt en de aanroeper dropt het.
pub fn to_host(f: &mut [u8], host_ip: u32) -> bool {
    let Some((l4, proto)) = gw_parse(f) else {
        return false;
    };
    let old = be32(f, ETH_LEN + 16);
    if old != host_ip4() {
        return false;
    }
    put32(f, ETH_LEN + 16, host_ip);
    fix_csum32(f, ETH_LEN + 10, old, host_ip);
    if proto != PROTO_ICMP {
        fix_l4_ip(f, l4, proto, old, host_ip);
    }
    true
}

/// Herschrijft een frame van HOP's stack richting het interne net: src
/// `host_ip` → 10.100.0.1 (plus checksums) en de MAC's uit het
/// deterministische slot-plan, geen ARP. `false` = geen vertaalbaar frame
/// of geen geldig slot-adres; niet bezorgen.
pub fn from_host(f: &mut [u8], host_ip: u32) -> bool {
    let Some((l4, proto)) = gw_parse(f) else {
        return false;
    };
    if be32(f, ETH_LEN + 12) != host_ip {
        return false;
    }
    let Some(slot) = ip4_port(be32(f, ETH_LEN + 16)).filter(|&s| s >= 1) else {
        return false;
    };
    let gw = host_ip4();
    put32(f, ETH_LEN + 12, gw);
    fix_csum32(f, ETH_LEN + 10, host_ip, gw);
    if proto != PROTO_ICMP {
        fix_l4_ip(f, l4, proto, host_ip, gw);
    }
    put_mac(f, 0, &port_mac(slot));
    put_mac(f, 6, &HOST_MAC);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::port_ip4;
    use crate::wire::testutil::*;
    use crate::wire::{be16, put16};

    // Het "echte" stack-adres: 192.168.2.100.
    const GW_TEST_HOST_IP: u32 = 0xC0A8_0264;

    #[test]
    fn gw_to_host() {
        let slot_ip = port_ip4(3);
        for proto in [PROTO_TCP, PROTO_UDP] {
            let mut f = mk_frame(
                proto,
                HOST_MAC,
                port_mac(3),
                slot_ip,
                host_ip4(),
                40000,
                8080,
                b"hop",
            );
            assert!(to_host(&mut f, GW_TEST_HOST_IP), "proto {proto}: geweigerd");
            assert_eq!(be32(&f, ETH_LEN + 16), GW_TEST_HOST_IP);
            assert_eq!(be32(&f, ETH_LEN + 12), slot_ip, "src aangeraakt");
            check_frame(&f, "GwToHost");
        }
        // Niet voor het gateway-IP: weigeren én onaangeraakt laten.
        let mut f = mk_frame(
            PROTO_TCP,
            HOST_MAC,
            port_mac(3),
            slot_ip,
            port_ip4(5),
            40000,
            80,
            &[],
        );
        let orig = f.clone();
        assert!(!to_host(&mut f, GW_TEST_HOST_IP));
        assert_eq!(f, orig, "geweigerd frame is toch aangeraakt");
    }

    #[test]
    fn gw_from_host() {
        for proto in [PROTO_TCP, PROTO_UDP] {
            let mut f = mk_frame(
                proto,
                [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
                [1, 2, 3, 4, 5, 6],
                GW_TEST_HOST_IP,
                port_ip4(3),
                8080,
                40000,
                b"antwoord",
            );
            assert!(
                from_host(&mut f, GW_TEST_HOST_IP),
                "proto {proto}: geweigerd"
            );
            assert_eq!(be32(&f, ETH_LEN + 12), host_ip4());
            assert_eq!(f[0..6], port_mac(3));
            assert_eq!(f[6..12], HOST_MAC);
            check_frame(&f, "GwFromHost");
        }
        // Bestemming buiten het interne subnet: weigeren (uplink-verkeer).
        let mut f = mk_frame(
            PROTO_TCP,
            [0; 6],
            [0; 6],
            GW_TEST_HOST_IP,
            0x0808_0808,
            8080,
            443,
            &[],
        );
        assert!(!from_host(&mut f, GW_TEST_HOST_IP));
        // Bestemming 10.100.0.1 zelf (slot 0 is geen bezorgdoel): weigeren.
        let mut f = mk_frame(
            PROTO_TCP,
            [0; 6],
            [0; 6],
            GW_TEST_HOST_IP,
            host_ip4(),
            8080,
            443,
            &[],
        );
        assert!(!from_host(&mut f, GW_TEST_HOST_IP));
        // Andere bron dan het stack-adres: weigeren.
        let mut f = mk_frame(
            PROTO_TCP,
            [0; 6],
            [0; 6],
            0x0102_0304,
            port_ip4(3),
            8080,
            443,
            &[],
        );
        assert!(!from_host(&mut f, GW_TEST_HOST_IP));
    }

    /// Ping naar 10.100.0.1: alleen de IP-checksum verandert.
    #[test]
    fn gw_icmp() {
        let mut f = vec![0u8; ETH_LEN + 20 + 8];
        f[0..6].copy_from_slice(&HOST_MAC);
        put16(&mut f, 12, 0x0800);
        {
            let ip = &mut f[ETH_LEN..];
            ip[0] = 0x45;
            put16(ip, 2, 28);
            ip[8] = 64;
            ip[9] = PROTO_ICMP;
            put32(ip, 12, port_ip4(2));
            put32(ip, 16, host_ip4());
            let c = !fold16(sum_words(&ip[..20]));
            put16(ip, 10, c);
            let icmp = &mut ip[20..];
            icmp[0] = 8;
            let c = !fold16(sum_words(icmp));
            put16(icmp, 2, c);
        }
        let before = be16(&f, ETH_LEN + 22);
        assert!(to_host(&mut f, GW_TEST_HOST_IP));
        assert!(ip_valid(&f[ETH_LEN..]));
        assert_eq!(be16(&f, ETH_LEN + 22), before, "ICMP-checksum aangeraakt");
    }

    /// Fragmenten kunnen hun L4-checksum niet dragen: weigeren.
    #[test]
    fn gw_fragment_refused() {
        for field in [0x00B9u16, 0x2000] {
            let mut f = mk_frame(
                PROTO_TCP,
                HOST_MAC,
                port_mac(3),
                port_ip4(3),
                host_ip4(),
                40000,
                8080,
                &[],
            );
            put16(&mut f, ETH_LEN + 6, field);
            put16(&mut f, ETH_LEN + 10, 0);
            let c = !fold16(sum_words(&f[ETH_LEN..ETH_LEN + 20]));
            put16(&mut f, ETH_LEN + 10, c);
            assert!(
                !to_host(&mut f, GW_TEST_HOST_IP),
                "fragment {field:#x} vertaald"
            );
        }
    }
}
