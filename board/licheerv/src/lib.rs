//! De Sipeed LicheeRV Nano (Sophgo SG2002 / CV181x, XuanTie C906) in machine
//! mode: ns16550, de dwmac met de interne 100M-ePHY, CLINT, PLIC, de
//! T-Head-caches, de DW-watchdog en de temperatuursensor ([`temp`]). Het eerste RISC-V-board van HopOS, en
//! het tweede van v3 na QEMU virt riscv64.
//!
//! Registeradressen uit de vendor-DTS (LicheeRV-Nano-Build, cv181x_riscv en
//! sg2002_licheervnano_sd), in Go geverifieerd op het bordje:
//!
//! ```text
//! PLIC     0x7000_0000
//! CLINT    0x7400_0000   c900, SiFive-indeling, GEEN mtime, GEEN 64-bit MMIO
//! UART0    0x0414_0000   DW-APB-16550, reg-shift 2, door de FSBL op 115200
//! GMAC     0x0407_0000   DWMAC1000 (versie 0x1037), interne ePHY via RMII
//! WDT      0x0301_0000   Synopsys DW-WDT, 25 MHz pclk
//! TEMPSEN  0x030E_0000   de on-die sensor, klokgate bit 9 van CLK_EN0
//! DRAM     0x8000_0000   256 MB
//! RTCCLK   25 MHz        de timebase van de TIME-CSR
//! ```
//!
//! Twee harts, genoemd zoals het resetblok ze noemt (beide lezen `mhartid`
//! 0): hart 0 is de C906B (1 GHz, de firmware-core), hart 1 de C906L
//! (700 MHz). Sinds 03-10 blijft de kern op de C906B, waar de FSBL hem
//! start: alleen de PLIC van dat hart heeft de dwmac ([`GMAC_IRQ`]). Hij
//! deelt zijn core als groep `system` (welcome en wat geen eigen core
//! vindt), en de C906L is het app-hart van Hop (`hopos.hop.sharegroup=hop` in de bordlaag image/cfg/licheerv.cfg). De
//! C906L komt via het resetblok op ([`LicheeRv::start_little`]): reset
//! vast, boot-vector zetten, reset los.
//!
//! Wat hier NIET is: een hardware-TRNG (luid, `cpu::riscv::trng`) en een
//! SD-driver (geen opslag; hopfs draait zonder schijf).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod ephy;
pub mod slots;
pub mod temp;
pub mod watchdog;

use abi::ring::Coherence;
use board::{Board, CoreClass, Dispatched, Error, NoDisk, Plan, Region};
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use cpu::irq::Line;
use cpu::riscv::clint::Clint;
use cpu::riscv::csr;
use cpu::riscv::plic::{Plic, machine_context};
use dev::Pa;
use driver_ns16550::Ns16550;
use driver_stmmac::dwmac1000::{self, Dwmac1000, IrqAck, Probe};
use netdev::AckSlot;
use netdev::Mac;
use sync::Signal;

/// UART0.
pub const UART0: Pa = Pa(0x0414_0000);
/// De CLINT.
pub const CLINT: Pa = Pa(0x7400_0000);
/// De PLIC.
pub const PLIC: Pa = Pa(0x7000_0000);
/// De PLIC-bronnen van de CV181x (`riscv,ndev = <101>`), plus bron 0.
pub const PLIC_SOURCES: u32 = 102;
/// De dwmac.
pub const GMAC: Pa = Pa(0x0407_0000);
/// De PLIC-bron van de dwmac (`macirq`) op de PLIC van de C906B: 31.
/// Linux `cv180x.dtsi` zegt `SOC_PERIPHERAL_IRQ(15)` en `sg2002.dtsi` maakt
/// daar `15 + 16` van, de vendor-DTS (`cv181x_base_riscv.dtsi`) zegt 31
/// direct, en de TRM (sophgo-doc, de tabel "Master RISCV C906 @ 1.0Ghz")
/// noemt 31 en 32 Ethnet0 (32 is de LPI-lijn). De PLIC van de C906L heeft
/// GEEN ethernetbron: in de TRM-tabel "Slave RISCV C906 @ 700Mhz" staat er
/// geen (daar is 31 UART1), en de vendor-FreeRTOS voor dat hart zet
/// `ETH0_SBD_INTR_O` op NA (`hal/cv181x/config/intr_conf.h`).
pub const GMAC_IRQ: u32 = 31;
/// De DW-watchdog.
pub const WDT: Pa = Pa(0x0301_0000);
/// De timebase: de vaste 25 MHz-osc, exact 40 ns per tik.
pub const TIMEBASE_HZ: u64 = 25_000_000;

/// De kern-RAM: image, stack en heap (`link-riscv.ld` met de basis en maat
/// van dit board uit build.rs), tot de DMA-regio.
pub const KERN_RAM: Region = Region {
    base: Pa(0x8400_0000),
    size: 0x0280_0000,
};
/// De DMA-regio: 1 MB achter de kern-RAM (Go had 8 MB; de rest is sinds
/// 02-10 de staging van de kern-flip, `slots::STAGE_PA`). Op de C906 GECACHET:
/// in machine mode is er geen MMU en bepaalt de sysmap de attributen. De
/// dwmac doet daarom cache-onderhoud per overdracht (dev::push/pull met
/// `thead`), en elke descriptor en buffer staat op een eigen regel (de les
/// van 30-07).
pub const DMA: Region = Region {
    base: Pa(0x8680_0000),
    size: 0x0010_0000,
};
/// De NIC-helft: wat de dwmac vraagt, ruim.
pub const NET_DMA: Region = Region {
    base: Pa(0x8680_0000),
    size: 0x0008_0000,
};

const _: () = {
    assert!(KERN_RAM.end().0 == DMA.base.0);
    assert!(dwmac1000::NEED_BYTES <= NET_DMA.size);
    assert!(DMA.end().0 == slots::STAGE_PA);
};

/// Het resetblok van de C906L: SOFT_CPU_RSTN, bit 6.
const C906L_RESET: Pa = Pa(0x0300_3024);
const RESET_BIT: u32 = 1 << 6;
/// SEC_SYS: bit 13 is de override van de boot-vector.
const SEC_SYS_CTRL: Pa = Pa(0x020B_0004);
const SEC_SYS_VEC_LO: Pa = Pa(0x020B_0020);
const SEC_SYS_VEC_HI: Pa = Pa(0x020B_0024);

/// De C906L, het hart met het resetblok.
pub const HART_LITTLE: usize = 1;
/// De C906B, de firmware-core: geen resetblok, parkeert.
pub const HART_BIG: usize = 0;

// SAFETY: de DW-APB-16550 van de SG2002 met 32-bit-stride; in machine mode
// altijd bereikbaar, en de FSBL zette hem op 115200.
static UART: Ns16550 = unsafe { Ns16550::new(UART0, 2) };
// SAFETY: de PLIC van de CV181x. De controller van `cpu::irq`.
static PLIC_DEV: Plic = unsafe { Plic::new(PLIC, PLIC_SOURCES) };
// SAFETY: de c900-CLINT, SiFive-indeling (msip en mtimecmp; geen mtime).
const CLINT_DEV: Clint = unsafe { Clint::new(CLINT) };

static CLINT_OK: AtomicBool = AtomicBool::new(false);
/// De bel van de NIC: de dispatch luidt hem, de RX-pomp wacht erop.
static NIC_BELL: Signal = Signal::new();
/// De ack van de NIC-lijn, gezet door `probe_nic`, gelezen door de
/// dispatch-taak. Beide draaien op de executor van de kern.
static NIC_ACK: AckSlot<IrqAck> = AckSlot::new();

/// De device-ack van de NIC-lijn bij de dispatcher: masker dicht en status
/// gewist (de level-lijn valt). De driver zet het masker weer open als de
/// pomp de ring leeg las.
fn nic_ack() {
    NIC_ACK.ack();
}
/// Leeft er een NIC uit `probe_nic`? Gezet na een gelukte probe (één per
/// boot).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);

fn console_write(b: &[u8]) {
    UART.write_bytes(b);
}

/// Wacht `us` microseconden op de TIME-CSR.
fn wait_us(us: u64) {
    dev::delay(cpu::riscv::idle::now, us.saturating_mul(1000));
}

/// De LicheeRV Nano als board.
pub struct LicheeRv;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = LicheeRv;

impl LicheeRv {
    /// Het board.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// De index van dit hart in zijn CLINT: altijd 0, want de CLINT is per
    /// core en beide cores noemen zichzelf hart 0 (gemeten 01-08, boot 8).
    #[must_use]
    pub const fn clint_hart(&self) -> usize {
        0
    }

    /// De CLINT van dit board.
    #[must_use]
    pub const fn clint(&self) -> Clint {
        CLINT_DEV
    }

    /// Wat de kooi van app-hart `hart` moet weten (Go,
    /// board/licheerv/hop/hart.go `HartTimer`), voor beide harts:
    ///
    /// - de CLINT is per core en beide cores noemen zichzelf hart 0
    ///   (gemeten 01-08, boot 8): er is GEEN bel tussen de harts, en de
    ///   comparator van elk hart is voor hemzelf `mtimecmp(0)`;
    /// - de kill-tick op die comparator: hij schrijft alleen de comparator
    ///   en vuurt terwijl een bewoner draait, nooit een `wfi` (Go, hart.go:
    ///   "wat stierf was een wfi"). Hij is de tijdschijf van het gedeelde
    ///   hart (alle apps wonen op één hart) en op de C906B, zonder
    ///   resetblok, het enige mes;
    /// - slapen: "alle stille doden staan op naam van de C906L" (01-08, de
    ///   wfi-klasse), en de C906B droeg in Go twee weken productie-`wfi`.
    ///   De switcher spint op allebei (slaapgrens 0) tot een soak op dít
    ///   silicium met déze switcher het anders bewijst (docs/boards-riscv.md);
    /// - het resetblok alleen op de C906L (reset vast wist ook zijn PMP,
    ///   gemeten 30-07); de C906B wordt nooit gereset (geen boot-vector-
    ///   override, zie Go hart.go).
    #[must_use]
    pub fn app_hart(&self, hart: usize) -> cpu::riscv::switch::AppHart {
        cpu::riscv::switch::AppHart {
            mtimecmp: CLINT_DEV.mtimecmp(0),
            msip: Pa(0),
            // Geen bel naar de kern: de CLINT is per core (de mailbox van de
            // CV181x is de kandidaat, op ijzer te bewijzen).
            kick: Pa(0),
            sleep_cap: 0,
            tick: cpu::riscv::idle::ns_to_ticks(cpu::riscv::switch::KILL_TICK_NS, TIMEBASE_HZ),
            attrs: cpu::riscv::sv39::Attrs::Thead,
            pmp: cpu::riscv::pmp::C906,
            resettable: hart == HART_LITTLE,
        }
    }

    /// Brengt app-hart `hart` naar de parkeerlus van de boot-stub: de C906L
    /// uit reset op de reset-ingang, met zijn logische hart-id erbij (zijn
    /// `mhartid` leest 0, net als dat van de C906B).
    pub fn start_app_hart(&self, hart: usize) {
        if hart == HART_LITTLE {
            cpu::riscv::boot::set_reset_hart(hart);
            self.start_little(cpu::riscv::boot::reset_pc());
        }
    }

    /// Zet app-hart `hart` in reset; `true` als dat kon (alleen de C906L).
    pub fn hold_app_hart(&self, hart: usize) -> bool {
        if hart == HART_LITTLE {
            self.hold_little();
        }
        hart == HART_LITTLE
    }

    /// Start de C906L op `entry` via het resetblok: reset vast, de
    /// boot-vector-override aan, de vector, reset los. Een reset-start kent
    /// geen argument; het hart begint kaal (caches uit, T-Head-regime nul),
    /// en de boot-stub zet het regime (cpu/riscv/boot.rs, `thead`).
    pub fn start_little(&self, entry: u64) {
        dev::write32(C906L_RESET, dev::read32(C906L_RESET) & !RESET_BIT);
        dev::write32(SEC_SYS_CTRL, dev::read32(SEC_SYS_CTRL) | 1 << 13);
        dev::write32(SEC_SYS_VEC_LO, entry as u32);
        dev::write32(SEC_SYS_VEC_HI, (entry >> 32) as u32);
        dev::write32(C906L_RESET, dev::read32(C906L_RESET) | RESET_BIT);
    }

    /// Zet de C906L in reset: de harde intrekking op dit hart (het wist ook
    /// zijn PMP, gemeten 30-07: na een hart-reset leest pmpcfg0 weer 0).
    pub fn hold_little(&self) {
        dev::write32(C906L_RESET, dev::read32(C906L_RESET) & !RESET_BIT);
    }

    /// De watchdog-probe (Go, `WatchdogProbe`): CCVR aanraken (de lees die
    /// bus-fault als het blok dood is), TORR schrijven en teruglezen. TORR is
    /// inert zolang CR.enable uit staat, en de waarde is exact wat het
    /// wapenen er straks toch in zet. Vanaf het hart van de kern is dit een
    /// gok die de node kan kosten (een bus-fout overleeft de kern niet),
    /// daarom gaat de regel ervoor naar de console: stilte daarna wijst de
    /// dader aan.
    pub fn watchdog_probe(&self) -> bool {
        const TOP: u32 = 15; // 2^31 cycli op 25 MHz, ~86 s
        cpu::println!(
            "watchdog: probing the DW-WDT at {:#x} (silence after this line = the block is not there)",
            WDT.0
        );
        let _ = dev::read32(WDT.add(0x08));
        dev::write32(WDT.add(0x04), TOP | TOP << 4);
        dev::read32(WDT.add(0x04)) == TOP | TOP << 4
    }

    /// Het MAC-adres uit `hopos.mac`, anders afgeleid van `hopos.node`
    /// (`net::nodemac`), allebei uit het config-venster; zonder beide het
    /// ingebouwde adres, luid.
    fn node_mac() -> Mac {
        let (m, src) = net::nodemac::identity(
            LicheeRv.boot_param("hopos.mac"),
            LicheeRv.boot_param("hopos.node"),
        );
        if src == net::nodemac::Source::Fallback {
            cpu::println!(
                "net: WARNING no hopos.mac and no hopos.node: the built-in MAC; a second LicheeRV on this LAN will collide HOPOS_MAC_FIXED"
            );
        }
        Mac(m)
    }
}

impl Default for LicheeRv {
    fn default() -> Self {
        Self::new()
    }
}

/// De DW-WDT van de SG2002 (`watchdog`): wapent alleen als de probe van de
/// boot antwoordde, en gaat daarna niet meer uit.
impl board::Watchdog for LicheeRv {
    type Armed = watchdog::Desc;

    fn arm(&self, timeout_ms: u64) -> Result<watchdog::Desc, &'static str> {
        watchdog::arm(timeout_ms)
    }

    fn pet(&self) {
        watchdog::pet();
    }
}

/// De TEMPSEN van de SoC (`temp`, Go `temp.go`).
impl board::Thermal for LicheeRv {
    /// Brengt de sensor op (10 ms busy-wait, één keer bij de boot) en meldt
    /// de eerste lezing.
    fn open_thermal(&self) {
        temp::open();
    }

    /// 0 = geen geldige code.
    fn temp_milli_c(&self) -> i32 {
        temp::temp_milli_c().unwrap_or(0)
    }
}

/// Geen knop: de klok blijft waar de FSBL hem liet.
impl board::ClockKnob for LicheeRv {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;
}

impl Board for LicheeRv {
    type Nic = Dwmac1000;
    type Sleeper = cpu::riscv::idle::RvSleeper;
    /// Geen: er is nog geen SD-driver.
    type Disk = NoDisk;

    const NAME: &'static str = "licheerv";
    const PSCI: bool = false;
    /// De harts van de C906 zijn niet coherent met elkaar; de host-ringen
    /// zijn beide de kern, op zijn eigen hart.
    const SLOT_RINGS: Coherence = Coherence::Maintained;
    const HOST_RINGS: Coherence = Coherence::Hardware;
    /// 256 MB, pool 200 MB: Hop meet er 0,5 tot 0,9 MB (03-10, slot 1 als
    /// systeemtaak), en elke MB is hier een app-MB.
    const HOP_MEM: u64 = 10 << 20;

    fn console(&self) -> fn(&[u8]) {
        console_write
    }

    fn privilege(&self, el: u8) -> Result<(), Error> {
        if el == 3 {
            Ok(())
        } else {
            Err(Error::Privilege { el })
        }
    }

    fn firmware(&self) -> &'static str {
        "boot: LicheeRV Nano (SG2002), machine mode monitor from the FIP (no SBI), the kern on hart 0 (C906B), app hart 1 (C906L)"
    }

    fn discover(&self, _dtb: u64) {
        cpu::riscv::idle::set_hz(TIMEBASE_HZ);
        match CLINT_DEV.probe(self.clint_hart(), csr::rdtime()) {
            Ok(()) => {
                CLINT_OK.store(true, Relaxed);
                cpu::println!("board: CLINT: mtimecmp writable, the kern sleeps on it (wfi)");
            }
            Err(e) => cpu::println!(
                "board: CLINT: {e}, hart sleep stays disabled, the kern polls HOPOS_CLINT_FAIL"
            ),
        }
        cpu::println!("{}", cpu::riscv::trng::WARNING);
        let ok = self.watchdog_probe();
        watchdog::probed(ok);
        cpu::println!(
            "watchdog: DW-WDT {}",
            if ok {
                "answers, the watchdog task may arm it"
            } else {
                "readback differs, it stays unarmed HOPOS_WD_PROBE_FAIL"
            }
        );
        match board::cfgwin::text().len() {
            0 => cpu::println!(
                "config: no hopos.cfg in the window in the image (CFG= of image/licheerv-agent.sh, or hop image), defaults HOPOS_CFG_NONE"
            ),
            n => cpu::println!(
                "config: hopos.cfg from the window in the image, {n} bytes HOPOS_CFG_UP"
            ),
        }
    }

    fn clock(&self) -> executor::Clock {
        cpu::riscv::idle::now
    }

    /// De slaap van de kern op de C906B: de `wfi` op de wekker (twee weken
    /// productie in Go, 30-07 tot 16-08), hooguit 2 ms: de wekker van de
    /// SG2002 wekt niet betrouwbaar (`cpu::riscv::idle::WFI_CAP_NS`).
    fn sleeper(&self) -> Self::Sleeper {
        let clint = CLINT_OK.load(Relaxed).then_some(CLINT_DEV);
        cpu::riscv::idle::RvSleeper::new(clint, self.clint_hart())
            .with_cap(cpu::riscv::idle::WFI_CAP_NS)
    }

    fn mem_total(&self) -> u64 {
        256 << 20
    }

    fn cores(&self) -> usize {
        2
    }

    fn core_class(&self, core: usize) -> CoreClass {
        if core == HART_LITTLE {
            CoreClass::Small
        } else {
            CoreClass::Big
        }
    }

    fn plan(&self) -> Plan {
        Plan {
            kern_ram: KERN_RAM,
            dma: DMA,
            net_dma: NET_DMA,
        }
    }

    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        // De PLIC is, net als de CLINT, per core en elke core is voor
        // zichzelf hart 0: context 0 is de zijne (context 2 vanaf de C906L
        // was een load access fault op het claim-register, 03-10, mtval
        // 0x7020_2004). De 102 bronnen zijn die van de C906B, het hart van
        // de kern.
        PLIC_DEV.set_context(machine_context(self.clint_hart()));
        cpu::irq::use_controller(&PLIC_DEV);
        cpu::println!("irq: {}", PLIC_DEV.describe());
        csr::mie_set(csr::MIP_MEIP | csr::MIP_MSIP);
        csr::restore(csr::MSTATUS_MIE);
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        CLINT_DEV.set_msip(self.clint_hart(), false);
        // De NIC heeft een lijn; niemand anders: wat verder vuurt, gaat uit.
        let pass = cpu::irq::global().dispatch();
        csr::mie_set(csr::MIP_MEIP | csr::MIP_MSIP);
        let nic = pass.claims(Line(GMAC_IRQ));
        Dispatched {
            timer: 0,
            nic,
            other: pass.claimed.saturating_sub(nic),
        }
    }

    /// De ethernet-keten (Go, board/licheerv/hop/net.go): klokken, leeft de
    /// MAC, de ePHY aan, PHY-scan, de start van de autonegotiatie, DMA. De
    /// probe wacht niet op de link: dat doet de taak (`netdev::Device::link`),
    /// en dan gaat de MAC aan op de snelheid. Elke stap meldt zich met het
    /// getal erbij: een boot-cyclus is hier duur (kaart eruit, in de Mac,
    /// terug).
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        if !ephy::clocks_on() {
            ephy::clocks_enable();
            wait_us(1_000);
            if !ephy::clocks_on() {
                return Err(Error::Nic("ethernet clock gates stay closed"));
            }
        }
        // SAFETY: GMAC is de dwmac van de SG2002 met open klokgates.
        let mut probe = unsafe { Probe::new(GMAC, dwmac1000::CSR_250_300M, cpu::riscv::idle::now) };
        let v = probe
            .check()
            .map_err(|_| Error::Nic("no MAC at 0x04070000 (version reads 0 or all-ones)"))?;
        ephy::init(wait_us);
        wait_us(50_000);
        let Some(phy) = driver_mdio::scan(&mut probe) else {
            return Err(Error::Nic(
                "no PHY on the MDIO bus (ePHY init did not take)",
            ));
        };
        probe
            .start_link(phy.addr)
            .map_err(|_| Error::Nic("MDIO stuck starting autoneg"))?;
        cpu::println!(
            "net: dwmac version {v:#x}, PHY {} id {:04x}:{:04x}, link pending (autoneg)",
            phy.addr,
            phy.id1,
            phy.id2
        );
        // SAFETY: NET_DMA is van deze driver alleen, lijn-gealigneerd en
        // onder 4 GB; de driver doet het cache-onderhoud (dev met `thead`).
        let mut nic =
            unsafe { probe.start(NET_DMA.base, NET_DMA.size, Self::node_mac(), phy.addr, None) }
                .map_err(|_| Error::Nic("dwmac start failed"))?;
        // De lijn op de PLIC van de C906B ([`GMAC_IRQ`]); wil hij niet aan,
        // dan is dat een fout (docs/sequence-hopos.md §14.1 O3). De kern
        // hoort een app op de C906L na elke slaap van hooguit 2 ms
        // (`cpu::riscv::idle::WFI_CAP_NS`): er is geen bel van de C906L naar
        // de C906B.
        NIC_ACK.set(nic.irq_ack());
        cpu::irq::enable(Line(GMAC_IRQ), Some(nic_ack), Some(&NIC_BELL))
            .map_err(|_| Error::Irq("dwmac source refused by the PLIC"))?;
        nic.set_irq(&NIC_BELL);
        cpu::println!("net: dwmac irq {GMAC_IRQ} on the PLIC (RX and TX done) HOPOS_NIC_IRQ");
        cpu::println!("net: dwmac {}", nic.diag());
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// Het hart waar de kern draait: de C906B, waar de FSBL hem start.
    /// `mhartid` zegt het niet (beide cores lezen 0).
    fn this_core(&self) -> usize {
        HART_BIG
    }

    /// De OS-core: het hart van de kern. Een `hopos.oscore` is er niet.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        (self.this_core(), None)
    }

    /// De bel van de arm64-rotatie: een stub, zie `board-qemuvirt-riscv`.
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell {
            sgi1r: 0,
            sgir: 0,
            intid: 0,
            pending: || 1023,
        }
    }

    /// De kick naar dit hart zelf.
    fn kick_self(&self) {
        CLINT_DEV.set_msip(self.clint_hart(), true);
    }

    /// Geen schijf: er is (nog) geen SD-driver.
    fn probe_disk(&self) -> Result<Option<NoDisk>, Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_mode() {
        let b = LicheeRv::new();
        assert_eq!(b.core_class(1), CoreClass::Small);
        assert_eq!(b.core_class(0), CoreClass::Big);
        assert!(b.privilege(3).is_ok());
    }

    #[test]
    fn both_harts_tick_on_their_own_comparator_and_spin() {
        let b = LicheeRv::new();
        for hart in [HART_BIG, HART_LITTLE] {
            let t = b.app_hart(hart);
            // De eigen comparator (index 0, de CLINT is per core), 10 ms op
            // 25 MHz, geen bel en geen slaap: de tick wel, de `wfi` niet.
            assert_eq!(t.mtimecmp, Pa(0x7400_4000));
            assert_eq!(t.tick, 250_000);
            assert_eq!(t.sleep_cap, 0);
            assert_eq!(t.msip, Pa(0));
            assert_eq!(t.kick, Pa(0));
        }
        // Alleen de C906L heeft het resetblok.
        assert!(b.app_hart(HART_LITTLE).resettable);
        assert!(!b.app_hart(HART_BIG).resettable);
        assert_eq!(b.clint_hart(), 0);
    }
}
