//! Het interne net-plan: subnet, per-slot IP en MAC, de gateway, en de
//! framematen van het slot-LAN.
//!
//! Deterministisch, geen tabellen die leren: HOP is de gateway op
//! 10.100.0.1, slot `i` op 10.100.0.1 + `i` in een /16, MAC
//! `02:00:00:00:<slot, zestien bits>` (HOP = `..:00:00`). Eén bron van
//! waarheid, zodat de switch en de app-stacks nooit uiteenlopen; beide kanten
//! leiden het uit het slotnummer af.
//!
//! De waarheid staat in `abi::layout` (Go: `metal/abi/layout`), de
//! adresfuncties ook: een slotnummer als `usize` waarin 0 de kern is, want
//! poort 0 bestaat als poort, niet als app (`abi::layout::Slot` begint bij
//! 1). Dit bestand voegt de framematen van het slot-LAN toe.

pub use abi::layout::{HOST_MAC, NET_PREFIX, ip4_port, port_ip4, port_mac};

/// De poort achter een MAC van het slot-LAN (de inverse van [`port_mac`]):
/// `02:00:00:00` en dan de zestien bits van de poort; `None` voor elke
/// andere MAC.
#[must_use]
pub const fn mac_port(m: [u8; 6]) -> Option<usize> {
    match m {
        [0x02, 0, 0, 0, hi, lo] => Some((hi as usize) << 8 | lo as usize),
        _ => None,
    }
}

/// De MTU van het slot-LAN (de frame-ringen). Geen draad, geen bitfouten,
/// dus zo groot als IPv4 toelaat: één 1 MiB-chunk is dan 16 segmenten in
/// plaats van 700, met 8 ACK's in plaats van 350 (gemeten 04-09). Beide
/// stacks klemmen de MSS per bestemming: naar buiten het prefix blijft het
/// 1500, de uplink-NIC ziet nooit een jumbo.
pub const LAN_MTU: usize = abi::layout::NET_MTU;

/// Het grootste frame op het slot-LAN: MTU plus Ethernet-kop en marge (Go:
/// `maxFrameLen = netdev.MTU + netdev.EthernetMaximumSize`). Een groter
/// record wordt vóór elke flood, gateway-kopie of NAT-route geweigerd: anders
/// kan één slot de MTU-buffer van een buurslot corrupt verklaren of de
/// LAN-ringen met reuzenrecords vullen.
pub const MAX_LAN_FRAME: usize = LAN_MTU + 18;

/// De klassieke Ethernet-grens van de fysieke NIC's (Go: `uplinkMaxFrame`).
pub const UPLINK_MAX_FRAME: usize = 1500 + 18;

/// HOP's interne adres: de gateway die de apps als default route krijgen.
#[must_use]
pub const fn host_ip4() -> u32 {
    abi::layout::HOST_IP4
}

/// Ligt `ip` in het interne subnet (10.100.0.0/16)?
#[must_use]
pub const fn is_internal(ip: u32) -> bool {
    ip >> (32 - NET_PREFIX) == host_ip4() >> (32 - NET_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_matches_the_go_numbers() {
        assert_eq!(host_ip4(), 0x0A64_0001);
        assert_eq!(port_ip4(1), 0x0A64_0002);
        assert_eq!(port_mac(3), [2, 0, 0, 0, 0, 3]);
        assert_eq!(MAX_LAN_FRAME, 65553);
        assert!(is_internal(port_ip4(7)));
        assert!(!is_internal(0x0A00_020F));
        assert!(is_internal(port_ip4(40_000)), "/16, niet /24");
        assert_eq!(HOST_MAC, port_mac(0));
        assert_eq!(mac_port(port_mac(40_000)), Some(40_000));
        assert_eq!(mac_port([2, 0, 0, 1, 0, 3]), None);
    }
}
