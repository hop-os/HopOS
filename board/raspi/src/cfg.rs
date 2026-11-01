//! De node-config van een Pi: `hopos.*`-sleutels uit het venster in het
//! image (`board::cfgwin`) en dan uit cmdline.txt (de firmware zet die in
//! /chosen/bootargs), en de stabiele identiteit uit het serienummer.
//!
//! Node-configuratie zonder rebuild (Derek, 11-07). Sleutels zijn
//! `hopos.`-geprefixt zodat Linux-restanten op de kaart onschadelijk zijn;
//! de parser is die van `fw::bootcfg`, één voor elk kanaal.
//!
//! Sleutels die deze kern leest:
//!
//! - `hopos.cores=N`: hoogstens N cores gebruiken (de kern-core
//!   meegeteld); meer dan de firmware meldt kan niet.

/// Het aantal cores: wat de firmware meldt, begrensd door `hopos.cores`
/// (0 = geen grens). Zonder firmware-getal telt de grens alleen.
#[must_use]
pub fn cores(firmware: usize, want: usize) -> usize {
    match (firmware, want) {
        (0, w) => w,
        (f, 0) => f,
        (f, w) => f.min(w),
    }
}

/// Een stabiel, lokaal beheerd MAC-adres (02:48 = "H") uit het serienummer
/// dat de firmware in de DTB zet (`/serial-number`, "10000000xxxxxxxx"):
/// uniek per board, gelijk over elke boot, precies wat een DHCP-server
/// nodig heeft om dezelfde lease terug te geven. Onleesbaar of krom: een
/// vaste terugval met het gegeven slotbyte (Go `raspi.MACFromSerial`).
#[must_use]
pub fn mac_from_serial(serial: Option<&str>, fallback: u8) -> [u8; 6] {
    let mut mac = [0x02, 0x48, 0x4f, 0x50, 0x00, fallback];
    let Some(s) = serial else { return mac };
    let Some(tail) = s.len().checked_sub(8).and_then(|i| s.get(i..)) else {
        return mac;
    };
    let mut b = [0u8; 4];
    for (i, c) in tail.bytes().enumerate() {
        let Some(v) = (c as char).to_digit(16) else {
            return mac;
        };
        if let Some(x) = b.get_mut(i / 2) {
            *x = (*x << 4) | v as u8;
        }
    }
    mac[2..].copy_from_slice(&b);
    mac
}

/// Een MAC als getal, om in een atomic te bewaren; bit 63 zegt "gezet".
#[must_use]
pub fn mac_word(mac: [u8; 6]) -> u64 {
    let mut w = [0u8; 8];
    w[..6].copy_from_slice(&mac);
    u64::from_le_bytes(w) | 1 << 63
}

/// Terug van [`mac_word`]; een ongezet woord geeft de terugval.
#[must_use]
pub fn mac_bytes(w: u64, fallback: u8) -> [u8; 6] {
    if w & 1 << 63 == 0 {
        return mac_from_serial(None, fallback);
    }
    let b = w.to_le_bytes();
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}
