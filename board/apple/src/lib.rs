//! De Mac mini M4 (Apple t8132, target J773g, Mac16,10): 6 E-cores
//! "sawtooth" in cluster 0 en 4 P-cores "everest" in cluster 1, 24 GB
//! LPDDR5 vanaf 1 TiB, één Broadcom 57762 achter een Apple-PCIe-rootpoort,
//! de SSD achter de ANS-coprocessor. Geen PSCI, geen GIC, geen device tree
//! van de fabrikant: iBoot levert boot_args in x0 en een Apple Device Tree
//! (ADT), en m1n1 (Asahi) is tot de installatie de laag die ons afzet.
//!
//! Wat dit crate bezit, en waar:
//!
//! - de voorkant van het image ([`head`]): de relocatie-stub op offset 0x800
//!   (`kmutil --entry-point 2048`), de brievenbus op offset 0 (RVBAR), de
//!   ingang `_start_apple` met het VHE-regime en de 48-bit-map ([`mmu`]);
//! - de console ([`console`]): de dockchannel en uart0;
//! - de feiten van de firmware ([`fwinfo`]): boot_args, de ADT, de cores met
//!   hun soort, het MAC, het serienummer, en wat alleen de m1n1-loader weet;
//! - de interrupts ([`irq`]): de AIC als controller van `cpu::irq`, de
//!   timer-FIQ, de fast IPI;
//! - de cores ([`cores`]): de CPU_ON van dit board (m1n1's spin-table, of
//!   PMGR plus de brievenbus als wij het bootobject zijn);
//! - de PCIe ([`pcie`], [`pmgr`]): de controller zelf opbrengen, de link, de
//!   DART, en dan de tg3;
//! - de opslag en de thermometer ([`storage`]): de ANS-NVMe achter RTKit met
//!   het schrijfvenster uit de GPT, en de SMC;
//! - de watchdogs en de klok van de clusters ([`wdt`]);
//! - het slot-plan ([`slots`]).
//!
//! De Go-voorganger is `OLD/metal/board/apple` (plus `hop/`), en
//! `OLD/docs/v1/archief/apple-m4.md` is het dossier met elke meting.
//!
//! Wat de kern voor dit board draagt (29-09): de EL2-smaak `AppleVhe` in de
//! kooi-lijm en de OS-core-rotatie met de fast IPI als kick
//! (`cpu::el2::Bell::apple`), en de CPU_ON-haak van `cpu::smp`
//! ([`cores::cpu_on_mpidr`], er is geen PSCI). Wat er op ijzer nog bewezen
//! moet worden, staat als checklist in `docs/boards-apple.md`.

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

pub mod console;
pub mod cores;
pub mod fwinfo;
mod head;
mod irq;
mod mmu;
pub mod pcie;
pub mod pmgr;
pub mod slots;
pub mod storage;
pub mod wdt;

use abi::ring::Coherence;
use board::{Board, CoreClass, Dispatched, Error, Plan, Region};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use cpu::el2::Flavor;
use cpu::println;
use dev::Pa;
use driver_tg3::Tg3;
use netdev::Mac;
use sync::Signal;

/// De DRAM-basis van élk Apple-silicon-systeem sinds de M1 (GEMETEN 28-08:
/// 0x100_0000_0000).
pub const DRAM_BASE: u64 = 0x100_0000_0000;
/// Het venster van de kern: 4 GB boven de DRAM-basis, want daaronder woont
/// de boot-keten (boot_args en de ADT, m1n1 met zijn heap op ~DRAM + 56 MB)
/// en de bovenkant is van iBoot (carve-outs, de framebuffer). Het image is
/// hierop gelinkt (`hopos/link-apple.ld`).
pub const RAM_BASE: u64 = DRAM_BASE + 0x1_0000_0000;
/// De scratch van de stub (waar hij vandaan kwam, x0, de brievenbus).
pub const SCRATCH: u64 = RAM_BASE + 0xE000;
/// Het param-blok van de m1n1-loader.
pub const PARAMS: u64 = RAM_BASE + 0xE100;

/// De kern-RAM: image, BSS (met de tabellen), stack en heap. Normal WB.
pub const KERN_RAM: Region = Region {
    base: Pa(RAM_BASE),
    size: 0x1000_0000,
};
/// De DMA-regio: 16 MB, Normal-NC (GEMETEN 03-09: Device kost ~290 ns per
/// 8-byte-load; NC is coherent met de tg3 en de ANS zonder onderhoud),
/// behalve de twee datablokken die hun driver zelf veegt: `NET_BUF` en
/// het datablok van de ANS (`storage::ANS_DATA`), allebei Normal WB.
pub const DMA: Region = Region {
    base: Pa(RAM_BASE + 0x1000_0000),
    size: 0x0100_0000,
};
/// De NIC-helft van de DMA-regio (de tg3 vraagt 4 MB).
pub const NET_DMA: Region = Region {
    base: Pa(RAM_BASE + 0x1000_0000),
    size: 0x0080_0000,
};
/// Het bufferblok van de tg3 binnen [`NET_DMA`]: Normal WB, de ringen en
/// het status-blok ervoor blijven NC. Een bewuste keuze voor dit board, van
/// de Go-kern overgenomen (board/apple/hop/net.go, 03-09: "Blijft het NC,
/// dan werkt alles nog, alleen op ~100MB/s aan ongecachete loads"). Het
/// mag omdat de driver het onderhoud zelf doet: `dev::push` vóór de
/// TX-descriptor, `dev::pull` na de RX-completion (`driver_tg3::BUF_OFF`).
pub(crate) const NET_BUF: (u64, u64) = (NET_DMA.base.0 + driver_tg3::BUF_OFF, driver_tg3::BUF_LEN);
/// De opslag-helft: de queues en TCB's van de ANS, de databuffer, en de
/// buffers die de coprocessor bij zijn opstart vraagt.
pub const BLK_DMA: Region = Region {
    base: Pa(RAM_BASE + 0x1080_0000),
    size: 0x0080_0000,
};
/// Het kooi-venster: 16 MB Device (control-pages, kooien, de flip-recorder
/// en de zwarte doos).
pub const ADMIN: Region = Region {
    base: Pa(RAM_BASE + 0x1100_0000),
    size: 0x0100_0000,
};
/// De loader-regio: boot-scratch en staging, 64 MB Normal.
pub const LOADER: Region = Region {
    base: Pa(RAM_BASE + 0x1200_0000),
    size: 0x0400_0000,
};
/// Het einde van het venster; daarboven begint de bovenste pool-regio.
pub const WINDOW_END: u64 = RAM_BASE + 0x1600_0000;

const _: () = {
    assert!(KERN_RAM.end().0 == DMA.base.0 && DMA.end().0 == ADMIN.base.0);
    assert!(NET_DMA.end().0 == BLK_DMA.base.0 && BLK_DMA.end().0 == DMA.end().0);
    assert!(ADMIN.end().0 == LOADER.base.0 && LOADER.end().0 == WINDOW_END);
    assert!(RAM_BASE.is_multiple_of(2 << 20) && WINDOW_END.is_multiple_of(2 << 20));
    assert!(driver_tg3::DMA_NEED <= NET_DMA.size);
    // Het WB-blok op eigen 2 MB-grenzen (de korrel van de map), binnen de
    // NIC-helft en na de ringen.
    assert!(NET_BUF.0.is_multiple_of(2 << 20) && NET_BUF.1 == 2 << 20);
    assert!(NET_BUF.0 > NET_DMA.base.0 && NET_BUF.0 + NET_BUF.1 <= NET_DMA.end().0);
    assert!(PARAMS == SCRATCH + 0x100);
};

/// De EL2-smaak van dit silicium: E2H is RES1 (VHE-only, GEMETEN 28-08) en
/// WFE slaapt er op een core die wij opbrachten niet (CYC_OVRD vergrendeld),
/// dus de switcher slaapt in WFI en wekt met de fast IPI (GEMETEN 02-09).
/// De kooi-lijm van de binary moet déze smaak installeren, niet Nvhe.
pub const FLAVOR: Flavor = Flavor::AppleVhe;

/// Leeft er een NIC uit `probe_nic`? Gezet na een gelukte probe (één per
/// boot).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);

/// De Mac mini M4.
pub struct Apple;

/// Dit board onder de naam die de kern-binary kiest (`vboard::Machine`).
pub type Machine = Apple;

impl Apple {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// De temperatuur van de die in milli-°C via de SMC; `None` = onbekend
    /// (alleen met `hopos.smc=1`). Elke aanroep praat de SMC wakker en weer
    /// in slaap (tot seconden, RTKit): niet voor een lus.
    #[must_use]
    pub fn smc_temp_milli_c(&self) -> Option<i32> {
        storage::temp_milli_c(self.boot_param("hopos.smc") == "1")
    }

    /// De temperatuur in de bootlog: één meting met `hopos.smc=1`, anders
    /// een regel waarom niet. Niet standaard: onder de Go-kern kwam het
    /// INITIALIZE-antwoord van de SMC nooit (31-08), en een half opgestarte
    /// RTKit-coprocessor die niemand meer pollt loopt vol (driver_smc
    /// `open`). Dat is op ijzer niet uitgesloten, dus blijft het één
    /// bewuste knop per installatie.
    fn report_temp(&self) {
        if self.boot_param("hopos.smc") != "1" {
            println!(
                "smc: no die temperature (hopos.smc=1 measures once at boot; the SMC answer is unproven on metal, 31-08) HOPOS_APPLE_SMC_OFF"
            );
            return;
        }
        match self.smc_temp_milli_c() {
            Some(t) => println!(
                "smc: die {}.{} C at boot HOPOS_APPLE_TEMP",
                t / 1000,
                (t % 1000).abs() / 100
            ),
            None => println!("smc: no die temperature from the SMC HOPOS_APPLE_SMC_FAIL"),
        }
    }

    /// De firmware-bootregels na `discover`.
    fn report(&self, x0: u64, args: Option<fw::xnuboot::Args>) {
        match args {
            Some(a) => println!(
                "xnuboot: rev {}.{} RAM {:#x}+{} MB (physical {} MB), firmware up to {:#x}, ADT {:#x}+{:#x}, framebuffer {:#x} {}x{} HOPOS_APPLE_BOOTARGS",
                a.revision,
                a.version,
                a.phys_base,
                a.mem_size >> 20,
                a.mem_size_actual >> 20,
                a.top_of_kernel_data,
                a.adt,
                a.adt_size,
                a.fb.base,
                a.fb.width,
                a.fb.height
            ),
            None => println!(
                "xnuboot: no boot_args at x0 {x0:#x}: no ADT, no cores, no devices HOPOS_APPLE_NO_BOOTARGS"
            ),
        }
        let Some(t) = fwinfo::adt() else { return };
        let p = (0..fwinfo::cpus())
            .filter(|&i| fwinfo::is_p_core(i))
            .count();
        println!(
            "adt: {} bytes, {} ({}), {} cores ({} E + {p} P), this is core {}, serial {}",
            t.size(),
            t.str(fw::adt::Node::ROOT, "model").unwrap_or("?"),
            t.str(fw::adt::Node::ROOT, "target-type").unwrap_or("?"),
            fwinfo::cpus(),
            fwinfo::cpus() - p,
            self.this_core(),
            fwinfo::serial().unwrap_or("?")
        );
        if let Some((d, _)) = fwinfo::reg("/arm-io/dockchannel-uart", 0)
            && d != console::DOCKCHANNEL
        {
            println!(
                "console: the ADT puts the dockchannel at {d:#x}, not {:#x} HOPOS_APPLE_CONSOLE_MISMATCH",
                console::DOCKCHANNEL
            );
        }
        match cores::own_cores(0) {
            Ok(()) => println!("cores: ours, RVBAR points at this image (PMGR + mailbox)"),
            Err(why) if fwinfo::has_params() => {
                println!("cores: via m1n1's spin-table ({why})");
            }
            Err(why) => println!("cores: NOT ours: {why}"),
        }
    }
}

impl Default for Apple {
    fn default() -> Self {
        Self::new()
    }
}

/// De primaire watchdog van `/arm-io/wdt` (`wdt`), dezelfde die `discover`
/// bij de boot stil zette (iBoot laat er meer dan één gewapend achter;
/// natief resette de node zonder dat op 1:43, 31-08).
impl board::Watchdog for Apple {
    type Armed = wdt::Desc;

    /// 30 s (Go `wdtTimeout`): de ANS en de SMC blokkeren bij hun opstart
    /// tot seconden (RTKit `POWER_TIMEOUT_NS`).
    const WD_TIMEOUT_MS: u64 = wdt::WDT_TIMEOUT_MS;

    fn arm(&self, timeout_ms: u64) -> Result<wdt::Desc, &'static str> {
        wdt::arm(timeout_ms)
    }

    fn pet(&self) {
        wdt::pet();
    }

    fn disarm(&self) -> bool {
        wdt::off()
    }
}

/// De thermometer is de SMC; die praat bij elke meting een
/// RTKit-coprocessor wakker en weer in slaap (tot seconden), dus hij meet
/// één keer bij de boot (`hopos.smc=1`, in de bootlog) en niet op de tik:
/// een oude waarde op de heartbeat van Hop zou liegen, dus hier 0 (`-`).
impl board::Thermal for Apple {}

/// De klok regelt het silicium zelf (de APSC, die `discover` aanzette onder
/// het plafond van `hopos.pstate`); in plaats van een knop meldt de wachter
/// elke sprong (Go `PStateWatch`, `wdt`).
impl board::ClockKnob for Apple {
    type Knob = board::NoKnob;
    type KnobError = board::NoKnob;

    fn no_knob(&self, exec: &'static executor::Executor) {
        let w = wdt::PStateWatch::new();
        if w.is_empty() {
            println!("dvfs: no cluster blocks in the ADT, the boot clock stays HOPOS_CLOCK_NONE");
            return;
        }
        println!(
            "dvfs: the APSC governs under the hopos.pstate ceiling; watching the p-states every {} s HOPOS_APPLE_PSTATE_WATCH",
            PSTATE_WATCH_EVERY.as_secs()
        );
        if let Err(e) = exec.spawn(pstate_watch(exec, w)) {
            println!("dvfs: p-state watch not spawned ({e:?})");
        }
    }
}

/// De cadans van de wachter op de p-states (Go: twee seconden). Een poll
/// met een reden: docs/description.md §4.11.
const PSTATE_WATCH_EVERY: core::time::Duration = core::time::Duration::from_secs(2);

/// De wachter: alleen-lezen, één regel per sprong.
async fn pstate_watch(exec: &'static executor::Executor, mut w: wdt::PStateWatch) {
    loop {
        w.poll();
        exec.after(PSTATE_WATCH_EVERY).await;
    }
}

impl Board for Apple {
    type Nic = Tg3;
    type Sleeper = cpu::idle::ArmSleeper;
    /// De ANS-NVMe.
    type Disk = storage::Disk;

    const NAME: &'static str = "apple-m4";
    const FLAVOR: Flavor = FLAVOR;
    const PSCI: bool = false;
    /// De kern mapt de pool Device (`mmu`, `dram_attr`); de ringen van een
    /// slot krijgen `Hardware` zodra de staart Normal gemapt is
    /// ([`Board::map_tail_normal`]).
    const SLOT_RINGS: Coherence = Coherence::Maintained;
    /// De heap is Normal write-back: het onderhoud kostte elk frame van de
    /// node-stack een veeg per cacheline (GEMETEN 01-10 op M10: 930 MB/s
    /// tegen 4300 app naar app).
    const HOST_RINGS: Coherence = Coherence::Hardware;
    /// WFE op EL1 slaapt op de M4 niet (board.go `IdleMode`, 02-09).
    const APP_IDLE_YIELD: bool = true;

    fn console(&self) -> fn(&[u8]) {
        console::write
    }

    fn firmware(&self) -> &'static str {
        "boot: iBoot or m1n1 (boot_args in x0, ADT), no PSCI, VHE-only EL2, 48-bit identity map"
    }

    /// Leest boot_args en de ADT (x0), zet de watchdogs van de firmware
    /// stil (ALLEREERST: natief reset de node anders op 1:43, 31-08), zet
    /// de CPU_ON van dit board in `cpu::smp` (vóór de verhuizing naar de
    /// OS-core), en zet de clusters op hun klok.
    fn discover(&self, dtb: u64) {
        serror_check("the boot, before any driver");
        let x0 = if dtb != 0 {
            dtb
        } else {
            head::FIRMWARE_X0.load(Relaxed)
        };
        let args = fwinfo::load(x0);
        println!("watchdog: {}", wdt::quiet().as_str());
        serror_check("silencing the firmware watchdogs");
        self.report(x0, args);
        serror_check("reading the ADT and the boot-args");
        cpu::smp::set_cpu_on(cores::cpu_on_mpidr);
        let cfg = self.config();
        match fwinfo::config_source() {
            _ if !board::cfgwin::text().is_empty() => println!(
                "cfg: hopos.cfg from the window in the image, {} bytes HOPOS_CFG",
                cfg.len()
            ),
            fwinfo::CfgSource::Image => println!(
                "cfg: hopos.cfg from the 0xF000 window (an image from before the config window, or carried over a flip), {} bytes HOPOS_CFG",
                cfg.len()
            ),
            fwinfo::CfgSource::Loader => println!(
                "cfg: hopos.cfg from the loader, {} bytes HOPOS_CFG",
                cfg.len()
            ),
            fwinfo::CfgSource::None => println!(
                "cfg: no hopos.cfg (an empty window: CFG= of image/apple-m4.sh or hop image; no loader) HOPOS_CFG_NONE"
            ),
        }
        self.report_temp();
        serror_check("the temperature probe (SMC)");
    }

    fn clock(&self) -> executor::Clock {
        cpu::idle::now
    }

    /// WFI op de timer als de timer-FIQ deze core bereikt (gemeten, niet
    /// aangenomen: `irq::timer_wakes`), anders WFE op de event-stream
    /// (CNTFRQ 1 GHz: met FEAT_ECV kiest `cpu::idle` EVNTI 11 met EVNTIS,
    /// 1,048 ms, 954 wekken/s, GEMETEN 29-08).
    fn sleeper(&self) -> Self::Sleeper {
        let (fired, wakes) = irq::timer_wakes();
        let mode = if wakes {
            cpu::idle::Mode::Wfi
        } else {
            cpu::idle::Mode::Wfe
        };
        println!(
            "idle: timer fired={fired} fiq-at-core={wakes}, {} Hz, sleeping in {mode:?} HOPOS_APPLE_IDLE",
            cpu::idle::freq()
        );
        // Onder E2H = 1 schrijft `cntkctl_el1` vanaf EL2 CNTHCTL_EL2; sinds
        // 29-09 vervangt `cpu::idle` daar alleen de stream-bits, dus de
        // timertoegang van de bewoners (EL1PCTEN/EL1PTEN, de ingang in
        // head.rs) blijft staan en hoeft het board niets terug te zetten.
        cpu::idle::ArmSleeper::new(mode)
    }

    fn mem_total(&self) -> u64 {
        fwinfo::ram().3
    }

    fn cores(&self) -> usize {
        fwinfo::cpus().max(1)
    }

    /// E-cores ("sawtooth", `cluster-type` E) zijn small, P-cores
    /// ("everest") big.
    fn core_class(&self, core: usize) -> CoreClass {
        if fwinfo::is_p_core(core) || fwinfo::cpus() == 0 {
            CoreClass::Big
        } else {
            CoreClass::Small
        }
    }

    fn plan(&self) -> Plan {
        Plan {
            kern_ram: KERN_RAM,
            dma: DMA,
            net_dma: NET_DMA,
        }
    }

    /// Eerst de klok van de clusters (`hopos.pstate`): hier en niet in
    /// `discover`, want pas op de OS-core komen de regels op 5555, en de
    /// tune is precies wat op ijzer bewezen moet worden (01-10).
    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        let ps = self.boot_param("hopos.pstate");
        match wdt::pstate_targets(ps) {
            Some(t) => wdt::pstate_tune(t, false, &serror_check),
            None => wdt::pstate_tune(wdt::PS_DEFAULT, true, &serror_check),
        }
        let target = irq::start().map_err(Error::Irq)?;
        serror_check("the AIC bring-up");
        if let Some(stage) = first_serror() {
            println!(
                "apple: the first SError of this boot was pending after {stage} HOPOS_APPLE_SERROR_FIRST"
            );
        }
        println!(
            "irq: {} (target {target} reaches this core), fast IPI and timer FIQ",
            irq::AIC.describe()
        );
        arch::unmask();
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        let r = irq::dispatch();
        Dispatched {
            timer: r.timer,
            nic: r.lines,
            other: r.unknown.saturating_add(r.ipi),
        }
    }

    /// De hele keten: de PCIe-controller, de link, de DART, de brug en het
    /// endpoint, en dan de tg3 met het MAC uit de ADT (na een PERST draagt
    /// de chip alleen nog Broadcom's default). Daarna de lijn: INTA van de
    /// poort op de AIC ([`wire_nic`]); zonder lijn een fout.
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        serror_check("the disk and everything before the NIC");
        let how = pcie::init().map_err(Error::Nic)?;
        println!("{how}");
        serror_check("the apcie bring-up");
        let ep = match pcie::enumerate_nic() {
            Ok(ep) => ep,
            Err(why) => {
                println!("net: {why}");
                return Ok(None);
            }
        };
        println!("apcie: link up, endpoint {} bar0 {:#x}", ep.f, ep.bar0);
        serror_check("the link and the enumeration");
        if !driver_tg3::drives(ep.f.vendor, ep.f.device) {
            println!("net: {} is not the tg3 we know", ep.f);
            return Ok(None);
        }
        let mac = fwinfo::nic_mac().ok_or(Error::Nic("no local-mac-address in the ADT"))?;
        // SAFETY: BAR0 is net toegewezen uit het 64-bit venster (onder 512 GB,
        // Device-gemapt) en `cfg` is de ECAM-config-space van deze functie;
        // NET_DMA is van deze driver alleen, Normal-NC met het bufferblok
        // Normal WB (`NET_BUF`, de driver veegt het zelf), en de DART staat
        // in bypass, dus een DMA-adres is een fysiek adres.
        let mut nic = unsafe {
            Tg3::new(
                Pa(ep.bar0),
                Pa(ep.cfg),
                Mac(mac),
                NET_DMA.base,
                NET_DMA.size,
                cpu::idle::now,
            )
        }
        .map_err(|e| {
            println!("tg3: {e} HOPOS_TG3_FAIL");
            Error::Nic("tg3 init failed")
        })?;
        println!("tg3: {}", nic.describe());
        serror_check("the tg3 init");
        match nic.link_up(8_000_000_000) {
            Ok(l) => println!(
                "tg3: LINK UP, {} Mb/s {} duplex",
                l.mbps,
                if l.full_duplex { "full" } else { "half" }
            ),
            Err(e) => {
                println!("tg3: {e}, cable plugged in? HOPOS_TG3_NOLINK");
                return Err(Error::Nic("no link"));
            }
        }
        wire_nic(self.boot_param("hopos.nicirq"), &mut nic)?;
        NIC_CLAIMED.store(true, Relaxed);
        Ok(Some(nic))
    }

    /// De fysieke index (ADT-volgorde) van de core waar dit draait.
    fn this_core(&self) -> usize {
        slots::core_of(cpu::mpidr())
    }

    /// De OS-core die `hopos.cfg` vraagt (`hopos.oscore=<small|mid|big|N>`),
    /// met een reden als de kern er niet heen kan. De firmware levert ons af
    /// op een P-core (cpu 6); `small` is de zuinige core die HopOS hoort te
    /// bewonen (Go: "de hop", 29-08). De verhuizing start die core via
    /// `cpu::smp::start_one`, en dat is sinds 29-09 de CPU_ON van dit board
    /// ([`cores::cpu_on_mpidr`], gezet in `discover`).
    ///
    /// Let op (29-08): een core die de kern zelf opbracht, krijgt op t8132
    /// geen timer-FIQ. De kern draait daar dan in WFE (`sleeper` meet het
    /// opnieuw) en de voorproef van de kooien zakt op de timer-beurt
    /// (`HOPOS_APPLE_PREFLIGHT_FAIL`): de verhuizing kost de kooien, luid.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        board::os_core(
            self.boot_param("hopos.oscore"),
            self.cores(),
            |c| self.core_class(c),
            self.this_core(),
        )
    }

    /// De kick van de OS-core voor de rotatie van `cpu::el2`: de fast IPI
    /// naar de core waar de kern draait (IPI_RR_GLOBAL, geackt via IPI_SR op
    /// EL2), want Apple heeft geen GIC-SGI.
    fn os_bell(&self) -> cpu::el2::Bell {
        cpu::el2::Bell::apple(cpu::mpidr())
    }

    /// De fast IPI naar deze core zelf: de zelftest van het IPI-pad.
    fn kick_self(&self) {
        cores::kick(cpu::mpidr());
    }

    /// `hopos.cfg`: het venster in het image (`board::cfgwin`, gevuld door
    /// `image/apple-m4.sh` of `hop image`), anders wat op 0xF000 staat (de
    /// tekst van de m1n1-loader, of het venster van een kern van vóór
    /// `board::cfgwin` dat een flip meenam); "" als er niets is.
    fn config(&self) -> &'static str {
        board::cfgwin::or(fwinfo::config_text())
    }

    /// De schijf: de ANS-NVMe, met het schrijfvenster uit de GPT.
    fn probe_disk(&self) -> Result<Option<storage::Disk>, Error> {
        storage::probe_disk(self.boot_param("hopos.disk") == "off")
    }

    fn map_tail_normal(&self, pa: u64, size: u64) -> Option<Result<(), board::TailError>> {
        Some(map_tail_normal(pa, size).map_err(board::TailError::Refused))
    }

    /// Daarnaast de plek 0xF000 (de loader, of het venster van een kern van
    /// vóór `board::cfgwin`), zoals voorheen.
    fn flip_carry_config(&self, src: u64, len: u64) -> bool {
        fwinfo::carry_config(src, len)
    }
}

/// De interrupt van de tg3 (`hopos.nicirq`: leeg of `auto`, of een
/// AIC-nummer), en één bootregel over de keuze (`HOPOS_NIC_IRQ`). INTx, geen
/// MSI: de doorbell van de Apple-poort is onbekend, en MSI aanzetten zonder
/// haar nam op 18-09 de node mee (Go bundel 18). Auto is het blok van de
/// poort uit de ADT plus INTA (`pcie::INTA`, gemeten 19-09: 1249 + 4 =
/// 1253). Komt de lijn niet aan, dan is dat een fout: de pomp pollt niet
/// (docs/sequence-hopos.md §14.1 O3).
fn wire_nic(v: &str, nic: &mut Tg3) -> Result<(), Error> {
    let base = pcie::port_irq_base();
    let line = match irq::NicIrq::parse(v) {
        Some(irq::NicIrq::Line(l)) => Ok(l),
        Some(irq::NicIrq::Auto) => base
            .and_then(|b| b.checked_add(pcie::INTA))
            .ok_or("no interrupts word for the port on /arm-io/apcie"),
        None => Err("hopos.nicirq is not auto or an AIC number"),
    };
    let base = base.unwrap_or(0);
    let l = line.map_err(|why| {
        println!("net: tg3 has no line: {why} (hopos.nicirq={v:?}) HOPOS_NIC_IRQ");
        Error::Irq("the tg3 line")
    })?;
    match irq::wire_nic(nic, l) {
        Ok(us) => {
            println!(
                "net: tg3 INTx on AIC irq {l} (port {} block {base} + INTA {}), first interrupt after {us} us HOPOS_NIC_IRQ",
                pcie::ETH_PORT,
                pcie::INTA
            );
            Ok(())
        }
        Err((why, (st, msk))) => {
            println!(
                "net: tg3 has no line, AIC irq {l} (port {} block {base}): {why}, port INTSTAT {st:#x} INTMSK {msk:#x} HOPOS_NIC_IRQ",
                pcie::ETH_PORT
            );
            Err(Error::Irq("the tg3 line did not come up"))
        }
    }
}

/// De eerste stap waarna een SError pending stond (adres en lengte van de
/// stapnaam, 0 = nog geen). De stappen van `discover` draaien vóór de ring
/// van 5555 ze ziet (hun regels gaan alleen naar de dockchannel), dus de
/// bring-up van de AIC zegt het nog eens (`HOPOS_APPLE_SERROR_FIRST`).
static FIRST_SERROR: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// De stap uit [`FIRST_SERROR`], of `None`.
fn first_serror() -> Option<&'static str> {
    let (p, n) = (FIRST_SERROR[0].load(Relaxed), FIRST_SERROR[1].load(Relaxed));
    if p == 0 {
        return None;
    }
    // SAFETY: `p` en `n` komen samen uit één `&'static str` in
    // [`serror_check`] (de boot-core schrijft ze, vóór er een tweede core
    // draait), dus de bytes leven en zijn UTF-8.
    Some(unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, n)) })
}

/// De ABI-staart van een slot Normal write-back in de kernmap (Go:
/// `mapTailNormal` in `kern/slots`, slot-ABI 7): de pool is op dit silicium
/// Device, en zonder dit loopt elke ringkopie van de kern per 8 bytes
/// vluchtig (de M4 01-10: app naar app 52 MB/s tegen 461 op de Pi 4). Weigert
/// buiten 2 MB-grenzen of in het kernvenster; dan blijft de staart Device:
/// traag maar correct, en de kooi houdt dan haar ringonderhoud.
pub fn map_tail_normal(pa: u64, size: u64) -> Result<(), &'static str> {
    mmu::remap_normal(pa, size)
}

/// Neemt een SError op die na `stage` pending staat en meldt hem; `true`
/// als er een was. Op dit silicium is een verboden schrijf stil en landt
/// de abort later als SError; op EL2 blijft hij achter PSTATE.A tot de
/// eerste beurt op EL1 (de M4 onder kmutil, 30-09: de voorproef zag ESR
/// 0xbe000000, vector 11, drie keer). Zo staat er bij welke stap hij hoort,
/// en start de voorproef schoon.
pub(crate) fn serror_check(stage: &'static str) -> bool {
    let Some((esr, elr, far)) = cpu::vectors::serror_drain() else {
        return false;
    };
    if FIRST_SERROR[0].load(Relaxed) == 0 {
        FIRST_SERROR[1].store(stage.len(), Relaxed);
        FIRST_SERROR[0].store(stage.as_ptr() as usize, Relaxed);
    }
    // De L2C-foutregisters van dit silicium (m1n1 `exception.c`:
    // SYS_IMP_APL_L2C_ERR_STS/ADR/INF): de ADR is het adres van de schrijf
    // die stil misging. m1n1 schrijft de STS terug om hem te wissen; op de
    // t8132 is die `msr` op EL2 UNDEFINED (sync-exception EC 0x0 precies op
    // de msr, 01-10), dus wij lezen alleen. Een SError die blijft hangen,
    // komt bij de volgende drain opnieuw langs, met dezelfde ADR: dat is dan
    // de bron die weg moet.
    let (sts, adr, inf) = arch::l2c_err();
    println!(
        "apple: SError pending after {stage}: esr={esr:#x} elr={elr:#x} far={far:#x} l2c sts={sts:#x} adr={adr:#x} inf={inf:#x} HOPOS_APPLE_SERROR"
    );
    true
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod arch {
    use core::arch::asm;

    /// HCR_EL2 zoals hij nu staat (de teruglezing van Go's el2Apple).
    pub(super) fn hcr() -> u64 {
        let v: u64;
        // SAFETY: HCR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, hcr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// CNTHCTL_EL2 zoals hij nu staat.
    pub(super) fn cnthctl() -> u64 {
        let v: u64;
        // SAFETY: CNTHCTL_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, cnthctl_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// CPTR_EL2 zoals hij nu staat.
    pub(super) fn cptr() -> u64 {
        let v: u64;
        // SAFETY: CPTR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, cptr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// SCTLR_EL2 zoals hij nu staat.
    pub(super) fn sctlr_el2() -> u64 {
        let v: u64;
        // SAFETY: SCTLR_EL2 lezen heeft geen neveneffect.
        unsafe { asm!("mrs {}, sctlr_el2", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// De L2C-foutregisters: STS (s3_3_c15_c8_0), ADR (s3_3_c15_c9_0) en
    /// INF (s3_3_c15_c10_0), zoals m1n1 ze bij elke exception drukt.
    pub(super) fn l2c_err() -> (u64, u64, u64) {
        let (sts, adr, inf): (u64, u64, u64);
        // SAFETY: lezen van de drie foutregisters heeft geen neveneffect;
        // m1n1 leest ze op elke Apple-SoC, ook de t8132 (het archief van
        // 29-08 toont L2C_ERR_ADR op deze M4).
        unsafe {
            asm!(
                "mrs {s}, s3_3_c15_c8_0",
                "mrs {a}, s3_3_c15_c9_0",
                "mrs {i}, s3_3_c15_c10_0",
                s = out(reg) sts, a = out(reg) adr, i = out(reg) inf,
                options(nomem, nostack)
            );
        }
        (sts, adr, inf)
    }

    pub(super) fn unmask() {
        // SAFETY: opent I en F op deze core; de vectoren staan (boot).
        unsafe { asm!("msr daifclr, #3", options(nomem, nostack)) };
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod arch {
    //! Host-stubs.
    pub(super) fn hcr() -> u64 {
        0
    }
    pub(super) fn cnthctl() -> u64 {
        0
    }
    pub(super) fn cptr() -> u64 {
        0
    }
    pub(super) fn sctlr_el2() -> u64 {
        0
    }
    pub(super) fn l2c_err() -> (u64, u64, u64) {
        (0, 0, 0)
    }
    pub(super) fn unmask() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_tree_one_big_core() {
        // Zonder boom: één core, big. (`os_core` leest het param-blok op
        // zijn vaste adres: dat is ijzer, geen host.)
        let a = Apple::new();
        assert_eq!(a.cores(), 1);
        assert_eq!(a.core_class(0), CoreClass::Big);
    }
}
