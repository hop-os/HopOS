//! Het gedeelde Pi-deel (BCM2711 = Pi 4, BCM2712 = Pi 5): de DTB-boot, de
//! geheugenkaart uit de DTB, het plan, de cmdline-config, de
//! VideoCore-mailbox, de GIC-400, de RNG200 als bron van de DRBG, de
//! PM-watchdog, de klokknop via de mailbox en de [`Board`]-implementatie.
//!
//! De specificatie is `OLD/metal/board/raspi` en de `hop`-helften van
//! `rpi4` en `rpi5`. Wat per SoC verschilt (de adressen, de
//! MPIDR-nummering, de NIC en de vaste tabellen) levert het board-crate via
//! de trait [`Soc`]; de rest staat één keer, hier, omdat twee kopieën
//! precies op de naden uit elkaar groeien waar drift stil misgaat (Go
//! 18-07: PSCI, timers, lease).
//!
//! Dit bezit de statics van de boot (DTB, DRAM, cores, pool, staging, de
//! mailbox) en de bedrading. De drivers kennen geen adres.

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

pub mod arch;
pub mod cfg;
// De klokknop van het klokbeleid: de ARM-klok via de mailbox (clock.rs).
pub mod clock;
pub mod map;
// De RNG200 als entropiebron van de kern (rng.rs).
pub mod rng;
pub mod slots;
// De USB-invoer van de Pi's (usb.rs): het DMA-stuk, de VL805-handshake.
pub mod usb;
// De PM-watchdog (watchdog.rs), voor de watchdog-taak van de kern.
pub mod watchdog;
// De framebuffer via de VideoCore: alleen in de gui-smaak (docs/gui.md);
// kaal een stub met dezelfde signatuur (handboek §7).
#[cfg(feature = "gui")]
mod vcfb;
#[cfg(not(feature = "gui"))]
mod vcfb {
    //! Kaal gebouwd: geen framebuffer.
    use core::cell::RefCell;
    use driver_vcmail::Mbox;

    /// Altijd headless.
    pub(crate) fn framebuffer(
        _mbox: &RefCell<Option<Mbox>>,
        _tables: Option<crate::map::Tables>,
    ) -> Option<driver_fb::Desc> {
        None
    }
}

#[cfg(test)]
mod tests;

use abi::Region as AbiRegion;
use board::{Board, CoreClass, Dispatched, Error, NoDisk, Plan, Region};
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use dev::Pa;
use driver_gicv2::Gic;
use driver_pl011::Pl011;
use driver_vcmail::Mbox;
use fw::fdt::Fdt;
use sync::{LocalCell, Signal};

pub use driver_gicv2;
pub use driver_vcmail;

/// De kern-RAM als board-regio.
pub const KERN_RAM: Region = Region {
    base: Pa(map::KERN_BASE),
    size: map::KERN_END - map::KERN_BASE,
};

/// De DMA-regio als board-regio (Normal non-cacheable).
pub const DMA: Region = Region {
    base: Pa(map::DMA.base),
    size: map::DMA.size,
};

/// Het NIC-contract van een SoC: wat het board-crate van de wiring
/// meekrijgt.
pub struct NicCtx {
    /// De NIC-helft van de DMA-regio (Normal non-cacheable).
    pub dma: AbiRegion,
    /// Het MAC-adres: van de firmware (OTP), anders uit het serienummer.
    pub mac: [u8; 6],
    /// Monotone nanoseconden.
    pub now: fn() -> u64,
}

/// Wat een SoC levert: de adressen, de nummering, de NIC en de tabellen.
pub trait Soc: 'static {
    /// De NIC van deze SoC.
    type Nic: netdev::Device;
    /// De boardnaam voor de bootlog ("rpi4").
    const NAME: &'static str;
    /// De SoC ("BCM2711").
    const SOC: &'static str;
    /// Het MAC-terugvalbyte als het serienummer onleesbaar is (Go: 0x04 op
    /// de Pi 4, 0x05 op de Pi 5).
    const MAC_FALLBACK: u8;

    /// De console-UART.
    fn uart() -> &'static Pl011;
    /// De GIC-400.
    fn gic() -> &'static Gic;
    /// De VideoCore-mailbox.
    const VCMAIL: Pa;
    /// De RNG200 (DT `brcm,bcm2711-rng200`), Device-gemapt in de vaste
    /// tabel.
    const RNG200: Pa;
    /// Het PM-blok met de watchdog (DT `watchdog@...`), Device-gemapt in de
    /// vaste tabel.
    const PM: Pa;
    /// Het MPIDR-target van logische core `core`.
    fn mpidr(core: usize) -> u64;
    /// De logische core bij een MPIDR: de inverse van [`mpidr`](Soc::mpidr).
    fn core_of(mpidr: u64) -> usize;
    /// De vaste tabellen van dit board.
    fn tables() -> Option<map::Tables>;
    /// Vindt en initialiseert de NIC. `Ok(None)` = geen NIC.
    fn probe_nic(ctx: &NicCtx) -> Result<Option<Self::Nic>, Error>;
    /// De USB-hostcontrollers, met hun PCIe-link en firmware-handshake
    /// gedaan (usb.rs). Alleen gevraagd in de gui-smaak; standaard geen.
    fn usb_hosts(ctx: &usb::UsbCtx) -> board::UsbHosts {
        let _ = ctx;
        board::UsbHosts::new()
    }
    /// Eén diagnose van de NIC voor de tik ([`Board::nic_diag`]).
    fn nic_diag() {}
}

/// De DTB van deze boot.
static DTB: board::dtb::Dtb = board::dtb::Dtb::new();
/// Het bij boot gevonden DRAM (bytes, 0 = onbekend).
static MEM_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Het aantal cores uit de DTB (of `hopos.cores`), 0 = onbekend.
static CORES: AtomicUsize = AtomicUsize::new(0);
/// Leeft er een NIC uit `probe_nic`? Pas gezet na een gelukte probe: een
/// mislukte (geen link) liet niets achter en mag opnieuw (hopos `nic_retry`).
static NIC_CLAIMED: AtomicBool = AtomicBool::new(false);
/// De mailbox: één eigenaar, de executor van core 0.
static MBOX: LocalCell<Option<Mbox>> = LocalCell::cell(None);
/// Het MAC-adres dat `discover` koos.
static MAC: AtomicU64 = AtomicU64::new(0);

/// Zonder DTB: vier cores (elke Pi 4 en Pi 5).
const CORES_DEFAULT: usize = 4;

/// Waar de DTB mag liggen: de vast gemapte Normal-RAM, van de kern-RAM tot
/// het eind van het laadvenster.
const DTB_RAM: board::Region = board::Region {
    base: Pa(map::KERN_BASE),
    size: map::LOADER.base + map::LOADER.size - map::KERN_BASE,
};

/// De DTB van deze boot.
#[must_use]
pub fn fdt() -> Option<Fdt<'static>> {
    DTB.fdt()
}

/// Staat het device met deze `compatible` aan in de DTB? `None` = geen DTB
/// of geen zo'n node ([`Fdt::enabled`]).
#[must_use]
pub fn device_enabled(compatible: &str) -> Option<bool> {
    fdt()?.enabled(compatible)
}

/// De Pi als board, geparametriseerd met zijn SoC.
pub struct Raspi<S: Soc> {
    _soc: PhantomData<fn() -> S>,
}

impl<S: Soc> Raspi<S> {
    /// Het board. Alle staat staat in statics; dit is het handvat.
    #[must_use]
    pub const fn new() -> Self {
        Self { _soc: PhantomData }
    }

    /// De mailbox op, en wat de firmware over zichzelf zegt.
    fn mailbox(&self) {
        // SAFETY: VCMAIL is het mailbox-blok van deze SoC (Device-gemapt in
        // de vaste tabel), VCMAIL_BUF ligt in de DMA-regio (Normal-NC, onder
        // 1 GB) en is van niemand anders.
        let mut m = unsafe { Mbox::new(S::VCMAIL, Pa(map::VCMAIL_BUF), cpu::idle::now) };
        let temp = m.temp();
        let arm = m.clock_rate(driver_vcmail::CLOCK_ARM);
        let max = m.max_clock_rate(driver_vcmail::CLOCK_ARM);
        match (temp, arm, max) {
            (Ok(t), Ok(a), Ok(x)) => cpu::println!(
                "vcmail: {}.{:03} C, ARM {} MHz (max {} MHz)",
                t / 1000,
                t % 1000,
                a / 1_000_000,
                x / 1_000_000
            ),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                cpu::println!("vcmail: {e} HOPOS_VCMAIL_FAIL");
            }
        }
        let mac = match m.board_mac() {
            Ok(mac) if mac != [0; 6] => mac,
            _ => cfg::mac_from_serial(
                fdt().and_then(|f| f.root_string("serial-number")),
                S::MAC_FALLBACK,
            ),
        };
        MAC.store(cfg::mac_word(mac), Relaxed);
        *MBOX.borrow_mut() = Some(m);
    }

    /// De SoC-blokken zonder eigen trait-methode: de PM-watchdog klaar voor
    /// de watchdog-taak, en de DRBG van de kern gezaaid uit de RNG200.
    fn hardware(&self) {
        watchdog::set_base(S::PM);
        rng::seed::<S>(device_enabled(rng::COMPATIBLE));
    }

    /// De rest van DRAM in de kaart, de pool eruit, de staging erbij.
    fn memory(&self, f: &Fdt<'static>, dtb: u64) {
        let Ok(mut regs) = f.mem_regions() else {
            cpu::println!("WARNING HOPOS_POOL_FALLBACK: no /memory in the DTB, no pool");
            return;
        };
        let Ok(n) = abi::layout::coalesce(regs.as_mut_slice()) else {
            cpu::println!("WARNING HOPOS_POOL_FALLBACK: DTB /memory wraps, no pool");
            return;
        };
        regs.truncate(n);
        let Some(t) = S::tables() else { return };
        let mapped = map::plan_ram(regs.as_slice(), &t, normal_block, dev::write64);
        arch::tables_changed();
        let stage = self.stage(f);
        let reserve = f.mem_reserve();
        let holes = map::holes(
            reserve.as_slice(),
            AbiRegion::new(dtb, f.size() as u64),
            stage,
        );
        match abi::layout::carve_pool(mapped.as_slice(), holes.as_slice(), map::MB2) {
            Ok(pool) => {
                let total: u64 = pool.iter().map(|r| r.size).sum();
                cpu::println!(
                    "mem: {} banks, {} MB mapped from the DTB, pool {} MB in {} regions",
                    regs.len(),
                    mapped.iter().map(|r| r.size).sum::<u64>() >> 20,
                    total >> 20,
                    pool.len()
                );
                *slots::POOL.borrow_mut() = pool;
            }
            Err(e) => cpu::println!("WARNING HOPOS_POOL_FALLBACK: {e}, no pool"),
        }
    }

    /// De staging uit /chosen/linux,initrd-* en de rol uit de cmdline.
    fn stage(&self, f: &Fdt<'static>) -> AbiRegion {
        let role = board::stage::role_code(self.boot_param("hopos.stage"));
        slots::ROLE.store(role, Relaxed);
        let Some((start, end)) = f.initrd() else {
            cpu::println!("stage: no initramfs in the DTB, nothing staged");
            return AbiRegion::default();
        };
        let Some((start, len)) = slots::check_stage(start, end) else {
            cpu::println!(
                "stage: initramfs {start:#x}..{end:#x} outside the loader window, ignored HOPOS_STAGE_REFUSED"
            );
            return AbiRegion::default();
        };
        slots::STAGE[0].store(start, Relaxed);
        slots::STAGE[1].store(len, Relaxed);
        cpu::println!(
            "stage: {} KB at {start:#x}, role {}",
            len >> 10,
            match role {
                0 => "app",
                1 => "hop",
                _ => "unknown (nothing will be placed)",
            }
        );
        AbiRegion::new(start, len)
    }
}

impl<S: Soc> Default for Raspi<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// De PPI van de EL2-fysieke timer (CNTHP).
pub const HYP_TIMER_PPI: u32 = 26;

/// De kick van de OS-core: SGI 8, gestuurd door de EL2-switcher van een
/// app-core die de kern nodig heeft terwijl die geen SEV hoort (PORT.md
/// beslissing 2).
///
/// Dezelfde 8 als QEMU virt, niet de 7 van Rockchip of de 1 van UEFI: daar
/// houdt TF-A SGI 8..15 als Secure Group 1, en een niet-beveiligde schrijf
/// is RAZ/WI. De Pi's hebben die beperking niet: de BL31 van de Pi 4 en de
/// Pi 5 (plat/rpi) kent geen beveiligde interrupts, `gicv2_pcpu_distif_init`
/// zet alle SGI's en PPI's dus in Group 1, en de VideoCore-firmware gebruikt
/// geen SGI (de secundaire cores wachten in een spin-table op WFE). 0..7
/// laten we vrij zoals op virt: Linux-achtige gasten rekenen erop. Op ijzer
/// NOG NIET GEMETEN (docs/boards-pi.md).
pub const KICK_SGI: u32 = 8;

/// De peek van de OS-core: GICC_HPPIR van de GIC-400 van deze SoC.
fn hppir<S: Soc>() -> u32 {
    S::gic().hppir()
}

/// Een Normal-WB-blok (1 GB of 2 MB, de beschrijving is dezelfde).
fn normal_block(pa: u64) -> u64 {
    cpu::boot::block(pa, cpu::boot::ATTR_NORMAL)
}

fn console_write<S: Soc>(b: &[u8]) {
    S::uart().write(b);
}

fn console_nowait<S: Soc>(b: &[u8]) -> usize {
    S::uart().write_nowait(b)
}

/// De PM-watchdog van de BCM-familie (`watchdog`): wapent alleen als zijn
/// teller loopt.
impl<S: Soc> board::Watchdog for Raspi<S> {
    type Armed = watchdog::Desc;

    fn arm(&self, timeout_ms: u64) -> Result<watchdog::Desc, &'static str> {
        watchdog::arm(timeout_ms)
    }

    fn pet(&self) {
        watchdog::pet();
    }

    /// Vóór een eigen `arm` telt de teller van de vorige kern door (de
    /// Pi 5, 30-09: gereset na 12 s tijdens 5 s framebuffer-geduld): dus
    /// ook een herlaad van wat die wapende.
    fn pet_now(&self) {
        watchdog::pet();
        watchdog::reload_if_armed(Self::WD_TIMEOUT_MS);
    }

    fn disarm(&self) -> bool {
        watchdog::off()
    }
}

/// De SoC-temperatuur via de VideoCore-mailbox (dezelfde tag als de
/// bootregel `vcmail: 58.713 C`).
impl<S: Soc> board::Thermal for Raspi<S> {
    /// 0 als de mailbox niet open is of de firmware niet antwoordde.
    fn temp_milli_c(&self) -> i32 {
        MBOX.borrow_mut()
            .as_mut()
            .and_then(|m| m.temp().ok())
            .map_or(0, |t| i32::try_from(t).unwrap_or(0))
    }
}

/// De ARM-klok via de mailbox (`clock`, Go `StartDVFS`).
impl<S: Soc> board::ClockKnob for Raspi<S> {
    type Knob = clock::MboxKnob;
    type KnobError = clock::Error;

    const HAS_KNOB: bool = true;

    /// Een reden als er niets te draaien valt (geen mailbox, of de firmware
    /// laat één klok over).
    fn clock_knob(&self, mhz: Option<u32>) -> Option<Result<clock::MboxKnob, clock::Error>> {
        Some(clock::knob(mhz))
    }
}

impl<S: Soc> Board for Raspi<S> {
    type Nic = S::Nic;
    type Sleeper = cpu::idle::ArmSleeper;
    /// Geen: de Pi's hebben nog geen blokdriver.
    type Disk = NoDisk;

    const NAME: &'static str = S::NAME;
    const DTB_IN_X0: bool = true;

    fn console(&self) -> fn(&[u8]) {
        S::uart().init();
        console_write::<S>
    }

    fn console_nowait(&self) -> Option<fn(&[u8]) -> usize> {
        Some(console_nowait::<S>)
    }

    fn firmware(&self) -> &'static str {
        "boot: Raspberry Pi firmware, EL2, PSCI via SMC (TF-A)"
    }

    /// De DTB uit x0 (de firmware legt hem op `device_tree_address`), dan
    /// de kaart, de pool, de staging en de mailbox. Vroeg: wat de kern
    /// straks vraagt, staat daarna in statics.
    fn discover(&self, dtb: u64) {
        let Some(f) = DTB.find(dtb, DTB_RAM) else {
            cpu::println!(
                "WARNING HOPOS_RAM_CHECK_SKIPPED: no valid DTB (x0={dtb:#x}), no pool, no staging"
            );
            self.mailbox();
            self.hardware();
            return;
        };
        MEM_TOTAL.store(f.mem_total().unwrap_or(0), Relaxed);
        let fw_cores = f.cpu_count().unwrap_or(0);
        let want = self.boot_param("hopos.cores").parse::<usize>().unwrap_or(0);
        CORES.store(cfg::cores(fw_cores, want), Relaxed);
        cpu::println!(
            "fdt: {} bytes at {dtb:#x}, model {:?}, serial {:?}, bootargs {:?}",
            f.size(),
            f.root_string("model").unwrap_or("?"),
            f.root_string("serial-number").unwrap_or("?"),
            f.bootargs().unwrap_or(""),
        );
        self.memory(&f, dtb);
        self.mailbox();
        self.hardware();
        match f.framebuffer() {
            Some(fb) => cpu::println!(
                "fb: firmware framebuffer {}x{} at {:#x} (no console on it yet)",
                fb.width,
                fb.height,
                fb.base
            ),
            None => cpu::println!("fb: no firmware framebuffer in the DTB"),
        }
    }

    fn clock(&self) -> executor::Clock {
        cpu::idle::now
    }

    /// WFE met de event-stream: op beide Pi's bewezen in de Go-kern (de
    /// stream zet de ingang, `pi_entry!`, voor EL2 aan). WFI op de timer
    /// is hier nog niet gemeten, en een slaap die niet wekt is een hang.
    fn sleeper(&self) -> Self::Sleeper {
        cpu::idle::ArmSleeper::new(cpu::idle::Mode::Wfe)
    }

    fn mem_total(&self) -> u64 {
        MEM_TOTAL.load(Relaxed)
    }

    fn cores(&self) -> usize {
        match CORES.load(Relaxed) {
            0 => CORES_DEFAULT,
            n => n,
        }
    }

    /// Homogeen (4x A72 of 4x A76): alles "big", de beste klasse die dít
    /// board heeft.
    fn core_class(&self, _core: usize) -> CoreClass {
        CoreClass::Big
    }

    fn plan(&self) -> Plan {
        Plan {
            kern_ram: KERN_RAM,
            dma: Region {
                base: Pa(map::DMA.base),
                size: map::DMA.size,
            },
            net_dma: Region {
                base: Pa(map::NET_DMA.base),
                size: map::NET_DMA.size,
            },
        }
    }

    fn start_interrupts(&self) -> Result<&'static Signal, Error> {
        let gic = S::gic();
        // Eerst schoon: een SPI die de vorige kern actief achterliet (de
        // flip van 30-09: de NIC-lijn zweeg voorgoed) zou anders nooit meer
        // melden. Eén keer, op de boot-core; de app-cores raken alleen hun
        // gebankte lijnen.
        gic.quiesce_spis();
        gic.init();
        cpu::irq::use_controller(gic);
        // De EL2-timer: de deadline van de executor terwijl een bewoner de
        // OS-core heeft (`cpu::el2::OsCore`). Zijn ack zet hem uit, zodat de
        // lijn valt tot de volgende beurt hem weer zet.
        if cpu::irq::enable(
            cpu::irq::Line(HYP_TIMER_PPI),
            Some(cpu::idle::hyp_timer_off),
            None,
        )
        .is_err()
        {
            cpu::println!(
                "irq: CNTHP PPI {HYP_TIMER_PPI} refused, the OS-core rotation runs without its timer"
            );
        }
        // De kick van de app-cores: zonder vermelding in de dispatch zou de
        // eerste claim hem als onbekende lijn uitzetten. Zijn "ack" telt
        // alleen: er is geen device om los te laten.
        if cpu::irq::enable(cpu::irq::Line(KICK_SGI), Some(cpu::el2::count_kick), None).is_err() {
            cpu::println!(
                "irq: kick SGI {KICK_SGI} refused, the OS core hears app cores only on its timer"
            );
        }
        cpu::println!("irq: {}", gic.describe());
        // Vanaf hier mag de vector komen: hij zet de vlag, wekt de
        // dispatch-taak via `cpu::irq::on_irq` en keert gemaskeerd terug.
        cpu::irq::unmask();
        Ok(&cpu::irq::IRQ_PENDING)
    }

    fn dispatch_interrupts(&self) -> Dispatched {
        let k0 = cpu::el2::OS_STATS.kicks.load(Relaxed);
        let pass = cpu::irq::global().dispatch();
        // De vector liet I dicht; de ronde is klaar, dus weer open.
        cpu::irq::unmask();
        // De kicks van deze ronde zijn geen NIC-werk.
        let kicks = cpu::el2::OS_STATS.kicks.load(Relaxed).wrapping_sub(k0);
        let kicks = u32::try_from(kicks).unwrap_or(u32::MAX);
        let other = u32::from(pass.disabled.is_some()).saturating_add(kicks);
        Dispatched {
            timer: 0,
            nic: pass.claimed.saturating_sub(other),
            other,
        }
    }

    fn framebuffer(&self) -> Option<driver_fb::Desc> {
        // De ene ontdekking van deze boot (vcfb.rs); kaal altijd `None`.
        vcfb::framebuffer(&MBOX, S::tables())
    }

    fn usb_hosts(&self) -> board::UsbHosts {
        usb::hosts::<S>()
    }

    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error> {
        if NIC_CLAIMED.load(Relaxed) {
            return Err(Error::Twice("probe_nic"));
        }
        let ctx = NicCtx {
            dma: map::NET_DMA,
            mac: cfg::mac_bytes(MAC.load(Relaxed), S::MAC_FALLBACK),
            now: cpu::idle::now,
        };
        let r = S::probe_nic(&ctx);
        if matches!(r, Ok(Some(_))) {
            NIC_CLAIMED.store(true, Relaxed);
        }
        r
    }

    /// De schijf. Een Pi heeft in deze kern geen blokapparaat (de Pi 5-NVMe
    /// via pcie1 heeft nog geen driver), dus altijd `Ok(None)`: de
    /// bestandscalls weigeren dan luid.
    fn probe_disk(&self) -> Result<Option<NoDisk>, Error> {
        Ok(None)
    }

    /// De fysieke index van de core waar dit draait.
    fn this_core(&self) -> usize {
        S::core_of(cpu::mpidr())
    }

    /// De OS-core die de bootargs vragen (`hopos.oscore=`), met een reden
    /// als het niet kan. Op de Pi blijft de kern op de boot-core: de cores
    /// zijn homogeen (elke vraag om een klasse is core 0 al), en een
    /// verhuizing zou de SPI-route van de GIC-400 (ITARGETSR wijst naar de
    /// core die `enable` riep) en de stop van de boot-core op ijzer vragen,
    /// en die zijn hier niet bewezen.
    fn os_core(&self) -> (usize, Option<&'static str>) {
        match self.boot_param("hopos.oscore") {
            "" | "0" | "small" | "mid" | "big" => (0, None),
            _ => (0, Some("the Pi keeps the OS on core 0 (GIC-400 SPI route)")),
        }
    }

    /// De kick van de OS-core voor de rotatie van `cpu::el2`: op een GIC-400
    /// is er geen ICC_SGI1R, een SGI is een MMIO-schrijf naar GICD_SGIR. Dus
    /// `sgir` = de PA daarvan (de switcher van een app-core draait met de
    /// MMU uit) en `sgi1r` = het 32-bit woord `(1 << (16 + cpu)) | intid`
    /// ([`driver_gicv2::Gic::sgi_word`]); de peek is GICC_HPPIR. Aanroepen
    /// op de OS-core, na `start_interrupts` (het masker komt uit
    /// `Gic::init`).
    fn os_bell(&self) -> cpu::el2::Bell {
        let gic = S::gic();
        cpu::el2::Bell {
            sgi1r: u64::from(gic.sgi_word(KICK_SGI)),
            sgir: gic.sgir_pa().0,
            intid: KICK_SGI,
            pending: hppir::<S>,
        }
    }

    /// De cmdline (cmdline.txt, door de firmware in /chosen/bootargs gezet).
    fn bootargs(&self) -> &'static str {
        fdt().and_then(|f| f.bootargs()).unwrap_or("")
    }

    fn nic_diag(&self) {
        S::nic_diag();
    }

    /// Stuurt de kick naar deze core zelf: de zelftest van het IPI-pad. Een
    /// SGI naar het eigen masker in de doellijst is op GICv2 gewoon een SGI
    /// (IHI 0048B 4.3.15); er is geen tweede core voor nodig, dus dit
    /// bewijst ook QEMU `raspi4b` zonder PSCI.
    fn kick_self(&self) {
        let gic = S::gic();
        gic.send_sgi(gic.sgi_word(KICK_SGI));
    }
}
