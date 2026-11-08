//! De opslag en de thermometer van de Mac mini: twee coprocessors aan
//! dezelfde RTKit-bus.
//!
//! De SSD is geen PCIe-NVMe maar ANS2: een coprocessor achter een
//! RTKit-mailbox, met een SART als adresfilter (`driver_nvme::apple`,
//! `driver_rtkit`). Twee dingen die dit board anders maken dan elk ander,
//! en die allebei in [`probe_disk`] zitten:
//!
//! 1. De coprocessor moet soms eerst gereset worden. iBoot laat hem draaien
//!    met de NVMe-kant dicht; komt hij niet ready, dan is de weg terug
//!    m1n1's volgorde: netjes in slaap praten, dán het power-domein resetten
//!    (`pmgr::reset_ans`), dán opnieuw (GEMETEN 30-08: resetten zónder die
//!    slaap levert een blok op dat niets meer zegt). Pas escaleren ná een
//!    mislukte poging, nooit ervoor.
//! 2. Deze SSD is niet van ons: macOS staat erop, en Recovery. Het
//!    schrijfvenster komt uit de partitietabel van de schijf zelf
//!    ([`fw::gpt`]): het grootste stuk bruikbare schijf zonder partitie,
//!    NA of TUSSEN de partities van macOS (wie macOS krimpt met
//!    `diskutil apfs resizeContainer`, krijgt zijn ruimte tussen de
//!    container en RecoveryOS, GEMETEN 30-08). Is er geen gat, dan geen
//!    schijf: liever geen volumes dan het bestandssysteem van de eigenaar.
//!    Lezen mag de hele schijf (de GPT); schrijven weigert de driver buiten
//!    het venster.
//!
//! De lijnen (docs/sequence-hopos.md §14.1 O3, [`wire_lines`]): de
//! completion van de NVMe en de ontvangst van de mailbox van de
//! coprocessor (die wacht op een bevestiging vóór hij verder DMA doet),
//! allebei op de AIC, allebei vastgehouden tot de driver ze bediende
//! (`cpu::irq::enable_held`). ONGETOETST op ijzer.
//!
//! De thermometer is de SMC ([`temp_milli_c`]). Welke sleutel de
//! temperatuur draagt, staat nergens en verschilt per machine; eerst de
//! namen van deze generatie, anders de warmste `T`-sleutel. Alleen met
//! `hopos.smc=1`: het INITIALIZE-antwoord met het shmem-adres kwam onder de
//! Go-kern nooit (31-08), dus dit is bring-up-hygiëne, één wijziging per
//! installatie.

use crate::{BLK_DMA, fwinfo, pmgr};
use board::Error;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use cpu::idle::now;
use cpu::irq::Line;
use cpu::println;
use dev::Pa;
use driver_nvme::apple::{self as ans, Ans, Config, Coprocessor};
use driver_rtkit::{Rtkit, ignore, sart::Sart};
use driver_smc::{Smc, key};
use sync::Signal;

/// De ANS-coprocessor voor `driver_nvme::apple`, op RTKit.
pub struct AnsRtkit(pub Rtkit);

impl Coprocessor for AnsRtkit {
    type Error = driver_rtkit::Error;
    fn boot(&mut self) -> Result<(), Self::Error> {
        self.0.boot(&mut ignore)
    }
    /// De mailbox leeg, en daarna de lijnen weer open ([`wire_lines`]):
    /// de driver roept dit bij elke submit en bij elke bel zolang er iets
    /// loopt.
    fn service(&mut self) -> Result<(), Self::Error> {
        let r = self.0.poll(&mut ignore).map(drop);
        for l in &ANS_LINES {
            match l.load(Relaxed) {
                0 => {}
                n => cpu::irq::release(Line(n)),
            }
        }
        r
    }
    fn sleep(&mut self) -> Result<(), Self::Error> {
        self.0.sleep(&mut ignore)
    }
    fn crashlog(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        write!(out, "{}", self.0.crashlog())
    }
}

/// De schijf van dit board: het venster op de ANS.
pub type Disk = Ans<AnsRtkit>;

/// De eerste 4 MB van de opslag-DMA: queues, TCB's, PRP en databuffer.
const ANS_DMA: (u64, u64) = (BLK_DMA.base.0, driver_nvme::DMA_NEED);
/// Het datablok van de ANS (2 MB, 2 MB-gealigneerd): de driver doet daar
/// zelf het cache-onderhoud (`dev::push` na het vullen, `dev::pull` vóór
/// het lezen), dus het board mapt het Normal WB ([`crate::mmu`]). Normal-NC
/// kostte het leespad zijn snelheid: elke byte die de kern uit de buffer
/// kopieerde, was een ongecachete load (GEMETEN 01-10 op M9: lezen 158 tot
/// 168 MB/s tegen schrijven 723 tot 780; Go mapte dit blok gecached).
pub(crate) const ANS_DATA: (u64, u64) = (ANS_DMA.0 + driver_nvme::DATA_OFF, driver_nvme::DATA_SIZE);
/// Daarachter de buffers die de ANS bij zijn opstart vraagt (syslog,
/// crashlog, ioreport).
const ANS_POOL: (u64, u64) = (BLK_DMA.base.0 + driver_nvme::DMA_NEED, 0x30_0000);
/// En die van de SMC.
const SMC_POOL: (u64, u64) = (ANS_POOL.0 + ANS_POOL.1, 0x10_0000);

const _: () = {
    assert!(SMC_POOL.0 + SMC_POOL.1 == BLK_DMA.base.0 + BLK_DMA.size);
    assert!(ANS_DMA.0.is_multiple_of(ans::DMA_ALIGN));
    assert!(ANS_DATA.0.is_multiple_of(2 << 20) && ANS_DATA.1 == 2 << 20);
    assert!(ANS_DATA.0 + ANS_DATA.1 == ANS_DMA.0 + ANS_DMA.1);
    assert!(ANS_POOL.0.is_multiple_of(driver_rtkit::BUF_ALIGN));
    assert!(SMC_POOL.0.is_multiple_of(driver_rtkit::BUF_ALIGN));
};

static DISK_CLAIMED: AtomicBool = AtomicBool::new(false);

/// De bel van de schijf: de completion van de NVMe en de ontvangst van de
/// mailbox luiden hem, de wachter van `blkdev::Queue` slaapt erop.
static DISK_BELL: Signal = Signal::new();

/// De AIC-lijnen van de ANS die [`AnsRtkit::service`] weer opent: de
/// ontvangst van de mailbox en de NVMe; 0 = (nog) niet bedraad.
static ANS_LINES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// Zet de ANS op zijn lijnen: uit `/arm-io/ans` "interrupts", in de
/// volgorde die Linux' devicetree van de M1 er ook heeft (t8103.dtsi): de
/// vier van de mailbox (send-empty, send-not-empty, recv-empty,
/// recv-not-empty) en daarna die van de NVMe. Wij nemen recv-not-empty
/// (woord 3) en de laatste. Beide zijn level en alleen de driver laat ze
/// zakken (de CQ-deurbel, de mailbox leeglezen): de dispatch houdt ze
/// dicht tot [`AnsRtkit::service`]. Geen stilte-toets (zie hieronder). De
/// volgorde is die van de M1; op de M4 (08-11) gaf de ADT er vijf (1005 is
/// woord 3, 1018 de laatste).
fn wire_lines(ints: &[u8], disk: &mut Disk) -> Result<(), Error> {
    let word = |i: usize| {
        let w = ints.get(i * 4..i * 4 + 4)?;
        Some(u32::from_le_bytes(w.try_into().ok()?))
    };
    let n = ints.len() / 4;
    let (Some(mbox), Some(nvme)) = (word(3), n.checked_sub(1).and_then(word)) else {
        return Err(Error::Irq("/arm-io/ans has fewer than four interrupts"));
    };
    if n < 5 {
        return Err(Error::Irq("/arm-io/ans has fewer than five interrupts"));
    }
    // Beide lijnen aan, zonder stilte-toets: de ANS2 van de M4 laat de
    // NVMe-lijn ook gaan zonder completion (08-11, op ijzer: 1018 vuurde op
    // een idle ANS), en Linux eist dat ook niet (`apple_nvme_irq` kijkt in
    // beide CQ's en geeft IRQ_NONE). Een loze bel wekt de wachter, die
    // niets vindt en weer slaapt; een lijn die nooit komt, laat de queue op
    // zijn termijn luid falen.
    for (slot, n) in ANS_LINES.iter().zip([mbox, nvme]) {
        cpu::irq::enable_held(Line(n), &DISK_BELL)
            .map_err(|_| Error::Irq("the AIC refused an ANS line"))?;
        slot.store(n, Relaxed);
    }
    disk.coprocessor().service().map_err(|e| {
        println!("disk: ans: {e}");
        Error::Disk("the ANS mailbox failed")
    })?;
    disk.set_irq(&DISK_BELL);
    println!(
        "disk: ans on AIC lines {mbox} (mailbox recv) and {nvme} (NVMe), words 3 and {} of {n} HOPOS_ANS_IRQ",
        n - 1
    );
    Ok(())
}

/// Eén poging: de mailbox, de controller, en `start`. Weigert `start`, dan
/// gaat de coprocessor eerst netjes in slaap (m1n1's volgorde: slaap, dán
/// de power-reset).
fn open(asc: u64, nvmmu: u64, nvme: u64, secure_bar: bool) -> Result<Disk, &'static str> {
    // SAFETY: `asc` is `/arm-io/ans` reg[0] (GEMETEN 29-08: 0x4_8160_0000),
    // onder 512 GB en dus Device-gemapt; ANS_POOL is van deze coprocessor
    // alleen, Normal-NC, en staat in het SART-venster dat `probe_disk` net
    // opende.
    let rt = unsafe { Rtkit::new(Pa(asc), "ans", Pa(ANS_POOL.0), ANS_POOL.1, now) }
        .map_err(|_| "rtkit pool refused")?;
    let cfg = Config {
        nvme: Pa(nvme),
        nvmmu: Pa(nvmmu),
        dma: Pa(ANS_DMA.0),
        dma_size: ANS_DMA.1,
        secure_bar,
    };
    // SAFETY: de NVMe- en NVMMU-vensters komen uit de ADT (reg[9] en reg[3])
    // en zijn Device-gemapt; ANS_DMA is van deze driver alleen, Normal-NC,
    // in het SART-venster, en DMA-adres == fysiek adres.
    let mut d = unsafe { Ans::new(cfg, AnsRtkit(rt), now) }.map_err(|e| {
        println!("disk: ans: {e}");
        "ans refused its regions"
    })?;
    match d.start() {
        Ok(()) => Ok(d),
        Err(e) => {
            println!("disk: ans: {e} ({})", d.diag());
            let _ = d.coprocessor().sleep();
            Err("ans did not come ready")
        }
    }
}

/// Brengt de ANS op en geeft het venster na de partities van macOS.
/// `Ok(None)` = geen ANS, geen gat, of `hopos.disk=off`.
pub(crate) fn probe_disk(off: bool) -> Result<Option<Disk>, Error> {
    if DISK_CLAIMED.swap(true, Relaxed) {
        return Err(Error::Twice("probe_disk"));
    }
    if off {
        println!("disk: hopos.disk=off, the internal SSD stays untouched");
        return Ok(None);
    }
    let t = fwinfo::adt().ok_or(Error::Disk("no device tree"))?;
    let Some(node) = t.path("/arm-io/ans") else {
        return Ok(None);
    };
    let reg = |i| t.reg("/arm-io/ans", i).map_or(0, |r| r.0);
    let (asc, nvmmu) = (reg(0), reg(3));
    // M4 (`nvme-secure-bar`): NVMe in reg[9]; M1-M3: één venster, reg[3].
    let nvme = match reg(9) {
        0 => nvmmu,
        n => n,
    };
    let secure_bar = t.prop(node, "nvme-secure-bar").is_some();
    let (sart, _) = t
        .reg("/arm-io/sart-ans", 0)
        .ok_or(Error::Disk("no /arm-io/sart-ans"))?;
    let version = t
        .path("/arm-io/sart-ans")
        .and_then(|s| t.u32(s, "sart-version"))
        .unwrap_or(driver_rtkit::sart::VERSION);
    println!(
        "disk: ans {asc:#x} nvmmu {nvmmu:#x} nvme {nvme:#x} sart {sart:#x} (v{version}) secure-bar {secure_bar}"
    );
    if asc == 0 || nvme == 0 {
        return Err(Error::Disk("ans reg windows missing"));
    }
    // SAFETY: `/arm-io/sart-ans` reg[0], Device-gemapt; alleen deze code
    // schrijft er, één keer bij boot.
    unsafe { Sart::new(Pa(sart), version) }
        .and_then(|mut s| s.allow(BLK_DMA.base, BLK_DMA.size))
        .map_err(|e| {
            println!("disk: {e}");
            Error::Disk("SART would not open a window for the queues")
        })?;

    let mut disk = match open(asc, nvmmu, nvme, secure_bar) {
        Ok(d) => d,
        Err(why) => {
            crate::serror_check("the first ANS open");
            println!("disk: {why}, resetting the ANS power domain and retrying");
            let n = pmgr::reset_ans();
            if n == 0 {
                return Err(Error::Disk("no ANS power domain to reset"));
            }
            dev::delay(now, 100_000_000);
            crate::serror_check("the ANS reset");
            open(asc, nvmmu, nvme, secure_bar).map_err(Error::Disk)?
        }
    };
    let mut block = [0u8; 4096];
    let table = fw::gpt::read(|lba, b| disk.read_at(lba, b).is_ok(), &mut block).map_err(|e| {
        println!("disk: {e}");
        Error::Disk("no partition table on the internal SSD")
    })?;
    for p in table.parts.iter() {
        println!(
            "gpt: {:<24} LBA {}..{} ({} MB)",
            p.name(),
            p.first,
            p.last,
            (p.blocks() * ans::BLOCK) >> 20
        );
    }
    let (first, count) = fw::gpt::largest_gap(&table);
    if count == 0 {
        println!(
            "disk: the internal SSD is full (macOS, Recovery); free space with `diskutil apfs resizeContainer` first HOPOS_ANS_FULL"
        );
        let _ = disk.shutdown();
        return Ok(None);
    }
    disk.set_window(first, count)
        .map_err(|_| Error::Disk("the free gap lies outside the usable GPT area"))?;
    println!(
        "disk: {} {} blocks of {}, write window LBA {first}..{} ({} MB) after macOS' partitions HOPOS_ANS_UP",
        disk.model(),
        disk.blocks(),
        disk.block_size(),
        first + count - 1,
        (count * ans::BLOCK) >> 20
    );
    wire_lines(t.prop(node, "interrupts").unwrap_or(&[]), &mut disk)?;
    Ok(Some(disk))
}

/// De temperatuur van de die, één meting via de SMC (`hopos.smc=1`), en de
/// SMC daarna weer in slaap. `None` = onbekend; een board zonder
/// thermometer is een board zonder thermometer.
pub(crate) fn temp_milli_c(smc: bool) -> Option<i32> {
    if !smc {
        return None;
    }
    let (base, _) = fwinfo::reg("/arm-io/smc", 0)?;
    let (sram, sram_size) = fwinfo::reg("/arm-io/smc", 1)?;
    // SAFETY: `/arm-io/smc` reg[0] is het ASC-blok van de SMC (Device-
    // gemapt); SMC_POOL is van deze coprocessor alleen.
    let rt = unsafe { Rtkit::new(Pa(base), "smc", Pa(SMC_POOL.0), SMC_POOL.1, now) }.ok()?;
    // SAFETY: reg[1] is de SRAM van de SMC, Device-gemapt, voor altijd.
    let mut smc = match unsafe { Smc::open(rt, Pa(sram), sram_size) } {
        Ok(s) => s,
        Err(e) => {
            println!("smc: {e}, no die temperature on this node");
            return None;
        }
    };
    let named = ["TC0P", "Tp09", "Tp0T", "TG0D"]
        .into_iter()
        .find_map(|k| smc.float(key(k)).ok());
    let c = named.or_else(|| smc.hottest().ok().flatten().map(|s| s.celsius));
    let _ = smc.sleep();
    c.map(|c| (c * 1000.0) as i32)
}
