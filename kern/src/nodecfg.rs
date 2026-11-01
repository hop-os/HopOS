//! Van `hopos.cfg` naar de env van Hop: de node-keuzes die de operator op
//! het bootmedium zet, als `KEY=waarde`-regels op de control-page van
//! Hop's slot.
//!
//! In de Go-kern las de kern `hopos.cfg` en gaf de sleutels in-proces aan
//! `agentboot`. Nu is Hop een app; wat hij van de node moet weten, gaat
//! als env-blob mee bij de plaatsing (`CTRL_ENV_DATA`, hoogstens
//! [`CTRL_ENV_MAX`] bytes). De lezer van de tekst is `fw::bootcfg` (één
//! parser voor elk kanaal); dit module kent alleen de sleutels en hun
//! vertaling:
//!
//! | `hopos.cfg` | env van Hop |
//! | --- | --- |
//! | `hopos.node` | `HOPOS_NODE` (anders `default_node` uit [`Facts`], zie [`default_node`]) |
//! | `hopos.cluster` | `HOPOS_CLUSTER` (anders `hopos`) |
//! | `hopos.apikey` | `HOPOS_APIKEY` |
//! | `hopos.insecure=1` | `HOPOS_INSECURE=1` |
//! | `hopos.s3.endpoint`, `.bucket`, `.region`, `.key`, `.secret`, `.pathstyle` | `HOPOS_S3_ENDPOINT` enzovoort |
//! | `hopos.ntp` (`host` of `host:poort`) | `HOPOS_NTP` (anders `pool.ntp.org`) |
//! | `hopos.lock.type` (`hoplockserver` of `s3`), `.url`, `.key`, `.apikey` | `HOPOS_LOCK_TYPE` enzovoort: de lease van het cluster; zonder blijft de node standalone leader |
//! | `hopos.lease_ttl` (seconden, minstens 15) | `HOPOS_LEASE_TTL` |
//! | `hopos.advertise` (`host:poort`) | `HOPOS_ADVERTISE`: het adres dat de andere nodes zien, achter een NAT |
//! | `hopos.init[]` (herhaald) | `HOPOS_INIT_JOBS`: één JSON-array |
//!
//! en uit de kern zelf ([`Facts`]): `HOPOS_NODE_IP`, `HOPOS_PORT`,
//! `HOPOS_CORES` (de eigen app-cores die Hop uitdeelt), `HOPOS_SYSTEM_CORE=1`
//! (de kern deelt zijn core als groep `system`), `HOPOS_MEMORY` en `DNS`
//! (de server uit de lease, voor de resolver van applib).
//!
//! Twee sleutels zijn voor de kern zelf, de plaatsing van Hop:
//! `hopos.hop.sharegroup` (zijn groep, `HOPOS_HOP_GROUP` in de env) en
//! `hopos.hop.core-class` ([`hop_core_class`]: de soort core die die groep
//! als eerste krijgt).
//!
//! # Per board twee bronnen
//!
//! Elk board geeft zijn config als bestand (`Board::config`): het venster
//! in het kern-image (`board::cfgwin`, op elk board), of bij een leeg
//! venster `hopos.cfg` van het bootmedium (de ESP op UEFI, 0xF000 op Apple,
//! de initrd op de Radxa), en als bootargs (`Board::bootargs`: de Pi's, de
//! Radxa, QEMU). [`NodeCfg::new`] leest ze in de volgorde van
//! `fw::bootcfg::layered`: eerst de bootargs, dan het bestand, en de laatste
//! waarde wint (04-10). Zo is een config in lagen (image/cfg: default, bord,
//! smaak) gewoon de aaneengeplakte tekst, en het bestand wint nog steeds van
//! de bootargs (Go: `rk3566.BootParam`). Alleen QEMU, dat geen bootmedium
//! heeft, krijgt voor Hop [`QEMU_CFG`] er nog vóór ([`NodeCfg::on_bank`],
//! Go: `board_virt.go`). Zonder
//! `hopos.apikey` is een node open (API en console, [`insecure`]): hij
//! draait uit de doos, ook als zijn config niet aankomt; een sleutel sluit.
//!
//! # De init-jobs: env, niet een bestand
//!
//! De keuze (29-09): de init-jobs gaan als `HOPOS_INIT_JOBS` in de env,
//! niet als bestand op hopfs. Een bestand vraagt een hopfs-schrijf vóór de
//! plaatsing, met een pad in het volume van een slot dat nog niet bestaat;
//! de env is er al en gaat atomair mee met de plaatsing. De ruimte is er:
//! de env heeft 3800 bytes, de headless-template één job van ~350, de
//! GUI-template twee van samen ~700. Past het geheel niet, dan valt
//! `HOPOS_INIT_JOBS` weg, luid (`HOPOS_CFG_INIT_TOO_BIG`), en start Hop
//! zonder init-jobs in plaats van helemaal niet: een node zonder basis is
//! bereikbaar en te repareren, een node zonder Hop niet. Hop leest dan
//! `/hop/init-jobs.json` in zijn volume als dat er staat; daar kan een
//! operator (of later de kern) een grotere set neerleggen.
//!
//! # Geheimen
//!
//! `hopos.apikey` en `hopos.s3.secret` staan in de env (Hop heeft ze
//! nodig), maar nooit op de console: [`EnvBlob::redacted`] vervangt die
//! waarden door hun lengte.

use crate::cage::CoreClass;
use abi::hopabi::CTRL_ENV_MAX;
use alloc::format;
use alloc::string::String;
use core::fmt::{self, Write as _};
use core::net::Ipv4Addr;
use fw::bootcfg;

/// De config van Hop op QEMU, dat geen bootmedium heeft: de bank, open en
/// luid, met de naam van de node. Vóór de bootargs, dus die winnen (een
/// tweede node van een cluster zet zijn naam en de sleutel van de rest).
pub const QEMU_CFG: &str = "hopos.node=hopos-qemu\nhopos.cluster=hopos\nhopos.insecure=1\n";

/// De clusternaam als `hopos.cluster` ontbreekt (Go: `hopos`).
const DEFAULT_CLUSTER: &str = "hopos";

/// De env-sleutels waarvan de waarde nooit op de console komt.
const SECRETS: [&str; 3] = ["HOPOS_APIKEY", "HOPOS_S3_SECRET", "HOPOS_LOCK_APIKEY"];

/// De S3-sleutels: `hopos.cfg` links, de env rechts.
const S3_KEYS: [(&str, &str); 6] = [
    ("hopos.s3.endpoint", "HOPOS_S3_ENDPOINT"),
    ("hopos.s3.bucket", "HOPOS_S3_BUCKET"),
    ("hopos.s3.region", "HOPOS_S3_REGION"),
    ("hopos.s3.key", "HOPOS_S3_KEY"),
    ("hopos.s3.secret", "HOPOS_S3_SECRET"),
    ("hopos.s3.pathstyle", "HOPOS_S3_PATHSTYLE"),
];

/// De cluster van Hop (`agentd_hopos::lock`): de lock, de lease, het adres
/// dat de andere nodes zien als de uplink achter een NAT zit (QEMU slirp), en
/// de tijdserver (een lease is een tijd: een node doet pas mee met een gezette
/// klok, en de kern pint de wandklok op een vaste datum tot SNTP hem zet).
/// `hopos.cfg` links, de env rechts. Zonder `hopos.lock.type` blijft de node
/// standalone leader; de S3-sleutels alleen maken geen cluster, want die zijn
/// ook de object-store van de apps.
const CLUSTER_KEYS: [(&str, &str); 7] = [
    ("hopos.lock.type", "HOPOS_LOCK_TYPE"),
    ("hopos.lock.url", "HOPOS_LOCK_URL"),
    ("hopos.lock.key", "HOPOS_LOCK_KEY"),
    ("hopos.lock.apikey", "HOPOS_LOCK_APIKEY"),
    ("hopos.lease_ttl", "HOPOS_LEASE_TTL"),
    ("hopos.advertise", "HOPOS_ADVERTISE"),
    ("hopos.ntp", "HOPOS_NTP"),
];

/// De config van een node: `hopos.cfg` met de bootargs ervóór
/// (`fw::bootcfg::layered`), en op QEMU de bank daar nog vóór. De laatste
/// waarde wint; een herhaalde sleutel telt alle lagen.
#[derive(Copy, Clone, Debug)]
pub struct NodeCfg<'a> {
    bank: &'a str,
    args: &'a str,
    file: &'a str,
}

impl<'a> NodeCfg<'a> {
    /// De config uit de tekst van een configbestand.
    #[must_use]
    pub const fn parse(text: &'a str) -> Self {
        Self::new(text, "")
    }

    /// De config van een board: het bestand `file` (`Board::config`) en de
    /// bootargs `args` (`Board::bootargs`).
    #[must_use]
    pub const fn new(file: &'a str, args: &'a str) -> Self {
        Self {
            bank: "",
            args,
            file,
        }
    }

    /// Met `bank` ([`QEMU_CFG`]) onder de rest: alles wint ervan.
    #[must_use]
    pub const fn on_bank(self, bank: &'a str) -> Self {
        Self { bank, ..self }
    }

    /// De laatste waarde van `key`, of "" (de enkelvoudige sleutel).
    #[must_use]
    pub fn one(&self, key: &'a str) -> &'a str {
        self.all(key).last().unwrap_or("")
    }

    /// Alle waarden van `key` (de herhaalde sleutel, `hopos.init[]`).
    fn all(&self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        bootcfg::all(self.bank, key).chain(bootcfg::layered(self.file, self.args, key))
    }
}

/// Wat de kern zelf over de node weet en Hop meegeeft.
#[derive(Copy, Clone, Debug)]
pub struct Facts<'a> {
    /// De naam als `hopos.node` ontbreekt.
    pub default_node: &'a str,
    /// Het uplink-adres uit de lease; zonder lease meldt Hop zijn slot-adres.
    pub node_ip: Option<Ipv4Addr>,
    /// De DNS-server uit de lease.
    pub dns: Option<Ipv4Addr>,
    /// De agent-poort; de leader luistert op poort + 1000.
    pub port: u16,
    /// Alle app-cores van het plan (Hop plant tegen deze, min de zijne als
    /// hij er zelf een bezet).
    pub app_cores: usize,
    /// Deelt Hop de OS-core met de kern? Dan bezet hij geen app-core en
    /// plant hij tegen alle app-cores (GEMETEN 02-10: de O6N gaf Hop 10 van
    /// 11 en de Pi 4 2 van 3 terwijl Hop op de OS-core zat: één core per
    /// node onbenut).
    pub hop_on_os: bool,
    /// Deelt de kern zijn core (de sharegroup `system`)? Dan zegt
    /// `HOPOS_SYSTEM_CORE=1` het Hop: die core deelt hij niet uit maar een
    /// job in `system` past er altijd, en een job zonder groep van één core
    /// mag bij een volle node door (de kern zet hem dan daar,
    /// HOPOS_PLACE_SYSTEM). Zonder dat weigerde Hop zelf al met "no
    /// capacity" (03-10).
    pub os_shared: bool,
    /// De sharegroup van Hop zelf (`hopos.hop.sharegroup`; `system` is de
    /// OS-core): jobs met die tag delen Hop's core, dus Hop telt haar vrij.
    pub hop_group: &'a str,
    /// Het geheugen van de pool (Hop plant tegen dit min het zijne).
    pub pool_bytes: u64,
    /// De partitie van Hop zelf.
    pub hop_mem: u64,
}

/// De env-blob van Hop: `KEY=waarde\n`-regels.
#[derive(Clone, Debug, Default)]
pub struct EnvBlob {
    text: String,
    /// Hoeveel init-jobs in `HOPOS_INIT_JOBS` staan.
    pub init_jobs: usize,
    /// Hoeveel init-jobs niet pasten en wegvielen.
    pub init_dropped: usize,
}

impl EnvBlob {
    /// De bytes voor de control-page.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// De lengte in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Is de env leeg?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// De waarde van `key`, als hij erin staat.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.text
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
    }

    /// De env zoals hij op de console mag: geheimen als hun lengte.
    #[must_use]
    pub fn redacted(&self) -> String {
        let mut out = String::new();
        for line in self.text.lines() {
            let (k, v) = line.split_once('=').unwrap_or((line, ""));
            let sep = if out.is_empty() { "" } else { " " };
            // Een schrijffout in een String bestaat niet (alleen OOM, en die
            // breekt af); de uitkomst negeren is hier dus geen verlies.
            let _ = if SECRETS.contains(&k) {
                write!(out, "{sep}{k}=<{} bytes>", v.len())
            } else if k == "HOPOS_INIT_JOBS" {
                write!(out, "{sep}{k}=<{} jobs, {} bytes>", self.init_jobs, v.len())
            } else {
                write!(out, "{sep}{line}")
            };
        }
        out
    }
}

/// Waarom er geen env voor Hop is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EnvError {
    /// Zonder de init-jobs is de env nog te groot (een absurde waarde in
    /// een andere sleutel).
    TooLarge {
        /// De lengte zonder init-jobs.
        len: usize,
        /// Wat de control-page draagt.
        max: usize,
    },
    /// `hopos.init[]` is geen JSON-object (de regel begint niet met `{` of
    /// eindigt niet met `}`); Hop leest hem daarna streng.
    BadInit {
        /// De plek (vanaf 0).
        index: usize,
    },
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => write!(f, "env is {len} bytes, the page holds {max}"),
            Self::BadInit { index } => {
                write!(
                    f,
                    "hopos.init[] entry {index} is not a one-line JSON object"
                )
            }
        }
    }
}

/// `hopos.hop.core-class`: de soort core waar de groep van Hop haar eerste
/// core zoekt, met de namen van `tags.core-class` van een job (`small`,
/// `mid`, `big`). Een voorkeur, geen eis (`pool::Placement::prefer`): is er
/// geen vrije core van die klasse, of kent het board geen klassen (de Pi's,
/// de Radxa, de Altra, de LicheeRV), dan een andere. Op de OS-core (groep
/// `system`) doet hij niets: daar kiest `hopos.oscore` de core.
///
/// # Errors
///
/// De waarde zelf als het geen klasse is; `Ok(None)` zonder sleutel.
pub fn hop_core_class<'a>(cfg: &NodeCfg<'a>) -> Result<Option<CoreClass>, &'a str> {
    match cfg.one("hopos.hop.core-class") {
        "" => Ok(None),
        "small" => Ok(Some(CoreClass::Small)),
        "mid" => Ok(Some(CoreClass::Mid)),
        "big" => Ok(Some(CoreClass::Big)),
        other => Err(other),
    }
}

/// De console over TCP (hopos conport.rs, net.rs): `hopos.console=1` zet
/// hem aan, `=0` uit; zonder die sleutel volgt hij `hopos.insecure=1` (de
/// testbank op het eigen LAN). Go: geen sleutel is geen poort, maar elke
/// bankconfig had hem; hier is de bank de insecure-vlag. De poort geeft
/// elke lezer de hele console, dus nooit stil aan op een node met een
/// sleutel: een board met een bootmedium zet `hopos.insecure` alleen als
/// de operator het schrijft.
#[must_use]
pub fn console_enabled(cfg: &NodeCfg<'_>) -> bool {
    match cfg.one("hopos.console") {
        "1" | "on" => true,
        "0" | "off" => false,
        _ => insecure(cfg),
    }
}

/// Open API en console: `hopos.insecure=1` zet het aan, `=0` uit; zonder
/// die sleutel is een node zonder `hopos.apikey` open (02-10: een node moet
/// uit de doos draaien, ook als zijn config niet aankomt), met een sleutel
/// dicht.
#[must_use]
pub fn insecure(cfg: &NodeCfg<'_>) -> bool {
    match cfg.one("hopos.insecure") {
        "1" => true,
        "0" => false,
        _ => cfg.one("hopos.apikey").is_empty(),
    }
}

/// `hopos.replay=N`: elke N seconden zet de kern zijn console-ring opnieuw
/// op de UART (hopos `conport::rewind`), voor een board
/// waarvan de lezer pas na de boot aanhaakt (de M4 over de dockchannel,
/// 30-09). 0 of niets = uit.
#[must_use]
pub fn replay_after(cfg: &NodeCfg<'_>) -> u64 {
    cfg.one("hopos.replay").parse().unwrap_or(0)
}

/// De env van Hop uit `cfg` en de feiten van de kern, zonder te loggen:
/// eerst alles behalve de init-jobs, dan de init-jobs als het geheel nog
/// past.
///
/// # Errors
///
/// [`EnvError::TooLarge`] als de env zonder init-jobs niet op de
/// control-page past, [`EnvError::BadInit`] als een `hopos.init[]` geen
/// JSON-object op één regel is.
pub fn build(cfg: &NodeCfg<'_>, facts: &Facts<'_>) -> Result<EnvBlob, EnvError> {
    let max = usize::try_from(CTRL_ENV_MAX).unwrap_or(usize::MAX);
    let mut text = String::new();
    base(&mut text, cfg, facts).map_err(|_| EnvError::TooLarge { len: 0, max })?;
    if text.len() > max {
        return Err(EnvError::TooLarge {
            len: text.len(),
            max,
        });
    }
    let mut jobs = String::new();
    let mut count = 0;
    for (index, spec) in cfg.all("hopos.init[]").enumerate() {
        if !(spec.starts_with('{') && spec.ends_with('}')) {
            return Err(EnvError::BadInit { index });
        }
        jobs.push(if count == 0 { '[' } else { ',' });
        jobs.push_str(spec);
        count += 1;
    }
    let mut blob = EnvBlob {
        text,
        init_jobs: 0,
        init_dropped: 0,
    };
    if count == 0 {
        return Ok(blob);
    }
    jobs.push(']');
    // "HOPOS_INIT_JOBS=" plus de array plus de regeleinde.
    let line = "HOPOS_INIT_JOBS=".len() + jobs.len() + 1;
    if blob.text.len() + line <= max {
        blob.text.push_str("HOPOS_INIT_JOBS=");
        blob.text.push_str(&jobs);
        blob.text.push('\n');
        blob.init_jobs = count;
    } else {
        blob.init_dropped = count;
    }
    Ok(blob)
}

/// De naam van een node zonder `hopos.node`: het board en de laatste twee
/// bytes van de uplink-MAC (`rpi4-4c54`). Zo draaien alle nodes op één
/// gedeelde config (image/cfg) en heeft toch elke node op het LAN een eigen
/// naam; Go gaf iedereen `hopos-1`.
#[must_use]
pub fn default_node(board: &str, mac: [u8; 6]) -> String {
    format!("{board}-{:02x}{:02x}", mac[4], mac[5])
}

/// De sleutels behalve de init-jobs.
fn base(out: &mut String, cfg: &NodeCfg<'_>, f: &Facts<'_>) -> fmt::Result {
    let node = match cfg.one("hopos.node") {
        "" => f.default_node,
        n => n,
    };
    let cluster = match cfg.one("hopos.cluster") {
        "" => DEFAULT_CLUSTER,
        c => c,
    };
    writeln!(out, "HOPOS_NODE={node}")?;
    writeln!(out, "HOPOS_CLUSTER={cluster}")?;
    let key = cfg.one("hopos.apikey");
    if !key.is_empty() {
        writeln!(out, "HOPOS_APIKEY={key}")?;
    }
    // Het vlag gaat mee zoals hij er staat, ook naast een sleutel: Hop
    // beslist (de sleutel wint) en zegt het zelf.
    if insecure(cfg) {
        writeln!(out, "HOPOS_INSECURE=1")?;
    }
    for (from, to) in S3_KEYS.iter().chain(CLUSTER_KEYS.iter()) {
        let v = cfg.one(from);
        if !v.is_empty() {
            writeln!(out, "{to}={v}")?;
        }
    }
    if let Some(ip) = f.node_ip {
        writeln!(out, "HOPOS_NODE_IP={ip}")?;
    }
    if let Some(dns) = f.dns {
        writeln!(out, "DNS={dns}")?;
    }
    writeln!(out, "HOPOS_PORT={}", f.port)?;
    // Hop plant tegen de cores die hij kan uitdelen: alle app-cores, min de
    // zijne als hij er een bezet (hij deelt zijn core niet met jobs, alleen
    // met zijn groep `hop`); op de OS-core bezet hij er geen. De OS-core
    // telt niet mee: die is van de kern en wordt gedeeld, niet uitgedeeld.
    // Met system-core is 0 de waarheid (de LicheeRV); zonder maakt Hop er
    // zelf minstens 1 van, zoals altijd.
    let own = usize::from(!f.hop_on_os);
    writeln!(out, "HOPOS_CORES={}", f.app_cores.saturating_sub(own))?;
    if f.os_shared {
        writeln!(out, "HOPOS_SYSTEM_CORE=1")?;
    }
    writeln!(out, "HOPOS_HOP_GROUP={}", f.hop_group)?;
    writeln!(
        out,
        "HOPOS_MEMORY={}",
        f.pool_bytes.saturating_sub(f.hop_mem)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTS: Facts<'static> = Facts {
        default_node: "hopos-1",
        node_ip: Some(Ipv4Addr::new(10, 0, 2, 15)),
        dns: Some(Ipv4Addr::new(10, 0, 2, 3)),
        port: 8080,
        app_cores: 3,
        hop_on_os: false,
        os_shared: false,
        hop_group: "hop",
        pool_bytes: 512 << 20,
        hop_mem: 64 << 20,
    };

    #[test]
    fn the_console_port_follows_the_key_and_then_the_insecure_flag() {
        let on = |t: &str| console_enabled(&NodeCfg::parse(t));
        assert!(on("hopos.insecure=1\n"));
        assert!(!on("hopos.console=0\nhopos.insecure=1\n"));
        assert!(on("hopos.console=on\n"));
        // Zonder sleutel is een node open, en dus ook zijn console.
        assert!(on(""));
        assert!(!on("hopos.apikey=geheim\n"));
        assert!(!on("hopos.insecure=0\n"));
        // De bootargs vullen aan wat het bestand niet zegt.
        let on2 = |f, a| console_enabled(&NodeCfg::new(f, a));
        assert!(on2("hopos.node=a\n", "hopos.console=1"));
        assert!(!on2("hopos.console=off", "hopos.console=1"));
    }

    #[test]
    fn the_default_name_is_the_board_and_the_mac_tail() {
        let mac = [0xdc, 0xa6, 0x32, 0x01, 0x4c, 0x54];
        assert_eq!(default_node("rpi4", mac), "rpi4-4c54");
        let name = default_node("o6n", [0, 0, 0, 0, 0, 7]);
        let facts = Facts {
            default_node: &name,
            ..FACTS
        };
        let env = build(&NodeCfg::parse("hopos.insecure=1\n"), &facts).unwrap();
        assert_eq!(env.get("HOPOS_NODE"), Some("o6n-0007"));
        // Een eigen hopos.node wint.
        let env = build(&NodeCfg::parse("hopos.node=lumen-1\n"), &facts).unwrap();
        assert_eq!(env.get("HOPOS_NODE"), Some("lumen-1"));
    }

    #[test]
    fn a_node_with_a_key_and_no_console_line_stays_closed() {
        // DE REGRESSIE (01-10): elk board kreeg de QEMU-config, dus Hop
        // insecure en 5555 open naast een sleutel, en de init-jobs weg.
        let cfg = NodeCfg::new(
            "hopos.node=pi5-1\nhopos.apikey=s3cr3t\n\
             hopos.init[]={\"name\":\"welcome\"}\n",
            "console=ttyAMA0 hopos.stage=hop",
        );
        assert!(!console_enabled(&cfg));
        let b = build(&cfg, &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_NODE"), Some("pi5-1"));
        assert_eq!(b.get("HOPOS_APIKEY"), Some("s3cr3t"));
        assert_eq!(b.get("HOPOS_INSECURE"), None);
        assert_eq!(b.get("HOPOS_INIT_JOBS"), Some(r#"[{"name":"welcome"}]"#));
    }

    #[test]
    fn the_file_wins_from_the_bootargs_and_the_bootargs_from_qemu() {
        // De Radxa: hopos.cfg in de initrd en een APPEND met dezelfde
        // sleutel; het bestand wint (Go: rk3566.BootParam).
        let cfg = NodeCfg::new("hopos.node=radxa-2", "hopos.node=radxa-1 hopos.replay=30");
        assert_eq!(cfg.one("hopos.node"), "radxa-2");
        assert_eq!(replay_after(&cfg), 30);
        // QEMU: QEMU_CFG vóór de bootargs, dus een tweede node heet anders.
        let qemu = NodeCfg::new("", "hopos.node=hopos-qemu-2 hopos.apikey=k root=/dev/vda")
            .on_bank(QEMU_CFG);
        let b = build(&qemu, &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_NODE"), Some("hopos-qemu-2"));
        assert_eq!(b.get("HOPOS_APIKEY"), Some("k"));
        assert_eq!(b.get("HOPOS_INSECURE"), Some("1"), "QEMU blijft de bank");
    }

    /// De config in lagen zoals de image-scripts hem in het venster zetten
    /// (image/cfg: default, het bord, de smaak).
    fn layers(board: &str, flavor: &str) -> String {
        let read = |f: &str| match f {
            "default" => include_str!("../../image/cfg/default.cfg"),
            "rpi4" => include_str!("../../image/cfg/rpi4.cfg"),
            "o6n" => include_str!("../../image/cfg/o6n.cfg"),
            "licheerv" => include_str!("../../image/cfg/licheerv.cfg"),
            "headless" => include_str!("../../image/cfg/headless.cfg"),
            _ => include_str!("../../image/cfg/headfull.cfg"),
        };
        format!("{}{}{}", read("default"), read(board), read(flavor))
    }

    #[test]
    fn the_last_value_wins_so_the_layers_stack() {
        let o6n = layers("o6n", "headless");
        let cfg = NodeCfg::parse(&o6n);
        assert_eq!(cfg.one("hopos.hop.sharegroup"), "hop");
        assert_eq!(hop_core_class(&cfg), Ok(Some(CoreClass::Small)));
        assert_eq!(cfg.one("hopos.cluster"), "hopos", "the default stays");
        // Een eigen regel erachter wint van alle lagen.
        let own = format!("{o6n}hopos.hop.sharegroup=system\nhopos.hop.core-class=big\n");
        let cfg = NodeCfg::parse(&own);
        assert_eq!(cfg.one("hopos.hop.sharegroup"), "system");
        assert_eq!(hop_core_class(&cfg), Ok(Some(CoreClass::Big)));
        // De Pi: Hop op de OS-core, geen klasse.
        let pi = layers("rpi4", "headless");
        let cfg = NodeCfg::parse(&pi);
        assert_eq!(cfg.one("hopos.hop.sharegroup"), "system");
        assert_eq!(hop_core_class(&cfg), Ok(None));
        let lrv = layers("licheerv", "headless");
        assert_eq!(NodeCfg::parse(&lrv).one("hopos.hop.sharegroup"), "hop");
        // Alleen headfull draagt init-jobs (welcome); een herhaalde sleutel
        // telt alle lagen.
        assert_eq!(NodeCfg::parse(&pi).all("hopos.init[]").count(), 0);
        let full = layers("rpi4", "headfull");
        let b = build(&NodeCfg::parse(&full), &FACTS).unwrap();
        assert_eq!(b.init_jobs, 1);
        assert!(
            b.get("HOPOS_INIT_JOBS")
                .unwrap()
                .contains(r#""name":"welcome""#)
        );
        // De sleutel is een klasse of niets.
        for (v, want) in [
            ("small", Ok(Some(CoreClass::Small))),
            ("mid", Ok(Some(CoreClass::Mid))),
            ("big", Ok(Some(CoreClass::Big))),
            ("", Ok(None)),
            ("e-core", Err("e-core")),
        ] {
            let t = format!("hopos.hop.core-class={v}\n");
            assert_eq!(hop_core_class(&NodeCfg::parse(&t)), want, "{v}");
        }
    }

    #[test]
    fn the_window_with_the_shared_config_wins_from_the_cmdline() {
        // De gedeelde config zoals image/hopcfg.py hem in het venster van de
        // kern zet (board/src/cfgwin.rs): kopregel, config, '#'-padding.
        let cfg = &layers("rpi4", "headfull");
        let mut window = format!("#HOPCFG1 window=16384 len={:010}\n{cfg}", cfg.len());
        while window.len() < 16384 {
            let n = (16384 - window.len()).min(65);
            window.extend(core::iter::repeat_n('#', n - 1));
            window.push('\n');
        }
        // De Pi: de rol en een naam in cmdline.txt, en een sleutel die het
        // venster ook zet.
        let args = "console=ttyAMA0 hopos.stage=hop hopos.node=pi4-keuken hopos.insecure=0";
        // Het hele venster lezen is hetzelfde als de config alleen (Go las
        // het venster als bestand): de padding is commentaar.
        for c in [NodeCfg::new(cfg, args), NodeCfg::new(&window, args)] {
            let b = build(&c, &FACTS).unwrap();
            assert_eq!(b.get("HOPOS_INSECURE"), Some("1"), "het venster wint");
            assert_eq!(
                b.get("HOPOS_NODE"),
                Some("pi4-keuken"),
                "de cmdline vult aan"
            );
            assert_eq!(b.get("HOPOS_CLUSTER"), Some("hopos"));
            assert!(
                b.get("HOPOS_INIT_JOBS")
                    .unwrap()
                    .contains(r#""name":"welcome""#)
            );
            assert!(console_enabled(&c));
            assert_eq!(c.one("hopos.hop.sharegroup"), "system");
        }
    }

    #[test]
    fn qemu_bootargs_add_the_s3_keys() {
        let cfg = NodeCfg::new(
            "",
            "hopos.stage=hop hopos.s3.endpoint=http://10.0.2.2:9000 hopos.s3.bucket=hop \
             hopos.s3.secret=geheim hopos.ntp=10.0.2.2:10123",
        )
        .on_bank(QEMU_CFG);
        let b = build(&cfg, &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_S3_ENDPOINT"), Some("http://10.0.2.2:9000"));
        assert_eq!(b.get("HOPOS_S3_BUCKET"), Some("hop"));
        assert_eq!(b.get("HOPOS_S3_REGION"), None);
        assert_eq!(b.get("HOPOS_NTP"), Some("10.0.2.2:10123"));
        assert_eq!(b.get("HOPOS_INSECURE"), Some("1"), "QEMU_CFG stays");
        assert!(!b.redacted().contains("geheim"));
    }

    #[test]
    fn qemu_default_is_what_the_kernel_gave_before() {
        let b = build(&NodeCfg::parse(QEMU_CFG), &FACTS).unwrap();
        assert_eq!(
            core::str::from_utf8(b.as_bytes()).unwrap(),
            "HOPOS_NODE=hopos-qemu\nHOPOS_CLUSTER=hopos\nHOPOS_INSECURE=1\n\
             HOPOS_NODE_IP=10.0.2.15\nDNS=10.0.2.3\nHOPOS_PORT=8080\nHOPOS_CORES=2\n\
             HOPOS_HOP_GROUP=hop\nHOPOS_MEMORY=469762048\n"
        );
    }

    #[test]
    fn a_shared_os_core_is_told_apart_from_the_cores_hop_hands_out() {
        let mut f = FACTS;
        f.os_shared = true;
        let b = build(&NodeCfg::parse(QEMU_CFG), &f).unwrap();
        // 3 app-cores min de eigen core van Hop; de OS-core apart.
        assert_eq!(b.get("HOPOS_CORES"), Some("2"));
        assert_eq!(b.get("HOPOS_SYSTEM_CORE"), Some("1"));
        // De LicheeRV: Hop op de enige app-core, alles via `system`.
        f.app_cores = 1;
        let b = build(&NodeCfg::parse(QEMU_CFG), &f).unwrap();
        assert_eq!(b.get("HOPOS_CORES"), Some("0"));
        assert_eq!(b.get("HOPOS_SYSTEM_CORE"), Some("1"));
        // Zonder gedeelde core (de M4) zegt de kern niets.
        f.os_shared = false;
        let b = build(&NodeCfg::parse(QEMU_CFG), &f).unwrap();
        assert_eq!(b.get("HOPOS_SYSTEM_CORE"), None);
    }

    #[test]
    fn hop_on_the_os_core_plans_against_every_app_core() {
        let mut f = FACTS;
        f.hop_on_os = true;
        let b = build(&NodeCfg::parse(QEMU_CFG), &f).unwrap();
        assert!(
            core::str::from_utf8(b.as_bytes())
                .unwrap()
                .contains("HOPOS_CORES=3\n")
        );
    }

    #[test]
    fn the_template_keys_arrive_and_secrets_stay_off_the_console() {
        let cfg = "# node\nhopos.node=hop-1\nhopos.cluster=hoplab\nhopos.apikey=s3cr3tkey\n\
            # hopos.insecure=1\nhopos.s3.endpoint=https://s3.example.com\nhopos.s3.bucket=hop-prod\n\
            hopos.s3.key=AKIA\nhopos.s3.secret=very-secret\nhopos.s3.pathstyle=1\n\
            hopos.init[]={\"name\":\"welcome\",\"artifacts\":[{\"url\":\"https://x/y.elf\"}]}\n\
            hopos.init[]={\"name\":\"two\", \"cmd\":\"met spaties\"}\n";
        let b = build(&NodeCfg::parse(cfg), &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_NODE"), Some("hop-1"));
        assert_eq!(b.get("HOPOS_CLUSTER"), Some("hoplab"));
        assert_eq!(b.get("HOPOS_APIKEY"), Some("s3cr3tkey"));
        assert_eq!(b.get("HOPOS_INSECURE"), None, "commentaar is geen config");
        assert_eq!(b.get("HOPOS_S3_ENDPOINT"), Some("https://s3.example.com"));
        assert_eq!(b.get("HOPOS_S3_SECRET"), Some("very-secret"));
        assert_eq!(b.get("HOPOS_S3_PATHSTYLE"), Some("1"));
        assert_eq!(b.get("HOPOS_S3_REGION"), None);
        assert_eq!(
            b.get("HOPOS_INIT_JOBS"),
            Some(
                r#"[{"name":"welcome","artifacts":[{"url":"https://x/y.elf"}]},{"name":"two", "cmd":"met spaties"}]"#
            )
        );
        assert_eq!((b.init_jobs, b.init_dropped), (2, 0));
        let shown = b.redacted();
        assert!(
            !shown.contains("s3cr3tkey") && !shown.contains("very-secret"),
            "{shown}"
        );
        assert!(shown.contains("HOPOS_APIKEY=<9 bytes>"), "{shown}");
        assert!(shown.contains("HOPOS_INIT_JOBS=<2 jobs,"), "{shown}");
        assert!(shown.contains("HOPOS_S3_KEY=AKIA"), "{shown}");
    }

    #[test]
    fn init_jobs_that_do_not_fit_are_dropped_not_hop() {
        let job = alloc::format!(
            "{{\"name\":\"big\",\"env\":{{\"X\":\"{}\"}}}}",
            "x".repeat(2000)
        );
        let cfg = alloc::format!("hopos.insecure=1\nhopos.init[]={job}\nhopos.init[]={job}\n");
        let b = build(&NodeCfg::parse(&cfg), &FACTS).unwrap();
        assert_eq!((b.init_jobs, b.init_dropped), (0, 2));
        assert_eq!(b.get("HOPOS_INIT_JOBS"), None);
        assert!(b.len() as u64 <= CTRL_ENV_MAX);
        assert_eq!(b.get("HOPOS_INSECURE"), Some("1"));
    }

    #[test]
    fn a_broken_init_line_and_an_absurd_value_are_refused() {
        let b = build(&NodeCfg::parse("hopos.init[]=name=welcome\n"), &FACTS);
        assert_eq!(b.unwrap_err(), EnvError::BadInit { index: 0 });
        let huge = alloc::format!("hopos.node={}\n", "n".repeat(4000));
        assert!(matches!(
            build(&NodeCfg::parse(&huge), &FACTS),
            Err(EnvError::TooLarge { .. })
        ));
    }

    #[test]
    fn no_node_name_takes_the_default() {
        let b = build(&NodeCfg::parse(""), &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_NODE"), Some("hopos-1"));
        assert_eq!(b.get("HOPOS_CLUSTER"), Some("hopos"));
        // Open uit de doos: zonder sleutel komt de vlag mee, met sleutel niet.
        assert_eq!(b.get("HOPOS_INSECURE"), Some("1"));
        let b = build(&NodeCfg::parse("hopos.apikey=geheim\n"), &FACTS).unwrap();
        assert_eq!(b.get("HOPOS_INSECURE"), None);
    }
}
