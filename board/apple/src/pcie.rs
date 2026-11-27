//! De PCIe-kant van de Mac mini: de controller zelf opbrengen, de link van
//! de ethernet-poort, de DART in bypass, en de brug en het endpoint
//! configureren. Wat op elk ander HopOS-board de firmware al gedaan had.
//!
//! De keten (Go `apcie.go` en `pcie.go`, BEWEZEN 29-08 in een chain-boot:
//! 263 tunables, 2 van 3 poorten, endpoint 02:00.0 14e4:1682, tg3 LINK UP):
//!
//! 1. power-domein aan ([`crate::pmgr`]), tunables op AXI, RC en PHY;
//! 2. de PHY-klokken aanvragen, de PHY uit reset, de gedeelde referentieklok
//!    en het root complex starten;
//! 3. per poort de vaste schrijfacties van m1n1's T8122-tak, de poort-PHY,
//!    de poort uit reset, en de DesignWare-kern (snelheid, breedte);
//! 4. dan de link van poort 2 ([`link_up`]): PERST# loopt op dit silicium
//!    over een AP-GPIO (ADT `function-perst` = GPIO 165) en de training
//!    moet expliciet gestart (PORT_LTSSMCTL); dat is van Linux'
//!    `pcie-apple.c`, niet van m1n1, en met die twee is de link binnen 50 ms
//!    up;
//! 5. de DART van de poort in bypass (DART én DAPF: half bypassen is niet
//!    bypassen) en de RID aan stream 0, zodat een DMA-adres een fysiek adres
//!    is. Dezelfde vertrouwensafspraak als op elk HopOS-board: de NIC is van
//!    de kern, apps praten via de interne switch;
//! 6. busnummers en het prefetch-venster van de brug, en pas dán de BAR's.
//!    Zonder dat venster staat de link, is het endpoint zichtbaar, en leest
//!    élk BAR-adres all-ones (29-08).
//!
//! Meevaller: t8132 heeft geen fuse-programmering (`fuse_bits = NULL` vanaf
//! t8122); dat was de enige tabel die niet uit de boom te lezen was. Alle
//! vaste offsets komen letterlijk uit m1n1 `src/pcie.c`, tak
//! `APCIE_T8122` met type `APCIE_T8132`: ongedocumenteerd, dus ze staan er
//! zoals ze daar staan, in dezelfde volgorde.
//!
//! Op deze bus is een externe abort UITGESTELD: hij landt bij de
//! eerstvolgende toegang, niet bij de schrijf die hem veroorzaakte. Daarom
//! meldt elke stap zich vóór hij hem doet ([`step`]), met een barrière
//! (29-08: één verkeerd masker, `PCIE_LNKCAP2_SLS` van 7 in plaats van 6
//! bits, nam het hele blok onderuit, en pas de tik na elke schrijf wees het
//! aan).

use crate::chip::Eth;
use crate::fwinfo;
use crate::pmgr::{self, Pmgr};
use cpu::idle::now;
use cpu::println;
use dev::Pa;
use driver_pcie::{Bdf, Config, Ecam, Function, MmioWindow};
use fw::adt::{Adt, Node};

/// De ethernet-poort van deze chip ([`crate::chip::Eth`]). Op de M4,
/// allemaal GEMETEN:
///
/// - `ecam`: de ECAM van `/arm-io/apcie` (reg[0]; 32 KB per poort op bus 0);
/// - `mmio`: het 64-bit prefetch-venster, 512 MB, PCI-adres == CPU-adres
///   (de ADT-ranges: het 32-bit venster vertaalt, dit niet);
/// - `port`: device 2 op bus 0 (`pci-bridge2`, `lan-1gb`), en bus 2 erachter;
/// - `port_base`: het poortregisterblok (reg[7 + 8 * 2] van de apcie-node);
/// - `perst_pin`: de PERST#-GPIO van de poort (ADT `function-perst`);
/// - `dart`: de DART van de poort (`dart,t8110`);
/// - `gpio`: `/arm-io/gpio0` (`gpio,t8101`): één register per pin; DATA
///   bit 0, MODE 3:1. Alleen de onderste vier bits aanraken: de rest is de
///   pinconfig van iBoot, en die wissen maakt de pin dood (GEMETEN: 0x74a02
///   → 0x2).
///
/// `None` op een chip die nog niet gemeten is: dan meldt [`survey`] wat de
/// ADT zegt, en komt er geen NIC.
static ETH: Option<Eth> = crate::chip::CHIP.eth;

/// De tabel, of waarom er geen is.
fn eth() -> Result<&'static Eth, Why> {
    ETH.as_ref()
        .ok_or("apcie: no ethernet table for this chip yet (board/apple/src/chip.rs)")
}

/// Het poortnummer van de ethernet-poort (0 zonder tabel).
#[must_use]
pub fn eth_port() -> u8 {
    ETH.as_ref().map_or(0, |e| e.port)
}

// De poortregisters (Linux `pcie-apple.c`; m1n1 dezelfde).
const PORT_LTSSMCTL: u64 = 0x080;
const PORT_LINKSTS: u64 = 0x208;
const PORT_APPCLK: u64 = 0x800;
const PORT_STATUS: u64 = 0x804;
const PORT_PERST: u64 = 0x814;
/// Op deze generatie is DIT de poort-reset, niet 0x814.
const PORT_RESET_T602X: u64 = 0x82c;
/// RID → DART-stream op t602x/t8132 (niet de 0x828 van de oudere chips);
/// m1n1 nult hem en zet er nooit iets in.
const PORT_RID2SID: u64 = 0x3000;
const PORT_MSIMAP: u64 = 0x3800;
/// INTSTAT en INTMSK van de poort (Linux `pcie-apple.c`). INTSTAT is
/// alleen te lezen: een W1C-schrijf is hier een synchrone abort (ESR
/// 0x96000410, FAR 0x492028100, Go bundel 23/24, 19-09). Hoeft ook niet:
/// INTA is level en valt met de interrupt-mailbox van de tg3.
const PORT_INTSTAT: u64 = 0x100;
const PORT_INTMSK: u64 = 0x104;
/// INTA van een rootpoort is de vijfde lijn van zijn AIC-blok: het blok
/// begint op het woord van de poort in `/arm-io/apcie` "interrupts" (één
/// per poort, 1231, 1240, 1249 op de M4), INTA is + 4. GEMETEN door
/// aflevering op 19-09 (Go bundel 25: AIC 1253 voor poort 2), niet uit
/// documentatie.
pub const INTA: u32 = 4;
const RID2SID_VALID: u32 = 1 << 31;

// DART (Linux `apple-dart.c`): per stream een TCR. Bit 2 (DAPF-bypass) is
// op deze DART niet schrijfbaar (GEMETEN: 0x6 leest 0x2), maar m1n1 zet
// beide, en DMA op 1 TiB+ loopt.
const DART_TCR: u64 = 0x1000;
const DART_BYPASS: u32 = (1 << 1) | (1 << 2);
const DART_STREAMS: u64 = 16;

// De apcie-node: config, RC en het PHY-blok staan op elke chip van de
// T8122-tak op dezelfde plek; de rest per recept ([`Recipe`]).
const IDX_CONFIG: usize = 0;
const IDX_RC: usize = 1;
const IDX_PHY: usize = 2;
const PHY_CMN_OFF: u64 = 0x4000;
/// PHYCMN_CLK: bit 31 = de referentieklok van 100 MHz staat (m1n1
/// `APCIE_PHYCMN_CLK_100MHZ`; alleen de M5-tak wacht erop).
const PHYCMN_CLK_100MHZ: u32 = 1 << 31;

/// Wat per chip verschilt in m1n1's T8122-tak (`struct reg_info` en de
/// takken op `type`), letterlijk overgenomen: de M4 uit `regs_t8132`
/// (BEWEZEN 29-08), de M5 uit PR #747 (`regs_t8142`, nog niet gemerged op
/// 27-11, niet op ons ijzer geweest). Voor de M6 heeft m1n1 nog niets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// De naam, zoals `hopos.apcie` hem noemt.
    pub name: &'static str,
    /// Gedeelde reg-vensters vóór die van de poorten.
    shared_regs: usize,
    /// De reg-index van PHY-IP en van AXI2AF.
    idx_phy_ip: usize,
    idx_axi: usize,
    /// De PHY op het PHY-blok (m1n1: `phy_base[0] += 0x8000`, op de M5
    /// 0x20000, "the PHY-PHY block is reg[3] = reg[2] + 0x20000").
    phy_off: u64,
    /// Wachten op de referentieklok vóór de PHY-klokken (M5, T602X).
    refclk_wait: bool,
    /// `AppleT8142PCIe::_initializeGen4Phy`: bit 16 van PHY-IP + 0x90 wissen.
    gen4_clear: bool,
    /// Wat de PHY_CTRL na de klok krijgt (0x200 op de T8122-tak, bit 27 op
    /// de M5: `APCIECoreRCGen4::_apcieGPInit`).
    phy_run: u32,
}

/// De M4 (`apcie,t8132`).
pub const T8132: Recipe = Recipe {
    name: "t8132",
    shared_regs: 7,
    idx_phy_ip: 3,
    idx_axi: 4,
    phy_off: 0x8000,
    refclk_wait: false,
    gen4_clear: false,
    phy_run: 0x200,
};

/// De M5 (`apcie,t8142`, m1n1 PR #747).
pub const T8142: Recipe = Recipe {
    name: "t8142",
    shared_regs: 8,
    idx_phy_ip: 4,
    idx_axi: 5,
    phy_off: 0x2_0000,
    refclk_wait: true,
    gen4_clear: true,
    phy_run: 1 << 27,
};

/// Het recept bij de compatible van `/arm-io/apcie`, of wat `hopos.apcie`
/// afdwingt (`t8132`, `t8142`): op een chip zonder recept is proberen een
/// keuze van de mens, niet van de kern.
#[must_use]
pub fn recipe(compatible: impl Fn(&str) -> bool, force: &str) -> Option<Recipe> {
    match force {
        "t8132" => return Some(T8132),
        "t8142" => return Some(T8142),
        _ => {}
    }
    if compatible("apcie,t8132") {
        Some(T8132)
    } else if compatible("apcie,t8142") {
        Some(T8142)
    } else {
        None
    }
}

const PHY_CTRL: u64 = 0x000;
const PHY_CLK0REQ: u32 = 1 << 0;
const PHY_CLK1REQ: u32 = 1 << 1;
const PHY_CLK0ACK: u32 = 1 << 2;
const PHY_CLK1ACK: u32 = 1 << 3;
const PHY_RESET_T8132: u32 = 1 << 4;

// De DesignWare-kern in de config-space van de poort.
const DBI_RO_WR: u64 = 0x8bc;
const PORT_LINK_CTL: u64 = 0x710;
const PORT_LINK_MODE: u32 = 0x3f << 16;
const LINK_WIDTH_CTL: u64 = 0x80c;
const LINK_WIDTH: u32 = 0x1f << 8;
const SPEED_CHANGE: u32 = 1 << 17;
const CAP: u64 = 0x70;
const LNKCAP: u64 = 0x0c;
const LNKCAP_SLS: u32 = 0xf;
const LNKCAP_MLW: u32 = 0x3f << 4;
const LNKCAP2: u64 = 0x2c;
/// GENMASK(6,1): ZES bits. Zeven nam bit 7 (Crosslink) mee en daarmee het
/// hele registerblok (29-08).
const LNKCAP2_SLS: u32 = 0x3f << 1;
const LNKCTL2: u64 = 0x30;
const LNKCTL2_TLS: u16 = 0xf;

const _: () = {
    assert!(LNKCAP2_SLS == 0b0111_1110);
    assert!(PORT_RID2SID < PORT_MSIMAP);
    if let Some(e) = &crate::chip::CHIP.eth {
        assert!(e.mmio.is_multiple_of(1 << 20) && e.mmio_size.is_power_of_two());
    }
};

const MS: u64 = 1_000_000;

fn rmw(a: u64, clear: u32, set: u32) {
    let a = Pa(a);
    dev::write32(a, (dev::read32(a) & !clear) | set);
    dev::mb();
}

/// Wacht tot `(read32(a) & mask) == want`, hoogstens `ns` (m1n1 `poll32`).
fn poll(a: u64, mask: u32, want: u32, ns: u64) -> bool {
    dev::poll_until(now, ns, || dev::read32(Pa(a)) & mask == want)
}

/// Meldt de volgende stap en zet een barrière, zodat een uitgestelde abort
/// bij de stap landt die hem veroorzaakte.
fn step(args: core::fmt::Arguments<'_>) {
    dev::mb();
    println!("{args}");
}

/// Waarom de PCIe-kant niet opkwam.
pub type Why = &'static str;

/// Brengt de controller en zijn poorten op. Draait hij al (m1n1 deed het
/// onder de loader), dan blijft alles staan: de bring-up twee keer doen zou
/// een werkende link neerhalen. Geeft de bootregel. `force` is
/// `hopos.apcie` ([`recipe`]).
pub fn init(force: &str) -> Result<&'static str, Why> {
    let e = eth()?;
    let root = dev::read32(Pa(e.ecam));
    if root != u32::MAX && root != 0 {
        return Ok("apcie: already up, left as the boot loader set it");
    }
    let t = fwinfo::adt().ok_or("no device tree")?;
    let chain = t.trace("/arm-io/apcie").ok_or("no /arm-io/apcie")?;
    let node = chain.node();
    let r = recipe(|c| fwinfo::compatible(&t, node, c), force).ok_or(
        "apcie: no recipe for this controller's compatible (m1n1 has none either); hopos.apcie=t8132|t8142 forces one",
    )?;
    let reg = |i: usize| t.reg_at(&chain, i).map_or(0, |r| r.0);
    let (config, rc, phy_ip, axi) = (
        reg(IDX_CONFIG),
        reg(IDX_RC),
        reg(r.idx_phy_ip),
        reg(r.idx_axi),
    );
    let phy_blk = reg(IDX_PHY);
    if config == 0 || rc == 0 || phy_blk == 0 || phy_ip == 0 || axi == 0 {
        return Err("incomplete reg set on /arm-io/apcie");
    }
    let (phy, phy_cmn) = (phy_blk + r.phy_off, phy_blk + PHY_CMN_OFF);
    let ports = t.u32(node, "#ports").unwrap_or(0) as usize;
    let nregs = t.prop(node, "reg").map_or(0, |r| r.len() / 16);
    if ports == 0 || nregs <= r.shared_regs || !(nregs - r.shared_regs).is_multiple_of(ports) {
        return Err("apcie: ports and reg windows do not divide");
    }
    let per_port = (nregs - r.shared_regs) / ports;
    step(format_args!(
        "apcie: recipe {} config {config:#x} rc {rc:#x} phy {phy:#x} phy-ip {phy_ip:#x} axi {axi:#x}, {ports} ports, {per_port} reg/port",
        r.name
    ));

    // 1. Stroom. Zonder dit antwoordt geen register hieronder.
    let gates = Pmgr::open(t).map_or(0, |p| p.power_on(node));
    step(format_args!("apcie: {gates} power gate(s) enabled"));
    let mut tuned = 0;
    let mut apply = |n: Node, prop: &str, base: u64| {
        let c = pmgr::tunables(&t, n, prop, base);
        tuned += c.unwrap_or(0);
        step(format_args!("apcie:   {prop} at {base:#x}: {c:?} line(s)"));
    };

    // 2. De gedeelde blokken.
    apply(node, "apcie-axi2af-tunables", axi);
    dev::write32(Pa(rc + 0x4), 0); // m1n1 zet dit zonder toelichting; wij ook.
    dev::mb();
    apply(node, "apcie-common-tunables", rc);
    apply(node, "apcie-phy-tunables", phy_blk);
    if r.refclk_wait && !poll(phy_cmn, PHYCMN_CLK_100MHZ, PHYCMN_CLK_100MHZ, 250 * MS) {
        return Err("apcie: reference clock not available");
    }

    // 3. De PHY: twee klokken, dan uit reset.
    phy_clocks(phy)?;
    rmw(phy + PHY_CTRL, PHY_RESET_T8132, 0);
    dev::delay(now, MS);
    rmw(phy + 4, 0, 0x01);
    apply(node, "apcie-phy-ip-pll-tunables", phy_ip);
    apply(node, "apcie-phy-ip-auspma-tunables", phy_ip);
    if r.gen4_clear {
        rmw(phy_ip + 0x90, 1 << 16, 0);
    }
    rmw(phy + 4, 0, 0x10);

    // 4. De referentieklok en het root complex.
    rmw(phy_cmn, 0b11, 1);
    if !poll(phy + 0x8, 1, 1, 250 * MS) {
        return Err("apcie: PHY clock did not start");
    }
    rmw(phy + PHY_CTRL, 0, r.phy_run);
    dev::write32(Pa(rc + 0x54), 0x140);
    dev::write32(Pa(rc + 0x50), 0x1);
    dev::mb();
    if !poll(rc + 0x58, 1, 1, 250 * MS) {
        return Err("apcie: root complex did not start");
    }

    // 5. De poorten. Het config-venster van poort N is dat van device N op
    // bus 0, RECHTSTREEKS uit het poortnummer (m1n1 schuift een teller mee
    // die op deze machine, zonder pci-bridge1, poort 2 in de config-space
    // van device 1 schreef; onschadelijk gebleken, niet bedoeld).
    let mut up = 0;
    for port in 0..ports {
        let Some(bridge) = t.child(node, bridge_name(port)) else {
            continue;
        };
        let i = port * per_port + r.shared_regs;
        let (pb, pphy) = (reg(i), reg(i + 2));
        if pb == 0 || pphy == 0 {
            return Err("apcie: a port has no register windows");
        }
        let cfg = config + ((port as u64) << 15);
        step(format_args!(
            "apcie: port {port} base {pb:#x} phy {pphy:#x} config {cfg:#x}"
        ));
        init_port(&t, bridge, pb, pphy, cfg, &mut apply)?;
        up += 1;
    }
    println!(
        "apcie: brought up by HopOS, {gates} power gate(s), {tuned} tunable(s), {up} of {ports} port(s)"
    );
    Ok("apcie: brought up by HopOS")
}

/// De naam van de brug van poort `p` ("pci-bridge0" tot en met 7).
fn bridge_name(p: usize) -> &'static str {
    const NAMES: [&str; 8] = [
        "pci-bridge0",
        "pci-bridge1",
        "pci-bridge2",
        "pci-bridge3",
        "pci-bridge4",
        "pci-bridge5",
        "pci-bridge6",
        "pci-bridge7",
    ];
    NAMES.get(p).copied().unwrap_or("")
}

/// Vraagt de twee PHY-klokken aan en wacht op elke bevestiging.
fn phy_clocks(phy: u64) -> Result<(), Why> {
    rmw(phy + PHY_CTRL, 0, PHY_CLK0REQ);
    if !poll(phy + PHY_CTRL, PHY_CLK0ACK, PHY_CLK0ACK, 50 * MS) {
        return Err("apcie: PHY CLK0 not acknowledged");
    }
    rmw(phy + PHY_CTRL, 0, PHY_CLK1REQ);
    if !poll(phy + PHY_CTRL, PHY_CLK1ACK, PHY_CLK1ACK, 50 * MS) {
        return Err("apcie: PHY CLK1 not acknowledged");
    }
    Ok(())
}

/// De bring-up van één poort, in de volgorde van m1n1's T8122-tak.
fn init_port(
    t: &Adt<'_>,
    bridge: Node,
    pb: u64,
    pphy: u64,
    cfg: u64,
    apply: &mut impl FnMut(Node, &str, u64),
) -> Result<(), Why> {
    const FIRST: [(u64, u32); 13] = [
        (0x088, 0x110),
        (0x100, 0xffff_ffff),
        (0x148, 0xffff_ffff),
        (0x210, 0xffff_ffff),
        (0x080, 0),
        (0x084, 0),
        (0x104, 0xffff_fff0),
        (0x124, 0x100),
        (0x16c, 0),
        (0x13c, 0x10),
        (0x800, 0x10_0100),
        (0x808, 0x10_00ff),
        (0x82c, 0),
    ];
    const THEN: [(u64, u32); 6] = [
        (0x130, 0x300_0000),
        (0x140, 0x10),
        (0x144, 0x25_3770),
        (0x21c, 0),
        (0x834, 0),
        (0x83c, 0),
    ];
    for (off, v) in FIRST {
        dev::write32(Pa(pb + off), v);
    }
    for i in 0..16 {
        dev::write32(Pa(pb + PORT_RID2SID + 4 * i), 0);
    }
    for i in 0..512 {
        dev::write32(Pa(pb + PORT_MSIMAP + 4 * i), 0);
    }
    for (off, v) in THEN {
        dev::write32(Pa(pb + off), v);
    }
    dev::mb();
    apply(bridge, "apcie-config-tunables", pb);
    rmw(pb + PORT_APPCLK, 0, 1);

    rmw(pphy + PHY_CTRL, PHY_CLK0REQ | PHY_CLK1REQ, 0);
    phy_clocks(pphy)?;
    rmw(pphy + PHY_CTRL, 0x10, 0);
    rmw(pphy + PHY_CTRL, 0, 0x200);
    rmw(pphy + PHY_CTRL, 0, 0x400);

    rmw(pb + PORT_RESET_T602X, 0, 1);
    if !poll(pb + PORT_STATUS, 1, 1, 250 * MS) {
        return Err("apcie: port did not come up");
    }
    if !poll(pb + PORT_LINKSTS, 1 << 2, 0, 250 * MS) {
        return Err("apcie: port stayed busy");
    }
    let touch = |what: &str| {
        dev::mb();
        println!(
            "apcie:   {what:<22} LINKSTS {:#x}",
            dev::read32(Pa(pb + PORT_LINKSTS))
        );
    };
    touch("port ready");

    // De DesignWare-kern: alleen-lezen-registers even open, de tunables van
    // de brug erin, snelheid en breedte vast, dicht.
    rmw(cfg + DBI_RO_WR, 0, 1);
    touch("DBI open");
    apply(bridge, "pcie-rc-tunables", cfg);
    apply(bridge, "pcie-rc-gen3-shadow-tunables", cfg);
    apply(bridge, "pcie-rc-gen4-shadow-tunables", cfg);
    touch("rc tunables");
    let speed = max_link_speed(t, bridge);
    if speed > 0 {
        rmw(cfg + CAP + LNKCAP, LNKCAP_SLS, speed);
        rmw(cfg + CAP + LNKCAP2, LNKCAP2_SLS, ((1 << speed) - 1) << 1);
        let a = Pa(cfg + CAP + LNKCTL2);
        dev::write16(
            a,
            (dev::read16(a) & !LNKCTL2_TLS) | (speed as u16 & LNKCTL2_TLS),
        );
        touch("speed");
        rmw(cfg + LINK_WIDTH_CTL, 0, SPEED_CHANGE);
        touch("speed change");
    }
    rmw(cfg + PORT_LINK_CTL, PORT_LINK_MODE, 1 << 16);
    touch("lane mode");
    rmw(cfg + LINK_WIDTH_CTL, LINK_WIDTH, 1 << 8);
    touch("link width");
    rmw(cfg + CAP + LNKCAP, LNKCAP_MLW, 1 << 4);
    touch("LNKCAP width");
    rmw(cfg + DBI_RO_WR, 1, 0);
    touch("DBI closed");
    Ok(())
}

/// De maximale linksnelheid van een brug. Staat er 1, dan mag het eerste
/// kind hem verhogen (`target-link-speed` of `expected-link-speed`, m1n1).
fn max_link_speed(t: &Adt<'_>, bridge: Node) -> u32 {
    let speed = t.u32(bridge, "maximum-link-speed").unwrap_or(0);
    if speed != 1 {
        return speed.min(4);
    }
    t.children(bridge)
        .next()
        .and_then(|c| {
            t.u32(c, "target-link-speed")
                .filter(|&v| v > 0)
                .or_else(|| t.u32(c, "expected-link-speed"))
        })
        .unwrap_or(speed)
        .min(4)
}

/// Zet een GPIO als uitgang op `v` (alleen DATA en MODE).
fn gpio_set(e: &Eth, pin: u64, v: bool) {
    let a = e.gpio + 4 * pin;
    rmw(a, 0xf, (1 << 1) | u32::from(v));
}

/// Brengt de link van de ethernet-poort op: referentieklok, PERST# over de
/// GPIO, de register-PERST, en de training. Tijden uit de ADT
/// (`t-refclk-to-perst`, `perst-to-config`: beide 100 ms).
pub fn link_up(timeout_ns: u64) -> Result<(), Why> {
    let e = eth()?;
    let b = e.port_base;
    if dev::read32(Pa(b + PORT_LINKSTS)) & 1 != 0 {
        return Ok(());
    }
    rmw(b + PORT_APPCLK, 0, 1);
    gpio_set(e, e.perst_pin, false);
    dev::delay(now, 10 * MS);
    rmw(b + PORT_PERST, 0, 1);
    gpio_set(e, e.perst_pin, true);
    dev::delay(now, 100 * MS);
    if dev::read32(Pa(b + PORT_STATUS)) & 1 == 0 {
        return Err("apcie: ethernet port not ready");
    }
    dev::write32(Pa(b + PORT_LTSSMCTL), 1);
    dev::mb();
    if !poll(b + PORT_LINKSTS, 1, 1, timeout_ns) {
        return Err("apcie: link did not come up");
    }
    Ok(())
}

/// Het eerste AIC-nummer van het blok van de ethernet-poort: woord
/// [`eth_port`] van `/arm-io/apcie` "interrupts".
#[must_use]
pub fn port_irq_base() -> Option<u32> {
    let t = fwinfo::adt()?;
    let v = t.prop(t.path("/arm-io/apcie")?, "interrupts")?;
    let at = usize::from(ETH.as_ref()?.port) * 4;
    let w = v.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(w.try_into().ok()?))
}

/// INTSTAT en INTMSK van de ethernet-poort, voor de regel van een lijn die
/// niet aankwam (INTMSK 0xfffffff0 laat INTA tot en met INTD door).
#[must_use]
pub fn port_intx() -> (u32, u32) {
    let Some(e) = ETH.as_ref() else { return (0, 0) };
    let b = e.port_base;
    (
        dev::read32(Pa(b + PORT_INTSTAT)),
        dev::read32(Pa(b + PORT_INTMSK)),
    )
}

/// Alle streams van de ethernet-DART op bypass: welke stream de poort aan
/// de RID van de NIC hangt, is niet gegarandeerd, en bypass op een
/// ongebruikte stream kost niets.
fn dart_bypass(e: &Eth) {
    for sid in 0..DART_STREAMS {
        dev::write32(Pa(e.dart + DART_TCR + 4 * sid), DART_BYPASS);
    }
    dev::mb();
}

/// Koppelt RID `bdf` aan DART-stream 0 op plek `idx`.
fn map_rid(e: &Eth, idx: u64, bdf: Bdf) {
    let v =
        RID2SID_VALID | (u32::from(bdf.bus) << 8) | (u32::from(bdf.dev) << 3) | u32::from(bdf.func);
    dev::write32(Pa(e.port_base + PORT_RID2SID + 4 * idx), v);
    dev::mb();
}

/// De config-space van de APCIe (bussen 0 tot en met de bus achter de
/// ethernet-poort).
fn ecam(e: &Eth) -> Ecam {
    // SAFETY: de ECAM van `/arm-io/apcie` (reg[0], 256 MB), onder 512 GB en
    // dus Device-gemapt (`mmu::build`); na [`init`] antwoordt hij.
    unsafe { Ecam::new(Pa(e.ecam), 0, e.port) }
}

/// Wat de enumeratie opleverde: het endpoint, zijn BAR's en zijn
/// config-space.
pub struct Endpoint {
    /// De functie.
    pub f: Function,
    /// BAR0 (CPU-adres; de tg3).
    pub bar0: u64,
    /// BAR2 (CPU-adres, 0 = geen; de Realtek: BAR0 is daar de I/O-alias).
    pub bar2: u64,
    /// De ECAM-config-space van de functie (4 KB).
    pub cfg: u64,
}

/// Link op, DART in bypass, brug geconfigureerd, BAR's toegewezen: het
/// endpoint achter de ethernet-poort.
pub fn enumerate_nic() -> Result<Endpoint, Why> {
    link_up(500 * MS)?;
    let t = eth()?;
    let (mmio, mmio_size, bus_n) = (t.mmio, t.mmio_size, t.port);
    dart_bypass(t);
    let e = ecam(t);
    let br = Bdf::new(0, t.port, 0).ok_or("bad bridge address")?;
    // Busnummers: primary 0, secondary en subordinate ETH_BUS.
    let bus = e.read32(br, 0x18);
    e.write32(
        br,
        0x18,
        (bus & !0x00ff_ffff) | (u32::from(bus_n) << 8) | (u32::from(bus_n) << 16),
    );
    // Het prefetch-venster (64-bit) en het niet-prefetchbare venster uit.
    let end = mmio + mmio_size - 1;
    e.write32(
        br,
        0x24,
        ((((end >> 20) & 0xfff) as u32) << 20)
            | (1 << 16)
            | ((((mmio >> 20) & 0xfff) as u32) << 4)
            | 1,
    );
    e.write32(br, 0x28, (mmio >> 32) as u32);
    e.write32(br, 0x2c, (end >> 32) as u32);
    e.write32(br, 0x20, 0x0000_fff0);
    dev::mb();
    dev::delay(now, 10 * MS);
    let f = driver_pcie::find(&e, bus_n, |f| !f.is_bridge()).ok_or("link up but no endpoint")?;
    let mut win = MmioWindow::new(mmio, mmio_size);
    let bars = f
        .assign_bars(&e, &mut win)
        .map_err(|_| "BAR assignment failed")?;
    // Alleen memory-BAR's (`assign_bars` laat I/O leeg): de tg3 heeft BAR0,
    // de Realtek BAR2 (zijn BAR0 is de I/O-alias).
    let mem = |i: usize| {
        bars.get(i)
            .and_then(driver_pcie::Bar::mem)
            .map_or(0, |b| b.0)
    };
    let (bar0, bar2) = (mem(0), mem(2));
    if bar0 == 0 && bar2 == 0 {
        return Err("no memory BAR on the endpoint");
    }
    map_rid(t, 0, f.bdf);
    f.enable(&e);
    // De brug: memory-decode en bus-master.
    e.write32(br, 0x04, e.read32(br, 0x04) | 0x6);
    dev::mb();
    let cfg = t.ecam + f.bdf.ecam_offset();
    Ok(Endpoint { f, bar0, bar2, cfg })
}

/// Een property als 32-bit-woorden (LE), hoogstens 48: genoeg voor
/// `function-perst`, `interrupts` en `ranges`, zonder een regel van een
/// kilometer.
struct Words<'a>(&'a [u8]);

impl core::fmt::Display for Words<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, w) in self.0.chunks(4).take(48).enumerate() {
            let mut b = [0u8; 4];
            for (d, s) in b.iter_mut().zip(w) {
                *d = *s;
            }
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{:x}", u32::from_le_bytes(b))?;
        }
        if self.0.len() > 48 * 4 {
            write!(f, " ... ({} bytes)", self.0.len())?;
        }
        Ok(())
    }
}

/// De teksten van een compatible, met `|` ertussen.
struct Compat<'a>(Option<&'a [u8]>);

impl core::fmt::Display for Compat<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Some(v) = self.0 else {
            return f.write_str("-");
        };
        let mut first = true;
        for part in v.split(|&c| c == 0).filter(|p| !p.is_empty()) {
            if !first {
                f.write_str("|")?;
            }
            first = false;
            f.write_str(core::str::from_utf8(part).unwrap_or("?"))?;
        }
        Ok(())
    }
}

/// Wat de ADT over de PCIe van een chip zonder [`ETH`]-tabel zegt: elk
/// getal dat `chip::Eth` en het [`Recipe`] nodig hebben, één keer in de
/// bootlog (`HOPOS_APCIE_SURVEY`). Alleen lezen uit de boom; geen register
/// van de controller wordt aangeraakt. Op de M6 (28-11) is dit de eerste
/// boot: daarna gaan de getallen in `chip.rs`.
pub fn survey() {
    let Some(t) = fwinfo::adt() else {
        println!("apcie: survey: no ADT HOPOS_APCIE_SURVEY");
        return;
    };
    let Some(chain) = t.trace("/arm-io/apcie") else {
        println!("apcie: survey: no /arm-io/apcie HOPOS_APCIE_SURVEY");
        return;
    };
    let n = chain.node();
    let nregs = t.prop(n, "reg").map_or(0, |r| r.len() / 16);
    println!(
        "apcie: survey: compatible {} #ports {} {} reg windows HOPOS_APCIE_SURVEY",
        Compat(t.prop(n, "compatible")),
        t.u32(n, "#ports").unwrap_or(0),
        nregs
    );
    for i in 0..nregs {
        if let Some((a, l)) = t.reg_at(&chain, i) {
            println!("apcie: survey:   reg[{i}] {a:#x}+{l:#x}");
        }
    }
    for p in ["interrupts", "ranges", "#address-cells", "#size-cells"] {
        if let Some(v) = t.prop(n, p) {
            println!("apcie: survey:   {p}: {}", Words(v));
        }
    }
    for port in 0..8 {
        let Some(b) = t.child(n, bridge_name(port)) else {
            continue;
        };
        println!(
            "apcie: survey: {} max-speed {} function-perst [{}]",
            bridge_name(port),
            t.u32(b, "maximum-link-speed").unwrap_or(0),
            Words(t.prop(b, "function-perst").unwrap_or(&[]))
        );
        for c in t.children(b) {
            println!(
                "apcie: survey:   child {} compatible {} mac [{}]",
                t.name(c).unwrap_or("?"),
                Compat(t.prop(c, "compatible")),
                Words(t.prop(c, "local-mac-address").unwrap_or(&[]))
            );
        }
    }
    const DARTS: [&str; 4] = [
        "/arm-io/dart-apcie0",
        "/arm-io/dart-apcie1",
        "/arm-io/dart-apcie2",
        "/arm-io/dart-apcie3",
    ];
    for d in DARTS.into_iter().chain(["/arm-io/gpio0"]) {
        if let (Some(node), Some((a, l))) = (t.path(d), t.reg(d, 0)) {
            println!(
                "apcie: survey: {d} {a:#x}+{l:#x} compatible {}",
                Compat(t.prop(node, "compatible"))
            );
        }
    }
    let mac = t
        .path("/chosen")
        .and_then(|c| t.prop(c, "mac-address-ethernet0"));
    println!(
        "apcie: survey: /chosen mac-address-ethernet0 [{}] HOPOS_APCIE_SURVEY_END",
        Words(mac.unwrap_or(&[]))
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recipe_follows_the_compatible_or_the_knob() {
        let m4 = |c: &str| c == "apcie,t8132";
        let m5 = |c: &str| c == "apcie,t8142";
        let m6 = |c: &str| c == "apcie,t8152";
        assert_eq!(recipe(m4, ""), Some(T8132));
        assert_eq!(recipe(m5, ""), Some(T8142));
        assert_eq!(recipe(m6, ""), None);
        assert_eq!(recipe(m6, "t8142"), Some(T8142));
        assert_eq!(recipe(m6, "nonsense"), None);
        // De M4 blijft wat op 29-08 bewezen is.
        assert_eq!(
            (T8132.shared_regs, T8132.idx_phy_ip, T8132.idx_axi),
            (7, 3, 4)
        );
        assert_eq!((T8132.phy_off, T8132.phy_run), (0x8000, 0x200));
        assert_eq!((T8132.refclk_wait, T8132.gen4_clear), (false, false));
    }

    #[test]
    fn words_and_compat_print_short() {
        let w = [1u8, 0, 0, 0, 0xa5, 0, 0, 0];
        assert_eq!(format!("{}", Words(&w)), "1 a5");
        assert_eq!(
            format!("{}", Compat(Some(b"apcie,t8152\0apcie\0"))),
            "apcie,t8152|apcie"
        );
        assert_eq!(format!("{}", Compat(None)), "-");
    }

    #[test]
    fn bridge_names_and_masks() {
        assert_eq!(bridge_name(2), "pci-bridge2");
        assert_eq!(bridge_name(9), "");
        // De GENMASK(6,1)-les van 29-08.
        assert_eq!(LNKCAP2_SLS & (1 << 7), 0);
        assert_eq!(((1u32 << 3) - 1) << 1, 0b1110);
    }
}
