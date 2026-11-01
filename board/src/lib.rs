//! Het board-contract: wat de kern van een machine vraagt.
//!
//! De Go-kern had één `board.Board`-interface met een register (`Use`,
//! `Current`) dat bij runtime gevuld werd. Hier is het een trait met
//! statische dispatch: de binary kiest precies één board via een feature,
//! en een gemiste methode is een compilefout in plaats van een verrassing
//! op het bord (handboek §7).
//!
//! De trait is met opzet klein. Wat de Go-interface verder droeg (de
//! wandklok, PCIe, de framebuffer, het netplan, de core-start via PSCI)
//! komt erbij zodra een laag erom vraagt; een methode zonder gebruiker is
//! een methode die niemand test.
//!
//! Wat de kern van een board vraagt, staat hier, en nergens anders: de
//! binary noemt zijn board alleen als `vboard::Machine` (en de module
//! `vboard::slots` met het plan en de flip-adressen, constanten die in
//! const-context gelezen worden). Naast [`Board`] drie kleine traits die
//! elk board draagt: [`Watchdog`] (de hardware-watchdog), [`Thermal`] (de
//! thermometer) en [`ClockKnob`] (de klokknop van het klokbeleid). Een
//! eigenschap van het board is een constante van [`Board`], geen
//! `cfg!(feature = "board-...")` in de binary.
//!
//! Naast de traits staan [`heap`]: de allocator met een plafond die het
//! board over zijn kern-RAM legt, [`stage`]: het image dat een lader
//! vóór de boot neerlegde, [`cfgwin`]: `hopos.cfg` in het kern-image, en
//! [`dtb`]: de DTB van deze boot, één keer gevonden en bewaard.

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

pub mod cfgwin;
pub mod dtb;
pub mod heap;
pub mod stage;
// De USB-hosts (usb.rs): alleen met de bouwsteen `usb` (de gui- of de
// media-smaak, docs/sequence-hopos.md §14.3).
#[cfg(feature = "usb")]
mod usb;
#[cfg(feature = "usb")]
pub use usb::{MAX_USB_HOSTS, UsbHost, UsbHosts, UsbKind, usb_dma_slice};

/// Het klokbeleid: de knop van [`ClockKnob`], en de rekenkern en de taak
/// voor de binary.
pub use driver_dvfs as dvfs;
/// De framebuffer-beschrijving van [`Board::framebuffer`], zodat een board
/// hem noemt zonder eigen dependency. Alleen in de gui-smaak.
#[cfg(feature = "gui")]
pub use driver_fb as fb;

use abi::ring::Coherence;
use core::fmt;
use cpu::el2::Flavor;
use dev::Pa;
use executor::Executor;
use sync::Signal;

/// Een fysiek geheugenbereik.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Region {
    /// Het beginadres.
    pub base: Pa,
    /// De grootte in bytes.
    pub size: u64,
}

impl Region {
    /// Het eerste adres na het bereik.
    #[must_use]
    pub const fn end(&self) -> Pa {
        self.base.add(self.size)
    }

    /// Ligt `pa` in het bereik?
    #[must_use]
    pub const fn contains(&self, pa: Pa) -> bool {
        pa.0 >= self.base.0 && pa.0 < self.base.0 + self.size
    }
}

/// Het PA-plan van een board: waar de kern woont en waar de DMA-regio's
/// liggen. De tegenhanger van `layout.Plan` uit de Go-kern, voor zover de
/// kern het nu gebruikt.
#[derive(Copy, Clone, Debug)]
pub struct Plan {
    /// De kern-RAM: image, stack en heap. Normal, gecached.
    pub kern_ram: Region,
    /// De DMA-regio van alle drivers samen, buiten de kern-RAM en niet
    /// gecached gemapt.
    pub dma: Region,
    /// Het deel van `dma` dat de NIC krijgt.
    pub net_dma: Region,
}

/// De clusterklasse van een core ("small", "mid", "big"). HOP's plaatsing
/// doet exact-match op klasse.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CoreClass {
    /// Een zuinige core.
    Small,
    /// Het middensegment.
    Mid,
    /// De beste (of enige) klasse.
    Big,
}

impl fmt::Display for CoreClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Small => "small",
            Self::Mid => "mid",
            Self::Big => "big",
        })
    }
}

/// De OS-core bij de waarde `v` van `hopos.oscore` (`small`, `mid`, `big`
/// of een core-nummer) op een board met `cores` cores en klassen `class`:
/// een klasse is de eerste core van die klasse. Zonder vraag de boot-core
/// `boot`; een vraag die niet kan, geeft ook `boot`, met de reden.
pub fn os_core(
    v: &str,
    cores: usize,
    class: impl Fn(usize) -> CoreClass,
    boot: usize,
) -> (usize, Option<&'static str>) {
    let want = match v {
        "" => return (boot, None),
        "small" => Some(CoreClass::Small),
        "mid" => Some(CoreClass::Mid),
        "big" => Some(CoreClass::Big),
        _ => None,
    };
    if let Some(k) = want {
        return match (0..cores).find(|c| class(*c) == k) {
            Some(c) => (c, None),
            None => (boot, Some("no core of that class")),
        };
    }
    match v.parse::<usize>() {
        Ok(n) if n < cores => (n, None),
        Ok(_) => (boot, Some("no such core")),
        Err(_) => (boot, Some("not small, mid, big or a core number")),
    }
}

/// Waarom een board iets weigert.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Het privilege-niveau kan geen kooi dragen (EL2 op ARM, machine mode
    /// op riscv).
    Privilege {
        /// Het niveau waarop we booten (op riscv de modus: 3 = machine).
        el: u8,
    },
    /// De NIC is gevonden maar zijn initialisatie faalde.
    Nic(&'static str),
    /// De schijf is gevonden maar haar initialisatie faalde.
    Disk(&'static str),
    /// De interruptcontroller kwam niet op.
    Irq(&'static str),
    /// Een methode die maar één keer mag, werd twee keer aangeroepen.
    Twice(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Privilege { el } => write!(
                f,
                "booted at level {el}: HopOS requires EL2 (ARM; QEMU: virtualization=on) or machine mode (riscv)"
            ),
            Self::Nic(why) => write!(f, "nic init: {why}"),
            Self::Disk(why) => write!(f, "disk init: {why}"),
            Self::Irq(why) => write!(f, "interrupt controller: {why}"),
            Self::Twice(what) => write!(f, "{what} called twice"),
        }
    }
}

/// Geen schijf: het blokapparaat van een board zonder blokdriver (de Pi's,
/// de Radxa, de LicheeRV). Een lege enum, dus er bestaat nooit een waarde
/// van; hun `probe_disk` geeft altijd `Ok(None)` en de bestandscalls
/// weigeren luid.
#[derive(Debug)]
pub enum NoDisk {}

impl blkdev::Disk for NoDisk {
    fn sectors(&self) -> u64 {
        match *self {}
    }
    fn model(&self) -> &str {
        match *self {}
    }
}

impl blkdev::AsyncBlockDevice for NoDisk {
    fn max_transfer(&self) -> usize {
        match *self {}
    }
    fn start_tag(&mut self, _op: blkdev::Op<'_>) -> blkdev::Result<usize> {
        match *self {}
    }
    fn poll_tag(&mut self, _t: usize, _into: &mut [u8]) -> core::task::Poll<blkdev::Result> {
        match *self {}
    }
}

/// De eis van ARM: HopOS draait op EL2. De Go-zin `board.RequireEL2`.
pub fn require_el2(el: u8) -> Result<(), Error> {
    if el < 2 {
        return Err(Error::Privilege { el });
    }
    Ok(())
}

/// Wat de interrupt-dispatch in één ronde deed; de meetlat van de
/// IRQ-taak.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct Dispatched {
    /// Timer-interrupts.
    pub timer: u32,
    /// NIC-interrupts.
    pub nic: u32,
    /// Lijnen die niemand kent (gemeld en afgesloten).
    pub other: u32,
}

/// Waarom een board de ABI-staart van een slot niet Normal kon mappen
/// ([`Board::map_tail_normal`]).
#[derive(Copy, Clone, Debug)]
pub enum TailError {
    /// De reden van het board.
    Refused(&'static str),
    /// De reden van `cpu::memattr`.
    Attr(cpu::memattr::Error),
}

impl fmt::Display for TailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => f.write_str(why),
            Self::Attr(e) => write!(f, "{e}"),
        }
    }
}

/// De hardware-watchdog van een board: alleen het ijzer (wapenen, aaien,
/// uitzetten). Het beleid (wanneer aaien) is `kern::watchdog`, de taak is
/// van de binary. Een board zonder watchdog (QEMU virt) neemt de
/// standaarden: `arm` weigert, en de node draait onbewaakt en luid.
pub trait Watchdog {
    /// Wat er gewapend staat, voor de consoleregel.
    type Armed: fmt::Display;

    /// De timeout die de kern vraagt (Go: 12 s).
    const WD_TIMEOUT_MS: u64 = 12_000;

    /// Wapent de watchdog op `timeout_ms` (of wat het ijzer daarvan kan).
    /// `Ok` met wat er staat, of waarom niet.
    fn arm(&self, _timeout_ms: u64) -> Result<Self::Armed, &'static str> {
        Err("no watchdog on this board")
    }

    /// Herstart de teller.
    fn pet(&self) {}

    /// Een aai buiten de watchdog-taak om: vlak vóór de sprong van een
    /// flip, meteen na een landing en vóór een tweede probe van de NIC.
    /// Dan is deze kern misschien nog niet gewapend en telt de teller van
    /// de vorige door.
    fn pet_now(&self) {
        self.pet();
    }
}

/// De thermometer van een board, voor de tik en de heartbeat van Hop. Een
/// board zonder neemt de standaarden: geen meting (0).
pub trait Thermal {
    /// Eén keer bij de boot: de sensor op, één regel over wat hij geeft.
    fn open_thermal(&self) {}

    /// De temperatuur in milligraden; 0 = geen meting.
    fn temp_milli_c(&self) -> i32 {
        0
    }
}

/// De klokknop van een board voor het klokbeleid ([`dvfs`]). Een board
/// zonder knop zet [`NoKnob`] en zegt bij de boot [`ClockKnob::no_knob`].
pub trait ClockKnob {
    /// De knop.
    type Knob: dvfs::Knob + fmt::Display;
    /// Waarom de knop niet opkwam.
    type KnobError: fmt::Display;

    /// Heeft dit board een knop? Zo niet, dan vraagt de kern er niet naar
    /// en leest hij `hopos.clock` niet.
    const HAS_KNOB: bool = false;

    /// De knop, met het plafond op `mhz` als die gegeven is (`hopos.mhz`),
    /// of waarom niet. `None` op een board zonder knop.
    fn clock_knob(&self, _mhz: Option<u32>) -> Option<Result<Self::Knob, Self::KnobError>> {
        None
    }

    /// Een board zonder knop, bij de boot: één regel, of wat het in plaats
    /// van een knop heeft (de wachter van de p-states op Apple).
    fn no_knob(&self, _exec: &'static Executor) {
        cpu::println!(
            "dvfs: no clock knob on this board, the firmware keeps its clock HOPOS_CLOCK_NONE"
        );
    }
}

/// Geen knop (en geen reden): het type van een board zonder klokknop.
#[derive(Debug)]
pub enum NoKnob {}

impl dvfs::Knob for NoKnob {
    fn full(&mut self) -> Option<dvfs::Level> {
        match *self {}
    }
    fn quiet(&mut self) -> Option<dvfs::Level> {
        match *self {}
    }
}

impl fmt::Display for NoKnob {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

/// Het board-contract. Alle methodes draaien op HOP's core; de binary
/// houdt het board in een `static` en geeft `&'static` door.
pub trait Board: Sync + Watchdog + Thermal + ClockKnob {
    /// De NIC-driver van dit board.
    type Nic: netdev::Device;
    /// De slaap van de executor op dit board.
    type Sleeper: executor::Sleeper;
    /// De schijf van dit board ([`NoDisk`] zonder blokdriver).
    type Disk: blkdev::Disk;

    /// De naam, voor de bootlog.
    const NAME: &'static str;

    /// De EL2-smaak van de switcher (`cpu::el2::Flavor`): `Nvhe` (E2H = 0,
    /// slapen in WFE), `Vhe` op een kern onder E2H = 1 (de O6N: op de A720
    /// stierf een EL1 onder nVHE binnen een halve seconde, Go 17-09; en het
    /// UEFI-board met de feature `vhe`), `AppleVhe` op Apple silicium (E2H
    /// is er RES1 en de kick is de fast IPI).
    const FLAVOR: Flavor = Flavor::Nvhe;

    /// Antwoordt er een PSCI op een SMC? Niet op Apple (geen EL3: een SMC
    /// op EL2 is een UNDEF) en niet op riscv64. QEMU virt zonder
    /// `secure=on` heeft ook geen EL3, maar emuleert PSCI over SMC.
    const PSCI: bool = true;

    /// Geeft de firmware een DTB in x0 (of a1), die een warme flip moet
    /// laten staan? Niet op UEFI (ImageHandle), Apple (de boot-args) en de
    /// LicheeRV.
    const DTB_IN_X0: bool = false;

    /// Heeft het board geen bootmedium (QEMU zonder ESP)? Dan krijgt Hop
    /// `kern::nodecfg::QEMU_CFG` achter zijn config.
    const NO_BOOT_MEDIUM: bool = false;

    /// De belofte van de kern voor de frame-ringen in de pool van de slots
    /// (`abi::ring::Coherence`): `Hardware` waar de kern de pool Normal
    /// write-back mapt (de Pi's, de UEFI-boards, QEMU virt); `Maintained`
    /// waar hij hem Device mapt (Apple, de Radxa) of waar de harts niet
    /// coherent zijn (riscv64).
    const SLOT_RINGS: Coherence = Coherence::Hardware;

    /// De belofte van de host-ringen van poort 0: die liggen in de
    /// kern-heap en beide kanten zijn de kern, dus `Hardware` waar de heap
    /// Normal is en één cache deelt (Apple, riscv64), anders als
    /// [`Board::SLOT_RINGS`].
    const HOST_RINGS: Coherence = Self::SLOT_RINGS;

    /// Idlet een app-core met een yield naar de switcher (`IDLE_YIELD`)?
    /// Op QEMU virt (de kern slaapt daar in WFI, die geen SEV hoort) en op
    /// Apple (WFE op EL1 slaapt op de M4 niet). Anders kiest de app zelf.
    const APP_IDLE_YIELD: bool = false;

    /// Het geheugen van Hop in bytes: 64 MiB, op een klein board minder.
    const HOP_MEM: u64 = 64 << 20;

    /// Eén keer, als eerste: de UART op. Geeft de console-haak.
    fn console(&self) -> fn(&[u8]);

    /// Dezelfde UART zonder wachten, voor de pomp van de console in de
    /// kern (hopos `conport`): schrijft van een stuk wat er nu in de
    /// zend-FIFO past en geeft hoeveel bytes eruit zijn.
    ///
    /// `None` (de standaard): elke regel gaat meteen en wachtend naar
    /// [`Board::console`]. Dat kost de OS-core per teken de baudrate zodra
    /// de FIFO vol is (115200: 87 us per teken, een regel van 100 tekens
    /// 7,7 ms waarin de switch en de node-stack stilstaan; gemeten 04-10 op
    /// de Pi 4). Een snelle console (de dockchannel van Apple, de 16550 op
    /// 1,5 Mbaud van de Radxa) mag dat houden.
    fn console_nowait(&self) -> Option<fn(&[u8]) -> usize> {
        None
    }

    /// Het exception level waarop we booten, zoals de stub het las.
    fn privilege(&self, el: u8) -> Result<(), Error> {
        require_el2(el)
    }

    /// Eén consoleregel over wie ons bootte en wat die kan. Diagnose, geen
    /// contract.
    fn firmware(&self) -> &'static str;

    /// Geeft `heap` het deel van de kern-RAM dat na image en stack over is.
    /// Eén keer, vóór de eerste allocatie. Standaard het bereik van het
    /// linkscript ([`cpu::boot::heap_bounds`]); de UEFI-boards nemen het uit
    /// de EFI-allocatie.
    fn init_heap(&self, heap: &heap::Heap) {
        let (start, end) = cpu::boot::heap_bounds();
        // SAFETY: het linkscript van elk board met deze default legt
        // `__heap_start` achter image, BSS en stack en `__heap_end` op het
        // einde van de kern-RAM of het begin van de DMA-regio; dat bereik
        // is gemapt en van niemand anders.
        unsafe { heap.init(start, end) };
    }

    /// Leest de firmware-beschrijving (op ARM de FDT uit x0 of een vaste
    /// plek) en onthoudt wat de kern later vraagt. `dtb` is x0 bij boot.
    fn discover(&self, dtb: u64);

    /// De klok: monotone nanoseconden sinds boot.
    fn clock(&self) -> executor::Clock;

    /// De slaap van de executor.
    fn sleeper(&self) -> Self::Sleeper;

    /// DRAM in bytes zoals de firmware het meldt; 0 = onbekend.
    fn mem_total(&self) -> u64;

    /// Het aantal cores, HOP's eigen core meegeteld.
    fn cores(&self) -> usize;

    /// De clusterklasse van `core`.
    fn core_class(&self, core: usize) -> CoreClass;

    /// Het PA-plan.
    fn plan(&self) -> Plan;

    /// Zet de interruptcontroller op HOP's core, de timers en de kick aan:
    /// als eerste, vóór elk device (die bedraden hun lijn in hun probe).
    /// Geeft de bel die de IRQ-deur luidt: een taak wacht erop en roept
    /// dan [`dispatch_interrupts`](Board::dispatch_interrupts). Een lijn
    /// die niet aan wil, is een fout: de kern start dan niet
    /// (docs/sequence-hopos.md §13.3).
    fn start_interrupts(&self) -> Result<&'static Signal, Error>;

    /// Claimt, behandelt en sluit elke wachtende interrupt. Draait in een
    /// taak, nooit in exception-context.
    fn dispatch_interrupts(&self) -> Dispatched;

    /// Vindt en initialiseert de NIC, met zijn lijn (§14.1 O3): een lijn
    /// die niet aan wil is [`Error::Irq`]. `Ok(None)` = geen NIC. Eén keer
    /// per boot: na een `Err` is er geen tweede poging, de guard van de
    /// watchdog reset de node en de koude boot probeert opnieuw. De probe
    /// wacht niet op de link: hij start de autonegotiatie, en de link-wacht
    /// is van de taak (`netdev::Device::link`, vóór de pomp start), want de
    /// pets van de hardware-watchdog komen pas met de executor en een
    /// autonegotiatie duurt tot 12 s (01-11, elk board).
    fn probe_nic(&self) -> Result<Option<Self::Nic>, Error>;

    /// De lineaire framebuffer van dit board, als er een beeld loopt (GOP,
    /// de VideoCore-mailbox, `ramfb`, of de eigen scanout van de RK3566).
    /// `None` = geen beeld, en dat is geen fout. Bestaat alleen in de
    /// gui-smaak (docs/gui.md).
    #[cfg(feature = "gui")]
    fn framebuffer(&self) -> Option<fb::Desc> {
        None
    }

    /// De USB-hostcontrollers van dit board, klaar voor de xHCI-driver:
    /// PCIe-link en BAR's opgebracht, de firmware-handshake gedaan. Eén keer,
    /// na het netwerk (de invoer gaat over de switch naar de display-app).
    /// Leeg = geen USB, en dat is geen fout. Bestaat alleen met de bouwsteen
    /// `usb` (de gui- of de media-smaak); elke stap die faalt, is één
    /// logregel van het board.
    #[cfg(feature = "usb")]
    fn usb_hosts(&self) -> UsbHosts {
        UsbHosts::new()
    }

    /// Vindt en initialiseert de schijf, met zijn lijn
    /// (docs/sequence-hopos.md §14.1 O3): na
    /// [`start_interrupts`](Board::start_interrupts). `Ok(None)` = geen
    /// schijf aan dit board; een lijn die niet aan wil is
    /// [`Error::Irq`]. Eén keer (de bench leent hem, dan neemt de opslag
    /// hem).
    fn probe_disk(&self) -> Result<Option<Self::Disk>, Error>;

    /// De fysieke index van de core waar dit draait.
    fn this_core(&self) -> usize;

    /// De OS-core die de config vraagt (`hopos.oscore=<small|mid|big|N>`),
    /// met een reden als de vraag niet kon: dan de boot-core, luid.
    fn os_core(&self) -> (usize, Option<&'static str>);

    /// De kick van de OS-core voor de rotatie van `cpu::el2`: de SGI naar
    /// deze core, en de peek waarmee de kern na een terugkeer ziet of het
    /// de kick was.
    fn os_bell(&self) -> cpu::el2::Bell;

    /// Stuurt de kick naar deze core zelf: de zelftest van het IPI-pad.
    fn kick_self(&self);

    /// `hopos.cfg` als tekst: het venster in het image ([`cfgwin`]), of
    /// bij een leeg venster het bestand van het bootmedium; "" zonder.
    fn config(&self) -> &'static str {
        cfgwin::text()
    }

    /// De bootargs van de firmware (de FDT, QEMU `-append`); "" zonder.
    fn bootargs(&self) -> &'static str {
        ""
    }

    /// Eén sleutel uit [`Board::config`] en [`Board::bootargs`]
    /// (`fw::bootcfg::param`: het bestand wint); "" als hij niet gezet is.
    /// De enige weg naar een sleutel, voor de kern en voor het board zelf.
    fn boot_param(&self, key: &'static str) -> &'static str {
        fw::bootcfg::param(self.config(), self.bootargs(), key)
    }

    /// De ABI-staart van een slot Normal write-back in de kernmap (Go:
    /// `mapTailNormal`, slot-ABI 7), op een board dat de pool Device mapt
    /// (Apple, de Radxa): dan beloven de ringen van dat slot `Hardware`.
    /// `None` = de pool is al gemapt zoals [`Board::SLOT_RINGS`] zegt.
    fn map_tail_normal(&self, _pa: u64, _size: u64) -> Option<Result<(), TailError>> {
        None
    }

    /// Draagt `hopos.cfg` van het bootmedium mee in het nieuwe image van
    /// een flip, dat gestaged op `src` ligt (`len` bytes)? `true` als dat
    /// gebeurde. Alleen Apple: de plek 0xF000 van de loader.
    fn flip_carry_config(&self, _src: u64, _len: u64) -> bool {
        false
    }

    /// Geeft de nieuwe kern van een flip zaad mee (de UEFI-boards: 64 bytes
    /// van de DRBG, want het EFI_RNG_PROTOCOL is na de koude boot weg)?
    /// `true` als dat gebeurde.
    fn flip_carry_seed(&self) -> bool {
        false
    }

    /// Eén diagnose van de NIC voor de tik (de RP1-keten van de Pi 5).
    fn nic_diag(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el2_is_required() {
        assert_eq!(require_el2(1), Err(Error::Privilege { el: 1 }));
        assert!(require_el2(2).is_ok());
        assert_eq!(
            Error::Privilege { el: 1 }.to_string(),
            "booted at level 1: HopOS requires EL2 (ARM; QEMU: virtualization=on) or machine mode (riscv)"
        );
    }

    #[test]
    fn the_os_core_follows_the_config() {
        // Een O6N-achtige indeling: 0-3 small, 4-7 mid, 8-11 big.
        let class = |c: usize| match c {
            0..=3 => CoreClass::Small,
            4..=7 => CoreClass::Mid,
            _ => CoreClass::Big,
        };
        assert_eq!(os_core("", 12, class, 0), (0, None));
        assert_eq!(os_core("mid", 12, class, 0), (4, None));
        assert_eq!(os_core("big", 12, class, 0), (8, None));
        assert_eq!(os_core("11", 12, class, 0), (11, None));
        assert_eq!(os_core("12", 12, class, 0), (0, Some("no such core")));
        assert_eq!(
            os_core("fast", 12, class, 0),
            (0, Some("not small, mid, big or a core number"))
        );
        // Een vraag die niet kan, blijft op de boot-core (de M4: cpu 6).
        assert_eq!(
            os_core("small", 4, |_| CoreClass::Big, 6),
            (6, Some("no core of that class"))
        );
        assert_eq!(os_core("", 10, class, 6), (6, None));
    }

    #[test]
    fn region_bounds() {
        let r = Region {
            base: Pa(0x1000),
            size: 0x1000,
        };
        assert!(r.contains(Pa(0x1000)));
        assert!(r.contains(Pa(0x1fff)));
        assert!(!r.contains(Pa(0x2000)));
        assert_eq!(r.end(), Pa(0x2000));
        assert_eq!(CoreClass::Big.to_string(), "big");
    }
}
